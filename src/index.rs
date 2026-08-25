use crate::render::truncate_chars;
use crate::semantic::SemanticIndex;
use crate::update;
use anyhow::{anyhow, bail, Context, Result};
use protobuf::{Message, MessageField};
use scip::types::Index;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    TypeScript,
    Python,
    Rust,
    Dart,
    Java,
    Cpp,
}

impl Language {
    fn as_str(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Dart => "dart",
            Self::Java => "java",
            Self::Cpp => "cpp",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

type IndexerCommand = (String, Vec<String>);

struct LanguageIndexer {
    language: Language,
    markers: &'static [&'static str],
    command: fn(&Path, &Path, Option<u64>) -> IndexerCommand,
    install_hint: &'static str,
}

const LANGUAGE_INDEXERS: &[LanguageIndexer] = &[
    LanguageIndexer {
        language: Language::TypeScript,
        markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        command: typescript_indexer_command,
        install_hint: "needs node/npx",
    },
    LanguageIndexer {
        language: Language::Python,
        markers: &["pyproject.toml", "setup.py", "requirements.txt"],
        command: python_indexer_command,
        install_hint: "needs node/npx",
    },
    LanguageIndexer {
        language: Language::Rust,
        markers: &["Cargo.toml"],
        command: rust_indexer_command,
        install_hint: "install rust-analyzer (rustup component add rust-analyzer)",
    },
    LanguageIndexer {
        language: Language::Dart,
        markers: &["pubspec.yaml"],
        command: dart_indexer_command,
        install_hint: "dart pub global activate scip_dart",
    },
    LanguageIndexer {
        language: Language::Java,
        markers: &[
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        command: java_indexer_command,
        install_hint: "install coursier (brew install coursier) — scip-java runs via cs launch",
    },
    LanguageIndexer {
        language: Language::Cpp,
        markers: &["compile_commands.json", "build/compile_commands.json"],
        command: cpp_indexer_command,
        install_hint: "download scip-clang from github.com/sourcegraph/scip-clang/releases",
    },
];

#[derive(Clone)]
pub(crate) struct LoadedIndex {
    pub(crate) index: Arc<SemanticIndex>,
    file_size: u64,
}

struct CachedIndex {
    modified: SystemTime,
    loaded: LoadedIndex,
}

#[derive(Default)]
pub(crate) struct IndexCache {
    entries: HashMap<PathBuf, CachedIndex>,
}

impl IndexCache {
    pub(crate) fn load(&mut self, project_root: &Path) -> Result<LoadedIndex> {
        let loaded = self.load_for_check(project_root)?;
        if let Some(message) = IndexStats::from_loaded(&loaded).empty_index_message() {
            bail!("{message}");
        }
        Ok(loaded)
    }

    pub(crate) fn load_for_check(&mut self, project_root: &Path) -> Result<LoadedIndex> {
        validate_project_root(project_root)?;
        let path = index_path(project_root);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                bail!("index not found; call scip_index first")
            }
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let modified = metadata
            .modified()
            .with_context(|| format!("read mtime for {}", path.display()))?;

        if let Some(cached) = self.entries.get(project_root) {
            if cached.modified == modified {
                return Ok(cached.loaded.clone());
            }
        }

        let loaded = load_index_file(&path, &metadata)?;
        self.entries.insert(
            project_root.to_path_buf(),
            CachedIndex {
                modified,
                loaded: loaded.clone(),
            },
        );
        Ok(loaded)
    }

    fn invalidate(&mut self, project_root: &Path) {
        self.entries.remove(project_root);
    }
}

fn load_index_file(path: &Path, metadata: &fs::Metadata) -> Result<LoadedIndex> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let index =
        Index::parse_from_bytes(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(LoadedIndex {
        index: Arc::new(SemanticIndex::new(index)),
        file_size: metadata.len(),
    })
}

fn load_uncached_index(path: &Path) -> Result<LoadedIndex> {
    let metadata = fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    load_index_file(path, &metadata)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexStats {
    documents: usize,
    symbols: usize,
    occurrences: usize,
    file_size: u64,
}

impl IndexStats {
    pub(crate) fn from_loaded(loaded: &LoadedIndex) -> Self {
        Self {
            documents: loaded.index.index().documents.len(),
            symbols: loaded
                .index
                .index()
                .documents
                .iter()
                .map(|document| document.symbols.len())
                .sum::<usize>()
                + loaded.index.index().external_symbols.len(),
            occurrences: loaded
                .index
                .index()
                .documents
                .iter()
                .map(|document| document.occurrences.len())
                .sum(),
            file_size: loaded.file_size,
        }
    }

    pub(crate) fn compact(self) -> String {
        format!(
            "documents {} | symbols {} | occurrences {} | size {} B",
            self.documents, self.symbols, self.occurrences, self.file_size
        )
    }

    pub(crate) fn empty_index_message(self) -> Option<String> {
        if self.documents == 0 {
            Some(
                "index is empty (0 documents) — likely a crashed indexer; run scip_index to rebuild"
                    .to_string(),
            )
        } else if self.symbols == 0 {
            Some(
                "index is empty (0 symbols) — likely a crashed indexer; run scip_index to rebuild"
                    .to_string(),
            )
        } else {
            None
        }
    }
}

fn validate_project_root(project_root: &Path) -> Result<()> {
    if !project_root.is_absolute() {
        bail!("project_root must be an absolute path");
    }
    if !project_root.is_dir() {
        bail!(
            "project_root is not a directory: {}",
            project_root.display()
        );
    }
    Ok(())
}

pub(crate) fn index_path(project_root: &Path) -> PathBuf {
    project_root.join(".scip-nav").join("index.scip")
}

fn pending_index_path(project_root: &Path) -> PathBuf {
    project_root.join(".scip-nav").join("index.scip.tmp")
}

fn language_index_path(project_root: &Path, language: Language) -> PathBuf {
    project_root
        .join(".scip-nav")
        .join(format!("index-{language}.scip"))
}

fn pending_language_index_path(project_root: &Path, language: Language) -> PathBuf {
    project_root
        .join(".scip-nav")
        .join(format!("index-{language}.scip.tmp"))
}

fn path_argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn typescript_indexer_command(
    _project_root: &Path,
    output_path: &Path,
    max_file_mb: Option<u64>,
) -> IndexerCommand {
    let mut args = vec![
        "--yes".to_string(),
        "@sourcegraph/scip-typescript".to_string(),
        "index".to_string(),
        "--output".to_string(),
        path_argument(output_path),
    ];
    if let Some(max_file_mb) = max_file_mb {
        args.extend([
            "--max-file-byte-size".to_string(),
            format!("{max_file_mb}mb"),
        ]);
    }
    ("npx".to_string(), args)
}

fn python_indexer_command(
    _project_root: &Path,
    output_path: &Path,
    _max_file_mb: Option<u64>,
) -> IndexerCommand {
    (
        "npx".to_string(),
        vec![
            "--yes".to_string(),
            "@sourcegraph/scip-python".to_string(),
            "index".to_string(),
            ".".to_string(),
            "--output".to_string(),
            path_argument(output_path),
        ],
    )
}

fn rust_indexer_command(
    _project_root: &Path,
    output_path: &Path,
    _max_file_mb: Option<u64>,
) -> IndexerCommand {
    (
        "rust-analyzer".to_string(),
        vec![
            "scip".to_string(),
            ".".to_string(),
            "--output".to_string(),
            path_argument(output_path),
        ],
    )
}

fn dart_indexer_command(
    _project_root: &Path,
    output_path: &Path,
    _max_file_mb: Option<u64>,
) -> IndexerCommand {
    (
        "dart".to_string(),
        vec![
            "pub".to_string(),
            "global".to_string(),
            "run".to_string(),
            "scip_dart".to_string(),
            ".".to_string(),
            "--output".to_string(),
            path_argument(output_path),
        ],
    )
}

fn java_indexer_command(
    _project_root: &Path,
    output_path: &Path,
    _max_file_mb: Option<u64>,
) -> IndexerCommand {
    (
        "cs".to_string(),
        vec![
            "launch".to_string(),
            "com.sourcegraph:scip-java_2.13:latest.stable".to_string(),
            "--".to_string(),
            "index".to_string(),
            "--output".to_string(),
            path_argument(output_path),
        ],
    )
}

fn cpp_indexer_command(
    project_root: &Path,
    output_path: &Path,
    _max_file_mb: Option<u64>,
) -> IndexerCommand {
    let root_compdb = project_root.join("compile_commands.json");
    let build_compdb = project_root.join("build").join("compile_commands.json");
    let compdb = if build_compdb.is_file() && !root_compdb.is_file() {
        build_compdb
    } else {
        root_compdb
    };
    (
        "scip-clang".to_string(),
        vec![
            "--compdb-path".to_string(),
            path_argument(&compdb),
            "--index-output-path".to_string(),
            path_argument(output_path),
        ],
    )
}

fn language_indexer(language: Language) -> &'static LanguageIndexer {
    LANGUAGE_INDEXERS
        .iter()
        .find(|indexer| indexer.language == language)
        .expect("all languages have an indexer")
}

fn indexer_command(
    language: Language,
    project_root: &Path,
    output_path: &Path,
    max_file_mb: Option<u64>,
) -> IndexerCommand {
    (language_indexer(language).command)(project_root, output_path, max_file_mb)
}

fn parse_language(language: &str) -> Result<Language> {
    match language {
        "typescript" => Ok(Language::TypeScript),
        "python" => Ok(Language::Python),
        "rust" => Ok(Language::Rust),
        "dart" => Ok(Language::Dart),
        "java" => Ok(Language::Java),
        "cpp" => Ok(Language::Cpp),
        _ => bail!(
            "unsupported language: {language} (expected typescript|python|rust|dart|java|cpp)"
        ),
    }
}

fn detect_languages(project_root: &Path) -> Vec<Language> {
    LANGUAGE_INDEXERS
        .iter()
        .filter(|indexer| {
            indexer
                .markers
                .iter()
                .any(|marker| project_root.join(marker).is_file())
        })
        .map(|indexer| indexer.language)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct LanguageSelection {
    languages: Vec<Language>,
    also_detected: Vec<Language>,
}

fn select_languages(
    project_root: &Path,
    override_language: Option<&str>,
    override_languages: Option<&[String]>,
) -> Result<LanguageSelection> {
    if override_language.is_some() && override_languages.is_some() {
        bail!("pass only one of language or languages");
    }

    let detected = detect_languages(project_root);
    if let Some(override_language) = override_language {
        let languages = vec![parse_language(override_language)?];
        let also_detected = detected
            .into_iter()
            .filter(|language| !languages.contains(language))
            .collect();
        return Ok(LanguageSelection {
            languages,
            also_detected,
        });
    }

    if let Some(override_languages) = override_languages {
        if override_languages.is_empty() {
            bail!("languages must contain at least one language");
        }

        let mut languages = Vec::new();
        for language in override_languages {
            let language = parse_language(language)?;
            if !languages.contains(&language) {
                languages.push(language);
            }
        }
        let also_detected = detected
            .into_iter()
            .filter(|language| !languages.contains(language))
            .collect();
        return Ok(LanguageSelection {
            languages,
            also_detected,
        });
    }

    if detected.is_empty() {
        bail!(
            "could not auto-detect a supported language — add a project marker or pass language (typescript|python|rust|dart|java|cpp)"
        );
    }

    Ok(LanguageSelection {
        languages: detected,
        also_detected: Vec::new(),
    })
}

struct LanguageIndexResult {
    language: Language,
    documents: usize,
}

struct LanguageIndexFailure {
    language: Language,
    message: String,
}

fn format_index_stats(
    stats: IndexStats,
    successes: &[LanguageIndexResult],
    failures: &[LanguageIndexFailure],
    selection: &LanguageSelection,
    warnings: &[String],
) -> String {
    let indexed = successes
        .iter()
        .map(|result| format!("{} {} documents", result.language, result.documents))
        .collect::<Vec<_>>()
        .join(", ");
    let mut output = format!("indexed: {indexed} | merged: {}", stats.compact());

    if !failures.is_empty() {
        let failures = failures
            .iter()
            .map(|failure| format!("{} ({})", failure.language, failure.message))
            .collect::<Vec<_>>()
            .join("; ");
        output.push_str(&format!("\nfailed: {failures}"));
    }
    if !selection.also_detected.is_empty() {
        let also_detected = selection
            .also_detected
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "\nalso detected: {also_detected} — pass language to index it"
        ));
    }

    for warning in warnings {
        output.push_str("\nwarning: ");
        output.push_str(warning);
    }
    output
}

fn stderr_tail(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(5);
    lines[start..]
        .iter()
        .map(|line| truncate_chars(line, 300))
        .collect::<Vec<_>>()
        .join("\n")
}

fn exit_status_detail(status: &ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }

    status
        .code()
        .map(|code| format!("exit status {code}"))
        .unwrap_or_else(|| "unknown exit status".to_string())
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn indexer_failure(
    language: Language,
    output: &Output,
    detail: Option<&str>,
    pending_path: &Path,
) -> anyhow::Error {
    let cleanup_error = remove_file_if_present(pending_path).err();
    let mut message = format!(
        "indexer for {language} failed ({})",
        exit_status_detail(&output.status)
    );
    if let Some(detail) = detail {
        message.push_str(": ");
        message.push_str(detail);
    }
    let tail = stderr_tail(&output.stderr);
    if !tail.is_empty() {
        message.push_str("\nstderr (last 5 lines):\n");
        message.push_str(&tail);
    }
    if let Some(cleanup_error) = cleanup_error {
        message.push_str("\ncleanup failed: ");
        message.push_str(&format!("{cleanup_error:#}"));
    }
    anyhow!(message)
}

fn execute_indexer_command(
    program: &str,
    args: &[String],
    project_root: &Path,
    language: Language,
) -> Result<Output> {
    match Command::new(program)
        .args(args)
        .current_dir(project_root)
        .output()
    {
        Ok(output) => Ok(output),
        Err(error) if error.kind() == io::ErrorKind::NotFound => bail!(
            "indexer for {language} not found — {}",
            language_indexer(language).install_hint
        ),
        Err(error) => Err(error).with_context(|| format!("run indexer for {language}")),
    }
}

struct MergeOutput {
    index: Index,
    warnings: Vec<String>,
}

fn merge_indexes(indexes: Vec<Index>) -> Result<MergeOutput> {
    let mut merged = Index::default();
    let mut document_paths = HashSet::new();
    let mut external_symbols = HashSet::new();
    let mut warnings = Vec::new();

    for mut index in indexes {
        if let Some(metadata) = index.metadata.take() {
            if let Some(first_metadata) = merged.metadata.as_ref() {
                if first_metadata.project_root != metadata.project_root {
                    bail!(
                        "conflicting metadata.project_root values: '{}' and '{}'",
                        first_metadata.project_root,
                        metadata.project_root
                    );
                }
            } else {
                merged.metadata = MessageField::some(metadata);
            }
        }

        for document in index.documents.drain(..) {
            if document_paths.insert(document.relative_path.clone()) {
                merged.documents.push(document);
            } else {
                warnings.push(format!(
                    "duplicate document relative_path '{}'; kept the first document",
                    document.relative_path
                ));
            }
        }

        for symbol in index.external_symbols.drain(..) {
            if external_symbols.insert(symbol.symbol.clone()) {
                merged.external_symbols.push(symbol);
            }
        }
    }

    Ok(MergeOutput {
        index: merged,
        warnings,
    })
}

fn language_index_paths(project_root: &Path) -> Result<Vec<PathBuf>> {
    let directory = project_root.join(".scip-nav");
    let entries =
        fs::read_dir(&directory).with_context(|| format!("read {}", directory.display()))?;
    let mut paths = Vec::new();

    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", directory.display()))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with("index-") && file_name.ends_with(".scip") && entry.path().is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn write_index_atomically(index: &Index, pending_path: &Path, destination: &Path) -> Result<()> {
    remove_file_if_present(pending_path)?;
    let bytes = index.write_to_bytes().context("serialize merged index")?;
    if let Err(error) = fs::write(pending_path, bytes) {
        let cleanup_error = remove_file_if_present(pending_path).err();
        let mut error = anyhow!(error).context(format!("write {}", pending_path.display()));
        if let Some(cleanup_error) = cleanup_error {
            error = error.context(format!("{cleanup_error:#}"));
        }
        return Err(error);
    }
    if let Err(error) = fs::rename(pending_path, destination) {
        let cleanup_error = remove_file_if_present(pending_path).err();
        let mut error = anyhow!(error).context(format!(
            "replace {} with {}",
            destination.display(),
            pending_path.display()
        ));
        if let Some(cleanup_error) = cleanup_error {
            error = error.context(format!("{cleanup_error:#}"));
        }
        return Err(error);
    }
    Ok(())
}

fn merge_language_indexes(
    project_root: &Path,
    cache: &mut IndexCache,
) -> Result<(LoadedIndex, Vec<String>)> {
    let paths = language_index_paths(project_root)?;
    if paths.is_empty() {
        bail!("no per-language indexes found");
    }

    let mut indexes = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let index =
            Index::parse_from_bytes(&bytes).with_context(|| format!("parse {}", path.display()))?;
        indexes.push(index);
    }

    let merged = merge_indexes(indexes)?;
    write_index_atomically(
        &merged.index,
        &pending_index_path(project_root),
        &index_path(project_root),
    )?;
    cache.invalidate(project_root);
    let loaded = load_uncached_index(&index_path(project_root))?;
    Ok((loaded, merged.warnings))
}

fn build_index(
    project_root: &Path,
    language: Language,
    command: IndexerCommand,
) -> Result<LoadedIndex> {
    let pending_path = pending_language_index_path(project_root, language);
    remove_file_if_present(&pending_path)?;

    let (program, args) = command;
    let output = match execute_indexer_command(&program, &args, project_root, language) {
        Ok(output) => output,
        Err(error) => {
            return match remove_file_if_present(&pending_path) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(error.context(format!("{cleanup_error:#}"))),
            };
        }
    };

    if !output.status.success() {
        return Err(indexer_failure(language, &output, None, &pending_path));
    }

    let loaded = match load_uncached_index(&pending_path) {
        Ok(loaded) if !loaded.index.index().documents.is_empty() => loaded,
        Ok(_) => {
            return Err(indexer_failure(
                language,
                &output,
                Some("output index contains no documents"),
                &pending_path,
            ));
        }
        Err(error) => {
            let detail = format!("output index is invalid: {error:#}");
            return Err(indexer_failure(
                language,
                &output,
                Some(&detail),
                &pending_path,
            ));
        }
    };

    let destination = language_index_path(project_root, language);
    if let Err(error) = fs::rename(&pending_path, &destination) {
        let detail = format!(
            "replace {} with {}: {error}",
            destination.display(),
            pending_path.display()
        );
        return Err(indexer_failure(
            language,
            &output,
            Some(&detail),
            &pending_path,
        ));
    }

    Ok(loaded)
}

pub(crate) fn run_indexer(
    project_root: &Path,
    override_language: Option<&str>,
    override_languages: Option<&[String]>,
    max_file_mb: Option<u64>,
    cache: &mut IndexCache,
) -> Result<String> {
    run_indexer_with(
        project_root,
        override_language,
        override_languages,
        max_file_mb,
        cache,
        indexer_command,
    )
}

fn run_indexer_with<F>(
    project_root: &Path,
    override_language: Option<&str>,
    override_languages: Option<&[String]>,
    max_file_mb: Option<u64>,
    cache: &mut IndexCache,
    mut command_builder: F,
) -> Result<String>
where
    F: FnMut(Language, &Path, &Path, Option<u64>) -> IndexerCommand,
{
    validate_project_root(project_root)?;
    if max_file_mb == Some(0) {
        bail!("max_file_mb must be at least 1");
    }
    let selection = select_languages(project_root, override_language, override_languages)?;
    fs::create_dir_all(project_root.join(".scip-nav"))
        .with_context(|| format!("create {}/.scip-nav", project_root.display()))?;
    let pending_update = update::start_background_check(&project_root.join(".scip-nav"));
    let mut successes = Vec::new();
    let mut failures = Vec::new();

    for language in selection.languages.iter().copied() {
        let pending_path = pending_language_index_path(project_root, language);
        let command = command_builder(language, project_root, &pending_path, max_file_mb);
        match build_index(project_root, language, command) {
            Ok(loaded) => successes.push(LanguageIndexResult {
                language,
                documents: loaded.index.index().documents.len(),
            }),
            Err(error) => failures.push(LanguageIndexFailure {
                language,
                message: format!("{error:#}"),
            }),
        }
    }

    if successes.is_empty() {
        let failures = failures
            .iter()
            .map(|failure| format!("{} ({})", failure.language, failure.message))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("all selected languages failed: {failures}");
    }

    let (loaded, warnings) = merge_language_indexes(project_root, cache)?;
    let mut stats = format_index_stats(
        IndexStats::from_loaded(&loaded),
        &successes,
        &failures,
        &selection,
        &warnings,
    );
    if let Some(notice) = pending_update.and_then(|pending| pending.try_notice()) {
        stats.push('\n');
        stats.push_str(&notice);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use scip::types::{Document, Metadata, SymbolInformation};
    use std::thread;
    use std::time::Duration;

    fn test_index(
        project_root: &str,
        language: &str,
        relative_paths: &[&str],
        external_symbols: &[(&str, &str)],
    ) -> Index {
        Index {
            metadata: MessageField::some(Metadata {
                project_root: project_root.to_string(),
                ..Default::default()
            }),
            documents: relative_paths
                .iter()
                .map(|relative_path| Document {
                    language: language.to_string(),
                    relative_path: (*relative_path).to_string(),
                    ..Default::default()
                })
                .collect(),
            external_symbols: external_symbols
                .iter()
                .map(|(symbol, display_name)| SymbolInformation {
                    symbol: (*symbol).to_string(),
                    display_name: (*display_name).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn write_index_file(path: &Path, index: &Index) {
        fs::write(path, index.write_to_bytes().expect("serialize test index"))
            .expect("write test index");
    }

    fn read_index_file(path: &Path) -> Index {
        let bytes = fs::read(path).expect("read test index");
        Index::parse_from_bytes(&bytes).expect("parse test index")
    }

    #[cfg(unix)]
    fn copy_index_command(source: &Path, output: &Path) -> IndexerCommand {
        (
            "cp".to_string(),
            vec![path_argument(source), path_argument(output)],
        )
    }

    fn disable_update_check(project: &TestProject) {
        fs::write(project.root.join(".scip-nav/update-check"), "").expect("disable update check");
    }

    #[test]
    fn merging_indexes_unions_documents() {
        let merged = merge_indexes(vec![
            test_index("file:///project", "typescript", &["src/app.ts"], &[]),
            test_index("file:///project", "rust", &["src/lib.rs"], &[]),
        ])
        .expect("merge indexes");

        let paths = merged
            .index
            .documents
            .iter()
            .map(|document| document.relative_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["src/app.ts", "src/lib.rs"]);
    }

    #[test]
    fn merging_indexes_deduplicates_external_symbols() {
        let merged = merge_indexes(vec![
            test_index(
                "file:///project",
                "typescript",
                &["src/app.ts"],
                &[("shared#", "first")],
            ),
            test_index(
                "file:///project",
                "rust",
                &["src/lib.rs"],
                &[("shared#", "second"), ("rust#", "rust")],
            ),
        ])
        .expect("merge indexes");

        assert_eq!(merged.index.external_symbols.len(), 2);
        assert_eq!(merged.index.external_symbols[0].symbol, "shared#");
        assert_eq!(merged.index.external_symbols[0].display_name, "first");
        assert_eq!(merged.index.external_symbols[1].symbol, "rust#");
    }

    #[test]
    fn merging_indexes_warns_and_keeps_first_duplicate_document() {
        let merged = merge_indexes(vec![
            test_index("file:///project", "typescript", &["src/shared.ts"], &[]),
            test_index("file:///project", "rust", &["src/shared.ts"], &[]),
        ])
        .expect("merge indexes");

        assert_eq!(merged.index.documents.len(), 1);
        assert_eq!(merged.index.documents[0].language, "typescript");
        assert_eq!(merged.warnings.len(), 1);
        assert!(merged.warnings[0].contains("src/shared.ts"));
        let stats = format_index_stats(
            IndexStats {
                documents: 1,
                symbols: 0,
                occurrences: 0,
                file_size: 1,
            },
            &[LanguageIndexResult {
                language: Language::TypeScript,
                documents: 1,
            }],
            &[],
            &LanguageSelection {
                languages: vec![Language::TypeScript],
                also_detected: Vec::new(),
            },
            &merged.warnings,
        );
        assert!(stats.contains("warning: duplicate document relative_path 'src/shared.ts'"));
    }

    #[test]
    fn merging_indexes_rejects_conflicting_project_roots() {
        let error = merge_indexes(vec![
            test_index("file:///first", "typescript", &["src/app.ts"], &[]),
            test_index("file:///second", "rust", &["src/lib.rs"], &[]),
        ])
        .err()
        .expect("conflicting roots should fail");
        let message = error.to_string();

        assert!(message.contains("file:///first"));
        assert!(message.contains("file:///second"));
    }

    #[test]
    fn typescript_indexer_command_includes_optional_file_limit() {
        let project = TestProject::new();
        let output_path = pending_language_index_path(&project.root, Language::TypeScript);
        let (program, args) =
            indexer_command(Language::TypeScript, &project.root, &output_path, Some(12));

        assert_eq!(program, "npx");
        assert_eq!(
            args,
            vec![
                "--yes",
                "@sourcegraph/scip-typescript",
                "index",
                "--output",
                &path_argument(&output_path),
                "--max-file-byte-size",
                "12mb",
            ]
        );
    }

    #[test]
    fn python_indexer_command_targets_the_language_output() {
        let project = TestProject::new();
        let output_path = pending_language_index_path(&project.root, Language::Python);
        let (program, args) = indexer_command(Language::Python, &project.root, &output_path, None);

        assert_eq!(program, "npx");
        assert_eq!(
            args,
            vec![
                "--yes",
                "@sourcegraph/scip-python",
                "index",
                ".",
                "--output",
                &path_argument(&output_path),
            ]
        );
    }

    #[test]
    fn rust_indexer_command_uses_the_supported_output_flag() {
        let project = TestProject::new();
        let output_path = pending_language_index_path(&project.root, Language::Rust);
        let (program, args) = indexer_command(Language::Rust, &project.root, &output_path, None);

        assert_eq!(program, "rust-analyzer");
        assert_eq!(
            args,
            vec!["scip", ".", "--output", &path_argument(&output_path),]
        );
    }

    #[test]
    fn dart_indexer_command_uses_the_supported_output_flag() {
        let project = TestProject::new();
        let output_path = pending_language_index_path(&project.root, Language::Dart);
        let (program, args) = indexer_command(Language::Dart, &project.root, &output_path, None);

        assert_eq!(program, "dart");
        assert_eq!(
            args,
            vec![
                "pub",
                "global",
                "run",
                "scip_dart",
                ".",
                "--output",
                &path_argument(&output_path),
            ]
        );
    }

    #[test]
    fn java_indexer_command_runs_scip_java_through_coursier() {
        let project = TestProject::new();
        let output_path = pending_language_index_path(&project.root, Language::Java);
        let (program, args) = indexer_command(Language::Java, &project.root, &output_path, None);

        assert_eq!(program, "cs");
        assert_eq!(
            args,
            vec![
                "launch",
                "com.sourcegraph:scip-java_2.13:latest.stable",
                "--",
                "index",
                "--output",
                &path_argument(&output_path),
            ]
        );
    }

    #[test]
    fn cpp_indexer_command_prefers_root_then_build_compdb() {
        let project = TestProject::new();
        fs::create_dir_all(project.root.join("build")).expect("create build directory");
        let build_compdb = project.root.join("build/compile_commands.json");
        fs::write(&build_compdb, "[]").expect("write build compilation database");
        let output_path = pending_language_index_path(&project.root, Language::Cpp);

        let (program, args) = indexer_command(Language::Cpp, &project.root, &output_path, None);
        assert_eq!(program, "scip-clang");
        assert_eq!(
            args,
            vec![
                "--compdb-path",
                &path_argument(&build_compdb),
                "--index-output-path",
                &path_argument(&output_path),
            ]
        );

        let root_compdb = project.root.join("compile_commands.json");
        fs::write(&root_compdb, "[]").expect("write root compilation database");
        let (_, args) = indexer_command(Language::Cpp, &project.root, &output_path, None);
        assert_eq!(args[1], path_argument(&root_compdb));
    }

    #[cfg(unix)]
    #[test]
    fn failed_indexer_removes_partial_output_and_preserves_previous_index() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let destination = language_index_path(&project.root, Language::Python);
        fs::copy(index_path(&project.root), &destination).expect("seed language index");
        let previous_index = fs::read(&destination).expect("read previous index");
        let script = project.root.join("fail-indexer.sh");
        fs::write(
            &script,
            r#"printf 'partial index' > "$1"
printf 'stderr line 1\nstderr line 2\nstderr line 3\nstderr line 4\nstderr line 5\nstderr line 6\nstderr line 7\n' >&2
exit 23
"#,
        )
        .expect("write failed indexer");
        let command = (
            "sh".to_string(),
            vec![
                path_argument(&script),
                path_argument(&pending_language_index_path(
                    &project.root,
                    Language::Python,
                )),
            ],
        );

        let error = match build_index(&project.root, Language::Python, command) {
            Ok(_) => panic!("failed indexer should return an error"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert_eq!(
            fs::read(&destination).expect("read preserved index"),
            previous_index
        );
        assert!(!pending_language_index_path(&project.root, Language::Python).exists());
        assert!(message.contains("exit status 23"));
        assert!(message.contains("stderr line 3"));
        assert!(message.contains("stderr line 7"));
        assert!(!message.contains("stderr line 2"));
    }

    #[cfg(unix)]
    #[test]
    fn empty_indexer_output_preserves_previous_index() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let destination = language_index_path(&project.root, Language::Python);
        fs::copy(index_path(&project.root), &destination).expect("seed language index");
        let previous_index = fs::read(&destination).expect("read previous index");
        let empty_index_path = project.root.join("empty.scip");
        fs::write(
            &empty_index_path,
            Index::default()
                .write_to_bytes()
                .expect("serialize empty index"),
        )
        .expect("write empty index");
        let script = project.root.join("empty-indexer.sh");
        fs::write(&script, "cp \"$1\" \"$2\"\n").expect("write empty indexer");
        let command = (
            "sh".to_string(),
            vec![
                path_argument(&script),
                path_argument(&empty_index_path),
                path_argument(&pending_language_index_path(
                    &project.root,
                    Language::Python,
                )),
            ],
        );

        let error = match build_index(&project.root, Language::Python, command) {
            Ok(_) => panic!("empty index should return an error"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert_eq!(
            fs::read(&destination).expect("read preserved index"),
            previous_index
        );
        assert!(!pending_language_index_path(&project.root, Language::Python).exists());
        assert!(message.contains("exit status 0"));
        assert!(message.contains("output index contains no documents"));
    }

    #[cfg(unix)]
    #[test]
    fn successful_indexer_atomically_replaces_previous_index() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let destination = language_index_path(&project.root, Language::Python);
        fs::copy(index_path(&project.root), &destination).expect("seed language index");
        let replacement = fixture(true)
            .write_to_bytes()
            .expect("serialize replacement index");
        let replacement_path = project.root.join("replacement.scip");
        fs::write(&replacement_path, &replacement).expect("write replacement index");
        let script = project.root.join("success-indexer.sh");
        fs::write(&script, "cp \"$1\" \"$2\"\n").expect("write successful indexer");
        let command = (
            "sh".to_string(),
            vec![
                path_argument(&script),
                path_argument(&replacement_path),
                path_argument(&pending_language_index_path(
                    &project.root,
                    Language::Python,
                )),
            ],
        );
        let mut cache = IndexCache::default();
        assert_eq!(
            cache
                .load(&project.root)
                .expect("load previous index")
                .index
                .index()
                .documents
                .len(),
            6
        );

        let loaded = build_index(&project.root, Language::Python, command)
            .expect("successful indexer should replace the index");

        assert_eq!(loaded.index.index().documents.len(), 7);
        assert_eq!(
            fs::read(&destination).expect("read replacement index"),
            replacement
        );
        assert!(!pending_language_index_path(&project.root, Language::Python).exists());
        merge_language_indexes(&project.root, &mut cache).expect("merge replacement index");
        assert_eq!(
            cache
                .load(&project.root)
                .expect("load replacement index")
                .index
                .index()
                .documents
                .len(),
            7
        );
    }

    #[test]
    fn detection_recognizes_every_registered_marker() {
        for indexer in LANGUAGE_INDEXERS {
            for marker in indexer.markers {
                let project = TestProject::new();
                let marker_path = project.root.join(marker);
                if let Some(parent) = marker_path.parent() {
                    fs::create_dir_all(parent).expect("create marker parent");
                }
                fs::write(marker_path, "").expect("write marker");
                assert_eq!(
                    detect_languages(&project.root),
                    vec![indexer.language],
                    "marker {marker}"
                );
            }
        }
    }

    #[test]
    fn detection_selects_all_languages_in_registry_order() {
        let project = TestProject::new();
        fs::write(project.root.join("package.json"), "{}").expect("write TypeScript marker");
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");
        fs::write(project.root.join("pubspec.yaml"), "").expect("write Dart marker");

        let selection = select_languages(&project.root, None, None).expect("detect languages");
        assert_eq!(
            selection.languages,
            vec![Language::TypeScript, Language::Rust, Language::Dart]
        );
        assert!(selection.also_detected.is_empty());
        assert_eq!(
            format_index_stats(
                IndexStats {
                    documents: 1,
                    symbols: 2,
                    occurrences: 3,
                    file_size: 4,
                },
                &[LanguageIndexResult {
                    language: Language::TypeScript,
                    documents: 1,
                }],
                &[],
                &selection,
                &[],
            ),
            "indexed: typescript 1 documents | merged: documents 1 | symbols 2 | occurrences 3 | size 4 B"
        );
    }

    #[test]
    fn language_override_bypasses_detection() {
        let project = TestProject::new();
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");

        let selection =
            select_languages(&project.root, Some("python"), None).expect("override language");
        assert_eq!(selection.languages, vec![Language::Python]);
        assert_eq!(selection.also_detected, vec![Language::Rust]);
    }

    #[cfg(unix)]
    #[test]
    fn no_language_override_indexes_every_detected_language() {
        let project = TestProject::new();
        disable_update_check(&project);
        fs::write(project.root.join("package.json"), "{}").expect("write TypeScript marker");
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");
        let project_root = project.root.to_string_lossy();
        let typescript_source = project.root.join("typescript.scip");
        let rust_source = project.root.join("rust.scip");
        write_index_file(
            &typescript_source,
            &test_index(&project_root, "typescript", &["src/app.ts"], &[]),
        );
        write_index_file(
            &rust_source,
            &test_index(&project_root, "rust", &["src/lib.rs"], &[]),
        );

        let mut cache = IndexCache::default();
        let stats = run_indexer_with(
            &project.root,
            None,
            None,
            None,
            &mut cache,
            |language, _, output, _| match language {
                Language::TypeScript => copy_index_command(&typescript_source, output),
                Language::Rust => copy_index_command(&rust_source, output),
                _ => panic!("unexpected language {language}"),
            },
        )
        .expect("index all detected languages");

        assert!(language_index_path(&project.root, Language::TypeScript).is_file());
        assert!(language_index_path(&project.root, Language::Rust).is_file());
        assert_eq!(
            read_index_file(&index_path(&project.root)).documents.len(),
            2
        );
        assert!(stats.contains("typescript 1 documents"));
        assert!(stats.contains("rust 1 documents"));
        assert!(!stats.contains("also detected"));
    }

    #[cfg(unix)]
    #[test]
    fn language_list_reindexes_one_language_and_retains_other_index() {
        let project = TestProject::new();
        disable_update_check(&project);
        fs::write(project.root.join("package.json"), "{}").expect("write TypeScript marker");
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");
        let project_root = project.root.to_string_lossy();
        let typescript_path = language_index_path(&project.root, Language::TypeScript);
        write_index_file(
            &typescript_path,
            &test_index(&project_root, "typescript", &["src/app.ts"], &[]),
        );
        let typescript_before = fs::read(&typescript_path).expect("read TypeScript index");
        let rust_source = project.root.join("rust.scip");
        write_index_file(
            &rust_source,
            &test_index(&project_root, "rust", &["src/lib.rs"], &[]),
        );
        let languages = vec!["rust".to_string()];

        let stats = run_indexer_with(
            &project.root,
            None,
            Some(&languages),
            None,
            &mut IndexCache::default(),
            |language, _, output, _| {
                assert_eq!(language, Language::Rust);
                copy_index_command(&rust_source, output)
            },
        )
        .expect("index Rust");

        assert_eq!(
            fs::read(&typescript_path).expect("read retained TypeScript index"),
            typescript_before
        );
        let mut paths = read_index_file(&index_path(&project.root))
            .documents
            .into_iter()
            .map(|document| document.relative_path)
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(paths, vec!["src/app.ts", "src/lib.rs"]);
        assert!(stats.contains("indexed: rust 1 documents"));
        assert!(stats.contains("also detected: typescript"));
    }

    #[test]
    fn singular_and_plural_language_overrides_conflict() {
        let project = TestProject::new();
        let languages = vec!["rust".to_string()];
        let error = run_indexer_with(
            &project.root,
            Some("rust"),
            Some(&languages),
            None,
            &mut IndexCache::default(),
            |_, _, _, _| panic!("conflicting arguments must not run an indexer"),
        )
        .expect_err("conflicting overrides should fail");

        assert_eq!(error.to_string(), "pass only one of language or languages");
    }

    #[test]
    fn language_list_rejects_unknown_language() {
        let project = TestProject::new();
        let languages = vec!["go".to_string()];
        let error = run_indexer_with(
            &project.root,
            None,
            Some(&languages),
            None,
            &mut IndexCache::default(),
            |_, _, _, _| panic!("invalid arguments must not run an indexer"),
        )
        .expect_err("unknown language should fail");

        assert_eq!(
            error.to_string(),
            "unsupported language: go (expected typescript|python|rust|dart|java|cpp)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_indexer_failure_still_merges_successful_languages() {
        let project = TestProject::new();
        disable_update_check(&project);
        fs::write(project.root.join("package.json"), "{}").expect("write TypeScript marker");
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");
        let project_root = project.root.to_string_lossy();
        let typescript_source = project.root.join("typescript.scip");
        write_index_file(
            &typescript_source,
            &test_index(&project_root, "typescript", &["src/app.ts"], &[]),
        );

        let stats = run_indexer_with(
            &project.root,
            None,
            None,
            None,
            &mut IndexCache::default(),
            |language, _, output, _| match language {
                Language::TypeScript => copy_index_command(&typescript_source, output),
                Language::Rust => (
                    "sh".to_string(),
                    vec!["-c".to_string(), "exit 19".to_string()],
                ),
                _ => panic!("unexpected language {language}"),
            },
        )
        .expect("partial indexing should succeed");

        assert!(language_index_path(&project.root, Language::TypeScript).is_file());
        assert!(!language_index_path(&project.root, Language::Rust).exists());
        assert_eq!(
            read_index_file(&index_path(&project.root)).documents.len(),
            1
        );
        assert!(stats.contains("indexed: typescript 1 documents"));
        assert!(stats.contains("failed: rust"));
        assert!(stats.contains("exit status 19"));
    }

    #[test]
    fn legacy_canonical_index_still_loads_without_language_indexes() {
        let project = TestProject::new();
        write_fixture(&project, false);

        assert!(language_index_paths(&project.root)
            .expect("list language indexes")
            .is_empty());
        let loaded = IndexCache::default()
            .load(&project.root)
            .expect("load legacy canonical index");
        assert_eq!(loaded.index.index().documents.len(), 6);
    }

    #[test]
    fn missing_indexer_binary_returns_the_registered_hint() {
        let project = TestProject::new();
        let program = path_argument(&project.root.join("definitely-not-an-indexer"));
        let error = execute_indexer_command(&program, &[], &project.root, Language::Cpp)
            .expect_err("missing indexer should fail");

        assert_eq!(
            error.to_string(),
            "indexer for cpp not found — download scip-clang from github.com/sourcegraph/scip-clang/releases"
        );
    }

    #[test]
    fn cache_reloads_when_index_mtime_changes() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let path = index_path(&project.root);
        let first_mtime = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("first mtime");
        let mut cache = IndexCache::default();
        let first = cache.load(&project.root).expect("first load");
        assert_eq!(first.index.index().documents.len(), 6);

        let mut changed = false;
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(5));
            write_fixture(&project, true);
            let next_mtime = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .expect("next mtime");
            if next_mtime != first_mtime {
                changed = true;
                break;
            }
        }
        assert!(changed, "test filesystem did not advance mtime");

        let second = cache.load(&project.root).expect("reload");
        assert_eq!(second.index.index().documents.len(), 7);
    }
}

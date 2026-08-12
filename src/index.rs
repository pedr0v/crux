use crate::render::truncate_chars;
use crate::semantic::SemanticIndex;
use crate::update;
use anyhow::{bail, Context, Result};
use protobuf::Message;
use scip::types::Index;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

type IndexerCommand = (String, Vec<String>, Option<PathBuf>);

struct LanguageIndexer {
    language: Language,
    markers: &'static [&'static str],
    command: fn(&Path, Option<u64>) -> IndexerCommand,
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

        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let index =
            Index::parse_from_bytes(&bytes).with_context(|| format!("parse {}", path.display()))?;
        let loaded = LoadedIndex {
            index: Arc::new(SemanticIndex::new(index)),
            file_size: metadata.len(),
        };
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

fn path_argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn typescript_indexer_command(project_root: &Path, max_file_mb: Option<u64>) -> IndexerCommand {
    let mut args = vec![
        "--yes".to_string(),
        "@sourcegraph/scip-typescript".to_string(),
        "index".to_string(),
        "--output".to_string(),
        path_argument(&index_path(project_root)),
    ];
    if let Some(max_file_mb) = max_file_mb {
        args.extend([
            "--max-file-byte-size".to_string(),
            format!("{max_file_mb}mb"),
        ]);
    }
    ("npx".to_string(), args, None)
}

fn python_indexer_command(project_root: &Path, _max_file_mb: Option<u64>) -> IndexerCommand {
    (
        "npx".to_string(),
        vec![
            "--yes".to_string(),
            "@sourcegraph/scip-python".to_string(),
            "index".to_string(),
            ".".to_string(),
            "--output".to_string(),
            path_argument(&index_path(project_root)),
        ],
        None,
    )
}

fn rust_indexer_command(project_root: &Path, _max_file_mb: Option<u64>) -> IndexerCommand {
    (
        "rust-analyzer".to_string(),
        vec![
            "scip".to_string(),
            ".".to_string(),
            "--output".to_string(),
            path_argument(&index_path(project_root)),
        ],
        None,
    )
}

fn dart_indexer_command(project_root: &Path, _max_file_mb: Option<u64>) -> IndexerCommand {
    (
        "dart".to_string(),
        vec![
            "pub".to_string(),
            "global".to_string(),
            "run".to_string(),
            "scip_dart".to_string(),
            ".".to_string(),
        ],
        Some(project_root.join("index.scip")),
    )
}

fn java_indexer_command(project_root: &Path, _max_file_mb: Option<u64>) -> IndexerCommand {
    (
        "cs".to_string(),
        vec![
            "launch".to_string(),
            "com.sourcegraph:scip-java_2.13:latest.stable".to_string(),
            "--".to_string(),
            "index".to_string(),
            "--output".to_string(),
            path_argument(&index_path(project_root)),
        ],
        None,
    )
}

fn cpp_indexer_command(project_root: &Path, _max_file_mb: Option<u64>) -> IndexerCommand {
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
            path_argument(&index_path(project_root)),
        ],
        None,
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
    max_file_mb: Option<u64>,
) -> IndexerCommand {
    (language_indexer(language).command)(project_root, max_file_mb)
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

struct LanguageSelection {
    language: Language,
    also_detected: Vec<Language>,
}

fn select_language(
    project_root: &Path,
    override_language: Option<&str>,
) -> Result<LanguageSelection> {
    if let Some(override_language) = override_language {
        return Ok(LanguageSelection {
            language: parse_language(override_language)?,
            also_detected: Vec::new(),
        });
    }

    let detected = detect_languages(project_root);
    let Some((&language, also_detected)) = detected.split_first() else {
        bail!(
            "could not auto-detect a supported language — add a project marker or pass language (typescript|python|rust|dart|java|cpp)"
        );
    };
    Ok(LanguageSelection {
        language,
        also_detected: also_detected.to_vec(),
    })
}

fn format_index_stats(stats: IndexStats, selection: &LanguageSelection) -> String {
    let mut output = format!("{} | language {}", stats.compact(), selection.language);
    if !selection.also_detected.is_empty() {
        let also_detected = selection
            .also_detected
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            " | also detected: {also_detected} — pass language to index it"
        ));
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
    let start = lines.len().saturating_sub(8);
    lines[start..]
        .iter()
        .map(|line| truncate_chars(line, 300))
        .collect::<Vec<_>>()
        .join("\n")
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

pub(crate) fn run_indexer(
    project_root: &Path,
    override_language: Option<&str>,
    max_file_mb: Option<u64>,
    cache: &mut IndexCache,
) -> Result<String> {
    validate_project_root(project_root)?;
    if max_file_mb == Some(0) {
        bail!("max_file_mb must be at least 1");
    }
    let selection = select_language(project_root, override_language)?;
    fs::create_dir_all(project_root.join(".scip-nav"))
        .with_context(|| format!("create {}/.scip-nav", project_root.display()))?;
    let pending_update = update::start_background_check(&project_root.join(".scip-nav"));
    let (program, args, needs_move) =
        indexer_command(selection.language, project_root, max_file_mb);
    let output = execute_indexer_command(&program, &args, project_root, selection.language)?;

    if !output.status.success() {
        let tail = stderr_tail(&output.stderr);
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        if tail.is_empty() {
            bail!("indexer for {} failed ({status})", selection.language);
        }
        bail!(
            "indexer for {} failed ({status})\n{tail}",
            selection.language
        );
    }

    if let Some(generated_path) = needs_move {
        let destination = index_path(project_root);
        fs::rename(&generated_path, &destination).with_context(|| {
            format!(
                "move generated index {} to {}",
                generated_path.display(),
                destination.display()
            )
        })?;
    }

    cache.invalidate(project_root);
    let loaded = cache.load(project_root)?;
    let mut stats = format_index_stats(IndexStats::from_loaded(&loaded), &selection);
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
    use std::thread;
    use std::time::Duration;

    #[test]
    fn typescript_indexer_command_includes_optional_file_limit() {
        let project = TestProject::new();
        let (program, args, needs_move) =
            indexer_command(Language::TypeScript, &project.root, Some(12));

        assert_eq!(program, "npx");
        assert_eq!(
            args,
            vec![
                "--yes",
                "@sourcegraph/scip-typescript",
                "index",
                "--output",
                &path_argument(&index_path(&project.root)),
                "--max-file-byte-size",
                "12mb",
            ]
        );
        assert_eq!(needs_move, None);
    }

    #[test]
    fn python_indexer_command_targets_the_shared_output() {
        let project = TestProject::new();
        let (program, args, needs_move) = indexer_command(Language::Python, &project.root, None);

        assert_eq!(program, "npx");
        assert_eq!(
            args,
            vec![
                "--yes",
                "@sourcegraph/scip-python",
                "index",
                ".",
                "--output",
                &path_argument(&index_path(&project.root)),
            ]
        );
        assert_eq!(needs_move, None);
    }

    #[test]
    fn rust_indexer_command_uses_the_supported_output_flag() {
        let project = TestProject::new();
        let (program, args, needs_move) = indexer_command(Language::Rust, &project.root, None);

        assert_eq!(program, "rust-analyzer");
        assert_eq!(
            args,
            vec![
                "scip",
                ".",
                "--output",
                &path_argument(&index_path(&project.root)),
            ]
        );
        assert_eq!(needs_move, None);
    }

    #[test]
    fn dart_indexer_command_moves_its_fixed_output() {
        let project = TestProject::new();
        let (program, args, needs_move) = indexer_command(Language::Dart, &project.root, None);

        assert_eq!(program, "dart");
        assert_eq!(args, vec!["pub", "global", "run", "scip_dart", "."]);
        assert_eq!(needs_move, Some(project.root.join("index.scip")));
    }

    #[test]
    fn java_indexer_command_runs_scip_java_through_coursier() {
        let project = TestProject::new();
        let (program, args, needs_move) = indexer_command(Language::Java, &project.root, None);

        assert_eq!(program, "cs");
        assert_eq!(
            args,
            vec![
                "launch",
                "com.sourcegraph:scip-java_2.13:latest.stable",
                "--",
                "index",
                "--output",
                &path_argument(&index_path(&project.root)),
            ]
        );
        assert_eq!(needs_move, None);
    }

    #[test]
    fn cpp_indexer_command_prefers_root_then_build_compdb() {
        let project = TestProject::new();
        fs::create_dir_all(project.root.join("build")).expect("create build directory");
        let build_compdb = project.root.join("build/compile_commands.json");
        fs::write(&build_compdb, "[]").expect("write build compilation database");

        let (program, args, needs_move) = indexer_command(Language::Cpp, &project.root, None);
        assert_eq!(program, "scip-clang");
        assert_eq!(
            args,
            vec![
                "--compdb-path",
                &path_argument(&build_compdb),
                "--index-output-path",
                &path_argument(&index_path(&project.root)),
            ]
        );
        assert_eq!(needs_move, None);

        let root_compdb = project.root.join("compile_commands.json");
        fs::write(&root_compdb, "[]").expect("write root compilation database");
        let (_, args, _) = indexer_command(Language::Cpp, &project.root, None);
        assert_eq!(args[1], path_argument(&root_compdb));
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
    fn detection_uses_registry_order_and_reports_alternatives() {
        let project = TestProject::new();
        fs::write(project.root.join("package.json"), "{}").expect("write TypeScript marker");
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");
        fs::write(project.root.join("pubspec.yaml"), "").expect("write Dart marker");

        let selection = select_language(&project.root, None).expect("detect languages");
        assert_eq!(selection.language, Language::TypeScript);
        assert_eq!(
            selection.also_detected,
            vec![Language::Rust, Language::Dart]
        );
        assert_eq!(
            format_index_stats(
                IndexStats {
                    documents: 1,
                    symbols: 2,
                    occurrences: 3,
                    file_size: 4,
                },
                &selection,
            ),
            "documents 1 | symbols 2 | occurrences 3 | size 4 B | language typescript | also detected: rust, dart — pass language to index it"
        );
    }

    #[test]
    fn language_override_bypasses_detection() {
        let project = TestProject::new();
        fs::write(project.root.join("Cargo.toml"), "").expect("write Rust marker");

        let selection = select_language(&project.root, Some("python")).expect("override language");
        assert_eq!(selection.language, Language::Python);
        assert!(selection.also_detected.is_empty());
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

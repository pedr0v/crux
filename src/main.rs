use anyhow::{anyhow, bail, Context, Result};
use protobuf::Message;
use scip::symbol::{is_local_symbol, parse_symbol};
use scip::types::{
    descriptor, occurrence, symbol_information, Document, Index, Occurrence, SymbolInformation,
    SymbolRole,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::SystemTime;

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_SEARCH_LIMIT: usize = 20;
const DEFAULT_REFS_LIMIT: usize = 50;
const DEFAULT_MAP_REFS_LIMIT: usize = 40;
const DEFAULT_CALLERS_LIMIT: usize = 40;
const DEFAULT_DEAD_LIMIT: usize = 100;
const MAX_LIMIT: usize = 200;
const MAX_CALLER_DEPTH: usize = 3;
const MAX_RESULT_LINES: usize = 39;
const MAX_MAP_NAMES: usize = 8;
const MAX_MAP_RESULT_LINES: usize = 250;
const DOC_CHAR_LIMIT: usize = 200;
const SOURCE_LINE_CHAR_LIMIT: usize = 140;

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
struct LoadedIndex {
    index: Arc<Index>,
    file_size: u64,
}

struct CachedIndex {
    modified: SystemTime,
    loaded: LoadedIndex,
}

#[derive(Default)]
struct IndexCache {
    entries: HashMap<PathBuf, CachedIndex>,
}

impl IndexCache {
    fn load(&mut self, project_root: &Path) -> Result<LoadedIndex> {
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
            index: Arc::new(index),
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
struct IndexStats {
    documents: usize,
    symbols: usize,
    occurrences: usize,
    file_size: u64,
}

impl IndexStats {
    fn from_loaded(loaded: &LoadedIndex) -> Self {
        Self {
            documents: loaded.index.documents.len(),
            symbols: loaded
                .index
                .documents
                .iter()
                .map(|document| document.symbols.len())
                .sum::<usize>()
                + loaded.index.external_symbols.len(),
            occurrences: loaded
                .index
                .documents
                .iter()
                .map(|document| document.occurrences.len())
                .sum(),
            file_size: loaded.file_size,
        }
    }

    fn compact(self) -> String {
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

fn index_path(project_root: &Path) -> PathBuf {
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

fn occurrence_start_line(occurrence: &Occurrence) -> Option<usize> {
    if matches!(occurrence.range.len(), 3 | 4) {
        return usize::try_from(occurrence.range[0]).ok();
    }

    match occurrence.typed_range.as_ref()? {
        occurrence::Typed_range::SingleLineRange(range) => usize::try_from(range.line).ok(),
        occurrence::Typed_range::MultiLineRange(range) => usize::try_from(range.start_line).ok(),
        _ => None,
    }
}

fn is_definition(occurrence: &Occurrence) -> bool {
    occurrence.symbol_roles & SymbolRole::Definition as i32 != 0
}

fn display_name(information: &SymbolInformation) -> String {
    let explicit = information.display_name.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }

    if let Ok(symbol) = parse_symbol(&information.symbol) {
        if let Some(descriptor) = symbol.descriptors.last() {
            return descriptor.name.clone();
        }
    }

    information
        .symbol
        .split_whitespace()
        .last()
        .unwrap_or(&information.symbol)
        .trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '$'
        })
        .to_string()
}

fn inferred_kind(information: &SymbolInformation) -> String {
    if let Ok(kind) = information.kind.enum_value() {
        if kind != symbol_information::Kind::UnspecifiedKind {
            return format!("{kind:?}").to_ascii_lowercase();
        }
    }

    parse_symbol(&information.symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().cloned())
        .and_then(|descriptor| descriptor.suffix.enum_value().ok())
        .map(|suffix| match suffix {
            descriptor::Suffix::Method => "method",
            descriptor::Suffix::Type => "type",
            descriptor::Suffix::Term => "symbol",
            descriptor::Suffix::Namespace | descriptor::Suffix::Package => "namespace",
            descriptor::Suffix::TypeParameter => "typeparameter",
            descriptor::Suffix::Parameter => "parameter",
            descriptor::Suffix::Meta => "meta",
            descriptor::Suffix::Local => "local",
            descriptor::Suffix::Macro => "macro",
            descriptor::Suffix::UnspecifiedSuffix => "symbol",
        })
        .unwrap_or("symbol")
        .to_string()
}

fn is_member_kind(kind: symbol_information::Kind) -> bool {
    matches!(
        kind,
        symbol_information::Kind::AbstractMethod
            | symbol_information::Kind::Accessor
            | symbol_information::Kind::Constructor
            | symbol_information::Kind::EnumMember
            | symbol_information::Kind::Field
            | symbol_information::Kind::Getter
            | symbol_information::Kind::Method
            | symbol_information::Kind::MethodAlias
            | symbol_information::Kind::MethodSpecification
            | symbol_information::Kind::Property
            | symbol_information::Kind::ProtocolMethod
            | symbol_information::Kind::PureVirtualMethod
            | symbol_information::Kind::Setter
            | symbol_information::Kind::SingletonMethod
            | symbol_information::Kind::StaticDataMember
            | symbol_information::Kind::StaticEvent
            | symbol_information::Kind::StaticField
            | symbol_information::Kind::StaticMethod
            | symbol_information::Kind::StaticProperty
            | symbol_information::Kind::TraitMethod
            | symbol_information::Kind::TypeClassMethod
    )
}

fn symbol_container(information: &SymbolInformation) -> Option<String> {
    let symbol = parse_symbol(&information.symbol).ok()?;
    let descriptors = &symbol.descriptors;
    if descriptors.len() < 2 {
        return None;
    }

    let last_suffix = descriptors.last()?.suffix.enum_value().ok();
    let kind = information.kind.enum_value().ok();
    if last_suffix != Some(descriptor::Suffix::Method) && !kind.is_some_and(is_member_kind) {
        return None;
    }

    let container_descriptor = descriptors.get(descriptors.len() - 2)?;
    if matches!(
        container_descriptor.suffix.enum_value().ok(),
        Some(descriptor::Suffix::Namespace | descriptor::Suffix::Package)
    ) {
        return None;
    }
    let container = container_descriptor.name.trim();
    (!container.is_empty()).then(|| container.to_string())
}

fn symbol_label(information: &SymbolInformation) -> String {
    let kind = inferred_kind(information);
    let name = display_name(information);
    match symbol_container(information) {
        Some(container) => format!("{kind} {name} ({container})"),
        None => format!("{kind} {name}"),
    }
}

fn compact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut compact = text
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    compact.push('…');
    compact
}

struct SourceCache<'a> {
    project_root: &'a Path,
    files: HashMap<String, Option<Vec<String>>>,
}

impl<'a> SourceCache<'a> {
    fn new(project_root: &'a Path) -> Self {
        Self {
            project_root,
            files: HashMap::new(),
        }
    }

    fn line(&mut self, file: &str, line: usize) -> Option<String> {
        let lines = self.files.entry(file.to_string()).or_insert_with(|| {
            fs::read_to_string(self.project_root.join(file))
                .ok()
                .map(|source| source.lines().map(str::to_string).collect())
        });
        lines.as_ref()?.get(line).cloned()
    }

    fn display_line(&mut self, file: &str, line: usize) -> Option<String> {
        self.line(file, line)
            .map(|source| truncate_chars(source.trim(), SOURCE_LINE_CHAR_LIMIT))
    }

    fn is_import_or_export(&mut self, file: &str, line: usize) -> bool {
        self.line(file, line)
            .is_some_and(|source| is_import_or_export_line(&source))
    }

    fn is_exported_declaration(&mut self, file: &str, line: usize) -> Option<bool> {
        let source = self.line(file, line)?;
        let trimmed = source.trim_start();
        if let Some(declaration) = trimmed.strip_prefix("export ") {
            return Some(starts_with_declaration_keyword(declaration));
        }
        if !starts_with_declaration_keyword(trimmed) {
            return Some(false);
        }

        Some((1..=2).any(|offset| {
            line.checked_sub(offset)
                .and_then(|above| self.line(file, above))
                .is_some_and(|source| is_multiline_export_marker(&source))
        }))
    }
}

fn starts_with_declaration_keyword(source: &str) -> bool {
    [
        "function ",
        "const ",
        "let ",
        "var ",
        "class ",
        "interface ",
        "enum ",
        "type ",
        "abstract ",
        "async ",
    ]
    .iter()
    .any(|keyword| source.starts_with(keyword))
}

fn is_multiline_export_marker(source: &str) -> bool {
    let trimmed = source.trim();
    let Some(remainder) = trimmed.strip_prefix("export") else {
        return false;
    };
    if remainder
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return false;
    }
    let remainder = remainder.trim_start();
    remainder.is_empty()
        || (!starts_with_declaration_keyword(remainder)
            && !remainder.starts_with("default ")
            && !remainder.starts_with('{')
            && !remainder.starts_with('*')
            && !remainder.starts_with("type {")
            && !remainder.starts_with("type *"))
}

fn is_import_or_export_line(source: &str) -> bool {
    let trimmed = source.trim_start();
    if trimmed.starts_with("import ")
        || trimmed.starts_with("import{")
        || trimmed.starts_with("export {")
        || trimmed.starts_with("export *")
        || trimmed.starts_with("export type {")
        || trimmed.starts_with("export type *")
    {
        return true;
    }
    if trimmed.starts_with("export ") {
        return false;
    }

    let trimmed = trimmed.trim_end();
    !trimmed.is_empty()
        && !trimmed.contains('(')
        && trimmed.chars().all(|character| {
            character.is_alphanumeric()
                || character.is_whitespace()
                || matches!(character, '_' | '$' | ',' | '{' | '}' | '\'' | '"')
        })
}

fn symbol_details(information: &SymbolInformation) -> String {
    let signature = information
        .signature_documentation
        .as_ref()
        .map(|signature| compact_whitespace(&signature.text))
        .filter(|signature| !signature.is_empty());
    let documentation = information
        .documentation
        .first()
        .map(|documentation| compact_whitespace(documentation))
        .filter(|documentation| !documentation.is_empty());

    let details = match (signature, documentation) {
        (Some(signature), Some(documentation)) => format!("{signature} — {documentation}"),
        (Some(signature), None) => signature,
        (None, Some(documentation)) => documentation,
        (None, None) => String::new(),
    };
    truncate_chars(&details, DOC_CHAR_LIMIT)
}

fn definition_line(document: &Document, symbol: &str) -> Option<usize> {
    document
        .occurrences
        .iter()
        .find(|occurrence| occurrence.symbol == symbol && is_definition(occurrence))
        .or_else(|| {
            document
                .occurrences
                .iter()
                .find(|occurrence| occurrence.symbol == symbol)
        })
        .and_then(occurrence_start_line)
}

struct MatchingSymbols {
    principals: HashSet<String>,
    references_by_principal: HashMap<String, HashSet<String>>,
    definition_sites: HashMap<String, (String, usize)>,
}

impl MatchingSymbols {
    fn is_empty(&self) -> bool {
        self.principals.is_empty()
    }

    fn reference_symbols(&self, principal: &str) -> HashSet<String> {
        self.references_by_principal
            .get(principal)
            .cloned()
            .unwrap_or_else(|| HashSet::from([principal.to_string()]))
    }

    fn all_reference_symbols(&self) -> HashSet<&str> {
        self.references_by_principal
            .values()
            .flat_map(|symbols| symbols.iter().map(String::as_str))
            .collect()
    }
}

fn symbol_information_richness(information: &SymbolInformation) -> (bool, bool) {
    let has_kind = information
        .kind
        .enum_value()
        .is_ok_and(|kind| kind != symbol_information::Kind::UnspecifiedKind);
    let has_documentation = information
        .documentation
        .iter()
        .any(|documentation| !documentation.trim().is_empty())
        || information
            .signature_documentation
            .as_ref()
            .is_some_and(|signature| !signature.text.trim().is_empty());
    (has_kind, has_documentation)
}

fn matching_symbol_ids(index: &Index, name: &str) -> MatchingSymbols {
    let all = index
        .documents
        .iter()
        .flat_map(|document| document.symbols.iter())
        .chain(index.external_symbols.iter());
    let name_lower = name.to_lowercase();
    let symbols = all.collect::<Vec<_>>();
    let mut matches = symbols
        .iter()
        .filter(|information| display_name(information) == name)
        .map(|information| information.symbol.clone())
        .collect::<HashSet<_>>();
    let exact_match = !matches.is_empty();

    if matches.is_empty() {
        matches = symbols
            .iter()
            .filter(|information| {
                display_name(information)
                    .to_lowercase()
                    .contains(&name_lower)
            })
            .map(|information| information.symbol.clone())
            .collect();
    }

    let information = symbol_information_by_id(index);
    let definition_sites = definition_sites(index, &matches);
    let mut principals = matches.clone();
    let mut folded_into = HashMap::new();

    if exact_match {
        let mut symbols_by_site: BTreeMap<(String, usize), Vec<String>> = BTreeMap::new();
        for symbol in &matches {
            if let Some(site) = definition_sites.get(symbol) {
                symbols_by_site
                    .entry(site.clone())
                    .or_default()
                    .push(symbol.clone());
            }
        }

        for mut colocated in symbols_by_site.into_values() {
            if colocated.len() < 2 {
                continue;
            }
            colocated.sort_by(|left, right| {
                let left_richness = information
                    .get(left.as_str())
                    .map(|information| symbol_information_richness(information))
                    .unwrap_or_default();
                let right_richness = information
                    .get(right.as_str())
                    .map(|information| symbol_information_richness(information))
                    .unwrap_or_default();
                right_richness
                    .cmp(&left_richness)
                    .then_with(|| left.cmp(right))
            });
            let principal = colocated[0].clone();
            for duplicate in colocated.into_iter().skip(1) {
                principals.remove(&duplicate);
                folded_into.insert(duplicate, principal.clone());
            }
        }
    }

    let mut symbols_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for symbol in &matches {
        if let Some(information) = information.get(symbol.as_str()) {
            symbols_by_name
                .entry(display_name(information))
                .or_default()
                .push(symbol.clone());
        }
    }
    for same_name in symbols_by_name.into_values() {
        let defined_principals = same_name
            .iter()
            .filter(|symbol| principals.contains(*symbol) && definition_sites.contains_key(*symbol))
            .collect::<Vec<_>>();
        if defined_principals.len() != 1 {
            continue;
        }
        let principal = defined_principals[0].clone();
        for alias in same_name
            .into_iter()
            .filter(|symbol| !definition_sites.contains_key(symbol))
        {
            principals.remove(&alias);
            folded_into.insert(alias, principal.clone());
        }
    }

    let mut references_by_principal = principals
        .iter()
        .map(|symbol| (symbol.clone(), HashSet::from([symbol.clone()])))
        .collect::<HashMap<_, _>>();
    for symbol in &matches {
        if let Some(principal) = folded_into.get(symbol) {
            references_by_principal
                .entry(principal.clone())
                .or_default()
                .insert(symbol.clone());
        }
    }

    MatchingSymbols {
        principals,
        references_by_principal,
        definition_sites,
    }
}

fn symbol_information_by_id(index: &Index) -> HashMap<&str, &SymbolInformation> {
    index
        .documents
        .iter()
        .flat_map(|document| document.symbols.iter())
        .chain(index.external_symbols.iter())
        .map(|information| (information.symbol.as_str(), information))
        .collect()
}

fn definition_sites(index: &Index, symbols: &HashSet<String>) -> HashMap<String, (String, usize)> {
    let mut sites = HashMap::new();
    for document in &index.documents {
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) || !symbols.contains(&occurrence.symbol) {
                continue;
            }
            let Some(line) = occurrence_start_line(occurrence) else {
                continue;
            };
            let candidate = (document.relative_path.clone(), line);
            let site = sites
                .entry(occurrence.symbol.clone())
                .or_insert_with(|| candidate.clone());
            if candidate < *site {
                *site = candidate;
            }
        }
    }
    sites
}

fn reference_sites(index: &Index, symbol: &str) -> BTreeSet<(String, usize)> {
    let mut sites = BTreeSet::new();
    for document in &index.documents {
        for occurrence in &document.occurrences {
            if is_definition(occurrence) || occurrence.symbol != symbol {
                continue;
            }
            if let Some(line) = occurrence_start_line(occurrence) {
                sites.insert((document.relative_path.clone(), line));
            }
        }
    }
    sites
}

fn occurrence_enclosing_lines(occurrence: &Occurrence) -> Option<(usize, usize)> {
    if let Some(range) = occurrence.typed_enclosing_range.as_ref() {
        let lines = match range {
            occurrence::Typed_enclosing_range::SingleLineEnclosingRange(range) => {
                let line = usize::try_from(range.line).ok()?;
                (line, line)
            }
            occurrence::Typed_enclosing_range::MultiLineEnclosingRange(range) => (
                usize::try_from(range.start_line).ok()?,
                usize::try_from(range.end_line).ok()?,
            ),
            _ => return None,
        };
        return (lines.0 <= lines.1).then_some(lines);
    }

    let lines = match occurrence.enclosing_range.as_slice() {
        [line, _, _] => {
            let line = usize::try_from(*line).ok()?;
            (line, line)
        }
        [start_line, _, end_line, _] => (
            usize::try_from(*start_line).ok()?,
            usize::try_from(*end_line).ok()?,
        ),
        _ => return None,
    };
    (lines.0 <= lines.1).then_some(lines)
}

fn is_callable(information: &SymbolInformation) -> bool {
    let callable_kind = information.kind.enum_value().is_ok_and(|kind| {
        matches!(
            kind,
            symbol_information::Kind::AbstractMethod
                | symbol_information::Kind::Accessor
                | symbol_information::Kind::Constructor
                | symbol_information::Kind::Function
                | symbol_information::Kind::Getter
                | symbol_information::Kind::Method
                | symbol_information::Kind::MethodAlias
                | symbol_information::Kind::MethodSpecification
                | symbol_information::Kind::ProtocolMethod
                | symbol_information::Kind::PureVirtualMethod
                | symbol_information::Kind::Setter
                | symbol_information::Kind::SingletonMethod
                | symbol_information::Kind::StaticMethod
                | symbol_information::Kind::TraitMethod
                | symbol_information::Kind::TypeClassMethod
        )
    });
    callable_kind
        || parse_symbol(&information.symbol)
            .ok()
            .and_then(|symbol| symbol.descriptors.last().cloned())
            .and_then(|descriptor| descriptor.suffix.enum_value().ok())
            == Some(descriptor::Suffix::Method)
}

fn has_parameter_descriptor(information: &SymbolInformation) -> bool {
    parse_symbol(&information.symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().cloned())
        .and_then(|descriptor| descriptor.suffix.enum_value().ok())
        .is_some_and(|suffix| {
            matches!(
                suffix,
                descriptor::Suffix::Parameter | descriptor::Suffix::TypeParameter
            )
        })
}

#[derive(Clone)]
struct CallableDefinition {
    symbol: String,
    kind: String,
    name: String,
    file: String,
    line: usize,
    enclosing_lines: Option<(usize, usize)>,
}

fn callable_definitions(index: &Index) -> HashMap<String, Vec<CallableDefinition>> {
    let information = symbol_information_by_id(index);
    let mut definitions: HashMap<String, Vec<CallableDefinition>> = HashMap::new();

    for document in &index.documents {
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) {
                continue;
            }
            let (Some(line), Some(information)) = (
                occurrence_start_line(occurrence),
                information.get(occurrence.symbol.as_str()).copied(),
            ) else {
                continue;
            };
            if has_parameter_descriptor(information) {
                continue;
            }
            let enclosing_lines = occurrence_enclosing_lines(occurrence);
            let has_multiline_enclosing_range =
                enclosing_lines.is_some_and(|(start, end)| start < end);
            if !is_callable(information) && !has_multiline_enclosing_range {
                continue;
            }
            definitions
                .entry(document.relative_path.clone())
                .or_default()
                .push(CallableDefinition {
                    symbol: occurrence.symbol.clone(),
                    kind: inferred_kind(information),
                    name: display_name(information),
                    file: document.relative_path.clone(),
                    line,
                    enclosing_lines,
                });
        }
    }

    for definitions in definitions.values_mut() {
        definitions
            .sort_by(|left, right| (&left.line, &left.symbol).cmp(&(&right.line, &right.symbol)));
    }
    definitions
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CallerIdentity {
    Symbol(String),
    Module(String),
}

#[derive(Clone)]
struct Caller {
    identity: CallerIdentity,
    kind: String,
    name: String,
    file: String,
    line: Option<usize>,
    count: usize,
}

impl Caller {
    fn from_definition(definition: &CallableDefinition) -> Self {
        Self {
            identity: CallerIdentity::Symbol(definition.symbol.clone()),
            kind: definition.kind.clone(),
            name: definition.name.clone(),
            file: definition.file.clone(),
            line: Some(definition.line),
            count: 0,
        }
    }

    fn module(file: &str) -> Self {
        Self {
            identity: CallerIdentity::Module(file.to_string()),
            kind: String::new(),
            name: String::new(),
            file: file.to_string(),
            line: None,
            count: 0,
        }
    }

    fn render(&self, indent: &str) -> String {
        match self.line {
            Some(line) => format!(
                "{indent}{} {} {}:{} (x{})",
                self.kind,
                self.name,
                self.file,
                line + 1,
                self.count
            ),
            None => format!("{indent}<module> {} (x{})", self.file, self.count),
        }
    }
}

fn resolve_caller(definitions: &[CallableDefinition], file: &str, reference_line: usize) -> Caller {
    let enclosing = definitions
        .iter()
        .filter(|definition| {
            definition
                .enclosing_lines
                .is_some_and(|(start, end)| start <= reference_line && reference_line <= end)
        })
        .min_by(|left, right| {
            let (left_start, left_end) = left.enclosing_lines.expect("filtered enclosing range");
            let (right_start, right_end) = right.enclosing_lines.expect("filtered enclosing range");
            (left_end - left_start)
                .cmp(&(right_end - right_start))
                .then_with(|| right_start.cmp(&left_start))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.symbol.cmp(&right.symbol))
        });
    if let Some(definition) = enclosing {
        return Caller::from_definition(definition);
    }

    definitions
        .iter()
        .filter(|definition| {
            definition.enclosing_lines.is_none() && definition.line <= reference_line
        })
        .max_by(|left, right| {
            left.line
                .cmp(&right.line)
                .then_with(|| right.symbol.cmp(&left.symbol))
        })
        .map(Caller::from_definition)
        .unwrap_or_else(|| Caller::module(file))
}

fn direct_callers(
    index: &Index,
    symbols: &HashSet<String>,
    definitions: &HashMap<String, Vec<CallableDefinition>>,
    sources: &mut SourceCache<'_>,
) -> Vec<Caller> {
    let mut callers: HashMap<CallerIdentity, Caller> = HashMap::new();

    for document in &index.documents {
        let document_definitions = definitions
            .get(&document.relative_path)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for occurrence in &document.occurrences {
            if is_definition(occurrence) || !symbols.contains(&occurrence.symbol) {
                continue;
            }
            let Some(line) = occurrence_start_line(occurrence) else {
                continue;
            };
            if sources.is_import_or_export(&document.relative_path, line) {
                continue;
            }
            let caller = resolve_caller(document_definitions, &document.relative_path, line);
            callers
                .entry(caller.identity.clone())
                .and_modify(|existing| existing.count += 1)
                .or_insert_with(|| Caller { count: 1, ..caller });
        }
    }

    let mut callers = callers.into_values().collect::<Vec<_>>();
    callers.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.name.cmp(&right.name))
    });
    callers
}

struct CallerWalk<'a> {
    index: &'a Index,
    definitions: &'a HashMap<String, Vec<CallableDefinition>>,
    sources: SourceCache<'a>,
    limit: usize,
    seen_symbols: HashSet<String>,
    lines: Vec<String>,
}

impl CallerWalk<'_> {
    fn render_symbols(&mut self, symbols: &HashSet<String>, remaining_depth: usize, level: usize) {
        let callers = direct_callers(self.index, symbols, self.definitions, &mut self.sources);
        let total = callers.len();
        let indent = "  ".repeat(level);

        for caller in callers.into_iter().take(self.limit) {
            let should_render = match &caller.identity {
                CallerIdentity::Symbol(symbol) => self.seen_symbols.insert(symbol.clone()),
                CallerIdentity::Module(_) => true,
            };
            if !should_render {
                continue;
            }
            self.lines.push(caller.render(&indent));
            if remaining_depth > 1 {
                if let CallerIdentity::Symbol(symbol) = &caller.identity {
                    self.render_symbols(
                        &HashSet::from([symbol.clone()]),
                        remaining_depth - 1,
                        level + 1,
                    );
                }
            }
        }

        if total > self.limit {
            self.lines
                .push(format!("{indent}… +{} more", total - self.limit));
        }
    }
}

fn callers(
    project_root: &Path,
    index: &Index,
    name: &str,
    depth: usize,
    limit: usize,
) -> Result<String> {
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let symbols = matching_symbol_ids(index, name);
    if symbols.is_empty() {
        return Ok("no matches".to_string());
    }

    let information = symbol_information_by_id(index);
    let mut matches = symbols
        .principals
        .iter()
        .filter_map(|symbol| {
            let information = information.get(symbol.as_str()).copied()?;
            let (file, line) = symbols.definition_sites.get(symbol)?.clone();
            Some((file, line, symbol.clone(), information))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    if matches.is_empty() {
        return Ok("no matches".to_string());
    }

    let definitions = callable_definitions(index);
    let mut lines = Vec::new();
    for (file, line, symbol, information) in matches {
        lines.push(format!(
            "## callers of {} {} {file}:{}",
            inferred_kind(information),
            display_name(information),
            line + 1
        ));
        let before = lines.len();
        let mut walk = CallerWalk {
            index,
            definitions: &definitions,
            sources: SourceCache::new(project_root),
            limit,
            seen_symbols: HashSet::new(),
            lines: Vec::new(),
        };
        walk.render_symbols(&symbols.reference_symbols(&symbol), depth, 0);
        lines.append(&mut walk.lines);
        if lines.len() == before {
            lines.push("no callers".to_string());
        }
    }

    if lines.len() > MAX_MAP_RESULT_LINES {
        let shown = MAX_MAP_RESULT_LINES - 1;
        let omitted = lines.len() - shown;
        lines.truncate(shown);
        lines.push(format!("… +{omitted} more lines"));
    }
    Ok(lines.join("\n"))
}

fn is_dead_candidate(information: &SymbolInformation) -> bool {
    if is_local_symbol(&information.symbol) {
        return false;
    }
    let suffix = parse_symbol(&information.symbol)
        .ok()
        .and_then(|symbol| symbol.descriptors.last().cloned())
        .and_then(|descriptor| descriptor.suffix.enum_value().ok());
    if matches!(
        suffix,
        Some(
            descriptor::Suffix::Local
                | descriptor::Suffix::Meta
                | descriptor::Suffix::Parameter
                | descriptor::Suffix::TypeParameter
        )
    ) {
        return false;
    }

    match information.kind.enum_value() {
        Ok(symbol_information::Kind::UnspecifiedKind) | Err(_) => {
            matches!(
                suffix,
                Some(
                    descriptor::Suffix::Method
                        | descriptor::Suffix::Term
                        | descriptor::Suffix::Type
                )
            )
        }
        Ok(kind) => {
            matches!(
                kind,
                symbol_information::Kind::AbstractMethod
                    | symbol_information::Kind::Accessor
                    | symbol_information::Kind::Class
                    | symbol_information::Kind::Constant
                    | symbol_information::Kind::Constructor
                    | symbol_information::Kind::Enum
                    | symbol_information::Kind::Field
                    | symbol_information::Kind::Function
                    | symbol_information::Kind::Getter
                    | symbol_information::Kind::Interface
                    | symbol_information::Kind::Method
                    | symbol_information::Kind::MethodAlias
                    | symbol_information::Kind::MethodSpecification
                    | symbol_information::Kind::Property
                    | symbol_information::Kind::ProtocolMethod
                    | symbol_information::Kind::PureVirtualMethod
                    | symbol_information::Kind::Setter
                    | symbol_information::Kind::SingletonMethod
                    | symbol_information::Kind::StaticDataMember
                    | symbol_information::Kind::StaticField
                    | symbol_information::Kind::StaticMethod
                    | symbol_information::Kind::StaticProperty
                    | symbol_information::Kind::StaticVariable
                    | symbol_information::Kind::TraitMethod
                    | symbol_information::Kind::Type
                    | symbol_information::Kind::TypeAlias
                    | symbol_information::Kind::TypeClassMethod
                    | symbol_information::Kind::Value
                    | symbol_information::Kind::Variable
            )
        }
    }
}

fn has_member_descriptor_shape(information: &SymbolInformation) -> bool {
    let Ok(symbol) = parse_symbol(&information.symbol) else {
        return false;
    };
    let descriptors = &symbol.descriptors;
    if descriptors.len() < 2 {
        return false;
    }
    let last = descriptors
        .last()
        .and_then(|descriptor| descriptor.suffix.enum_value().ok());
    let container = descriptors
        .get(descriptors.len() - 2)
        .and_then(|descriptor| descriptor.suffix.enum_value().ok());
    matches!(
        last,
        Some(descriptor::Suffix::Method | descriptor::Suffix::Term | descriptor::Suffix::Type)
    ) && matches!(
        container,
        Some(descriptor::Suffix::Type | descriptor::Suffix::Term)
    )
}

fn is_default_or_anonymous(name: &str) -> bool {
    let normalized = name
        .trim()
        .trim_matches(|character| matches!(character, '<' | '>' | '(' | ')'))
        .to_ascii_lowercase();
    normalized.is_empty()
        || normalized == "default"
        || normalized == "anonymous"
        || normalized.starts_with("default export")
        || normalized.starts_with("anonymous ")
}

struct DeadCandidate {
    symbol: String,
    kind: String,
    name: String,
    file: String,
    line: usize,
}

fn dead_exports(
    project_root: &Path,
    index: &Index,
    path_prefix: Option<&str>,
    limit: usize,
    exports_only: bool,
) -> Result<String> {
    let path_prefix = path_prefix
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| {
            if Path::new(prefix).is_absolute() {
                bail!("path_prefix must be relative to project_root");
            }
            if prefix
                .replace('\\', "/")
                .split('/')
                .any(|component| component == "..")
            {
                bail!("path_prefix must not contain '..'");
            }
            Ok(prefix.trim_start_matches("./").replace('\\', "/"))
        })
        .transpose()?;
    let information = symbol_information_by_id(index);
    let mut sources = SourceCache::new(project_root);
    let mut candidates = Vec::new();

    for document in &index.documents {
        if path_prefix
            .as_ref()
            .is_some_and(|prefix| !document.relative_path.starts_with(prefix))
        {
            continue;
        }
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) {
                continue;
            }
            let (Some(line), Some(information)) = (
                occurrence_start_line(occurrence),
                information.get(occurrence.symbol.as_str()).copied(),
            ) else {
                continue;
            };
            let name = display_name(information);
            if !is_dead_candidate(information)
                || name.starts_with('_')
                || is_default_or_anonymous(&name)
                || sources
                    .line(&document.relative_path, line)
                    .is_some_and(|source| source.trim_start().starts_with("export default "))
            {
                continue;
            }
            if exports_only
                && (has_member_descriptor_shape(information)
                    || sources
                        .is_exported_declaration(&document.relative_path, line)
                        .is_some_and(|exported| !exported))
            {
                continue;
            }
            candidates.push(DeadCandidate {
                symbol: occurrence.symbol.clone(),
                kind: inferred_kind(information),
                name,
                file: document.relative_path.clone(),
                line,
            });
        }
    }
    candidates.sort_by(|left, right| {
        (&left.file, &left.line, &left.symbol).cmp(&(&right.file, &right.line, &right.symbol))
    });
    let mut seen_symbols = HashSet::new();
    candidates.retain(|candidate| seen_symbols.insert(candidate.symbol.clone()));

    let mut reference_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    for document in &index.documents {
        for occurrence in &document.occurrences {
            if is_definition(occurrence) {
                continue;
            }
            *reference_counts
                .entry(occurrence.symbol.clone())
                .or_default()
                .entry(document.relative_path.clone())
                .or_default() += 1;
        }
    }

    let mut entries = Vec::new();
    for candidate in candidates {
        let counts = reference_counts.get(&candidate.symbol);
        let outside_file = counts
            .into_iter()
            .flat_map(|counts| counts.iter())
            .filter(|(file, _)| *file != &candidate.file)
            .map(|(_, count)| *count)
            .sum::<usize>();
        if outside_file > 0 {
            continue;
        }
        let in_file = counts
            .and_then(|counts| counts.get(&candidate.file))
            .copied()
            .unwrap_or_default();
        entries.push(if in_file == 0 {
            format!(
                "dead: {} {} {}:{}",
                candidate.kind,
                candidate.name,
                candidate.file,
                candidate.line + 1
            )
        } else {
            format!(
                "file-local: {} {} {}:{} (x{in_file} in-file)",
                candidate.kind,
                candidate.name,
                candidate.file,
                candidate.line + 1
            )
        });
    }

    let total = entries.len();
    let mut lines = vec![format!(
        "# dead exports (exports_only={exports_only}) — dynamic/string-based uses aren't visible to SCIP"
    )];
    lines.extend(entries.into_iter().take(limit));
    if total == 0 {
        lines.push("no matches".to_string());
    } else if total > limit {
        lines.push(format!("… +{} more", total - limit));
    }
    Ok(lines.join("\n"))
}

fn format_limited(lines: Vec<String>, limit: usize) -> String {
    let total = lines.len();
    if total == 0 {
        return "no matches".to_string();
    }
    let shown = total.min(limit);
    let mut output = lines.into_iter().take(shown).collect::<Vec<_>>();
    if shown < total {
        output.push(format!("… +{} more", total - shown));
    }
    output.join("\n")
}

fn search(index: &Index, query: &str, limit: usize) -> Result<String> {
    if query.trim().is_empty() {
        bail!("query must not be empty");
    }
    let query_lower = query.to_lowercase();
    let mut hits = Vec::new();

    for document in &index.documents {
        for information in &document.symbols {
            let name = display_name(information);
            let name_lower = name.to_lowercase();
            let rank = if name_lower == query_lower {
                0
            } else if name_lower.starts_with(&query_lower) {
                1
            } else if name_lower.contains(&query_lower) {
                2
            } else {
                continue;
            };
            let Some(line) = definition_line(document, &information.symbol) else {
                continue;
            };
            hits.push((
                rank,
                name_lower,
                document.relative_path.clone(),
                line,
                format!(
                    "{} {}:{}",
                    symbol_label(information),
                    document.relative_path,
                    line + 1
                ),
            ));
        }
    }

    hits.sort_by(|left, right| {
        (&left.0, &left.1, &left.2, &left.3).cmp(&(&right.0, &right.1, &right.2, &right.3))
    });
    Ok(format_limited(
        hits.into_iter().map(|hit| hit.4).collect(),
        limit,
    ))
}

fn definitions(project_root: &Path, index: &Index, name: &str) -> Result<String> {
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let symbols = matching_symbol_ids(index, name);
    if symbols.is_empty() {
        return Ok("no matches".to_string());
    }
    let information = symbol_information_by_id(index);
    let mut definitions = Vec::new();

    for document in &index.documents {
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) || !symbols.principals.contains(&occurrence.symbol) {
                continue;
            }
            let Some(line) = occurrence_start_line(occurrence) else {
                continue;
            };
            definitions.push((
                document.relative_path.clone(),
                line,
                occurrence.symbol.clone(),
            ));
        }
    }

    definitions.sort();
    definitions.dedup();
    let mut sources = SourceCache::new(project_root);
    let mut lines = Vec::new();
    for (file, line, symbol) in definitions {
        let Some(information) = information.get(symbol.as_str()) else {
            continue;
        };
        let location = format!("{} {file}:{}", symbol_label(information), line + 1);
        let details = symbol_details(information);
        lines.push(if details.is_empty() {
            location
        } else {
            format!("{location} {details}")
        });
        if let Some(source) = sources.display_line(&file, line) {
            lines.push(format!("def> {source}"));
        }
    }
    Ok(format_limited(lines, MAX_RESULT_LINES - 1))
}

fn map_bundle(
    project_root: &Path,
    index: &Index,
    names: &[String],
    context: bool,
    refs_limit: usize,
    include_imports: bool,
) -> Result<String> {
    if !(1..=MAX_MAP_NAMES).contains(&names.len()) {
        bail!("names must contain between 1 and {MAX_MAP_NAMES} entries");
    }
    if names.iter().any(|name| name.trim().is_empty()) {
        bail!("names must not contain empty entries");
    }

    let information = symbol_information_by_id(index);
    let mut sources = SourceCache::new(project_root);
    let mut lines = Vec::new();

    for name in names {
        let symbols = matching_symbol_ids(index, name);
        let mut matches = symbols
            .principals
            .iter()
            .filter_map(|symbol| {
                let information = information.get(symbol.as_str()).copied()?;
                let (file, line) = symbols.definition_sites.get(symbol)?.clone();
                Some((file, line, symbol.clone(), information))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2))
        });

        if matches.is_empty() {
            lines.push(format!("## no matches: {name}"));
            continue;
        }

        for (file, line, symbol, information) in matches {
            lines.push(format!(
                "## {} {file}:{}",
                symbol_label(information),
                line + 1
            ));
            let details = symbol_details(information);
            if !details.is_empty() {
                lines.push(details);
            }
            if context {
                if let Some(source) = sources.display_line(&file, line) {
                    lines.push(format!("def> {source}"));
                }
            }

            let references = symbols
                .reference_symbols(&symbol)
                .into_iter()
                .flat_map(|reference_symbol| reference_sites(index, &reference_symbol))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|(reference_file, reference_line)| {
                    include_imports || !sources.is_import_or_export(reference_file, *reference_line)
                })
                .collect::<Vec<_>>();
            let total_references = references.len();
            let shown_references = references.into_iter().take(refs_limit).collect::<Vec<_>>();

            if context {
                for (reference_file, reference_line) in shown_references {
                    let prefix = format!("{reference_file}:{}:", reference_line + 1);
                    match sources.display_line(&reference_file, reference_line) {
                        Some(source) => lines.push(format!("{prefix} {source}")),
                        None => lines.push(prefix),
                    }
                }
            } else {
                let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                for (reference_file, reference_line) in shown_references {
                    grouped
                        .entry(reference_file)
                        .or_default()
                        .push(reference_line + 1);
                }
                lines.extend(
                    grouped
                        .into_iter()
                        .map(|(reference_file, reference_lines)| {
                            let reference_lines = reference_lines
                                .into_iter()
                                .map(|reference_line| reference_line.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{reference_file}: {reference_lines}")
                        }),
                );
            }

            let omitted = total_references.saturating_sub(refs_limit);
            if omitted > 0 {
                lines.push(format!("… +{omitted} more refs"));
            }
        }
    }

    if lines.len() > MAX_MAP_RESULT_LINES {
        let shown = MAX_MAP_RESULT_LINES - 1;
        let omitted = lines.len() - shown;
        lines.truncate(shown);
        lines.push(format!("… +{omitted} more lines"));
    }
    Ok(lines.join("\n"))
}

fn references(index: &Index, name: &str, limit: usize) -> Result<String> {
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let symbols = matching_symbol_ids(index, name);
    if symbols.is_empty() {
        return Ok("no matches".to_string());
    }
    let reference_symbols = symbols.all_reference_symbols();

    let mut locations = BTreeSet::new();
    for document in &index.documents {
        for occurrence in &document.occurrences {
            if is_definition(occurrence) || !reference_symbols.contains(occurrence.symbol.as_str())
            {
                continue;
            }
            if let Some(line) = occurrence_start_line(occurrence) {
                locations.insert((document.relative_path.clone(), line + 1));
            }
        }
    }
    if locations.is_empty() {
        return Ok("no references".to_string());
    }

    let total = locations.len();
    let mut grouped: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (file, line) in locations.into_iter().take(limit) {
        grouped.entry(file).or_default().push(line);
    }
    let mut rendered = grouped
        .into_iter()
        .map(|(file, lines)| {
            let count = lines.len();
            let lines = lines
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            (format!("{file}: {lines}"), count)
        })
        .collect::<Vec<_>>();

    let mut omitted = total.saturating_sub(limit);
    if rendered.len() >= MAX_RESULT_LINES {
        omitted += rendered
            .iter()
            .skip(MAX_RESULT_LINES - 1)
            .map(|(_, count)| count)
            .sum::<usize>();
        rendered.truncate(MAX_RESULT_LINES - 1);
    }
    let mut lines = rendered
        .into_iter()
        .map(|(line, _)| line)
        .collect::<Vec<_>>();
    if omitted > 0 {
        lines.push(format!("… +{omitted} more"));
    }
    Ok(lines.join("\n"))
}

fn outline(index: &Index, file: &str) -> Result<String> {
    if file.trim().is_empty() {
        bail!("file must not be empty");
    }
    if Path::new(file).is_absolute() {
        bail!("file must be relative to project_root");
    }
    let normalized = file.trim_start_matches("./").replace('\\', "/");
    let Some(document) = index
        .documents
        .iter()
        .find(|document| document.relative_path == normalized)
    else {
        bail!("file not found in index: {normalized}");
    };
    let information = symbol_information_by_id(index);
    let mut definitions = Vec::new();

    for occurrence in &document.occurrences {
        if !is_definition(occurrence) {
            continue;
        }
        let (Some(line), Some(information)) = (
            occurrence_start_line(occurrence),
            information.get(occurrence.symbol.as_str()),
        ) else {
            continue;
        };
        definitions.push((line, inferred_kind(information), display_name(information)));
    }
    definitions
        .sort_by(|left, right| (&left.0, &left.1, &left.2).cmp(&(&right.0, &right.1, &right.2)));
    definitions.dedup();

    Ok(format_limited(
        definitions
            .into_iter()
            .map(|(line, kind, name)| format!("{} {kind} {name}", line + 1))
            .collect(),
        MAX_RESULT_LINES - 1,
    ))
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

fn run_indexer(
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
    Ok(format_index_stats(
        IndexStats::from_loaded(&loaded),
        &selection,
    ))
}

#[derive(Deserialize)]
struct IndexArgs {
    project_root: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    max_file_mb: Option<u64>,
}

#[derive(Deserialize)]
struct SearchArgs {
    project_root: String,
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct NameArgs {
    project_root: String,
    name: String,
}

#[derive(Deserialize)]
struct MapArgs {
    project_root: String,
    names: Vec<String>,
    #[serde(default = "default_true")]
    context: bool,
    #[serde(default = "default_map_refs_limit")]
    refs_limit: usize,
    #[serde(default)]
    include_imports: bool,
}

#[derive(Deserialize)]
struct RefsArgs {
    project_root: String,
    name: String,
    #[serde(default = "default_refs_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct CallersArgs {
    project_root: String,
    name: String,
    #[serde(default = "default_caller_depth")]
    depth: usize,
    #[serde(default = "default_callers_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct DeadArgs {
    project_root: String,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default = "default_dead_limit")]
    limit: usize,
    #[serde(default = "default_true")]
    exports_only: bool,
}

#[derive(Deserialize)]
struct OutlineArgs {
    project_root: String,
    file: String,
}

fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

fn default_refs_limit() -> usize {
    DEFAULT_REFS_LIMIT
}

fn default_map_refs_limit() -> usize {
    DEFAULT_MAP_REFS_LIMIT
}

fn default_callers_limit() -> usize {
    DEFAULT_CALLERS_LIMIT
}

fn default_dead_limit() -> usize {
    DEFAULT_DEAD_LIMIT
}

fn default_caller_depth() -> usize {
    1
}

fn default_true() -> bool {
    true
}

fn bounded_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

fn parse_arguments<T: DeserializeOwned>(arguments: &Value) -> Result<T> {
    serde_json::from_value(arguments.clone()).context("invalid arguments")
}

#[derive(Debug, PartialEq, Eq)]
struct ToolResult {
    text: String,
    is_error: bool,
}

impl ToolResult {
    fn success(text: String) -> Self {
        Self {
            text,
            is_error: false,
        }
    }

    fn error(error: impl std::fmt::Display) -> Self {
        Self {
            text: error.to_string(),
            is_error: true,
        }
    }

    fn into_json(self) -> Value {
        json!({
            "content": [{"type": "text", "text": self.text}],
            "isError": self.is_error
        })
    }
}

#[derive(Default)]
struct Server {
    cache: IndexCache,
}

impl Server {
    fn call_tool(&mut self, name: &str, arguments: &Value) -> ToolResult {
        let result = match name {
            "scip_index" => parse_arguments::<IndexArgs>(arguments).and_then(|arguments| {
                run_indexer(
                    Path::new(&arguments.project_root),
                    arguments.language.as_deref(),
                    arguments.max_file_mb,
                    &mut self.cache,
                )
            }),
            "scip_search" => parse_arguments::<SearchArgs>(arguments).and_then(|arguments| {
                let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                search(
                    &loaded.index,
                    &arguments.query,
                    bounded_limit(arguments.limit),
                )
            }),
            "scip_def" => parse_arguments::<NameArgs>(arguments).and_then(|arguments| {
                let project_root = Path::new(&arguments.project_root);
                let loaded = self.cache.load(project_root)?;
                definitions(project_root, &loaded.index, &arguments.name)
            }),
            "scip_map" => parse_arguments::<MapArgs>(arguments).and_then(|arguments| {
                let project_root = Path::new(&arguments.project_root);
                let loaded = self.cache.load(project_root)?;
                map_bundle(
                    project_root,
                    &loaded.index,
                    &arguments.names,
                    arguments.context,
                    bounded_limit(arguments.refs_limit),
                    arguments.include_imports,
                )
            }),
            "scip_refs" => parse_arguments::<RefsArgs>(arguments).and_then(|arguments| {
                let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                references(
                    &loaded.index,
                    &arguments.name,
                    bounded_limit(arguments.limit),
                )
            }),
            "scip_callers" => parse_arguments::<CallersArgs>(arguments).and_then(|arguments| {
                let project_root = Path::new(&arguments.project_root);
                let loaded = self.cache.load(project_root)?;
                callers(
                    project_root,
                    &loaded.index,
                    &arguments.name,
                    arguments.depth.clamp(1, MAX_CALLER_DEPTH),
                    bounded_limit(arguments.limit),
                )
            }),
            "scip_dead" => parse_arguments::<DeadArgs>(arguments).and_then(|arguments| {
                let project_root = Path::new(&arguments.project_root);
                let loaded = self.cache.load(project_root)?;
                dead_exports(
                    project_root,
                    &loaded.index,
                    arguments.path_prefix.as_deref(),
                    bounded_limit(arguments.limit),
                    arguments.exports_only,
                )
            }),
            "scip_outline" => parse_arguments::<OutlineArgs>(arguments).and_then(|arguments| {
                let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                outline(&loaded.index, &arguments.file)
            }),
            _ => Err(anyhow!("unknown tool: {name}")),
        };

        match result {
            Ok(text) => ToolResult::success(text),
            Err(error) => ToolResult::error(error),
        }
    }

    fn dispatch_line(&mut self, line: &str) -> Option<String> {
        let request: Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(error) => {
                return Some(rpc_error(
                    Value::Null,
                    -32700,
                    &format!("parse error: {error}"),
                ))
            }
        };
        let has_id = request.get("id").is_some();
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return has_id.then(|| rpc_error(id, -32600, "invalid request"));
        };

        if method == "notifications/initialized" {
            return None;
        }

        let response = match method {
            "initialize" => {
                let protocol_version = request
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                rpc_success(
                    id,
                    json!({
                        "protocolVersion": protocol_version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {
                            "name": "crux",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                )
            }
            "tools/list" => rpc_success(id, json!({"tools": tool_definitions()})),
            "tools/call" => {
                let Some(name) = request.pointer("/params/name").and_then(Value::as_str) else {
                    return has_id.then(|| rpc_error(id, -32602, "missing tool name"));
                };
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                rpc_success(id, self.call_tool(name, &arguments).into_json())
            }
            "ping" => rpc_success(id, json!({})),
            _ => rpc_error(id, -32601, "method not found"),
        };

        has_id.then_some(response)
    }
}

fn rpc_success(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
    .to_string()
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "scip_index",
            "description": "Create or refresh the project's SCIP index.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "language": {
                        "type": "string",
                        "enum": ["typescript", "python", "rust", "dart", "java", "cpp"],
                        "description": "Optional language override; otherwise detected from project markers"
                    },
                    "max_file_mb": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional scip-typescript maximum file size in MB"
                    }
                }),
                &["project_root"]
            )
        }),
        json!({
            "name": "scip_map",
            "description": "one call answers: definitions, signatures, and all reference sites with source context for up to 8 symbols — prefer this over repeated scip_def/scip_refs",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "names": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_MAP_NAMES
                    },
                    "context": {
                        "type": "boolean",
                        "default": true
                    },
                    "refs_limit": {
                        "type": "integer",
                        "default": DEFAULT_MAP_REFS_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    },
                    "include_imports": {
                        "type": "boolean",
                        "default": false
                    }
                }),
                &["project_root", "names"]
            )
        }),
        json!({
            "name": "scip_search",
            "description": "Search symbol display names and return disambiguated definition locations.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "query": {"type": "string"},
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_SEARCH_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    }
                }),
                &["project_root", "query"]
            )
        }),
        json!({
            "name": "scip_def",
            "description": "Find definition sites with signatures and source lines for one name; prefer scip_map for multi-symbol navigation.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "name": {"type": "string"}
                }),
                &["project_root", "name"]
            )
        }),
        json!({
            "name": "scip_refs",
            "description": "Find reference lines grouped by file for one name; prefer scip_map for source context.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "name": {"type": "string"},
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_REFS_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    }
                }),
                &["project_root", "name"]
            )
        }),
        json!({
            "name": "scip_callers",
            "description": "who calls X (transitive) — resolve references to their enclosing functions or module",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "name": {"type": "string"},
                    "depth": {
                        "type": "integer",
                        "default": 1,
                        "minimum": 1,
                        "maximum": MAX_CALLER_DEPTH
                    },
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_CALLERS_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    }
                }),
                &["project_root", "name"]
            )
        }),
        json!({
            "name": "scip_dead",
            "description": "find exports nothing uses — audit before deleting; separates fully unused from file-local symbols",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "path_prefix": {
                        "type": "string",
                        "description": "Optional project-relative path prefix"
                    },
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_DEAD_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    },
                    "exports_only": {
                        "type": "boolean",
                        "default": true,
                        "description": "Only report top-level exported declarations"
                    }
                }),
                &["project_root"]
            )
        }),
        json!({
            "name": "scip_outline",
            "description": "Return the definition skeleton for one indexed file.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "file": {
                        "type": "string",
                        "description": "Project-relative file path"
                    }
                }),
                &["project_root", "file"]
            )
        }),
    ]
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn run_stdio() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut server = Server::default();

    for line in stdin.lock().lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.dispatch_line(&line) {
            writeln!(stdout, "{response}").context("write stdout")?;
            stdout.flush().context("flush stdout")?;
        }
    }
    Ok(())
}

fn run() -> Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => run_stdio(),
        [flag] if flag == "--version" || flag == "-V" => {
            println!("crux {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [command, project_root] if command == "check" => {
            let mut cache = IndexCache::default();
            let loaded = cache.load(Path::new(project_root))?;
            println!("{}", IndexStats::from_loaded(&loaded).compact());
            Ok(())
        }
        _ => bail!("usage: crux [--version | check <absolute-project-root>]"),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::{EnumOrUnknown, MessageField};
    use scip::types::{Signature, SingleLineRange};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    const DATE_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 date.ts/formatDate().";
    const LONG_DATE_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 date.ts/formatDateLong().";
    const RNG_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 rng.ts/rngState().";
    const RNG_PROPERTY_SYMBOL: &str =
        "scip-typescript npm fixture 1.0.0 persistence.ts/SaveData#rngState.";
    const USER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 user.ts/User#";
    const DEAD_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/neverUsed().";
    const FILE_LOCAL_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/localOnly().";
    const PARAMETER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/localOnly().(input)";
    const DEFAULT_EXPORT_SYMBOL: &str =
        "scip-typescript npm fixture 1.0.0 dead.ts/defaultHelper().";
    const INTERFACE_MEMBER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/Worker#run().";
    const EXPORTED_LIMIT_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/LIMIT.";
    const PRIVATE_HELPER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 dead.ts/helper.";
    const INNER_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 date.ts/inner().";
    const BARREL_ALIAS_SYMBOL: &str = "scip-typescript npm fixture 1.0.0 barrel.ts/formatDate().";
    const DUPLICATE_DATE_SYMBOL: &str =
        "scip-typescript npm fixture 1.0.0 date.ts/formatDateDuplicate().";
    const OBJECT_METHOD_SYMBOL: &str =
        "scip-typescript npm fixture 1.0.0 app.ts/registerCodeFix().getCodeActions.";
    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestProject {
        root: PathBuf,
    }

    impl TestProject {
        fn new() -> Self {
            let sequence = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after unix epoch")
                .as_nanos();
            let root = env::temp_dir().join(format!(
                "crux-test-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(root.join(".scip-nav")).expect("create test project");
            Self { root }
        }
    }

    impl Drop for TestProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn symbol_information(
        symbol: &str,
        display_name: &str,
        kind: symbol_information::Kind,
        signature: &str,
        documentation: &str,
    ) -> SymbolInformation {
        SymbolInformation {
            symbol: symbol.to_string(),
            documentation: vec![documentation.to_string()],
            kind: EnumOrUnknown::new(kind),
            display_name: display_name.to_string(),
            signature_documentation: MessageField::some(Signature {
                language: "typescript".to_string(),
                text: signature.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn legacy_occurrence(symbol: &str, line: i32, definition: bool) -> Occurrence {
        Occurrence {
            range: vec![line, 0, 8],
            symbol: symbol.to_string(),
            symbol_roles: if definition {
                SymbolRole::Definition as i32
            } else {
                SymbolRole::ReadAccess as i32
            },
            enclosing_range: if definition {
                vec![line, 0, line + 8, 0]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    fn typed_occurrence(symbol: &str, line: i32, definition: bool) -> Occurrence {
        Occurrence {
            symbol: symbol.to_string(),
            symbol_roles: if definition {
                SymbolRole::Definition as i32
            } else {
                SymbolRole::ReadAccess as i32
            },
            typed_range: Some(occurrence::Typed_range::SingleLineRange(SingleLineRange {
                line,
                start_character: 0,
                end_character: 8,
                ..Default::default()
            })),
            enclosing_range: if definition {
                vec![line, 0, line + 8, 0]
            } else {
                Vec::new()
            },
            ..Default::default()
        }
    }

    fn fixture(include_extra_document: bool) -> Index {
        let date = symbol_information(
            DATE_SYMBOL,
            "formatDate",
            symbol_information::Kind::Function,
            "function formatDate(input: Date): string",
            "Formats a date\nfor display.",
        );
        let date_long = symbol_information(
            LONG_DATE_SYMBOL,
            "formatDateLong",
            symbol_information::Kind::Function,
            "function formatDateLong(input: Date): string",
            "Formats a long date.",
        );
        let user = symbol_information(
            USER_SYMBOL,
            "User",
            symbol_information::Kind::Class,
            "class User",
            "Application user.",
        );
        let rng = symbol_information(
            RNG_SYMBOL,
            "rngState",
            symbol_information::Kind::Function,
            "function rngState(): number",
            "Returns the next random state.",
        );
        let rng_property = symbol_information(
            RNG_PROPERTY_SYMBOL,
            "rngState",
            symbol_information::Kind::Property,
            "rngState: number",
            "Persisted random state.",
        );
        let dead = symbol_information(
            DEAD_SYMBOL,
            "neverUsed",
            symbol_information::Kind::Function,
            "function neverUsed(): void",
            "Never referenced.",
        );
        let file_local = symbol_information(
            FILE_LOCAL_SYMBOL,
            "localOnly",
            symbol_information::Kind::Function,
            "function localOnly(): void",
            "Only referenced in this file.",
        );
        let parameter = symbol_information(
            PARAMETER_SYMBOL,
            "input",
            symbol_information::Kind::Parameter,
            "input: string",
            "Function parameter.",
        );
        let default_export = symbol_information(
            DEFAULT_EXPORT_SYMBOL,
            "defaultHelper",
            symbol_information::Kind::Function,
            "function defaultHelper(): void",
            "Named default export.",
        );
        let interface_member = symbol_information(
            INTERFACE_MEMBER_SYMBOL,
            "run",
            symbol_information::Kind::Method,
            "run(): void",
            "Interface method.",
        );
        let exported_limit = symbol_information(
            EXPORTED_LIMIT_SYMBOL,
            "LIMIT",
            symbol_information::Kind::Constant,
            "const LIMIT: number",
            "Exported limit.",
        );
        let private_helper = symbol_information(
            PRIVATE_HELPER_SYMBOL,
            "helper",
            symbol_information::Kind::Constant,
            "const helper: number",
            "Private helper.",
        );

        let mut documents = vec![
            Document {
                language: "typescript".to_string(),
                relative_path: "src/lib/date.ts".to_string(),
                occurrences: vec![
                    legacy_occurrence(LONG_DATE_SYMBOL, 9, true),
                    legacy_occurrence(DATE_SYMBOL, 14, false),
                    legacy_occurrence(DATE_SYMBOL, 41, true),
                ],
                symbols: vec![date, date_long],
                ..Default::default()
            },
            Document {
                language: "typescript".to_string(),
                relative_path: "src/lib/dead.ts".to_string(),
                occurrences: vec![
                    legacy_occurrence(FILE_LOCAL_SYMBOL, 1, true),
                    legacy_occurrence(PARAMETER_SYMBOL, 2, true),
                    legacy_occurrence(PARAMETER_SYMBOL, 3, false),
                    legacy_occurrence(FILE_LOCAL_SYMBOL, 5, false),
                    legacy_occurrence(DEAD_SYMBOL, 10, true),
                    legacy_occurrence(DEFAULT_EXPORT_SYMBOL, 12, true),
                    legacy_occurrence(INTERFACE_MEMBER_SYMBOL, 15, true),
                    legacy_occurrence(EXPORTED_LIMIT_SYMBOL, 17, true),
                    legacy_occurrence(PRIVATE_HELPER_SYMBOL, 18, true),
                ],
                symbols: vec![
                    dead,
                    file_local,
                    parameter,
                    default_export,
                    interface_member,
                    exported_limit,
                    private_helper,
                ],
                ..Default::default()
            },
            Document {
                language: "typescript".to_string(),
                relative_path: "src/model/user.ts".to_string(),
                occurrences: vec![typed_occurrence(USER_SYMBOL, 4, true)],
                symbols: vec![user],
                ..Default::default()
            },
            Document {
                language: "typescript".to_string(),
                relative_path: "src/lib/rng.ts".to_string(),
                occurrences: vec![legacy_occurrence(RNG_SYMBOL, 2, true)],
                symbols: vec![rng],
                ..Default::default()
            },
            Document {
                language: "typescript".to_string(),
                relative_path: "src/persistence.ts".to_string(),
                occurrences: vec![legacy_occurrence(RNG_PROPERTY_SYMBOL, 33, true)],
                symbols: vec![rng_property],
                ..Default::default()
            },
            Document {
                language: "typescript".to_string(),
                relative_path: "src/app.ts".to_string(),
                occurrences: vec![
                    legacy_occurrence(USER_SYMBOL, 5, false),
                    legacy_occurrence(DATE_SYMBOL, 7, false),
                    legacy_occurrence(DATE_SYMBOL, 11, false),
                    legacy_occurrence(RNG_SYMBOL, 19, false),
                    legacy_occurrence(RNG_PROPERTY_SYMBOL, 24, false),
                    legacy_occurrence(RNG_SYMBOL, 29, false),
                    typed_occurrence(DATE_SYMBOL, 39, false),
                    legacy_occurrence(LONG_DATE_SYMBOL, 40, false),
                ],
                ..Default::default()
            },
        ];

        if include_extra_document {
            documents.push(Document {
                language: "typescript".to_string(),
                relative_path: "src/extra.ts".to_string(),
                occurrences: vec![legacy_occurrence(DATE_SYMBOL, 2, false)],
                ..Default::default()
            });
        }

        Index {
            documents,
            ..Default::default()
        }
    }

    fn write_index(project: &TestProject, index: &Index) {
        let bytes = index.write_to_bytes().expect("serialize fixture");
        fs::write(index_path(&project.root), bytes).expect("write fixture");
    }

    fn write_source(
        project: &TestProject,
        relative_path: &str,
        line_count: usize,
        populated_lines: &[(usize, &str)],
    ) {
        let path = project.root.join(relative_path);
        fs::create_dir_all(path.parent().expect("source parent")).expect("create source parent");
        let mut lines = vec![String::new(); line_count];
        for (line, source) in populated_lines {
            lines[*line] = (*source).to_string();
        }
        fs::write(path, lines.join("\n")).expect("write source fixture");
    }

    fn write_fixture(project: &TestProject, include_extra_document: bool) {
        write_index(project, &fixture(include_extra_document));
        write_source(
            project,
            "src/lib/date.ts",
            50,
            &[
                (9, "export function formatDateLong(input: Date): string {"),
                (14, "return formatDate(input);"),
                (41, "export function formatDate(input: Date): string {"),
            ],
        );
        write_source(
            project,
            "src/lib/dead.ts",
            24,
            &[
                (1, "export function localOnly(input: string): void {"),
                (2, "void input;"),
                (3, "console.log(input);"),
                (5, "localOnly('again');"),
                (10, "export function neverUsed(): void {}"),
                (12, "export default function defaultHelper(): void {}"),
                (14, "export interface Worker {"),
                (15, "run(): void;"),
                (16, "}"),
                (17, "export const LIMIT = 5;"),
                (18, "const helper = 1;"),
            ],
        );
        write_source(
            project,
            "src/model/user.ts",
            10,
            &[(4, "export class User {")],
        );
        write_source(
            project,
            "src/lib/rng.ts",
            10,
            &[(2, "export function rngState(): number {")],
        );
        write_source(project, "src/persistence.ts", 40, &[(33, "rngState: 7,")]);
        write_source(
            project,
            "src/app.ts",
            45,
            &[
                (5, "const user = new User();"),
                (6, "import {"),
                (7, "formatDate"),
                (8, "} from './lib/date';"),
                (11, "import { formatDate } from './lib/date';"),
                (19, "const seed = rngState();"),
                (24, "save.rngState += 1;"),
                (29, "export const EPOCH = rngState();"),
                (31, "getCodeActions(context) {"),
                (34, "return formatDate(today);"),
                (36, "}"),
                (39, "const label = formatDate(today);"),
                (40, "const longLabel = formatDateLong(today);"),
            ],
        );
        if include_extra_document {
            write_source(
                project,
                "src/extra.ts",
                5,
                &[(2, "const extraLabel = formatDate(today);")],
            );
        }
    }

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
    fn scip_index_empty_project_returns_auto_detection_hint() {
        let project = TestProject::new();
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_index",
            &json!({
                "project_root": project.root
            }),
        );

        assert!(result.is_error);
        assert_eq!(
            result.text,
            "could not auto-detect a supported language — add a project marker or pass language (typescript|python|rust|dart|java|cpp)"
        );
    }

    #[test]
    fn search_ranks_exact_then_prefix_and_truncates() {
        let output = search(&fixture(false), "FORMATDATE", 1).expect("search");
        assert_eq!(output, "function formatDate src/lib/date.ts:42\n… +1 more");
    }

    #[test]
    fn definition_includes_compact_signature_and_documentation() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = fixture(false);
        let output = definitions(&project.root, &index, "formatDate").expect("definitions");
        assert_eq!(
            output,
            "function formatDate src/lib/date.ts:42 function formatDate(input: Date): string — Formats a date for display.\ndef> export function formatDate(input: Date): string {"
        );

        let fallback =
            definitions(&project.root, &index, "formatDateLo").expect("fallback definition");
        assert!(fallback.starts_with("function formatDateLong src/lib/date.ts:10"));
        assert!(fallback.contains("def> export function formatDateLong"));
    }

    #[test]
    fn scip_map_returns_a_complete_basic_bundle_with_defaults() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_map",
            &json!({
                "project_root": project.root,
                "names": ["formatDate"]
            }),
        );

        assert!(!result.is_error);
        assert_eq!(
            result.text,
            "## function formatDate src/lib/date.ts:42\nfunction formatDate(input: Date): string — Formats a date for display.\ndef> export function formatDate(input: Date): string {\nsrc/app.ts:40: const label = formatDate(today);\nsrc/lib/date.ts:15: return formatDate(input);"
        );
    }

    #[test]
    fn same_named_symbols_stay_disambiguated() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = fixture(false);

        let search_output = search(&index, "rngState", 10).expect("search rngState");
        assert_eq!(
            search_output,
            "function rngState src/lib/rng.ts:3\nproperty rngState (SaveData) src/persistence.ts:34"
        );

        let definition_output =
            definitions(&project.root, &index, "rngState").expect("define rngState");
        assert!(definition_output.contains("function rngState src/lib/rng.ts:3"));
        assert!(definition_output.contains("property rngState (SaveData) src/persistence.ts:34"));

        let map_output = map_bundle(
            &project.root,
            &index,
            &["rngState".to_string()],
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("map rngState");
        assert_eq!(map_output.matches("## ").count(), 2);
        assert!(map_output.contains("## function rngState src/lib/rng.ts:3"));
        assert!(map_output.contains("src/app.ts:20: const seed = rngState();"));
        assert!(map_output.contains("src/app.ts:30: export const EPOCH = rngState();"));
        assert!(map_output.contains("## property rngState (SaveData) src/persistence.ts:34"));
        assert!(map_output.contains("src/app.ts:25: save.rngState += 1;"));
    }

    #[test]
    fn import_export_classifier_only_filters_imports_and_reexports() {
        for source in [
            "import { value } from './module';",
            "import{value} from './module';",
            "export { value } from './module';",
            "export * from './module';",
            "export type { Value } from './module';",
            "export type * from './module';",
            "formatDate",
        ] {
            assert!(
                is_import_or_export_line(source),
                "expected filter: {source}"
            );
        }

        for source in [
            "export const X = f();",
            "export function f() {}",
            "export class X {}",
            "export let x = 1;",
            "export var x = 1;",
            "export default f(value);",
        ] {
            assert!(
                !is_import_or_export_line(source),
                "expected reference line: {source}"
            );
        }
    }

    #[test]
    fn scip_map_filters_import_lines_unless_requested() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let index = fixture(false);
        let names = ["formatDate".to_string()];

        let filtered = map_bundle(
            &project.root,
            &index,
            &names,
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("filtered map");
        assert!(!filtered.contains("src/app.ts:8:"));
        assert!(!filtered.contains("src/app.ts:12:"));
        assert!(filtered.contains("src/app.ts:40: const label = formatDate(today);"));

        let included = map_bundle(
            &project.root,
            &index,
            &names,
            true,
            DEFAULT_MAP_REFS_LIMIT,
            true,
        )
        .expect("unfiltered map");
        assert!(included.contains("src/app.ts:8: formatDate"));
        assert!(included.contains("src/app.ts:12: import { formatDate } from './lib/date';"));
    }

    #[test]
    fn exact_matches_at_one_definition_site_keep_the_richer_symbol() {
        let mut index = fixture(false);
        let date_document = index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/lib/date.ts")
            .expect("date document");
        date_document.symbols.push(SymbolInformation {
            symbol: DUPLICATE_DATE_SYMBOL.to_string(),
            display_name: "formatDate".to_string(),
            ..Default::default()
        });
        date_document
            .occurrences
            .push(legacy_occurrence(DUPLICATE_DATE_SYMBOL, 41, true));

        let matches = matching_symbol_ids(&index, "formatDate");
        assert_eq!(matches.principals, HashSet::from([DATE_SYMBOL.to_string()]));
        assert_eq!(
            matches.reference_symbols(DATE_SYMBOL),
            HashSet::from([DATE_SYMBOL.to_string(), DUPLICATE_DATE_SYMBOL.to_string()])
        );
    }

    #[test]
    fn pure_alias_references_fold_into_the_defined_symbol() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        index.external_symbols.push(SymbolInformation {
            symbol: BARREL_ALIAS_SYMBOL.to_string(),
            display_name: "formatDate".to_string(),
            ..Default::default()
        });
        index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/app.ts")
            .expect("app document")
            .occurrences
            .push(legacy_occurrence(BARREL_ALIAS_SYMBOL, 34, false));

        let map = map_bundle(
            &project.root,
            &index,
            &["formatDate".to_string()],
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("alias map");
        assert_eq!(map.matches("## ").count(), 1);
        assert!(map.contains("src/app.ts:35: return formatDate(today);"));
        assert!(map.contains("src/app.ts:40: const label = formatDate(today);"));

        let refs = references(&index, "formatDate", DEFAULT_REFS_LIMIT).expect("alias refs");
        assert!(refs.contains("src/app.ts: 8, 12, 35, 40"));

        let callers = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("alias callers");
        assert!(callers.contains("<module> src/app.ts (x2)"));
    }

    #[test]
    fn scip_map_without_context_uses_grouped_reference_format() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = map_bundle(
            &project.root,
            &fixture(false),
            &["formatDate".to_string()],
            false,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("context-free map");

        assert_eq!(
            output,
            "## function formatDate src/lib/date.ts:42\nfunction formatDate(input: Date): string — Formats a date for display.\nsrc/app.ts: 40\nsrc/lib/date.ts: 15"
        );
        assert!(!output.contains("def>"));
    }

    #[test]
    fn scip_map_caps_global_output_at_250_lines() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let mut occurrences = Vec::new();
        let mut sources = Vec::new();
        for line in 0..130 {
            occurrences.push(legacy_occurrence(RNG_SYMBOL, line * 2, false));
            occurrences.push(legacy_occurrence(RNG_PROPERTY_SYMBOL, line * 2 + 1, false));
            sources.push(format!("const seed{line} = rngState();"));
            sources.push(format!("save.rngState += {line};"));
        }
        index.documents.push(Document {
            language: "typescript".to_string(),
            relative_path: "src/many.ts".to_string(),
            occurrences,
            ..Default::default()
        });
        fs::write(project.root.join("src/many.ts"), sources.join("\n"))
            .expect("write many-reference source");

        let output = map_bundle(
            &project.root,
            &index,
            &["rngState".to_string()],
            true,
            MAX_LIMIT,
            false,
        )
        .expect("large map");
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), MAX_MAP_RESULT_LINES);
        assert_eq!(lines.last(), Some(&"… +20 more lines"));
    }

    #[test]
    fn scip_map_reports_each_zero_match_name() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = map_bundle(
            &project.root,
            &fixture(false),
            &["missingSymbol".to_string()],
            true,
            DEFAULT_MAP_REFS_LIMIT,
            false,
        )
        .expect("zero-match map");
        assert_eq!(output, "## no matches: missingSymbol");
    }

    #[test]
    fn references_are_grouped_by_file_and_limited() {
        let output = references(&fixture(true), "formatDate", 2).expect("references");
        assert_eq!(output, "src/app.ts: 8, 12\n… +3 more");
    }

    #[test]
    fn scip_callers_filters_imports_and_resolves_enclosing_definitions() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_callers",
            &json!({
                "project_root": project.root,
                "name": "formatDate"
            }),
        );

        assert!(!result.is_error);
        assert_eq!(
            result.text,
            "## callers of function formatDate src/lib/date.ts:42\n<module> src/app.ts (x1)\nfunction formatDateLong src/lib/date.ts:10 (x1)"
        );
    }

    #[test]
    fn scip_callers_depth_two_walks_up_to_the_callers_caller() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = callers(
            &project.root,
            &fixture(false),
            "formatDate",
            2,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("transitive callers");
        assert_eq!(
            output,
            "## callers of function formatDate src/lib/date.ts:42\n<module> src/app.ts (x1)\nfunction formatDateLong src/lib/date.ts:10 (x1)\n  <module> src/app.ts (x1)"
        );
    }

    #[test]
    fn scip_callers_falls_back_when_definition_enclosing_range_is_empty() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let long_date_definition = index
            .documents
            .iter_mut()
            .flat_map(|document| document.occurrences.iter_mut())
            .find(|occurrence| occurrence.symbol == LONG_DATE_SYMBOL && is_definition(occurrence))
            .expect("formatDateLong definition");
        long_date_definition.enclosing_range.clear();

        let output = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("fallback callers");
        assert!(output.contains("function formatDateLong src/lib/date.ts:10 (x1)"));
    }

    #[test]
    fn scip_callers_prefers_the_innermost_enclosing_definition() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let date_document = index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/lib/date.ts")
            .expect("date document");
        date_document.symbols.push(symbol_information(
            INNER_SYMBOL,
            "inner",
            symbol_information::Kind::Function,
            "function inner(): void",
            "Nested function.",
        ));
        let mut inner_definition = legacy_occurrence(INNER_SYMBOL, 12, true);
        inner_definition.enclosing_range = vec![12, 0, 16, 0];
        date_document.occurrences.push(inner_definition);

        let output = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("innermost callers");
        assert!(output.contains("function inner src/lib/date.ts:13 (x1)"));
        assert!(!output.contains("function formatDateLong"));
    }

    #[test]
    fn scip_callers_attributes_object_literal_methods_with_enclosing_ranges() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut index = fixture(false);
        let app_document = index
            .documents
            .iter_mut()
            .find(|document| document.relative_path == "src/app.ts")
            .expect("app document");
        app_document.symbols.push(symbol_information(
            OBJECT_METHOD_SYMBOL,
            "getCodeActions",
            symbol_information::Kind::Property,
            "getCodeActions(context): string",
            "Object-literal method.",
        ));
        let mut method_definition = legacy_occurrence(OBJECT_METHOD_SYMBOL, 31, true);
        method_definition.enclosing_range = vec![31, 0, 36, 0];
        app_document.occurrences.push(method_definition);
        app_document
            .occurrences
            .push(legacy_occurrence(DATE_SYMBOL, 34, false));

        let output = callers(
            &project.root,
            &index,
            "formatDate",
            1,
            DEFAULT_CALLERS_LIMIT,
        )
        .expect("object-literal callers");
        assert!(output.contains("property getCodeActions src/app.ts:32 (x1)"));
        assert!(output.contains("<module> src/app.ts (x1)"));
    }

    #[test]
    fn scip_dead_separates_unused_and_file_local_exports_and_skips_parameters() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_dead",
            &json!({
                "project_root": project.root,
                "path_prefix": "src/lib/dead.ts"
            }),
        );
        assert!(!result.is_error);
        let output = result.text;
        assert_eq!(
            output,
            "# dead exports (exports_only=true) — dynamic/string-based uses aren't visible to SCIP\nfile-local: function localOnly src/lib/dead.ts:2 (x1 in-file)\ndead: function neverUsed src/lib/dead.ts:11\ndead: constant LIMIT src/lib/dead.ts:18"
        );
        assert!(!output.contains("parameter"));
        assert!(!output.contains(" input "));
        assert!(!output.contains("defaultHelper"));
        assert!(!output.contains("method run"));
        assert!(!output.contains("constant helper"));
    }

    #[test]
    fn scip_dead_exports_only_false_preserves_all_declaration_candidates() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let output = dead_exports(
            &project.root,
            &fixture(false),
            Some("src/lib/dead.ts"),
            DEFAULT_DEAD_LIMIT,
            false,
        )
        .expect("all dead declarations");
        assert_eq!(
            output,
            "# dead exports (exports_only=false) — dynamic/string-based uses aren't visible to SCIP\nfile-local: function localOnly src/lib/dead.ts:2 (x1 in-file)\ndead: function neverUsed src/lib/dead.ts:11\ndead: method run src/lib/dead.ts:16\ndead: constant LIMIT src/lib/dead.ts:18\ndead: constant helper src/lib/dead.ts:19"
        );
    }

    #[test]
    fn outline_is_a_sorted_file_skeleton() {
        let output = outline(&fixture(false), "./src/lib/date.ts").expect("outline");
        assert_eq!(output, "10 function formatDateLong\n42 function formatDate");
    }

    #[test]
    fn compact_formatting_uses_one_truncation_line() {
        let lines = vec!["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(format_limited(lines, 2), "a\nb\n… +2 more");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }

    #[test]
    fn json_rpc_dispatches_initialize_list_and_tool_call() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let root = project.root.to_string_lossy();
        let input = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {}
                }
            })
            .to_string(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "scip_outline",
                    "arguments": {
                        "project_root": root,
                        "file": "src/lib/date.ts"
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");

        let mut server = Server::default();
        let responses = input
            .lines()
            .filter_map(|line| server.dispatch_line(line))
            .map(|line| serde_json::from_str::<Value>(&line).expect("valid response json"))
            .collect::<Vec<_>>();

        assert_eq!(responses.len(), 3);
        assert_eq!(
            responses[0].pointer("/result/protocolVersion"),
            Some(&json!("2024-11-05"))
        );
        assert_eq!(
            responses[0].pointer("/result/serverInfo/name"),
            Some(&json!("crux"))
        );
        let tools = responses[1]
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools array");
        assert_eq!(tools.len(), 8);
        assert_eq!(
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            vec![
                "scip_index",
                "scip_map",
                "scip_search",
                "scip_def",
                "scip_refs",
                "scip_callers",
                "scip_dead",
                "scip_outline"
            ]
        );
        assert_eq!(
            responses[2].pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            responses[2].pointer("/result/content/0/text"),
            Some(&json!("10 function formatDateLong\n42 function formatDate"))
        );
    }

    #[test]
    fn json_rpc_ignores_notifications_and_errors_on_unknown_methods() {
        let mut server = Server::default();
        assert!(server
            .dispatch_line(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string()
            )
            .is_none());
        assert!(server
            .dispatch_line(&json!({"jsonrpc": "2.0", "method": "unknown-notification"}).to_string())
            .is_none());

        let response = server
            .dispatch_line(&json!({"jsonrpc": "2.0", "id": 9, "method": "unknown"}).to_string())
            .expect("response");
        let response: Value = serde_json::from_str(&response).expect("valid response");
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32601)));
    }

    #[test]
    fn missing_index_is_a_tool_error_with_recovery_hint() {
        let project = TestProject::new();
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_search",
            &json!({
                "project_root": project.root,
                "query": "format"
            }),
        );
        assert!(result.is_error);
        assert!(result.text.contains("call scip_index first"));
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
        assert_eq!(first.index.documents.len(), 6);

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
        assert_eq!(second.index.documents.len(), 7);
    }
}

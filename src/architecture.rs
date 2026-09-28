use crate::index::{index_path, load_uncached_index, run_indexer, IndexCache};
use crate::render::truncate_chars;
use crate::semantic::is_definition;
use crate::semantic::SemanticIndex;
use anyhow::{bail, Context, Result};
use scip::symbol::{is_local_symbol, parse_symbol};
use scip::types::Index;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_OUTPUT_DIRECTORY: &str = ".crux/architecture";
const GRAPH_SCHEMA_VERSION: u32 = 1;
const MAX_MCP_OUTPUT_BYTES: usize = 12_000;
const MAX_MCP_LINE_CHARS: usize = 360;
const MAX_CYCLE_MODULES: usize = 8;

pub(crate) const USAGE: &str =
    "crux architecture [project-dir] [--index <file>] [--output <directory>]";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ArchitectureDirection {
    Incoming,
    Outgoing,
    #[default]
    Both,
}

impl ArchitectureDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
            Self::Both => "both",
        }
    }

    fn includes_incoming(self) -> bool {
        matches!(self, Self::Incoming | Self::Both)
    }

    fn includes_outgoing(self) -> bool {
        matches!(self, Self::Outgoing | Self::Both)
    }
}

pub(crate) struct ArchitectureQuery<'a> {
    pub(crate) scope: Option<&'a str>,
    pub(crate) direction: ArchitectureDirection,
    pub(crate) limit: usize,
    pub(crate) offset: usize,
}

pub(crate) struct ArchitectureProjection {
    graph: ArchitectureGraph,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ArchitectureArgs {
    project_root: PathBuf,
    index: Option<PathBuf>,
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct PackageIdentity {
    scheme: String,
    manager: String,
    name: String,
    version: String,
}

impl PackageIdentity {
    fn from_symbol(symbol: &str) -> Option<Self> {
        let parsed = parse_symbol(symbol).ok()?;
        if is_local_symbol(symbol) {
            return None;
        }
        let package = parsed.package.as_ref();
        Some(Self {
            scheme: parsed.scheme,
            manager: package.map_or_else(String::new, |value| value.manager.clone()),
            name: package.map_or_else(String::new, |value| value.name.clone()),
            version: package.map_or_else(String::new, |value| value.version.clone()),
        })
    }

    fn id(&self) -> String {
        format!(
            "package:{}:{}:{}:{}",
            encode_id_part(&self.scheme),
            encode_id_part(&self.manager),
            encode_id_part(&self.name),
            encode_id_part(&self.version)
        )
    }

    fn label(&self) -> String {
        let package = match (self.manager.is_empty(), self.name.is_empty()) {
            (_, false) if self.version.is_empty() => {
                if self.manager.is_empty() {
                    self.name.clone()
                } else {
                    format!("{}:{}", self.manager, self.name)
                }
            }
            (_, false) => {
                if self.manager.is_empty() {
                    format!("{}@{}", self.name, self.version)
                } else {
                    format!("{}:{}@{}", self.manager, self.name, self.version)
                }
            }
            _ => "no package".to_string(),
        };
        if self.scheme.is_empty() {
            package
        } else {
            format!("{} · {}", self.scheme, package)
        }
    }
}

fn encode_id_part(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.') {
            output.push(char::from(*byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum SymbolKey {
    Local { module: String, symbol: String },
    Global(String),
}

impl SymbolKey {
    fn new(module: &str, symbol: &str) -> Self {
        if is_local_symbol(symbol) {
            Self::Local {
                module: module.to_string(),
                symbol: symbol.to_string(),
            }
        } else {
            Self::Global(symbol.to_string())
        }
    }

    fn symbol(&self) -> &str {
        match self {
            Self::Local { symbol, .. } | Self::Global(symbol) => symbol,
        }
    }
}

#[derive(Default)]
struct DefinitionData {
    modules: BTreeSet<String>,
    sites: BTreeMap<String, usize>,
    package: Option<PackageIdentity>,
}

#[derive(Default)]
struct ModuleBuilder {
    languages: BTreeSet<String>,
    package_counts: BTreeMap<PackageIdentity, BTreeSet<String>>,
    definition_count: usize,
    reference_count: usize,
    internal_reference_count: usize,
    within_module_reference_count: usize,
    external_reference_count: usize,
    unknown_reference_count: usize,
    ambiguous_reference_count: usize,
    malformed_reference_count: usize,
}

#[derive(Default)]
struct EdgeBuilder {
    references: usize,
    symbols: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum EdgeTarget {
    Module(String),
    Package(PackageIdentity),
}

#[derive(Default)]
struct DiagnosticCount {
    count: usize,
    roles: BTreeSet<String>,
}

#[derive(Serialize)]
struct ArchitectureGraph {
    schema_version: u32,
    generator: Generator,
    summary: Summary,
    coverage: Coverage,
    packages: Vec<PackageRecord>,
    modules: Vec<ModuleRecord>,
    edges: Vec<EdgeRecord>,
    boundaries: Vec<BoundaryRecord>,
    cycles: Vec<CycleRecord>,
    diagnostics: Diagnostics,
}

#[derive(Serialize)]
struct Generator {
    name: &'static str,
    version: &'static str,
    evidence: &'static str,
}

#[derive(Serialize)]
struct Summary {
    module_count: usize,
    internal_package_count: usize,
    external_package_count: usize,
    dependency_count: usize,
    module_dependency_count: usize,
    external_dependency_count: usize,
    dependency_reference_count: usize,
    boundary_count: usize,
    cycle_count: usize,
    isolated_module_count: usize,
}

#[derive(Serialize)]
struct Coverage {
    document_count: usize,
    definition_occurrence_count: usize,
    reference_occurrence_count: usize,
    resolved_internal_reference_count: usize,
    within_module_reference_count: usize,
    external_reference_count: usize,
    unknown_reference_count: usize,
    ambiguous_reference_count: usize,
    malformed_occurrence_count: usize,
    duplicate_definition_symbol_count: usize,
    indexed_symbol_information_count: usize,
    external_symbol_information_count: usize,
    limitations: Vec<&'static str>,
}

#[derive(Serialize)]
struct PackageRecord {
    id: String,
    label: String,
    kind: &'static str,
    scheme: String,
    manager: String,
    name: String,
    version: String,
    module_count: usize,
    incoming_boundary_count: usize,
    outgoing_boundary_count: usize,
    incoming_reference_count: usize,
    outgoing_reference_count: usize,
}

#[derive(Serialize)]
struct ModuleRecord {
    id: String,
    path: String,
    label: String,
    directory: String,
    languages: Vec<String>,
    package_id: String,
    definition_count: usize,
    reference_count: usize,
    internal_reference_count: usize,
    within_module_reference_count: usize,
    external_reference_count: usize,
    unknown_reference_count: usize,
    ambiguous_reference_count: usize,
    malformed_reference_count: usize,
    incoming_module_dependencies: usize,
    outgoing_module_dependencies: usize,
    external_package_dependencies: usize,
    incoming_reference_count: usize,
    outgoing_reference_count: usize,
    in_degree_centrality: f64,
    out_degree_centrality: f64,
    degree_centrality: f64,
    isolated: bool,
}

#[derive(Serialize)]
struct EdgeRecord {
    id: String,
    source: String,
    target: String,
    target_kind: &'static str,
    reference_count: usize,
    symbol_count: usize,
    symbols: Vec<DependencySymbol>,
}

#[derive(Serialize)]
struct DependencySymbol {
    symbol: String,
    reference_count: usize,
}

#[derive(Serialize)]
struct BoundaryRecord {
    source_package: String,
    target_package: String,
    dependency_count: usize,
    reference_count: usize,
}

#[derive(Serialize)]
struct CycleRecord {
    id: String,
    modules: Vec<String>,
    module_count: usize,
}

#[derive(Serialize)]
struct Diagnostics {
    duplicate_definitions: Vec<DuplicateDefinition>,
    malformed_symbols: Vec<SymbolDiagnostic>,
    unknown_references: Vec<ReferenceDiagnostic>,
    ambiguous_references: Vec<ReferenceDiagnostic>,
    mixed_package_modules: Vec<MixedPackageModule>,
}

#[derive(Serialize)]
struct DuplicateDefinition {
    symbol: String,
    modules: Vec<String>,
    sites: Vec<DefinitionSite>,
    definition_count: usize,
}

#[derive(Serialize)]
struct DefinitionSite {
    module: String,
    definition_count: usize,
}

#[derive(Serialize)]
struct SymbolDiagnostic {
    module: String,
    symbol: String,
    occurrence_count: usize,
    roles: Vec<String>,
}

#[derive(Serialize)]
struct ReferenceDiagnostic {
    module: String,
    symbol: String,
    reason: String,
    reference_count: usize,
}

#[derive(Serialize)]
struct MixedPackageModule {
    module: String,
    selected_package: String,
    observed_packages: Vec<String>,
}

pub(crate) fn run(arguments: &[String]) -> Result<()> {
    if matches!(arguments, [argument] if argument == "--help" || argument == "-h") {
        println!("Usage: {USAGE}");
        return Ok(());
    }
    let arguments = parse_arguments(arguments)?;
    let project_root = canonical_directory(&arguments.project_root)?;
    let explicit_index = arguments.index.is_some();
    let index = arguments
        .index
        .map(absolute_from_current)
        .transpose()?
        .unwrap_or_else(|| index_path(&project_root));
    let output = arguments
        .output
        .map(absolute_from_current)
        .transpose()?
        .unwrap_or_else(|| project_root.join(DEFAULT_OUTPUT_DIRECTORY));

    ensure_index(&project_root, &index, explicit_index)?;
    let loaded = load_uncached_index(&index)?;
    let graph = build_graph(loaded.index.index());
    write_outputs(&output, &graph)?;

    println!(
        "wrote graph.html, GRAPH_REPORT.md, and graph.json to {} ({} modules, {} dependencies)",
        output.display(),
        graph.summary.module_count,
        graph.summary.dependency_count
    );
    Ok(())
}

fn parse_arguments(arguments: &[String]) -> Result<ArchitectureArgs> {
    let mut project_root = None;
    let mut index = None;
    let mut output = None;
    let mut position = 0;
    while position < arguments.len() {
        let argument = &arguments[position];
        if argument == "--index" || argument == "--output" {
            let value = arguments
                .get(position + 1)
                .with_context(|| format!("{argument} requires a path\nUsage: {USAGE}"))?;
            if value.starts_with('-') {
                bail!("{argument} requires a path\nUsage: {USAGE}");
            }
            if argument == "--index" {
                if index.replace(PathBuf::from(value)).is_some() {
                    bail!("--index can appear only once\nUsage: {USAGE}");
                }
            } else if output.replace(PathBuf::from(value)).is_some() {
                bail!("--output can appear only once\nUsage: {USAGE}");
            }
            position += 2;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--index=") {
            if value.is_empty() || index.replace(PathBuf::from(value)).is_some() {
                bail!("--index requires one path\nUsage: {USAGE}");
            }
        } else if let Some(value) = argument.strip_prefix("--output=") {
            if value.is_empty() || output.replace(PathBuf::from(value)).is_some() {
                bail!("--output requires one path\nUsage: {USAGE}");
            }
        } else if argument.starts_with('-') {
            bail!("unknown architecture option: {argument}\nUsage: {USAGE}");
        } else if project_root.replace(PathBuf::from(argument)).is_some() {
            bail!("architecture accepts one project directory\nUsage: {USAGE}");
        }
        position += 1;
    }
    Ok(ArchitectureArgs {
        project_root: project_root.unwrap_or_else(|| PathBuf::from(".")),
        index,
        output,
    })
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_from_current(path.to_path_buf())?;
    if !absolute.is_dir() {
        bail!("project directory does not exist: {}", absolute.display());
    }
    absolute
        .canonicalize()
        .with_context(|| format!("resolve project directory {}", absolute.display()))
}

fn absolute_from_current(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()
            .context("read current directory")?
            .join(path))
    }
}

fn ensure_index(project_root: &Path, index: &Path, explicit: bool) -> Result<()> {
    if index.is_file() {
        return Ok(());
    }
    if explicit {
        bail!(
            "index not found: {}. Pass an existing SCIP file, or omit --index to create {}",
            index.display(),
            index_path(project_root).display()
        );
    }
    let expected = index_path(project_root);
    if index != expected {
        bail!("default index path mismatch: {}", index.display());
    }
    let mut cache = IndexCache::default();
    run_indexer(project_root, None, None, None, None, &mut cache).with_context(|| {
        format!(
            "index not found at {} and automatic indexing failed. Run scip_index after you install the required indexer",
            index.display()
        )
    })?;
    if !index.is_file() {
        bail!(
            "the indexer did not create {}. Run scip_index and inspect its diagnostics",
            index.display()
        );
    }
    Ok(())
}

fn build_graph(index: &Index) -> ArchitectureGraph {
    let mut modules: BTreeMap<String, ModuleBuilder> = BTreeMap::new();
    for document in &index.documents {
        let module = modules.entry(document.relative_path.clone()).or_default();
        if !document.language.is_empty() {
            module.languages.insert(document.language.clone());
        }
    }

    let mut definitions: BTreeMap<SymbolKey, DefinitionData> = BTreeMap::new();
    let mut malformed: BTreeMap<(String, String), DiagnosticCount> = BTreeMap::new();
    let mut definition_occurrence_count = 0;
    for document in &index.documents {
        for occurrence in &document.occurrences {
            if !is_definition(occurrence) {
                continue;
            }
            definition_occurrence_count += 1;
            let module = modules.entry(document.relative_path.clone()).or_default();
            module.definition_count += 1;
            if parse_symbol(&occurrence.symbol).is_err() {
                let diagnostic = malformed
                    .entry((document.relative_path.clone(), occurrence.symbol.clone()))
                    .or_default();
                diagnostic.count += 1;
                diagnostic.roles.insert("definition".to_string());
                continue;
            }
            let key = SymbolKey::new(&document.relative_path, &occurrence.symbol);
            let definition = definitions.entry(key.clone()).or_default();
            definition.modules.insert(document.relative_path.clone());
            *definition
                .sites
                .entry(document.relative_path.clone())
                .or_default() += 1;
            if let Some(package) = PackageIdentity::from_symbol(key.symbol()) {
                definition.package = Some(package.clone());
                module
                    .package_counts
                    .entry(package)
                    .or_default()
                    .insert(key.symbol().to_string());
            }
        }
    }

    let internal_packages = definitions
        .values()
        .filter_map(|definition| definition.package.clone())
        .collect::<BTreeSet<_>>();
    let external_symbols = index
        .external_symbols
        .iter()
        .map(|information| information.symbol.as_str())
        .collect::<BTreeSet<_>>();

    let mut edges: BTreeMap<(String, EdgeTarget), EdgeBuilder> = BTreeMap::new();
    let mut unknown: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    let mut ambiguous: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    let mut reference_occurrence_count = 0;
    let mut resolved_internal_reference_count = 0;
    let mut within_module_reference_count = 0;
    let mut external_reference_count = 0;
    let mut unknown_reference_count = 0;
    let mut ambiguous_reference_count = 0;

    for document in &index.documents {
        for occurrence in &document.occurrences {
            if is_definition(occurrence) || occurrence.symbol.is_empty() {
                continue;
            }
            reference_occurrence_count += 1;
            let source = document.relative_path.clone();
            let module = modules.entry(source.clone()).or_default();
            module.reference_count += 1;
            let parsed = match parse_symbol(&occurrence.symbol) {
                Ok(parsed) => parsed,
                Err(_) => {
                    let diagnostic = malformed
                        .entry((source, occurrence.symbol.clone()))
                        .or_default();
                    diagnostic.count += 1;
                    diagnostic.roles.insert("reference".to_string());
                    module.malformed_reference_count += 1;
                    continue;
                }
            };
            let key = SymbolKey::new(&document.relative_path, &occurrence.symbol);
            if let Some(definition) = definitions.get(&key) {
                if definition.modules.len() == 1 {
                    let target = definition.modules.iter().next().expect("one definition");
                    resolved_internal_reference_count += 1;
                    module.internal_reference_count += 1;
                    if target == &document.relative_path {
                        within_module_reference_count += 1;
                        module.within_module_reference_count += 1;
                    } else {
                        add_edge(
                            &mut edges,
                            &document.relative_path,
                            EdgeTarget::Module(target.clone()),
                            &occurrence.symbol,
                        );
                    }
                } else {
                    let reason = format!(
                        "symbol has definitions in {} modules",
                        definition.modules.len()
                    );
                    *ambiguous
                        .entry((source, occurrence.symbol.clone(), reason))
                        .or_default() += 1;
                    module.ambiguous_reference_count += 1;
                    ambiguous_reference_count += 1;
                }
                continue;
            }

            if is_local_symbol(&occurrence.symbol) {
                let reason = "local symbol has no definition in this document".to_string();
                *unknown
                    .entry((source, occurrence.symbol.clone(), reason))
                    .or_default() += 1;
                module.unknown_reference_count += 1;
                unknown_reference_count += 1;
                continue;
            }

            let package = package_from_parsed(&parsed);
            let listed_external = external_symbols.contains(occurrence.symbol.as_str());
            if !internal_packages.contains(&package) {
                add_edge(
                    &mut edges,
                    &document.relative_path,
                    EdgeTarget::Package(package),
                    &occurrence.symbol,
                );
                module.external_reference_count += 1;
                external_reference_count += 1;
            } else {
                let reason = if listed_external {
                    "external symbol shares an internal package identity"
                } else {
                    "symbol package is indexed but its definition is missing"
                }
                .to_string();
                *unknown
                    .entry((source, occurrence.symbol.clone(), reason))
                    .or_default() += 1;
                module.unknown_reference_count += 1;
                unknown_reference_count += 1;
            }
        }
    }

    let selected_packages = select_module_packages(&modules);
    let mixed_package_modules = mixed_package_modules(&modules, &selected_packages);
    let duplicate_definitions = duplicate_definitions(&definitions);
    let malformed_symbols = malformed_symbols(malformed);
    let unknown_references = reference_diagnostics(unknown);
    let ambiguous_references = reference_diagnostics(ambiguous);

    let (edge_records, module_metrics, external_packages) =
        create_edges(&modules, edges, &selected_packages);
    let boundaries = create_boundaries(&edge_records, &selected_packages);
    let cycles = find_cycles(modules.keys(), &edge_records);
    let packages = create_packages(
        &modules,
        &selected_packages,
        &internal_packages,
        &external_packages,
        &boundaries,
    );
    let module_records = create_modules(modules, &selected_packages, &module_metrics);
    let isolated_module_count = module_records
        .iter()
        .filter(|module| module.isolated)
        .count();
    let module_dependency_count = edge_records
        .iter()
        .filter(|edge| edge.target_kind == "module")
        .count();
    let external_dependency_count = edge_records.len() - module_dependency_count;
    let dependency_reference_count = edge_records.iter().map(|edge| edge.reference_count).sum();
    let indexed_symbol_information_count = index
        .documents
        .iter()
        .map(|document| document.symbols.len())
        .sum();

    ArchitectureGraph {
        schema_version: GRAPH_SCHEMA_VERSION,
        generator: Generator {
            name: "crux",
            version: env!("CARGO_PKG_VERSION"),
            evidence: "SCIP definition and reference occurrences",
        },
        summary: Summary {
            module_count: module_records.len(),
            internal_package_count: internal_packages.len(),
            external_package_count: external_packages.len(),
            dependency_count: edge_records.len(),
            module_dependency_count,
            external_dependency_count,
            dependency_reference_count,
            boundary_count: boundaries.len(),
            cycle_count: cycles.len(),
            isolated_module_count,
        },
        coverage: Coverage {
            document_count: index.documents.len(),
            definition_occurrence_count,
            reference_occurrence_count,
            resolved_internal_reference_count,
            within_module_reference_count,
            external_reference_count,
            unknown_reference_count,
            ambiguous_reference_count,
            malformed_occurrence_count: malformed_symbols
                .iter()
                .map(|diagnostic| diagnostic.occurrence_count)
                .sum(),
            duplicate_definition_symbol_count: duplicate_definitions.len(),
            indexed_symbol_information_count,
            external_symbol_information_count: index.external_symbols.len(),
            limitations: vec![
                "The graph contains only documents and occurrences present in the SCIP index.",
                "The graph omits runtime, reflective, and generated dependencies when the indexer omits them.",
                "The graph excludes malformed, unknown, and ambiguous references from dependency edges.",
                "The graph aggregates external symbols by SCIP package identity because their modules are not indexed.",
                "An undefined global symbol outside the defined package set counts as an external package reference.",
                "Degree centrality uses unique internal module neighbors and excludes external package nodes.",
            ],
        },
        packages,
        modules: module_records,
        edges: edge_records,
        boundaries,
        cycles,
        diagnostics: Diagnostics {
            duplicate_definitions,
            malformed_symbols,
            unknown_references,
            ambiguous_references,
            mixed_package_modules,
        },
    }
}

impl ArchitectureProjection {
    pub(crate) fn new(index: &SemanticIndex) -> Self {
        Self {
            graph: build_graph(index.index()),
        }
    }

    pub(crate) fn query(&self, query: ArchitectureQuery<'_>) -> Result<String> {
        let limit = query.limit.clamp(1, 200);
        let scope = normalize_scope(query.scope)?;
        match scope {
            Some(scope) => Ok(self.render_scope(&scope, query.direction, limit, query.offset)),
            None => Ok(self.render_overview(limit, query.offset)),
        }
    }

    fn render_overview(&self, limit: usize, offset: usize) -> String {
        let graph = &self.graph;
        let mut output = CompactOutput::new();
        output.line("architecture overview");
        output.line("evidence: SCIP definition and reference occurrences");
        output.line("direction: all dependencies retain source -> target direction");
        output.line(&format!(
            "summary: modules {} | internal packages {} | external packages {} | direct module dependencies {} | external dependencies {}",
            graph.summary.module_count,
            graph.summary.internal_package_count,
            graph.summary.external_package_count,
            graph.summary.module_dependency_count,
            graph.summary.external_dependency_count
        ));
        output.line(&format!(
            "structure: package boundaries {} | cycles {} | isolated modules {} | dependency references {}",
            graph.summary.boundary_count,
            graph.summary.cycle_count,
            graph.summary.isolated_module_count,
            graph.summary.dependency_reference_count
        ));
        output.line(&format!(
            "coverage: documents {} | definitions {} | references {} | resolved internal {} | external {}",
            graph.coverage.document_count,
            graph.coverage.definition_occurrence_count,
            graph.coverage.reference_occurrence_count,
            graph.coverage.resolved_internal_reference_count,
            graph.coverage.external_reference_count
        ));
        output.line(&format!(
            "diagnostics: unknown references {} | ambiguous references {} | malformed occurrences {} | duplicate definition symbols {}",
            graph.coverage.unknown_reference_count,
            graph.coverage.ambiguous_reference_count,
            graph.coverage.malformed_occurrence_count,
            graph.coverage.duplicate_definition_symbol_count
        ));

        let package_labels = package_labels(graph);
        let mut modules = graph.modules.iter().collect::<Vec<_>>();
        modules.sort_by(|left, right| {
            right
                .degree_centrality
                .total_cmp(&left.degree_centrality)
                .then_with(|| {
                    right
                        .incoming_reference_count
                        .cmp(&left.incoming_reference_count)
                })
                .then_with(|| left.path.cmp(&right.path))
        });
        output.page("central modules", &modules, offset, limit, |module| {
            format!(
                "- {} | incoming {} | outgoing {} | external {} | centrality {:.3}",
                module.path,
                module.incoming_module_dependencies,
                module.outgoing_module_dependencies,
                module.external_package_dependencies,
                module.degree_centrality
            )
        });
        output.page(
            "observed package boundaries",
            &graph.boundaries,
            offset,
            limit,
            |boundary| format_boundary(boundary, &package_labels),
        );
        let module_paths = module_paths(graph);
        output.page("dependency cycles", &graph.cycles, offset, limit, |cycle| {
            format_cycle(cycle, &module_paths)
        });
        output.line("degree centrality: unique internal module neighbors / (module count - 1)");
        output.line("coverage limit: results include only dependencies emitted in the SCIP index");
        output.line(
            "next: pass scope with a project-relative file or directory for direct dependencies",
        );
        output.finish()
    }

    fn render_scope(
        &self,
        scope: &str,
        direction: ArchitectureDirection,
        limit: usize,
        offset: usize,
    ) -> String {
        let graph = &self.graph;
        let matches = matching_modules(graph, scope);
        let mut output = CompactOutput::new();
        output.line(&format!("architecture scope: {scope}"));
        output.line("evidence: SCIP definition and reference occurrences");
        output.line(&format!("direction: {}", direction.as_str()));
        if matches.is_empty() {
            output.line("matched modules: 0");
            output
                .line("no indexed module matches this exact path or directory component boundary");
            output.line("next: use the overview to find an indexed path, then use rg if the index has no match");
            output.line("stop: do not retry an equivalent empty scope");
            return output.finish();
        }

        let selected_ids = matches
            .iter()
            .map(|module| module.id.as_str())
            .collect::<BTreeSet<_>>();
        let selected_paths = matches
            .iter()
            .map(|module| module.path.as_str())
            .collect::<Vec<_>>();
        output.line(&format!("matched modules: {}", matches.len()));
        output.page("scope modules", &selected_paths, offset, limit, |path| {
            format!("- {path}")
        });

        let module_paths = module_paths(graph);
        let package_labels = package_labels(graph);
        let mut outgoing = graph
            .edges
            .iter()
            .filter(|edge| selected_ids.contains(edge.source.as_str()))
            .collect::<Vec<_>>();
        let mut incoming = graph
            .edges
            .iter()
            .filter(|edge| {
                edge.target_kind == "module" && selected_ids.contains(edge.target.as_str())
            })
            .collect::<Vec<_>>();
        sort_edges(&mut outgoing);
        sort_edges(&mut incoming);
        if direction.includes_outgoing() {
            output.page(
                "direct outgoing dependencies",
                &outgoing,
                offset,
                limit,
                |edge| format_edge(edge, &module_paths, &package_labels),
            );
        }
        if direction.includes_incoming() {
            output.page(
                "direct incoming dependencies",
                &incoming,
                offset,
                limit,
                |edge| format_edge(edge, &module_paths, &package_labels),
            );
            output.line("possible impact: incoming modules reference this scope directly; transitive impact is not computed");
        }

        let boundaries = scoped_boundaries(graph, &outgoing, &incoming, direction);
        output.page(
            "relevant package boundaries",
            &boundaries,
            offset,
            limit,
            |boundary| format_boundary(boundary, &package_labels),
        );

        let cycles = graph
            .cycles
            .iter()
            .filter(|cycle| {
                cycle
                    .modules
                    .iter()
                    .any(|module| selected_ids.contains(module.as_str()))
            })
            .collect::<Vec<_>>();
        output.page(
            "relevant dependency cycles",
            &cycles,
            offset,
            limit,
            |cycle| format_cycle(cycle, &module_paths),
        );

        let definitions = matches
            .iter()
            .map(|module| module.definition_count)
            .sum::<usize>();
        let references = matches
            .iter()
            .map(|module| module.reference_count)
            .sum::<usize>();
        let resolved = matches
            .iter()
            .map(|module| module.internal_reference_count)
            .sum::<usize>();
        let external = matches
            .iter()
            .map(|module| module.external_reference_count)
            .sum::<usize>();
        let unknown = matches
            .iter()
            .map(|module| module.unknown_reference_count)
            .sum::<usize>();
        let ambiguous = matches
            .iter()
            .map(|module| module.ambiguous_reference_count)
            .sum::<usize>();
        let malformed = matches
            .iter()
            .map(|module| module.malformed_reference_count)
            .sum::<usize>();
        output.line(&format!(
            "scope coverage: definitions {definitions} | references {references} | resolved internal {resolved} | external {external}"
        ));
        output.line(&format!(
            "scope diagnostics: unknown {unknown} | ambiguous {ambiguous} | malformed {malformed}"
        ));
        let diagnostics = scope_diagnostics(graph, &selected_paths);
        output.page(
            "diagnostic details",
            &diagnostics,
            offset,
            limit,
            |detail| format!("- {detail}"),
        );
        output.line("coverage limit: absence from this result means absence from the index, not proven runtime absence");
        output.finish()
    }
}

struct CompactOutput {
    text: String,
    byte_truncated: bool,
}

impl CompactOutput {
    fn new() -> Self {
        Self {
            text: String::new(),
            byte_truncated: false,
        }
    }

    fn line(&mut self, value: &str) -> bool {
        if self.byte_truncated {
            return false;
        }
        let value = value.replace(['\r', '\n'], " ");
        let value = if value.chars().count() > MAX_MCP_LINE_CHARS {
            format!(
                "{} [line shortened]",
                truncate_chars(&value, MAX_MCP_LINE_CHARS.saturating_sub(18))
            )
        } else {
            value
        };
        if self.text.len() + value.len() + 1 > MAX_MCP_OUTPUT_BYTES.saturating_sub(96) {
            self.byte_truncated = true;
            return false;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&value);
        true
    }

    fn page<T>(
        &mut self,
        title: &str,
        records: &[T],
        offset: usize,
        limit: usize,
        mut render: impl FnMut(&T) -> String,
    ) {
        if !self.line(&format!("{title}:")) {
            return;
        }
        if records.is_empty() {
            self.line("- none observed in the index");
            return;
        }
        if offset >= records.len() {
            self.line(&format!(
                "- no entries at offset {offset}; total {} (use offset=0)",
                records.len()
            ));
            return;
        }
        let end = offset.saturating_add(limit).min(records.len());
        for record in &records[offset..end] {
            if !self.line(&render(record)) {
                return;
            }
        }
        if end < records.len() {
            self.line(&format!(
                "… {} more; pass offset={end} with limit={limit}",
                records.len() - end
            ));
        }
    }

    fn finish(mut self) -> String {
        if self.byte_truncated {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str(
                "… output byte limit reached; use a narrower scope, direction, limit, or offset",
            );
        }
        self.text
    }
}

fn normalize_scope(scope: Option<&str>) -> Result<Option<String>> {
    let Some(scope) = scope else {
        return Ok(None);
    };
    let replaced = scope.trim().replace('\\', "/");
    if replaced.is_empty() || replaced == "." {
        return Ok(None);
    }
    if replaced.starts_with('/')
        || replaced.starts_with("//")
        || replaced
            .split('/')
            .next()
            .is_some_and(|component| component.ends_with(':'))
    {
        bail!("scip_architecture: scope must be a project-relative file or directory");
    }
    let mut components = Vec::new();
    for component in replaced.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                bail!("scip_architecture: scope cannot contain '..'");
            }
            value => components.push(value),
        }
    }
    if components.is_empty() {
        Ok(None)
    } else {
        Ok(Some(components.join("/")))
    }
}

fn normalize_index_path(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn matching_modules<'a>(graph: &'a ArchitectureGraph, scope: &str) -> Vec<&'a ModuleRecord> {
    graph
        .modules
        .iter()
        .filter(|module| {
            let path = normalize_index_path(&module.path);
            path == scope
                || path
                    .strip_prefix(scope)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .collect()
}

fn module_paths(graph: &ArchitectureGraph) -> BTreeMap<&str, &str> {
    graph
        .modules
        .iter()
        .map(|module| (module.id.as_str(), module.path.as_str()))
        .collect()
}

fn package_labels(graph: &ArchitectureGraph) -> BTreeMap<&str, &str> {
    graph
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package.label.as_str()))
        .collect()
}

fn sort_edges(edges: &mut Vec<&EdgeRecord>) {
    edges.sort_by(|left, right| {
        right
            .reference_count
            .cmp(&left.reference_count)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn format_edge(
    edge: &EdgeRecord,
    modules: &BTreeMap<&str, &str>,
    packages: &BTreeMap<&str, &str>,
) -> String {
    let source = modules
        .get(edge.source.as_str())
        .copied()
        .unwrap_or(edge.source.as_str());
    let target = if edge.target_kind == "module" {
        modules
            .get(edge.target.as_str())
            .copied()
            .unwrap_or(edge.target.as_str())
    } else {
        packages
            .get(edge.target.as_str())
            .copied()
            .unwrap_or(edge.target.as_str())
    };
    format!(
        "- {source} -> {target} | references {} | symbols {}",
        edge.reference_count, edge.symbol_count
    )
}

fn format_boundary(boundary: &BoundaryRecord, packages: &BTreeMap<&str, &str>) -> String {
    let source = packages
        .get(boundary.source_package.as_str())
        .copied()
        .unwrap_or(boundary.source_package.as_str());
    let target = packages
        .get(boundary.target_package.as_str())
        .copied()
        .unwrap_or(boundary.target_package.as_str());
    format!(
        "- {source} -> {target} | dependencies {} | references {}",
        boundary.dependency_count, boundary.reference_count
    )
}

fn scoped_boundaries(
    graph: &ArchitectureGraph,
    outgoing: &[&EdgeRecord],
    incoming: &[&EdgeRecord],
    direction: ArchitectureDirection,
) -> Vec<BoundaryRecord> {
    let module_packages = graph
        .modules
        .iter()
        .map(|module| (module.id.as_str(), module.package_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut selected_edges = BTreeMap::new();
    if direction.includes_outgoing() {
        selected_edges.extend(outgoing.iter().map(|edge| (edge.id.as_str(), *edge)));
    }
    if direction.includes_incoming() {
        selected_edges.extend(incoming.iter().map(|edge| (edge.id.as_str(), *edge)));
    }

    let mut boundaries: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
    for edge in selected_edges.values() {
        let source_package = *module_packages
            .get(edge.source.as_str())
            .expect("edge source package");
        let target_package = if edge.target_kind == "module" {
            *module_packages
                .get(edge.target.as_str())
                .expect("edge target package")
        } else {
            edge.target.as_str()
        };
        if source_package == target_package {
            continue;
        }
        let boundary = boundaries
            .entry((source_package.to_string(), target_package.to_string()))
            .or_default();
        boundary.0 += 1;
        boundary.1 += edge.reference_count;
    }

    boundaries
        .into_iter()
        .map(
            |((source_package, target_package), (dependency_count, reference_count))| {
                BoundaryRecord {
                    source_package,
                    target_package,
                    dependency_count,
                    reference_count,
                }
            },
        )
        .collect()
}

fn format_cycle(cycle: &CycleRecord, modules: &BTreeMap<&str, &str>) -> String {
    let mut paths = cycle
        .modules
        .iter()
        .take(MAX_CYCLE_MODULES)
        .map(|module| {
            modules
                .get(module.as_str())
                .copied()
                .unwrap_or(module.as_str())
        })
        .collect::<Vec<_>>();
    if cycle.modules.len() > MAX_CYCLE_MODULES {
        paths.push("[more modules omitted]");
    }
    let suffix = if cycle.modules.len() > MAX_CYCLE_MODULES {
        format!(" (+{} modules)", cycle.modules.len() - MAX_CYCLE_MODULES)
    } else {
        String::new()
    };
    format!("- {}: {}{suffix}", cycle.id, paths.join(" -> "))
}

fn scope_diagnostics(graph: &ArchitectureGraph, selected_paths: &[&str]) -> Vec<String> {
    let selected = selected_paths.iter().copied().collect::<BTreeSet<_>>();
    let mut details = Vec::new();
    for diagnostic in &graph.diagnostics.unknown_references {
        if selected.contains(diagnostic.module.as_str()) {
            details.push(format!(
                "unknown {} in {} | {} | references {}",
                diagnostic.symbol, diagnostic.module, diagnostic.reason, diagnostic.reference_count
            ));
        }
    }
    for diagnostic in &graph.diagnostics.ambiguous_references {
        if selected.contains(diagnostic.module.as_str()) {
            details.push(format!(
                "ambiguous {} in {} | {} | references {}",
                diagnostic.symbol, diagnostic.module, diagnostic.reason, diagnostic.reference_count
            ));
        }
    }
    for diagnostic in &graph.diagnostics.malformed_symbols {
        if selected.contains(diagnostic.module.as_str()) {
            details.push(format!(
                "malformed {} in {} | occurrences {} | roles {}",
                diagnostic.symbol,
                diagnostic.module,
                diagnostic.occurrence_count,
                diagnostic.roles.join(",")
            ));
        }
    }
    for diagnostic in &graph.diagnostics.duplicate_definitions {
        if diagnostic
            .modules
            .iter()
            .any(|module| selected.contains(module.as_str()))
        {
            details.push(format!(
                "duplicate definition {} | modules {} | definitions {}",
                diagnostic.symbol,
                diagnostic.modules.join(","),
                diagnostic.definition_count
            ));
        }
    }
    for diagnostic in &graph.diagnostics.mixed_package_modules {
        if selected.contains(diagnostic.module.as_str()) {
            details.push(format!(
                "mixed packages in {} | selected {} | observed {}",
                diagnostic.module,
                diagnostic.selected_package,
                diagnostic.observed_packages.join(",")
            ));
        }
    }
    details.sort();
    details
}

fn package_from_parsed(symbol: &scip::types::Symbol) -> PackageIdentity {
    let package = symbol.package.as_ref();
    PackageIdentity {
        scheme: symbol.scheme.clone(),
        manager: package.map_or_else(String::new, |value| value.manager.clone()),
        name: package.map_or_else(String::new, |value| value.name.clone()),
        version: package.map_or_else(String::new, |value| value.version.clone()),
    }
}

fn add_edge(
    edges: &mut BTreeMap<(String, EdgeTarget), EdgeBuilder>,
    source: &str,
    target: EdgeTarget,
    symbol: &str,
) {
    let edge = edges.entry((source.to_string(), target)).or_default();
    edge.references += 1;
    *edge.symbols.entry(symbol.to_string()).or_default() += 1;
}

fn select_module_packages(
    modules: &BTreeMap<String, ModuleBuilder>,
) -> BTreeMap<String, Option<PackageIdentity>> {
    modules
        .iter()
        .map(|(path, module)| {
            let package = module
                .package_counts
                .iter()
                .max_by(
                    |(left_package, left_symbols), (right_package, right_symbols)| {
                        left_symbols
                            .len()
                            .cmp(&right_symbols.len())
                            .then_with(|| right_package.cmp(left_package))
                    },
                )
                .map(|(package, _)| package.clone());
            (path.clone(), package)
        })
        .collect()
}

fn mixed_package_modules(
    modules: &BTreeMap<String, ModuleBuilder>,
    selected: &BTreeMap<String, Option<PackageIdentity>>,
) -> Vec<MixedPackageModule> {
    modules
        .iter()
        .filter(|(_, module)| module.package_counts.len() > 1)
        .map(|(path, module)| MixedPackageModule {
            module: path.clone(),
            selected_package: package_id(selected.get(path).and_then(Option::as_ref)),
            observed_packages: module
                .package_counts
                .keys()
                .map(PackageIdentity::id)
                .collect(),
        })
        .collect()
}

fn duplicate_definitions(
    definitions: &BTreeMap<SymbolKey, DefinitionData>,
) -> Vec<DuplicateDefinition> {
    definitions
        .iter()
        .filter(|(_, definition)| definition.sites.values().sum::<usize>() > 1)
        .map(|(key, definition)| DuplicateDefinition {
            symbol: key.symbol().to_string(),
            modules: definition.modules.iter().cloned().collect(),
            sites: definition
                .sites
                .iter()
                .map(|(module, count)| DefinitionSite {
                    module: module.clone(),
                    definition_count: *count,
                })
                .collect(),
            definition_count: definition.sites.values().sum(),
        })
        .collect()
}

fn malformed_symbols(
    malformed: BTreeMap<(String, String), DiagnosticCount>,
) -> Vec<SymbolDiagnostic> {
    malformed
        .into_iter()
        .map(|((module, symbol), diagnostic)| SymbolDiagnostic {
            module,
            symbol,
            occurrence_count: diagnostic.count,
            roles: diagnostic.roles.into_iter().collect(),
        })
        .collect()
}

fn reference_diagnostics(
    diagnostics: BTreeMap<(String, String, String), usize>,
) -> Vec<ReferenceDiagnostic> {
    diagnostics
        .into_iter()
        .map(
            |((module, symbol, reason), reference_count)| ReferenceDiagnostic {
                module,
                symbol,
                reason,
                reference_count,
            },
        )
        .collect()
}

#[derive(Default)]
struct ModuleMetrics {
    incoming: BTreeSet<String>,
    outgoing: BTreeSet<String>,
    external: BTreeSet<String>,
    incoming_references: usize,
    outgoing_references: usize,
}

fn create_edges(
    modules: &BTreeMap<String, ModuleBuilder>,
    edges: BTreeMap<(String, EdgeTarget), EdgeBuilder>,
    _selected_packages: &BTreeMap<String, Option<PackageIdentity>>,
) -> (
    Vec<EdgeRecord>,
    BTreeMap<String, ModuleMetrics>,
    BTreeSet<PackageIdentity>,
) {
    let mut metrics = modules
        .keys()
        .map(|path| (path.clone(), ModuleMetrics::default()))
        .collect::<BTreeMap<_, _>>();
    let mut external_packages = BTreeSet::new();
    let mut records = Vec::new();
    for ((source, target), edge) in edges {
        let (target_id, target_kind) = match &target {
            EdgeTarget::Module(target) => {
                let source_metrics = metrics.get_mut(&source).expect("source module exists");
                source_metrics.outgoing.insert(target.clone());
                source_metrics.outgoing_references += edge.references;
                let target_metrics = metrics.get_mut(target).expect("target module exists");
                target_metrics.incoming.insert(source.clone());
                target_metrics.incoming_references += edge.references;
                (module_id(target), "module")
            }
            EdgeTarget::Package(package) => {
                external_packages.insert(package.clone());
                let source_metrics = metrics.get_mut(&source).expect("source module exists");
                source_metrics.external.insert(package.id());
                source_metrics.outgoing_references += edge.references;
                (package.id(), "package")
            }
        };
        let source_id = module_id(&source);
        let id = format!("edge:{source_id}->{target_id}");
        let symbols = edge
            .symbols
            .into_iter()
            .map(|(symbol, reference_count)| DependencySymbol {
                symbol,
                reference_count,
            })
            .collect::<Vec<_>>();
        records.push(EdgeRecord {
            id,
            source: source_id,
            target: target_id,
            target_kind,
            reference_count: edge.references,
            symbol_count: symbols.len(),
            symbols,
        });
    }
    (records, metrics, external_packages)
}

fn create_boundaries(
    edges: &[EdgeRecord],
    selected: &BTreeMap<String, Option<PackageIdentity>>,
) -> Vec<BoundaryRecord> {
    let module_packages = selected
        .iter()
        .map(|(module, package)| (module_id(module), package_id(package.as_ref())))
        .collect::<BTreeMap<_, _>>();
    let mut boundaries: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
    for edge in edges {
        let source_package = module_packages
            .get(&edge.source)
            .expect("edge source package")
            .clone();
        let target_package = if edge.target_kind == "module" {
            module_packages
                .get(&edge.target)
                .expect("edge target package")
                .clone()
        } else {
            edge.target.clone()
        };
        if source_package == target_package {
            continue;
        }
        let boundary = boundaries
            .entry((source_package, target_package))
            .or_default();
        boundary.0 += 1;
        boundary.1 += edge.reference_count;
    }
    boundaries
        .into_iter()
        .map(
            |((source_package, target_package), (dependency_count, reference_count))| {
                BoundaryRecord {
                    source_package,
                    target_package,
                    dependency_count,
                    reference_count,
                }
            },
        )
        .collect()
}

fn create_packages(
    modules: &BTreeMap<String, ModuleBuilder>,
    selected: &BTreeMap<String, Option<PackageIdentity>>,
    internal: &BTreeSet<PackageIdentity>,
    external: &BTreeSet<PackageIdentity>,
    boundaries: &[BoundaryRecord],
) -> Vec<PackageRecord> {
    let mut identities = internal
        .iter()
        .chain(external.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    for package in selected.values().flatten() {
        identities.insert(package.clone());
    }
    let has_unassigned = selected.values().any(Option::is_none);
    let mut records = identities
        .into_iter()
        .map(|identity| {
            let id = identity.id();
            let kind = if internal.contains(&identity) {
                "internal"
            } else {
                "external"
            };
            package_record(
                id,
                identity.label(),
                kind,
                identity,
                modules,
                selected,
                boundaries,
            )
        })
        .collect::<Vec<_>>();
    if has_unassigned {
        let id = unassigned_package_id().to_string();
        records.push(package_record(
            id,
            "Unassigned modules".to_string(),
            "unassigned",
            PackageIdentity {
                scheme: String::new(),
                manager: String::new(),
                name: String::new(),
                version: String::new(),
            },
            modules,
            selected,
            boundaries,
        ));
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    records
}

#[allow(clippy::too_many_arguments)]
fn package_record(
    id: String,
    label: String,
    kind: &'static str,
    identity: PackageIdentity,
    modules: &BTreeMap<String, ModuleBuilder>,
    selected: &BTreeMap<String, Option<PackageIdentity>>,
    boundaries: &[BoundaryRecord],
) -> PackageRecord {
    let module_count = modules
        .keys()
        .filter(|module| package_id(selected.get(*module).and_then(Option::as_ref)) == id)
        .count();
    PackageRecord {
        id: id.clone(),
        label,
        kind,
        scheme: identity.scheme,
        manager: identity.manager,
        name: identity.name,
        version: identity.version,
        module_count,
        incoming_boundary_count: boundaries
            .iter()
            .filter(|boundary| boundary.target_package == id)
            .count(),
        outgoing_boundary_count: boundaries
            .iter()
            .filter(|boundary| boundary.source_package == id)
            .count(),
        incoming_reference_count: boundaries
            .iter()
            .filter(|boundary| boundary.target_package == id)
            .map(|boundary| boundary.reference_count)
            .sum(),
        outgoing_reference_count: boundaries
            .iter()
            .filter(|boundary| boundary.source_package == id)
            .map(|boundary| boundary.reference_count)
            .sum(),
    }
}

fn create_modules(
    modules: BTreeMap<String, ModuleBuilder>,
    selected: &BTreeMap<String, Option<PackageIdentity>>,
    metrics: &BTreeMap<String, ModuleMetrics>,
) -> Vec<ModuleRecord> {
    let denominator = modules.len().saturating_sub(1);
    modules
        .into_iter()
        .map(|(path, module)| {
            let metric = metrics.get(&path).expect("module metrics");
            let neighbors = metric.incoming.union(&metric.outgoing).count();
            let isolated = metric.incoming.is_empty()
                && metric.outgoing.is_empty()
                && metric.external.is_empty();
            ModuleRecord {
                id: module_id(&path),
                label: module_label(&path),
                directory: module_directory(&path),
                path: path.clone(),
                languages: module.languages.into_iter().collect(),
                package_id: package_id(selected.get(&path).and_then(Option::as_ref)),
                definition_count: module.definition_count,
                reference_count: module.reference_count,
                internal_reference_count: module.internal_reference_count,
                within_module_reference_count: module.within_module_reference_count,
                external_reference_count: module.external_reference_count,
                unknown_reference_count: module.unknown_reference_count,
                ambiguous_reference_count: module.ambiguous_reference_count,
                malformed_reference_count: module.malformed_reference_count,
                incoming_module_dependencies: metric.incoming.len(),
                outgoing_module_dependencies: metric.outgoing.len(),
                external_package_dependencies: metric.external.len(),
                incoming_reference_count: metric.incoming_references,
                outgoing_reference_count: metric.outgoing_references,
                in_degree_centrality: centrality(metric.incoming.len(), denominator),
                out_degree_centrality: centrality(metric.outgoing.len(), denominator),
                degree_centrality: centrality(neighbors, denominator),
                isolated,
            }
        })
        .collect()
}

fn centrality(degree: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    let value = degree as f64 / denominator as f64;
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn package_id(package: Option<&PackageIdentity>) -> String {
    package.map_or_else(|| unassigned_package_id().to_string(), PackageIdentity::id)
}

fn unassigned_package_id() -> &'static str {
    "package:unassigned"
}

fn module_id(path: &str) -> String {
    format!("module:{}", encode_id_part(path))
}

fn module_label(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn module_directory(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|parent| parent.to_string_lossy().replace('\\', "/"))
        .filter(|parent| !parent.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

fn find_cycles<'a>(
    modules: impl Iterator<Item = &'a String>,
    edges: &[EdgeRecord],
) -> Vec<CycleRecord> {
    let module_ids = modules.map(|path| module_id(path)).collect::<BTreeSet<_>>();
    let mut adjacency = module_ids
        .iter()
        .map(|module| (module.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    let mut reverse = adjacency.clone();
    for edge in edges.iter().filter(|edge| edge.target_kind == "module") {
        adjacency
            .get_mut(&edge.source)
            .expect("cycle source")
            .insert(edge.target.clone());
        reverse
            .get_mut(&edge.target)
            .expect("cycle target")
            .insert(edge.source.clone());
    }

    let mut visited = BTreeSet::new();
    let mut order = Vec::new();
    for node in &module_ids {
        finish_order(node, &adjacency, &mut visited, &mut order);
    }
    visited.clear();
    let mut components = Vec::new();
    for node in order.into_iter().rev() {
        if visited.contains(&node) {
            continue;
        }
        let mut component = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            component.push(current.clone());
            if let Some(neighbors) = reverse.get(&current) {
                stack.extend(neighbors.iter().rev().cloned());
            }
        }
        component.sort();
        if component.len() > 1 {
            components.push(component);
        }
    }
    components.sort();
    components
        .into_iter()
        .enumerate()
        .map(|(index, modules)| CycleRecord {
            id: format!("cycle:{}", index + 1),
            module_count: modules.len(),
            modules,
        })
        .collect()
}

fn finish_order(
    start: &str,
    adjacency: &BTreeMap<String, BTreeSet<String>>,
    visited: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) {
    if visited.contains(start) {
        return;
    }
    let mut stack = vec![(start.to_string(), false)];
    while let Some((node, expanded)) = stack.pop() {
        if expanded {
            order.push(node);
            continue;
        }
        if !visited.insert(node.clone()) {
            continue;
        }
        stack.push((node.clone(), true));
        if let Some(neighbors) = adjacency.get(&node) {
            stack.extend(
                neighbors
                    .iter()
                    .rev()
                    .map(|neighbor| (neighbor.clone(), false)),
            );
        }
    }
}

fn write_outputs(output: &Path, graph: &ArchitectureGraph) -> Result<()> {
    if output.exists() && !output.is_dir() {
        bail!(
            "architecture output is not a directory: {}",
            output.display()
        );
    }
    fs::create_dir_all(output)
        .with_context(|| format!("create architecture output {}", output.display()))?;
    let json = serde_json::to_string_pretty(graph).context("serialize architecture graph")?;
    let report = render_report(graph);
    let html = render_html(graph)?;
    fs::write(output.join("graph.json"), format!("{json}\n"))
        .with_context(|| format!("write {}/graph.json", output.display()))?;
    fs::write(output.join("GRAPH_REPORT.md"), report)
        .with_context(|| format!("write {}/GRAPH_REPORT.md", output.display()))?;
    fs::write(output.join("graph.html"), html)
        .with_context(|| format!("write {}/graph.html", output.display()))?;
    Ok(())
}

fn render_report(graph: &ArchitectureGraph) -> String {
    let mut output = String::new();
    output.push_str("# Architecture report\n\n");
    output.push_str("Crux derived this report from SCIP definitions and references.\n");
    output.push_str("Crux did not infer dependencies from source text.\n\n");
    output.push_str("## Summary\n\n");
    output.push_str(&format!(
        "- Indexed modules: {}\n",
        graph.summary.module_count
    ));
    output.push_str(&format!(
        "- Internal packages: {}\n",
        graph.summary.internal_package_count
    ));
    output.push_str(&format!(
        "- External packages: {}\n",
        graph.summary.external_package_count
    ));
    output.push_str(&format!(
        "- Module dependencies: {}\n",
        graph.summary.module_dependency_count
    ));
    output.push_str(&format!(
        "- External package dependencies: {}\n",
        graph.summary.external_dependency_count
    ));
    output.push_str(&format!(
        "- Dependency references: {}\n",
        graph.summary.dependency_reference_count
    ));
    output.push_str(&format!(
        "- Observed boundaries: {}\n",
        graph.summary.boundary_count
    ));
    output.push_str(&format!(
        "- Dependency cycles: {}\n",
        graph.summary.cycle_count
    ));
    output.push_str(&format!(
        "- Isolated modules: {}\n\n",
        graph.summary.isolated_module_count
    ));

    output.push_str("## Package groups\n\n");
    if graph.packages.is_empty() {
        output.push_str("The index contains no package groups.\n\n");
    } else {
        output.push_str("| Package | Kind | Modules | Incoming | Outgoing |\n");
        output.push_str("| --- | --- | ---: | ---: | ---: |\n");
        for package in &graph.packages {
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                markdown_cell(&package.label),
                package.kind,
                package.module_count,
                package.incoming_boundary_count,
                package.outgoing_boundary_count
            ));
        }
        output.push('\n');
    }

    output.push_str("## Central modules\n\n");
    output.push_str(
        "Degree centrality is the unique internal neighbor count divided by `module_count - 1`.\n",
    );
    output.push_str("Incoming and outgoing centrality use the same denominator.\n\n");
    if graph.modules.is_empty() {
        output.push_str("The index contains no modules.\n\n");
    } else {
        let mut modules = graph.modules.iter().collect::<Vec<_>>();
        modules.sort_by(|left, right| {
            right
                .degree_centrality
                .total_cmp(&left.degree_centrality)
                .then_with(|| {
                    right
                        .incoming_reference_count
                        .cmp(&left.incoming_reference_count)
                })
                .then_with(|| left.path.cmp(&right.path))
        });
        output.push_str("| Module | Incoming | Outgoing | External | Centrality |\n");
        output.push_str("| --- | ---: | ---: | ---: | ---: |\n");
        for module in modules.into_iter().take(20) {
            output.push_str(&format!(
                "| <code>{}</code> | {} | {} | {} | {:.3} |\n",
                markdown_code(&module.path),
                module.incoming_module_dependencies,
                module.outgoing_module_dependencies,
                module.external_package_dependencies,
                module.degree_centrality
            ));
        }
        output.push('\n');
    }

    output.push_str("## Observed boundary connections\n\n");
    if graph.boundaries.is_empty() {
        output.push_str("The index contains no cross-package dependency.\n\n");
    } else {
        output.push_str("| Source package | Target package | Dependencies | References |\n");
        output.push_str("| --- | --- | ---: | ---: |\n");
        for boundary in &graph.boundaries {
            output.push_str(&format!(
                "| <code>{}</code> | <code>{}</code> | {} | {} |\n",
                markdown_code(&boundary.source_package),
                markdown_code(&boundary.target_package),
                boundary.dependency_count,
                boundary.reference_count
            ));
        }
        output.push('\n');
    }

    output.push_str("## Dependency cycles\n\n");
    if graph.cycles.is_empty() {
        output.push_str("The module graph contains no multi-module cycle.\n\n");
    } else {
        output.push_str("Each row is one strongly connected module group.\n\n");
        output.push_str("| Cycle | Modules |\n");
        output.push_str("| --- | --- |\n");
        for cycle in &graph.cycles {
            let modules = cycle
                .modules
                .iter()
                .map(|module| format!("<code>{}</code>", markdown_code(module)))
                .collect::<Vec<_>>()
                .join("<br>");
            output.push_str(&format!("| {} | {} |\n", cycle.id, modules));
        }
        output.push('\n');
    }

    output.push_str("## Coverage\n\n");
    output.push_str(&format!(
        "The index contains {} document records, {} definitions, and {} references.\n\n",
        graph.coverage.document_count,
        graph.coverage.definition_occurrence_count,
        graph.coverage.reference_occurrence_count
    ));
    output.push_str(&format!(
        "Crux resolved {} internal references and {} external references.\n\n",
        graph.coverage.resolved_internal_reference_count, graph.coverage.external_reference_count
    ));
    output.push_str(&format!(
        "Crux excluded {} unknown, {} ambiguous, and {} malformed occurrences.\n\n",
        graph.coverage.unknown_reference_count,
        graph.coverage.ambiguous_reference_count,
        graph.coverage.malformed_occurrence_count
    ));
    for limitation in &graph.coverage.limitations {
        output.push_str(&format!("- {limitation}\n"));
    }
    output.push('\n');

    output.push_str("## Diagnostics\n\n");
    output.push_str(&format!(
        "- Duplicate definition symbols: {}\n",
        graph.diagnostics.duplicate_definitions.len()
    ));
    output.push_str(&format!(
        "- Malformed symbols: {}\n",
        graph.diagnostics.malformed_symbols.len()
    ));
    output.push_str(&format!(
        "- Unknown reference groups: {}\n",
        graph.diagnostics.unknown_references.len()
    ));
    output.push_str(&format!(
        "- Ambiguous reference groups: {}\n",
        graph.diagnostics.ambiguous_references.len()
    ));
    output.push_str(&format!(
        "- Mixed-package modules: {}\n",
        graph.diagnostics.mixed_package_modules.len()
    ));
    output
}

fn markdown_cell(value: &str) -> String {
    escape_html(value).replace('|', "\\|").replace('\n', " ")
}

fn markdown_code(value: &str) -> String {
    escape_html(value).replace('\n', " ")
}

fn render_html(graph: &ArchitectureGraph) -> Result<String> {
    #[derive(Serialize)]
    struct HtmlGraph<'a> {
        summary: &'a Summary,
        packages: &'a [PackageRecord],
        modules: &'a [ModuleRecord],
        edges: Vec<HtmlEdge<'a>>,
    }

    #[derive(Serialize)]
    struct HtmlEdge<'a> {
        id: &'a str,
        source: &'a str,
        target: &'a str,
        target_kind: &'a str,
        reference_count: usize,
    }

    let html_graph = HtmlGraph {
        summary: &graph.summary,
        packages: &graph.packages,
        modules: &graph.modules,
        edges: graph
            .edges
            .iter()
            .map(|edge| HtmlEdge {
                id: &edge.id,
                source: &edge.source,
                target: &edge.target,
                target_kind: edge.target_kind,
                reference_count: edge.reference_count,
            })
            .collect(),
    };
    let data =
        serde_json::to_string(&html_graph).context("serialize embedded architecture graph")?;
    let escaped = escape_script_data(&data);
    Ok(HTML_TEMPLATE.replace("__CRUX_GRAPH_JSON__", &escaped))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_script_data(value: &str) -> String {
    value
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

const HTML_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'">
<title>Crux architecture graph</title>
<style>
:root{color-scheme:dark;--bg:#0b1020;--panel:#121a2d;--line:#2b3955;--text:#e7edf7;--muted:#92a1b9;--accent:#73daca;--external:#f6c177}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:14px/1.4 ui-sans-serif,system-ui,sans-serif;height:100vh;overflow:hidden}
header{height:64px;display:flex;align-items:center;gap:12px;padding:10px 16px;background:var(--panel);border-bottom:1px solid var(--line)}
h1{font-size:16px;margin:0 12px 0 0;white-space:nowrap}input,select,button{background:#0d1425;color:var(--text);border:1px solid var(--line);border-radius:7px;padding:8px 10px}input{width:min(360px,32vw)}button{cursor:pointer}.status{color:var(--muted);margin-left:auto;white-space:nowrap}
main{display:grid;grid-template-columns:minmax(0,1fr) 330px;height:calc(100vh - 64px)}#stage{position:relative;overflow:hidden}canvas{width:100%;height:100%;display:block}.hint{position:absolute;left:12px;bottom:10px;color:var(--muted);background:#0b1020cc;padding:6px 8px;border-radius:6px;pointer-events:none}
aside{background:var(--panel);border-left:1px solid var(--line);padding:16px;overflow:auto}h2{font-size:15px;margin:0 0 12px}h3{font-size:12px;color:var(--muted);text-transform:uppercase;letter-spacing:.08em;margin:20px 0 8px}.path{word-break:break-word;color:var(--accent)}dl{display:grid;grid-template-columns:1fr auto;gap:7px 12px;margin:12px 0}dt{color:var(--muted)}dd{margin:0;text-align:right}.connections{list-style:none;padding:0;margin:0}.connections li{padding:8px 0;border-bottom:1px solid var(--line);word-break:break-word}.empty{color:var(--muted)}
@media(max-width:800px){main{grid-template-columns:1fr}aside{position:absolute;right:0;top:64px;bottom:0;width:min(330px,88vw);box-shadow:-8px 0 30px #0008}header{gap:7px;padding:8px}h1{display:none}.status{display:none}input{width:42vw}}
</style>
</head>
<body>
<header><h1>Crux architecture</h1><input id="search" type="search" placeholder="Search modules or packages" aria-label="Search graph"><select id="direction" aria-label="Connection direction"><option value="both">Both directions</option><option value="out">Outgoing</option><option value="in">Incoming</option></select><button id="fit" type="button">Fit graph</button><span class="status" id="status"></span></header>
<main><section id="stage"><canvas id="canvas" aria-label="Architecture dependency graph"></canvas><div class="hint">Select a node. Drag to pan. Scroll to zoom.</div></section><aside><h2>Selection</h2><div id="details" class="empty">Select a module or package.</div></aside></main>
<script id="graph-data" type="application/json">__CRUX_GRAPH_JSON__</script>
<script>
"use strict";
const graph=JSON.parse(document.getElementById("graph-data").textContent);
const LIMITS={nodes:600,edges:2000,labels:90};
const packageById=new Map(graph.packages.map(p=>[p.id,p]));
const nodes=[...graph.modules.map(m=>({...m,kind:"module",group:m.package_id,score:m.incoming_reference_count+m.outgoing_reference_count})),...graph.packages.filter(p=>p.kind==="external").map(p=>({...p,kind:"package",group:p.id,score:p.incoming_reference_count}))];
const nodeById=new Map(nodes.map(n=>[n.id,n]));
const ranked=[...nodes].sort((a,b)=>b.score-a.score||a.id.localeCompare(b.id));
let visibleNodes=ranked.slice(0,LIMITS.nodes),selected=null,direction="both";
let transform={x:0,y:0,scale:1},drag=null;
const canvas=document.getElementById("canvas"),ctx=canvas.getContext("2d"),stage=document.getElementById("stage"),status=document.getElementById("status"),details=document.getElementById("details");
let positions=new Map(),visibleEdges=[];
function rebuild(query=""){
  const q=query.trim().toLowerCase();
  if(q){const matches=nodes.filter(n=>(n.path||n.label||"").toLowerCase().includes(q)||(packageById.get(n.group)?.label||"").toLowerCase().includes(q));const ids=new Set(matches.map(n=>n.id));for(const e of graph.edges){if(ids.has(e.source))ids.add(e.target);if(ids.has(e.target))ids.add(e.source)}visibleNodes=nodes.filter(n=>ids.has(n.id)).sort((a,b)=>a.id.localeCompare(b.id)).slice(0,LIMITS.nodes)}else visibleNodes=ranked.slice(0,LIMITS.nodes);
  if(selected&&!visibleNodes.some(n=>n.id===selected.id))visibleNodes=[selected,...visibleNodes.slice(0,LIMITS.nodes-1)];
  const ids=new Set(visibleNodes.map(n=>n.id));visibleEdges=graph.edges.filter(e=>ids.has(e.source)&&ids.has(e.target)).sort((a,b)=>b.reference_count-a.reference_count||a.id.localeCompare(b.id)).slice(0,LIMITS.edges);layout();fitGraph();draw();updateStatus();
}
function layout(){positions=new Map();const groups=new Map();for(const n of visibleNodes){if(!groups.has(n.group))groups.set(n.group,[]);groups.get(n.group).push(n)}const ordered=[...groups].sort((a,b)=>a[0].localeCompare(b[0])),maxGroup=Math.max(1,...ordered.map(([,list])=>list.length)),spread=44*Math.sqrt(maxGroup),groupRadius=Math.max(260+spread,ordered.length*70+spread);ordered.forEach(([group,list],gi)=>{const ga=ordered.length===1?0:(Math.PI*2*gi/ordered.length)-Math.PI/2,gx=ordered.length===1?0:Math.cos(ga)*groupRadius,gy=ordered.length===1?0:Math.sin(ga)*groupRadius;list.sort((a,b)=>a.id.localeCompare(b.id)).forEach((n,i)=>{const a=i*2.399963229728653,r=i?44*Math.sqrt(i):0;positions.set(n.id,{x:gx+Math.cos(a)*r,y:gy+Math.sin(a)*r})})})}
function fitGraph(){const r=stage.getBoundingClientRect(),points=[...positions.values()];if(!points.length){transform={x:0,y:0,scale:1};return}const xs=points.map(p=>p.x),ys=points.map(p=>p.y),minX=Math.min(...xs),maxX=Math.max(...xs),minY=Math.min(...ys),maxY=Math.max(...ys),width=Math.max(120,maxX-minX+300),height=Math.max(120,maxY-minY+160),scale=Math.max(.15,Math.min(2,Math.min(r.width/width,r.height/height)));transform={x:-(minX+maxX)*scale/2,y:-(minY+maxY)*scale/2,scale}}
function resize(){const dpr=Math.min(devicePixelRatio||1,2),r=stage.getBoundingClientRect();canvas.width=Math.floor(r.width*dpr);canvas.height=Math.floor(r.height*dpr);canvas.style.width=r.width+"px";canvas.style.height=r.height+"px";ctx.setTransform(dpr,0,0,dpr,0,0);fitGraph();draw()}
function screen(p){const r=stage.getBoundingClientRect();return{x:r.width/2+transform.x+p.x*transform.scale,y:r.height/2+transform.y+p.y*transform.scale}}
function related(e){if(!selected)return false;if(direction==="out")return e.source===selected.id;if(direction==="in")return e.target===selected.id;return e.source===selected.id||e.target===selected.id}
function draw(){const r=stage.getBoundingClientRect();ctx.clearRect(0,0,r.width,r.height);for(const e of visibleEdges){const a=positions.get(e.source),b=positions.get(e.target);if(!a||!b)continue;const sa=screen(a),sb=screen(b),active=related(e);ctx.strokeStyle=selected?(active?"#73daca":"#253149"):(e.target_kind==="package"?"#806b46":"#405171");ctx.globalAlpha=selected&&!active?.22:.72;ctx.lineWidth=Math.min(5,1+Math.log2(e.reference_count+1));ctx.beginPath();ctx.moveTo(sa.x,sa.y);ctx.lineTo(sb.x,sb.y);ctx.stroke();arrow(sa,sb,ctx.strokeStyle)}ctx.globalAlpha=1;ctx.font="11px ui-sans-serif,system-ui";const labelSet=new Set(),boxes=[],candidates=[...visibleNodes].sort((a,b)=>(selected?.id===b.id)-(selected?.id===a.id)||b.score-a.score||a.id.localeCompare(b.id));for(const n of candidates){if(labelSet.size>=LIMITS.labels)break;const p=screen(positions.get(n.id)),text=trim(n.path||n.label,42),box={x:p.x+12,y:p.y-8,w:ctx.measureText(text).width+6,h:16},overlap=boxes.some(other=>box.x<other.x+other.w&&box.x+box.w>other.x&&box.y<other.y+other.h&&box.y+box.h>other.y);if(!overlap||selected?.id===n.id){labelSet.add(n.id);boxes.push(box)}}for(const n of visibleNodes){const p=screen(positions.get(n.id)),isSelected=selected?.id===n.id,radius=n.kind==="package"?9:Math.min(11,5+Math.log2(n.score+1));ctx.fillStyle=isSelected?"#ffffff":n.kind==="package"?"#f6c177":color(n.group);ctx.beginPath();ctx.arc(p.x,p.y,radius,0,Math.PI*2);ctx.fill();if(labelSet.has(n.id)){ctx.font=(isSelected?"600 ":"")+"11px ui-sans-serif,system-ui";ctx.fillStyle="#dce6f5";ctx.fillText(trim(n.path||n.label,42),p.x+radius+4,p.y+4)}}}
function arrow(a,b,colorValue){const dx=b.x-a.x,dy=b.y-a.y,len=Math.hypot(dx,dy);if(len<18)return;const ux=dx/len,uy=dy/len,x=b.x-ux*10,y=b.y-uy*10;ctx.fillStyle=colorValue;ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x-ux*8-uy*4,y-uy*8+ux*4);ctx.lineTo(x-ux*8+uy*4,y-uy*8-ux*4);ctx.closePath();ctx.fill()}
function color(id){let h=0;for(const c of id)h=(h*31+c.charCodeAt(0))%360;return `hsl(${h} 55% 58%)`}
function trim(s,n){return s.length>n?s.slice(0,n-1)+"…":s}
function updateStatus(){status.textContent=`${visibleNodes.length}/${nodes.length} nodes · ${visibleEdges.length}/${graph.edges.length} dependencies${nodes.length>LIMITS.nodes?" · bounded view":""}`}
function select(node){selected=node;showDetails();draw()}
function showDetails(){if(!selected){details.className="empty";details.textContent="Select a module or package.";return}details.className="";const n=selected,p=n.kind==="module"?packageById.get(n.package_id):n;const connections=graph.edges.filter(e=>e.source===n.id||e.target===n.id).sort((a,b)=>b.reference_count-a.reference_count||a.id.localeCompare(b.id)).slice(0,30);details.replaceChildren();const title=document.createElement("div");title.className="path";title.textContent=n.path||n.label;details.append(title);const dl=document.createElement("dl");const rows=n.kind==="module"?[["Package",p?.label||n.package_id],["Languages",n.languages.join(", ")||"unknown"],["Definitions",n.definition_count],["References",n.reference_count],["Incoming modules",n.incoming_module_dependencies],["Outgoing modules",n.outgoing_module_dependencies],["External packages",n.external_package_dependencies],["Unknown references",n.unknown_reference_count],["Ambiguous references",n.ambiguous_reference_count],["Malformed references",n.malformed_reference_count],["Degree centrality",n.degree_centrality.toFixed(3)]]:[["Kind",n.kind],["Modules",n.module_count],["Incoming boundaries",n.incoming_boundary_count],["Outgoing boundaries",n.outgoing_boundary_count]];for(const [k,v] of rows){const dt=document.createElement("dt"),dd=document.createElement("dd");dt.textContent=k;dd.textContent=String(v);dl.append(dt,dd)}details.append(dl);const h=document.createElement("h3");h.textContent="Connections";details.append(h);if(!connections.length){const empty=document.createElement("div");empty.className="empty";empty.textContent="No rendered dependency.";details.append(empty);return}const ul=document.createElement("ul");ul.className="connections";for(const e of connections){const li=document.createElement("li"),other=nodeById.get(e.source===n.id?e.target:e.source);li.textContent=`${e.source===n.id?"→":"←"} ${other?.path||other?.label||e.target} · ${e.reference_count} references`;ul.append(li)}details.append(ul)}
canvas.addEventListener("pointerdown",e=>{canvas.setPointerCapture(e.pointerId);drag={x:e.clientX,y:e.clientY,tx:transform.x,ty:transform.y,moved:false}});canvas.addEventListener("pointermove",e=>{if(!drag)return;const dx=e.clientX-drag.x,dy=e.clientY-drag.y;if(Math.abs(dx)+Math.abs(dy)>3)drag.moved=true;transform.x=drag.tx+dx;transform.y=drag.ty+dy;draw()});canvas.addEventListener("pointerup",e=>{if(!drag?.moved){const r=canvas.getBoundingClientRect(),x=e.clientX-r.left,y=e.clientY-r.top;let best=null,d=18;for(const n of visibleNodes){const p=screen(positions.get(n.id)),next=Math.hypot(p.x-x,p.y-y);if(next<d){best=n;d=next}}if(best)select(best)}drag=null});canvas.addEventListener("wheel",e=>{e.preventDefault();transform.scale=Math.max(.15,Math.min(4,transform.scale*Math.exp(-e.deltaY*.001)));draw()},{passive:false});
document.getElementById("search").addEventListener("input",e=>rebuild(e.target.value));document.getElementById("direction").addEventListener("change",e=>{direction=e.target.value;draw()});document.getElementById("fit").addEventListener("click",()=>{fitGraph();draw()});new ResizeObserver(resize).observe(stage);rebuild();resize();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::{EnumOrUnknown, Message};
    use scip::types::{symbol_information, Document, Occurrence, SymbolInformation, SymbolRole};

    fn occurrence(symbol: &str, definition: bool) -> Occurrence {
        Occurrence {
            range: vec![0, 0, 1],
            symbol: symbol.to_string(),
            symbol_roles: if definition {
                SymbolRole::Definition as i32
            } else {
                SymbolRole::ReadAccess as i32
            },
            ..Default::default()
        }
    }

    fn information(symbol: &str) -> SymbolInformation {
        SymbolInformation {
            symbol: symbol.to_string(),
            display_name: symbol.to_string(),
            kind: EnumOrUnknown::new(symbol_information::Kind::Function),
            ..Default::default()
        }
    }

    fn document(path: &str, symbols: &[(&str, bool)]) -> Document {
        Document {
            relative_path: path.to_string(),
            language: "rust".to_string(),
            occurrences: symbols
                .iter()
                .map(|(symbol, definition)| occurrence(symbol, *definition))
                .collect(),
            symbols: symbols
                .iter()
                .filter(|(_, definition)| *definition)
                .map(|(symbol, _)| information(symbol))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn graph_uses_definitions_and_references_for_edges_and_cycles() {
        let a = "rust-analyzer cargo app 1.0.0 a().";
        let b = "rust-analyzer cargo app 1.0.0 b().";
        let external = "rust-analyzer cargo serde 1.0.0 Serialize#";
        let graph = build_graph(&Index {
            documents: vec![
                document("src/a.rs", &[(a, true), (b, false), (external, false)]),
                document("src/b.rs", &[(b, true), (a, false)]),
                document("src/isolated.rs", &[]),
            ],
            external_symbols: vec![information(external)],
            ..Default::default()
        });

        assert_eq!(graph.summary.module_count, 3);
        assert_eq!(graph.summary.module_dependency_count, 2);
        assert_eq!(graph.summary.external_dependency_count, 1);
        assert_eq!(graph.summary.cycle_count, 1);
        assert_eq!(graph.summary.isolated_module_count, 1);
        assert_eq!(graph.summary.external_package_count, 1);
        assert_eq!(graph.cycles[0].modules.len(), 2);
        assert_eq!(graph.boundaries.len(), 1);
    }

    #[test]
    fn compact_overview_reports_structure_and_coverage() {
        let a = "rust-analyzer cargo app 1.0.0 a().";
        let b = "rust-analyzer cargo app 1.0.0 b().";
        let index = Index {
            documents: vec![
                document("src/a.rs", &[(a, true), (b, false)]),
                document("src/b.rs", &[(b, true), (a, false)]),
                document("src/isolated.rs", &[]),
            ],
            ..Default::default()
        };
        let result = ArchitectureProjection::new(&SemanticIndex::new(index))
            .query(ArchitectureQuery {
                scope: None,
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();

        assert!(result.contains("architecture overview"));
        assert!(result.contains("direct module dependencies 2"));
        assert!(result.contains("cycles 1"));
        assert!(result.contains("isolated modules 1"));
        assert!(result.contains("coverage:"));
        assert!(!result.contains("{\"schema_version\""));
    }

    #[test]
    fn compact_scope_matching_uses_path_component_boundaries() {
        let foo = "rust-analyzer cargo app 1.0.0 foo().";
        let foobar = "rust-analyzer cargo app 1.0.0 foobar().";
        let index = Index {
            documents: vec![
                document("src/foo/mod.rs", &[(foo, true)]),
                document("src/foobar/mod.rs", &[(foobar, true)]),
            ],
            ..Default::default()
        };
        let projection = ArchitectureProjection::new(&SemanticIndex::new(index));
        let result = projection
            .query(ArchitectureQuery {
                scope: Some("./src/foo/"),
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();

        assert!(result.contains("matched modules: 1"));
        assert!(result.contains("src/foo/mod.rs"));
        assert!(!result.contains("src/foobar/mod.rs"));
        assert!(projection
            .query(ArchitectureQuery {
                scope: Some("../src/foo"),
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap_err()
            .to_string()
            .contains("cannot contain '..'"));
    }

    #[test]
    fn compact_scope_respects_dependency_direction() {
        let a = "rust-analyzer cargo app 1.0.0 a().";
        let b = "rust-analyzer cargo app 1.0.0 b().";
        let c = "rust-analyzer cargo app 1.0.0 c().";
        let index = Index {
            documents: vec![
                document("src/a.rs", &[(a, true), (b, false)]),
                document("src/b.rs", &[(b, true)]),
                document("src/c.rs", &[(c, true), (a, false)]),
            ],
            ..Default::default()
        };
        let projection = ArchitectureProjection::new(&SemanticIndex::new(index));
        let outgoing = projection
            .query(ArchitectureQuery {
                scope: Some("src/a.rs"),
                direction: ArchitectureDirection::Outgoing,
                limit: 20,
                offset: 0,
            })
            .unwrap();
        let incoming = projection
            .query(ArchitectureQuery {
                scope: Some("src/a.rs"),
                direction: ArchitectureDirection::Incoming,
                limit: 20,
                offset: 0,
            })
            .unwrap();

        assert!(outgoing.contains("direction: outgoing"));
        assert!(outgoing.contains("src/a.rs -> src/b.rs"));
        assert!(!outgoing.contains("direct incoming dependencies:"));
        assert!(!outgoing.contains("src/c.rs -> src/a.rs"));
        assert!(incoming.contains("direction: incoming"));
        assert!(incoming.contains("src/c.rs -> src/a.rs"));
        assert!(!incoming.contains("direct outgoing dependencies:"));
        assert!(!incoming.contains("src/a.rs -> src/b.rs"));
        assert!(incoming.contains("possible impact:"));
    }

    #[test]
    fn compact_scope_excludes_boundaries_from_unrelated_same_package_modules() {
        let selected = "rust-analyzer cargo app 1.0.0 selected().";
        let unrelated = "rust-analyzer cargo app 1.0.0 unrelated().";
        let external = "rust-analyzer cargo dependency 2.0.0 external().";
        let projection = ArchitectureProjection::new(&SemanticIndex::new(Index {
            documents: vec![
                document("src/selected.rs", &[(selected, true)]),
                document("src/unrelated.rs", &[(unrelated, true), (external, false)]),
            ],
            ..Default::default()
        }));

        let scoped = projection
            .query(ArchitectureQuery {
                scope: Some("src/selected.rs"),
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();
        let overview = projection
            .query(ArchitectureQuery {
                scope: None,
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();

        assert!(scoped.contains("relevant package boundaries:\n- none observed in the index"));
        assert!(!scoped.contains("cargo:dependency@2.0.0"));
        assert!(overview.contains("package boundaries 1"));
        assert!(overview.contains("cargo:dependency@2.0.0"));
    }

    #[test]
    fn compact_scope_explains_isolated_and_empty_matches() {
        let isolated = "rust-analyzer cargo app 1.0.0 isolated().";
        let projection = ArchitectureProjection::new(&SemanticIndex::new(Index {
            documents: vec![document("src/isolated.rs", &[(isolated, true)])],
            ..Default::default()
        }));
        let isolated_result = projection
            .query(ArchitectureQuery {
                scope: Some("src/isolated.rs"),
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();
        let empty_result = projection
            .query(ArchitectureQuery {
                scope: Some("src/missing.rs"),
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();

        assert_eq!(
            isolated_result
                .matches("- none observed in the index")
                .count(),
            5
        );
        assert!(empty_result.contains("matched modules: 0"));
        assert!(empty_result.contains("use the overview"));
        assert!(empty_result.contains("then use rg"));
        assert!(empty_result.contains("stop: do not retry"));
    }

    #[test]
    fn compact_output_is_bounded_and_reports_every_truncation() {
        let documents = (0..300)
            .map(|index| {
                let path = format!(
                    "src/very-long-directory-name-{index:03}/another-long-component-{index:03}/module.rs"
                );
                let symbol = format!("rust-analyzer cargo app 1.0.0 module{index}().");
                document(&path, &[(symbol.as_str(), true)])
            })
            .collect();
        let projection = ArchitectureProjection::new(&SemanticIndex::new(Index {
            documents,
            ..Default::default()
        }));
        let first = projection
            .query(ArchitectureQuery {
                scope: None,
                direction: ArchitectureDirection::Both,
                limit: 200,
                offset: 0,
            })
            .unwrap();
        let second = projection
            .query(ArchitectureQuery {
                scope: None,
                direction: ArchitectureDirection::Both,
                limit: 200,
                offset: 0,
            })
            .unwrap();

        assert_eq!(first, second);
        assert!(first.len() <= MAX_MCP_OUTPUT_BYTES);
        assert!(first.contains("output byte limit reached") || first.contains("… 100 more"));
    }

    #[test]
    fn local_symbols_remain_scoped_to_their_document() {
        let graph = build_graph(&Index {
            documents: vec![
                document("src/a.rs", &[("local 1", true)]),
                document("src/b.rs", &[("local 1", false)]),
            ],
            ..Default::default()
        });

        assert!(graph.edges.is_empty());
        assert_eq!(graph.coverage.unknown_reference_count, 1);
        assert_eq!(graph.diagnostics.unknown_references.len(), 1);
        assert_eq!(
            graph.diagnostics.unknown_references[0].reason,
            "local symbol has no definition in this document"
        );
    }

    #[test]
    fn duplicate_definitions_make_references_ambiguous() {
        let symbol = "scip-typescript npm app 1.0.0 duplicate().";
        let graph = build_graph(&Index {
            documents: vec![
                document("a.ts", &[(symbol, true)]),
                document("b.ts", &[(symbol, true)]),
                document("use.ts", &[(symbol, false)]),
            ],
            ..Default::default()
        });

        assert!(graph.edges.is_empty());
        assert_eq!(graph.coverage.duplicate_definition_symbol_count, 1);
        assert_eq!(graph.coverage.ambiguous_reference_count, 1);
        assert_eq!(graph.diagnostics.ambiguous_references.len(), 1);
    }

    #[test]
    fn compact_scope_preserves_unknown_ambiguous_and_malformed_diagnostics() {
        let duplicate = "rust-analyzer cargo app 1.0.0 duplicate().";
        let unknown = "rust-analyzer cargo app 1.0.0 missing().";
        let index = Index {
            documents: vec![
                document("src/a.rs", &[(duplicate, true)]),
                document("src/b.rs", &[(duplicate, true)]),
                document(
                    "src/use.rs",
                    &[
                        (duplicate, false),
                        (unknown, false),
                        ("not a SCIP symbol", false),
                    ],
                ),
            ],
            ..Default::default()
        };
        let result = ArchitectureProjection::new(&SemanticIndex::new(index))
            .query(ArchitectureQuery {
                scope: Some("src/use.rs"),
                direction: ArchitectureDirection::Both,
                limit: 20,
                offset: 0,
            })
            .unwrap();

        assert!(result.contains("scope diagnostics: unknown 1 | ambiguous 1 | malformed 1"));
        assert!(result.contains("- unknown rust-analyzer cargo app 1.0.0 missing()."));
        assert!(result.contains("- ambiguous rust-analyzer cargo app 1.0.0 duplicate()."));
        assert!(result.contains("- malformed not a SCIP symbol"));
    }

    #[test]
    fn malformed_symbols_and_empty_indexes_are_explicit() {
        let graph = build_graph(&Index {
            documents: vec![document("bad.rs", &[("not a SCIP symbol", false)])],
            ..Default::default()
        });
        assert_eq!(graph.coverage.malformed_occurrence_count, 1);
        assert_eq!(graph.modules[0].malformed_reference_count, 1);
        assert_eq!(graph.modules[0].unknown_reference_count, 0);

        let empty = build_graph(&Index::default());
        assert_eq!(empty.summary.module_count, 0);
        assert!(empty.modules.is_empty());
        assert!(render_report(&empty).contains("no modules"));
    }

    #[test]
    fn graph_is_stable_when_document_and_occurrence_order_changes() {
        let a = "rust-analyzer cargo stable 1.0.0 a().";
        let b = "rust-analyzer cargo stable 1.0.0 b().";
        let first = Index {
            documents: vec![
                document("a.rs", &[(a, true), (b, false)]),
                document("b.rs", &[(b, true)]),
            ],
            ..Default::default()
        };
        let mut second = first.clone();
        second.documents.reverse();
        second.documents[1].occurrences.reverse();

        assert_eq!(
            serde_json::to_string(&build_graph(&first)).unwrap(),
            serde_json::to_string(&build_graph(&second)).unwrap()
        );
    }

    #[test]
    fn package_identity_uses_the_scip_parser() {
        let package = PackageIdentity::from_symbol(
            "scip-typescript npm package-name 1.2.3 `path with space`/value.",
        )
        .unwrap();
        assert_eq!(package.manager, "npm");
        assert_eq!(package.name, "package-name");
        assert_eq!(package.version, "1.2.3");
        assert!(package.id().contains("package-name"));
    }

    #[test]
    fn html_is_offline_bounded_and_escapes_embedded_data() {
        let symbol = "scip-typescript npm app 1.0.0 `</script><b>`().";
        let graph = build_graph(&Index {
            documents: vec![document("</script>.ts", &[(symbol, true)])],
            ..Default::default()
        });
        let html = render_html(&graph).unwrap();
        assert!(!html.contains("</script>.ts"));
        assert!(html.contains("\\u003c/script\\u003e.ts"));
        assert!(html.contains("nodes:600,edges:2000"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("http://"));
    }

    #[test]
    fn output_writer_creates_all_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let graph = build_graph(&Index::default());
        write_outputs(directory.path(), &graph).unwrap();

        for name in ["graph.html", "GRAPH_REPORT.md", "graph.json"] {
            assert!(directory.path().join(name).is_file(), "{name}");
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("graph.json")).unwrap())
                .unwrap();
        assert_eq!(parsed["schema_version"], GRAPH_SCHEMA_VERSION);
        assert!(Index::parse_from_bytes(&Index::default().write_to_bytes().unwrap()).is_ok());
    }

    #[test]
    fn argument_parser_accepts_options_in_any_order() {
        let parsed = parse_arguments(&[
            "--output=out".to_string(),
            "project".to_string(),
            "--index".to_string(),
            "index.scip".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.project_root, PathBuf::from("project"));
        assert_eq!(parsed.index, Some(PathBuf::from("index.scip")));
        assert_eq!(parsed.output, Some(PathBuf::from("out")));
    }
}

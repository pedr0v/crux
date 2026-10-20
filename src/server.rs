use crate::architecture::{ArchitectureDirection, ArchitectureProjection, ArchitectureQuery};
use crate::index::{run_indexer, IndexCache, IndexStats};
use crate::queries::{
    callers, dead_exports, definitions, find, map_symbols, outline, references, search,
    MAX_MAP_NAMES,
};
use crate::semantic::SemanticIndex;
use crate::setup;
use crate::update;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
/// The server echoes a requested protocol version only from this list.
/// Any other request gets DEFAULT_PROTOCOL_VERSION.
/// The server is tools-only over stdio and accepts JSON-RPC batches.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
/// The answer to an unknown version. The MCP lifecycle says: answer with
/// the latest version the server supports.
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const FULL_INSTRUCTIONS: &str = "Use scip_architecture once for explicit architecture questions, module or package dependencies, cycles, or unclear scope before a change across files. Pass a known project-relative file or directory as scope. Reuse relevant architecture results until the index changes. Use scip_map for named-symbol references and callers. Use scip_outline for one file's symbols. Use scip_find for fragments or unreferenced audits. Prefer rg for simple text or definition search. Do not require architecture before every edit or symbol query. If a Crux result is empty, use repository-native search and stop equivalent retries. If a scip_map section is truncated, call it once more with ref_limit=200. If the index is missing or stale, call scip_index once. Make at most two index calls before answering.";
const SLIM_INSTRUCTIONS: &str = "Use scip_architecture once for explicit architecture questions, module or package dependencies, cycles, or unclear scope before a change across files. Pass a known project-relative file or directory as scope. Reuse relevant architecture results until the index changes. Use scip_map for named-symbol references and callers. Use scip_outline for one file's symbols. Use scip_find for fragments or unreferenced audits. Prefer rg for simple text or definition search. Do not require architecture before every edit or symbol query. If a Crux result is empty, use repository-native search and stop equivalent retries. If a scip_map section is truncated, call it once more with ref_limit=200. If the index is missing or stale, call scip_index once. Make at most two index calls before answering. Need finer-grained tools? Call scip_expand.";
const EXPANDED_TOOLS: &str = "expanded: scip_search, scip_def, scip_refs, scip_callers, scip_dead";
/// scip_index accepts these language names. index.rs parses the same list.
const INDEX_LANGUAGES: &[&str] = &["typescript", "python", "rust", "dart", "java", "cpp", "go"];
const DEFAULT_FIND_LIMIT: usize = 20;
const DEFAULT_MAP_REF_LIMIT: usize = 20;
const DEFAULT_NARROW_REFS_LIMIT: usize = 50;
const DEFAULT_SEARCH_LIMIT: usize = 20;
#[cfg(test)]
pub(crate) const DEFAULT_REFS_LIMIT: usize = 20;
#[cfg(test)]
pub(crate) const DEFAULT_MAP_REFS_LIMIT: usize = 20;
pub(crate) const DEFAULT_CALLERS_LIMIT: usize = 40;
pub(crate) const DEFAULT_DEAD_LIMIT: usize = 100;
const DEFAULT_OUTLINE_LIMIT: usize = 38;
const DEFAULT_ARCHITECTURE_LIMIT: usize = 20;
pub(crate) const MAX_LIMIT: usize = 200;
const MAX_CALLER_DEPTH: usize = 3;
/// Subcommands that print their own usage for `--help` or `-h`.
const SUBCOMMANDS: &[&str] = &[
    "architecture",
    "prepare",
    "census",
    "capabilities",
    "check",
    "self-update",
    "setup",
    "unsetup",
];
const HELP_TEXT: &str = "Usage:\n  crux architecture [project-dir] [--index <file>] [--output <directory>]\n  crux prepare --repo <dir> --output <external-index> [--format json]\n  crux census --repo <dir> [--format json]\n  crux capabilities\n  crux check --index <file>\n  crux [--profile slim|full]\n  crux [--profile slim|full] --version\n  crux self-update [--check]\n  crux check <absolute-project-root>\n  crux setup <codex|claude> [--project <dir>]\n  crux unsetup <codex|claude> [--project <dir>]";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Profile {
    #[default]
    Slim,
    Full,
}

impl Profile {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "slim" => Ok(Self::Slim),
            "full" => Ok(Self::Full),
            _ => bail!("invalid profile '{value}'; expected slim or full"),
        }
    }

    fn instructions(self) -> &'static str {
        match self {
            Self::Slim => SLIM_INSTRUCTIONS,
            Self::Full => FULL_INSTRUCTIONS,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexArgs {
    project_root: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    languages: Option<Vec<String>>,
    #[serde(default)]
    max_file_mb: Option<u64>,
    #[serde(default)]
    discover_depth: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    project_root: String,
    name: String,
    #[serde(default = "default_find_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    unreferenced: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    project_root: String,
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    project_root: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapArgs {
    project_root: String,
    names: Vec<String>,
    #[serde(default = "default_map_ref_limit")]
    ref_limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefsArgs {
    project_root: String,
    name: String,
    #[serde(default = "default_refs_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallersArgs {
    project_root: String,
    name: String,
    #[serde(default = "default_caller_depth")]
    depth: usize,
    #[serde(default = "default_callers_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct OutlineArgs {
    project_root: String,
    file: String,
    #[serde(default = "default_outline_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchitectureToolArgs {
    project_root: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    direction: ArchitectureDirection,
    #[serde(default = "default_architecture_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_find_limit() -> usize {
    DEFAULT_FIND_LIMIT
}

fn default_search_limit() -> usize {
    DEFAULT_SEARCH_LIMIT
}

fn default_map_ref_limit() -> usize {
    DEFAULT_MAP_REF_LIMIT
}

fn default_refs_limit() -> usize {
    DEFAULT_NARROW_REFS_LIMIT
}

fn default_caller_depth() -> usize {
    1
}

fn default_callers_limit() -> usize {
    DEFAULT_CALLERS_LIMIT
}

fn default_dead_limit() -> usize {
    DEFAULT_DEAD_LIMIT
}

fn default_outline_limit() -> usize {
    DEFAULT_OUTLINE_LIMIT
}

fn default_architecture_limit() -> usize {
    DEFAULT_ARCHITECTURE_LIMIT
}

fn default_true() -> bool {
    true
}

fn bounded_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

fn required_fields(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "scip_index" | "scip_dead" | "scip_architecture" => &["project_root"],
        "scip_find" | "scip_def" | "scip_refs" | "scip_callers" => &["project_root", "name"],
        "scip_search" => &["project_root", "query"],
        "scip_map" => &["project_root", "names"],
        "scip_outline" => &["project_root", "file"],
        "scip_expand" => &[],
        _ => panic!("missing required-fields metadata for {tool_name}"),
    }
}

fn parse_arguments<T: DeserializeOwned>(tool_name: &str, arguments: &Value) -> Result<T> {
    serde_json::from_value(arguments.clone()).map_err(|error| {
        anyhow!(
            "{tool_name}: invalid arguments ({error}). Required: {}.",
            required_fields(tool_name).join(", ")
        )
    })
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

struct Server {
    cache: IndexCache,
    architecture_cache: HashMap<PathBuf, CachedArchitecture>,
    profile: Profile,
    expanded: bool,
    list_changed_pending: bool,
    savings_ledger: Option<SavingsLedger>,
    #[cfg(test)]
    architecture_build_count: usize,
}

struct CachedArchitecture {
    index: Arc<SemanticIndex>,
    projection: ArchitectureProjection,
}

/// The ledger appends one JSON line per successful tool call.
/// CRUX_SAVINGS_LEDGER sets the file path.
/// External tools can attribute Crux token savings to a session from this file.
/// An unset variable disables the ledger.
struct SavingsLedger {
    path: std::path::PathBuf,
}

impl SavingsLedger {
    fn from_environment() -> Option<Self> {
        let path = std::env::var_os("CRUX_SAVINGS_LEDGER")?;
        if path.is_empty() {
            return None;
        }
        Some(Self {
            path: std::path::PathBuf::from(path),
        })
    }

    /// Paired Codex CLI benchmarks (Django + SymPy, 48 questions) measured
    /// a grep baseline at 1.88x the crux session tokens per correct answer.
    /// The estimate books 0.8x the response tokens as saved - a floor below
    /// both measured repositories (0.88x and 1.42x).
    fn record(&self, tool: &str, response: &str) {
        let response_tokens = (response.len() / 4) as u64;
        let saved_tokens = response_tokens * 4 / 5;
        if saved_tokens == 0 {
            return;
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let line = json!({
            "ts": timestamp,
            "tool": tool,
            "response_tokens": response_tokens,
            "saved_tokens": saved_tokens,
        })
        .to_string();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let _ = append_line(&mut file, line);
    }
}

/// Writes one JSON line with one write_all call.
/// A file opened with O_APPEND then gets one write(2) for the full line.
/// On a local file system, concurrent servers that share the ledger do
/// not interleave parts of a line.
fn append_line(writer: &mut impl Write, line: String) -> io::Result<()> {
    let mut record = line;
    record.push('\n');
    writer.write_all(record.as_bytes())
}

impl Default for Server {
    fn default() -> Self {
        Self::new(Profile::Slim)
    }
}

impl Server {
    fn new(profile: Profile) -> Self {
        Self {
            cache: IndexCache::default(),
            architecture_cache: HashMap::new(),
            profile,
            expanded: profile == Profile::Full,
            list_changed_pending: false,
            savings_ledger: SavingsLedger::from_environment(),
            #[cfg(test)]
            architecture_build_count: 0,
        }
    }

    fn expand(&mut self) -> String {
        if self.expanded {
            return "already expanded".to_string();
        }
        self.expanded = true;
        self.list_changed_pending = true;
        EXPANDED_TOOLS.to_string()
    }

    fn call_tool(&mut self, name: &str, arguments: &Value) -> ToolResult {
        let result = match name {
            "scip_index" => parse_arguments::<IndexArgs>(name, arguments).and_then(|arguments| {
                let project_root = Path::new(&arguments.project_root);
                let result = run_indexer(
                    project_root,
                    arguments.language.as_deref(),
                    arguments.languages.as_deref(),
                    arguments.max_file_mb,
                    arguments.discover_depth,
                    &mut self.cache,
                );
                if result.is_ok() {
                    self.architecture_cache.remove(project_root);
                }
                result
            }),
            "scip_architecture" => parse_arguments::<ArchitectureToolArgs>(name, arguments)
                .and_then(|arguments| {
                    let project_root = PathBuf::from(&arguments.project_root);
                    let loaded = self.cache.load(&project_root)?;
                    let needs_projection = self
                        .architecture_cache
                        .get(&project_root)
                        .is_none_or(|cached| !Arc::ptr_eq(&cached.index, &loaded.index));
                    if needs_projection {
                        self.architecture_cache.insert(
                            project_root.clone(),
                            CachedArchitecture {
                                index: Arc::clone(&loaded.index),
                                projection: ArchitectureProjection::new(&loaded.index),
                            },
                        );
                        #[cfg(test)]
                        {
                            self.architecture_build_count += 1;
                        }
                    }
                    self.architecture_cache
                        .get(&project_root)
                        .expect("architecture projection exists")
                        .projection
                        .query(ArchitectureQuery {
                            scope: arguments.scope.as_deref(),
                            direction: arguments.direction,
                            limit: bounded_limit(arguments.limit),
                            offset: arguments.offset,
                        })
                }),
            "scip_find" => parse_arguments::<FindArgs>(name, arguments).and_then(|arguments| {
                let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                find(
                    &loaded.index,
                    &arguments.name,
                    bounded_limit(arguments.limit),
                    arguments.offset,
                    arguments.unreferenced,
                )
            }),
            "scip_search" if self.expanded => parse_arguments::<SearchArgs>(name, arguments)
                .and_then(|arguments| {
                    let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                    search(
                        &loaded.index,
                        &arguments.query,
                        bounded_limit(arguments.limit),
                    )
                }),
            "scip_def" if self.expanded => {
                parse_arguments::<NameArgs>(name, arguments).and_then(|arguments| {
                    let project_root = Path::new(&arguments.project_root);
                    let loaded = self.cache.load(project_root)?;
                    definitions(project_root, &loaded.index, &arguments.name)
                })
            }
            "scip_map" => parse_arguments::<MapArgs>(name, arguments).and_then(|arguments| {
                let project_root = Path::new(&arguments.project_root);
                let loaded = self.cache.load(project_root)?;
                map_symbols(
                    project_root,
                    &loaded.index,
                    &arguments.names,
                    bounded_limit(arguments.ref_limit),
                    arguments.offset,
                )
            }),
            "scip_refs" if self.expanded => {
                parse_arguments::<RefsArgs>(name, arguments).and_then(|arguments| {
                    let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                    references(
                        &loaded.index,
                        &arguments.name,
                        bounded_limit(arguments.limit),
                        arguments.offset,
                    )
                })
            }
            "scip_callers" if self.expanded => parse_arguments::<CallersArgs>(name, arguments)
                .and_then(|arguments| {
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
            "scip_dead" if self.expanded => {
                parse_arguments::<DeadArgs>(name, arguments).and_then(|arguments| {
                    let project_root = Path::new(&arguments.project_root);
                    let loaded = self.cache.load(project_root)?;
                    dead_exports(
                        project_root,
                        &loaded.index,
                        arguments.path_prefix.as_deref(),
                        bounded_limit(arguments.limit),
                        arguments.exports_only,
                    )
                })
            }
            "scip_outline" => {
                parse_arguments::<OutlineArgs>(name, arguments).and_then(|arguments| {
                    let loaded = self.cache.load(Path::new(&arguments.project_root))?;
                    outline(
                        &loaded.index,
                        &arguments.file,
                        bounded_limit(arguments.limit),
                        arguments.offset,
                    )
                })
            }
            "scip_expand" => Ok(self.expand()),
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
        let Value::Array(batch) = &request else {
            return self.dispatch_value(&request);
        };
        if batch.is_empty() {
            return Some(rpc_error(Value::Null, -32600, "invalid request"));
        }
        // JSON-RPC 2.0 sends one array of responses for a batch.
        // A batch of notifications only gets no output.
        let responses = batch
            .iter()
            .filter_map(|item| self.dispatch_value(item))
            .collect::<Vec<_>>();
        (!responses.is_empty()).then(|| format!("[{}]", responses.join(",")))
    }

    fn dispatch_value(&mut self, request: &Value) -> Option<String> {
        if !request.is_object() {
            return Some(rpc_error(Value::Null, -32600, "invalid request"));
        }
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
                let protocol_version = match request
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                {
                    Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => version,
                    Some(_) => LATEST_PROTOCOL_VERSION,
                    None => DEFAULT_PROTOCOL_VERSION,
                };
                rpc_success(
                    id,
                    json!({
                        "protocolVersion": protocol_version,
                        "capabilities": {"tools": {"listChanged": true}},
                        "instructions": self.profile.instructions(),
                        "serverInfo": {
                            "name": "crux",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                )
            }
            "tools/list" => rpc_success(id, json!({"tools": tool_definitions(self.expanded)})),
            "tools/call" => {
                let Some(name) = request.pointer("/params/name").and_then(Value::as_str) else {
                    return has_id.then(|| rpc_error(id, -32602, "missing tool name"));
                };
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let result = self.call_tool(name, &arguments);
                // scip_index is a build step and scip_expand only shows more
                // tools. They are not answers. A ledger line for them books
                // savings for work the agent never avoided.
                if !result.is_error && savings_eligible(name) {
                    if let Some(ledger) = &self.savings_ledger {
                        ledger.record(name, &result.text);
                    }
                }
                rpc_success(id, result.into_json())
            }
            "ping" => rpc_success(id, json!({})),
            _ => rpc_error(id, -32601, "method not found"),
        };

        has_id.then_some(response)
    }

    fn dispatch_messages(&mut self, line: &str) -> Vec<String> {
        let mut messages = self.dispatch_line(line).into_iter().collect::<Vec<_>>();
        if std::mem::take(&mut self.list_changed_pending) {
            messages.push(rpc_notification("notifications/tools/list_changed"));
        }
        messages
    }
}

fn savings_eligible(tool: &str) -> bool {
    !matches!(tool, "scip_index" | "scip_architecture" | "scip_expand")
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

fn rpc_notification(method: &str) -> String {
    json!({"jsonrpc": "2.0", "method": method}).to_string()
}

fn tool_definitions(expanded: bool) -> Vec<Value> {
    let mut tools = slim_tool_definitions();
    if expanded {
        tools.extend(narrow_tool_definitions());
    }
    tools
}

fn slim_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "scip_index",
            "description": "Builds or refreshes the code index when another tool reports a missing index.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "language": {
                        "type": "string",
                        "enum": INDEX_LANGUAGES
                    },
                    "languages": {
                        "type": "array",
                        "items": {"type": "string", "enum": INDEX_LANGUAGES}
                    },
                    "max_file_mb": {
                        "type": "integer",
                        "description": "Skip TypeScript files over this many MB."
                    },
                    "discover_depth": {
                        "type": "integer",
                        "description": "Sub-project discovery depth. 0 = root only. Default 3."
                    }
                }),
                required_fields("scip_index")
            )
        }),
        json!({
            "name": "scip_architecture",
            "description": "Shows module and package architecture. Use once for explicit architecture, dependency, cycle, or unclear multi-file scope questions. Pass a known project-relative scope. Do not use for named symbols or text search.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Project-relative file or directory."
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["incoming", "outgoing", "both"],
                        "default": "both",
                        "description": "Dependency direction for a scoped query."
                    },
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_ARCHITECTURE_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Per-section result limit."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Per-section result offset."
                    }
                }),
                required_fields("scip_architecture")
            )
        }),
        json!({
            "name": "scip_find",
            "description": "Finds or disambiguates symbols by name or fragment. Set unreferenced=true to list dead symbols.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "name": {
                        "type": "string",
                        "description": "Name fragment, or * for unreferenced symbols."
                    },
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_FIND_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Candidate limit."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Candidate offset."
                    },
                    "unreferenced": {"type": "boolean"}
                }),
                required_fields("scip_find")
            )
        }),
        json!({
            "name": "scip_map",
            "description": "Answers named-symbol reference and caller questions in one call. It also reports definitions. Use rg for simple text or definition-only search.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "names": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": MAX_MAP_NAMES,
                        "description": "Bare or qualified symbol names."
                    },
                    "ref_limit": {
                        "type": "integer",
                        "default": DEFAULT_MAP_REF_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Per-symbol reference and caller limit."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Per-symbol result offset."
                    }
                }),
                required_fields("scip_map")
            )
        }),
        json!({
            "name": "scip_outline",
            "description": "Lists which symbols are defined in this file.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "file": {
                        "type": "string",
                        "description": "Project-relative file."
                    },
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_OUTLINE_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Symbol offset."
                    }
                }),
                required_fields("scip_outline")
            )
        }),
        json!({
            "name": "scip_expand",
            "description": "Reveals finer-grained search, definition, reference, caller, and dead-code tools.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

fn narrow_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "scip_search",
            "description": "Searches symbol display names for disambiguated definition locations.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "query": {"type": "string"},
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_SEARCH_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    }
                }),
                required_fields("scip_search")
            )
        }),
        json!({
            "name": "scip_def",
            "description": "Find definition sites with signatures and source lines for one name; prefer scip_map for multi-symbol navigation.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "name": {"type": "string"}
                }),
                required_fields("scip_def")
            )
        }),
        json!({
            "name": "scip_refs",
            "description": "Find reference lines grouped by file for one name; prefer scip_map for source context.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "name": {"type": "string"},
                    "limit": {
                        "type": "integer",
                        "default": DEFAULT_NARROW_REFS_LIMIT,
                        "minimum": 1,
                        "maximum": MAX_LIMIT
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Reference offset."
                    }
                }),
                required_fields("scip_refs")
            )
        }),
        json!({
            "name": "scip_callers",
            "description": "Finds transitive callers of one name as their enclosing functions or modules.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
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
                required_fields("scip_callers")
            )
        }),
        json!({
            "name": "scip_dead",
            "description": "Finds unused exports for an audit before deletion. It separates fully unused from file-local symbols.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "path_prefix": {
                        "type": "string",
                        "description": "Project-relative path prefix."
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
                        "description": "Only top-level exported declarations."
                    }
                }),
                required_fields("scip_dead")
            )
        }),
    ]
}

/// Every argument struct denies unknown fields.
/// The schema tells clients the same rule.
fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn serve_stdio(input: impl BufRead, mut output: impl Write, profile: Profile) -> Result<()> {
    let mut server = Server::new(profile);

    for line in input.lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        for message in server.dispatch_messages(&line) {
            writeln!(output, "{message}").context("write stdout")?;
            output.flush().context("flush stdout")?;
        }
    }
    Ok(())
}

fn run_stdio(profile: Profile) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::BufWriter::new(io::stdout().lock());
    serve_stdio(stdin.lock(), stdout, profile)
}

fn check_index(project_root: &Path) -> Result<String> {
    let mut cache = IndexCache::default();
    let loaded = cache.load_for_check(project_root)?;
    let stats = IndexStats::from_loaded(&loaded);
    let mut output = stats.compact();
    if let Some(message) = stats.empty_index_message() {
        output.push_str("\nwarning: ");
        output.push_str(&message);
    }
    Ok(output)
}

fn resolve_profile(
    arguments: Vec<String>,
    environment_profile: Option<&str>,
) -> Result<(Profile, Vec<String>)> {
    let mut explicit_profile = None;
    let mut remaining = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--profile" {
            let value = arguments
                .next()
                .context("--profile requires slim or full")?;
            explicit_profile = Some(Profile::parse(&value)?);
        } else if let Some(value) = argument.strip_prefix("--profile=") {
            explicit_profile = Some(Profile::parse(value)?);
        } else {
            remaining.push(argument);
        }
    }
    let profile = match explicit_profile {
        Some(profile) => profile,
        None => environment_profile
            .map(Profile::parse)
            .transpose()?
            .unwrap_or_default(),
    };
    Ok((profile, remaining))
}

/// Returns the usage lines of HELP_TEXT for one known subcommand.
fn subcommand_usage(command: &str) -> Option<String> {
    if !SUBCOMMANDS.contains(&command) {
        return None;
    }
    let prefix = format!("  crux {command}");
    let lines = HELP_TEXT
        .lines()
        .filter(|line| {
            line.strip_prefix(&prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
        })
        .collect::<Vec<_>>();
    Some(format!("Usage:\n{}", lines.join("\n")))
}

/// Finds a `--help` or `-h` flag after a known subcommand.
/// The flag wins over all other subcommand arguments.
fn requested_subcommand_help(arguments: &[String]) -> Option<&str> {
    let (command, rest) = arguments.split_first()?;
    if !SUBCOMMANDS.contains(&command.as_str()) {
        return None;
    }
    rest.iter()
        .any(|argument| argument == "--help" || argument == "-h")
        .then_some(command.as_str())
}

fn print_subcommand_help(command: &str) -> Result<()> {
    if command == "architecture" {
        // The architecture module owns its detailed usage text.
        return crate::architecture::run(&["--help".to_string()]);
    }
    let usage = subcommand_usage(command).context("known subcommand has usage text")?;
    println!("{usage}");
    Ok(())
}

pub fn run() -> Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    // Subcommand help does not run the subcommand or read the MCP profile.
    if let Some(command) = requested_subcommand_help(&arguments) {
        return print_subcommand_help(command);
    }
    // Standalone commands do not use the MCP profile setting.
    if arguments
        .first()
        .is_some_and(|command| command == "prepare")
    {
        return crate::prepare::run(&arguments[1..]);
    }
    if arguments
        .first()
        .is_some_and(|command| command == "architecture")
    {
        return crate::architecture::run(&arguments[1..]);
    }
    let environment_profile = env::var("CRUX_PROFILE").ok();
    let (profile, arguments) = resolve_profile(arguments, environment_profile.as_deref())?;
    if let Some(command) = requested_subcommand_help(&arguments) {
        return print_subcommand_help(command);
    }
    match arguments.as_slice() {
        [command, arguments @ ..] if command == "architecture" => {
            crate::architecture::run(arguments)
        }
        [command, arguments @ ..] if command == "prepare" => crate::prepare::run(arguments),
        [command, arguments @ ..] if command == "census" => crate::prepare::census(arguments),
        [command] if command == "capabilities" => {
            println!("{}", crate::prepare::capabilities());
            Ok(())
        }
        [command, language, repo, output] if command == "__index-adapter" => {
            crate::prepare::index_adapter(language, Path::new(repo), Path::new(output), None)
        }
        [command, language, repo, output, limit] if command == "__index-adapter" => {
            crate::prepare::index_adapter(
                language,
                Path::new(repo),
                Path::new(output),
                Some(limit.parse()?),
            )
        }
        [command, flag, path] if command == "check" && flag == "--index" => {
            println!("{}", crate::prepare::validate(Path::new(path), 1)?);
            Ok(())
        }
        [] => run_stdio(profile),
        [flag] if flag == "--help" || flag == "-h" => {
            println!("{HELP_TEXT}");
            Ok(())
        }
        [flag] if flag == "--version" || flag == "-V" => {
            println!("crux {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [command] if command == "self-update" => update::run_self_update(false),
        [command, flag] if command == "self-update" && flag == "--check" => {
            update::run_self_update(true)
        }
        [command, project_root] if command == "check" => {
            println!("{}", check_index(Path::new(project_root))?);
            Ok(())
        }
        [command, arguments @ ..] if command == "setup" || command == "unsetup" => {
            let (client, project) = setup::parse_client_arguments(arguments)?;
            if command == "setup" {
                setup::run_setup(client, project.as_deref())
            } else {
                setup::run_unsetup(client, project.as_deref())
            }
        }
        _ => {
            bail!("{HELP_TEXT}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use protobuf::Message;
    use scip::types::{Document, Index};
    use std::fs;

    fn write_test_index(project: &TestProject, index: &Index) {
        let bytes = index.write_to_bytes().expect("serialize test index");
        fs::write(project.root.join(".scip-nav/index.scip"), bytes).expect("write test index");
    }

    #[test]
    fn savings_ledger_records_successful_tool_calls_as_json_lines() {
        let project = TestProject::new();
        let ledger_path = project.root.join("ledger/savings.jsonl");
        let ledger = SavingsLedger {
            path: ledger_path.clone(),
        };

        ledger.record("scip_map", &"x".repeat(400));
        ledger.record("scip_find", &"y".repeat(40));
        // Tiny responses save nothing and write nothing.
        ledger.record("scip_find", "ok");

        let lines = fs::read_to_string(&ledger_path)
            .expect("ledger file should exist")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("ledger line should be JSON"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["tool"], "scip_map");
        assert_eq!(lines[0]["response_tokens"], 100);
        assert_eq!(lines[0]["saved_tokens"], 80);
        assert_eq!(lines[1]["saved_tokens"], 8);
        assert!(savings_eligible("scip_map"));
        assert!(!savings_eligible("scip_index"));
        assert!(!savings_eligible("scip_architecture"));
        assert!(!savings_eligible("scip_expand"));
    }

    #[test]
    fn empty_index_rejects_every_query_tool_and_warns_during_check() {
        let project = TestProject::new();
        write_test_index(&project, &Index::default());
        let expected =
            "index is empty (0 documents) — likely a crashed indexer; run scip_index to rebuild";
        let cases = vec![
            ("scip_architecture", json!({"project_root": &project.root})),
            (
                "scip_map",
                json!({"project_root": &project.root, "names": ["missing"]}),
            ),
            (
                "scip_find",
                json!({"project_root": &project.root, "name": "missing"}),
            ),
            (
                "scip_refs",
                json!({"project_root": &project.root, "name": "missing"}),
            ),
            (
                "scip_callers",
                json!({"project_root": &project.root, "name": "missing"}),
            ),
            (
                "scip_search",
                json!({"project_root": &project.root, "query": "missing"}),
            ),
            (
                "scip_outline",
                json!({"project_root": &project.root, "file": "missing.rs"}),
            ),
            ("scip_dead", json!({"project_root": &project.root})),
        ];
        let mut server = Server::new(Profile::Full);

        for (tool, arguments) in cases {
            let result = server.call_tool(tool, &arguments);
            assert!(result.is_error, "{tool}");
            assert_eq!(result.text, expected, "{tool}");
        }

        let check = check_index(&project.root).expect("check empty index");
        assert!(check.contains("documents 0 | symbols 0"));
        assert!(check.ends_with(&format!("warning: {expected}")));
    }

    #[test]
    fn symbol_less_index_is_rejected() {
        let project = TestProject::new();
        write_test_index(
            &project,
            &Index {
                documents: vec![Document {
                    relative_path: "src/empty.rs".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let mut server = Server::new(Profile::Full);
        let result = server.call_tool(
            "scip_find",
            &json!({"project_root": &project.root, "name": "missing"}),
        );

        assert!(result.is_error);
        assert_eq!(
            result.text,
            "index is empty (0 symbols) — likely a crashed indexer; run scip_index to rebuild"
        );
    }

    #[test]
    fn tool_schemas_match_dispatch_required_fields() {
        for tool in tool_definitions(true) {
            let name = tool.get("name").and_then(Value::as_str).expect("tool name");
            let schema_required = tool
                .pointer("/inputSchema/required")
                .and_then(Value::as_array)
                .map(|fields| fields.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();

            assert_eq!(schema_required, required_fields(name), "{name}");
        }
    }

    #[test]
    fn architecture_tool_is_in_slim_and_full_profiles() {
        for expanded in [false, true] {
            let tools = tool_definitions(expanded);
            let architecture = tools
                .iter()
                .find(|tool| tool["name"] == "scip_architecture")
                .expect("architecture tool");
            assert_eq!(
                architecture.pointer("/inputSchema/required/0"),
                Some(&json!("project_root"))
            );
            assert_eq!(
                architecture.pointer("/inputSchema/properties/direction/default"),
                Some(&json!("both"))
            );
        }
    }

    #[test]
    fn scip_find_dispatch_reports_unknown_field_and_required_fields() {
        let mut server = Server::default();
        let response = server
            .dispatch_line(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "scip_find",
                        "arguments": {"query": "x"}
                    }
                })
                .to_string(),
            )
            .expect("tool response");
        let response: Value = serde_json::from_str(&response).expect("valid response json");

        assert_eq!(
            response.pointer("/result/isError"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            response.pointer("/result/content/0/text"),
            Some(&json!("scip_find: invalid arguments (unknown field `query`, expected one of `project_root`, `name`, `limit`, `offset`, `unreferenced`). Required: project_root, name."))
        );
    }

    #[test]
    fn scip_find_dispatch_accepts_valid_arguments() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let response = server
            .dispatch_line(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "scip_find",
                        "arguments": {
                            "project_root": project.root,
                            "name": "formatDate"
                        }
                    }
                })
                .to_string(),
            )
            .expect("tool response");
        let response: Value = serde_json::from_str(&response).expect("valid response json");

        assert_eq!(
            response.pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert!(response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("formatDate")));
    }

    #[test]
    fn scip_index_empty_project_returns_auto_detection_hint() {
        let project = TestProject::new();
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_index",
            &json!({
                "project_root": project.root,
                "discover_depth": 0
            }),
        );

        assert!(result.is_error);
        assert_eq!(
            result.text,
            "could not auto-detect a supported language within 0 directory levels — add a project marker, raise discover_depth, or pass language (typescript|python|rust|dart|java|cpp|go)"
        );
    }

    #[test]
    fn scip_map_resolves_a_unique_bare_name_with_a_note() {
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
            "resolved formatDate → date.ts/formatDate().\n## date.ts/formatDate().\ndefinition: function src/lib/date.ts:42\nsignature: function formatDate(input: Date): string\nreferences:\nsrc/lib/date.ts:15: return formatDate(input);\nsrc/app.ts:40: const label = formatDate(today);\ncallers:\n<module> src/app.ts (x1)\nfunction formatDateLong src/lib/date.ts:10 (x1)\ncallers: <module> src/app.ts; formatDateLong\nfiles: src/app.ts; src/lib/date.ts"
        );
    }

    #[test]
    fn scip_map_auto_resolves_a_dominant_bare_name() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_map",
            &json!({
                "project_root": project.root,
                "names": ["rngState"]
            }),
        );

        assert!(!result.is_error);
        assert!(result
            .text
            .starts_with("resolved rngState → rng.ts/rngState().\n"));
        assert!(result
            .text
            .contains("other candidates: 1 (scip_find to list)"));
        assert!(result.text.contains("## rng.ts/rngState()."));
        assert!(!result.text.contains("## ambiguous:"));
    }

    #[test]
    fn scip_map_returns_a_find_hint_when_no_bare_name_matches() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_map",
            &json!({
                "project_root": project.root,
                "names": ["formatDateLo"]
            }),
        );

        assert!(!result.is_error);
        assert_eq!(
            result.text,
            "no symbol named formatDateLo; try scip_find with a fragment"
        );
    }

    #[test]
    fn scip_map_answers_mixed_bare_and_qualified_names() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_map",
            &json!({
                "project_root": project.root,
                "names": ["formatDate", "rng.ts/rngState()."]
            }),
        );

        assert!(!result.is_error);
        assert!(result
            .text
            .starts_with("resolved formatDate → date.ts/formatDate().\n"));
        assert_eq!(result.text.matches("resolved ").count(), 1);
        assert!(result.text.contains("## date.ts/formatDate()."));
        assert!(result.text.contains("## rng.ts/rngState()."));
    }

    #[test]
    fn scip_map_filters_imports_and_resolves_direct_callers() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_map",
            &json!({
                "project_root": project.root,
                "names": ["date.ts/formatDate()."]
            }),
        );

        assert!(!result.is_error);
        assert!(!result.text.contains("src/app.ts:8:"));
        assert!(!result.text.contains("src/app.ts:12:"));
        assert!(result.text.contains("<module> src/app.ts (x1)"));
        assert!(result
            .text
            .contains("function formatDateLong src/lib/date.ts:10 (x1)"));
    }

    #[test]
    fn scip_find_unreferenced_returns_only_zero_inbound_symbols() {
        let project = TestProject::new();
        write_fixture(&project, false);
        let mut server = Server::default();
        let result = server.call_tool(
            "scip_find",
            &json!({
                "project_root": project.root,
                "name": "*",
                "unreferenced": true
            }),
        );
        assert!(!result.is_error);
        let output = result.text;
        assert!(output.contains("dead.ts/neverUsed(). | function | src/lib/dead.ts:11"));
        assert!(!output.contains("localOnly"));
        assert!(!output.contains("parameter"));
        assert!(!output.contains(" input "));
    }

    #[test]
    fn json_rpc_smoke_runs_initialize_list_find_and_map() {
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
                    "name": "scip_find",
                    "arguments": {
                        "project_root": root,
                        "name": "formatDate"
                    }
                }
            })
            .to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "scip_map",
                    "arguments": {
                        "project_root": root,
                        "names": ["date.ts/formatDate()."]
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");

        let mut output = Vec::new();
        serve_stdio(std::io::Cursor::new(input), &mut output, Profile::Slim)
            .expect("stdio exchange");
        let output = String::from_utf8(output).expect("UTF-8 responses");
        let responses = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid response json"))
            .collect::<Vec<_>>();

        assert_eq!(responses.len(), 4);
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
        assert_eq!(tools.len(), 6);
        assert_eq!(
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            vec![
                "scip_index",
                "scip_architecture",
                "scip_find",
                "scip_map",
                "scip_outline",
                "scip_expand"
            ]
        );
        assert_eq!(
            responses[2].pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            responses[2].pointer("/result/content/0/text"),
            Some(&json!("date.ts/formatDate(). | function | src/lib/date.ts:42 | function formatDate(input: Date): string\ndate.ts/formatDateLong(). | function | src/lib/date.ts:10 | function formatDateLong(input: Date): string"))
        );
        assert_eq!(
            responses[3].pointer("/result/isError"),
            Some(&Value::Bool(false))
        );
        assert!(responses[3]
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("callers:")));
    }

    #[test]
    fn initialize_response_uses_profile_instructions() {
        let required_lead = "Use scip_architecture once for explicit architecture questions";
        for (profile, expansion_hint) in [(Profile::Slim, true), (Profile::Full, false)] {
            let mut server = Server::new(profile);
            let response = server
                .dispatch_line(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {}
                    })
                    .to_string(),
                )
                .expect("initialize response");
            let response: Value = serde_json::from_str(&response).expect("valid response json");
            let instructions = response
                .pointer("/result/instructions")
                .and_then(Value::as_str)
                .expect("instructions string");

            assert!(instructions.starts_with(required_lead));
            assert!(instructions.split_whitespace().count() < 135);
            assert!(instructions.contains("Pass a known project-relative file or directory"));
            assert!(instructions.contains("Reuse relevant architecture results"));
            assert!(instructions.contains("Use scip_map for named-symbol references and callers"));
            assert!(instructions.contains("Use scip_outline for one file's symbols"));
            assert!(instructions.contains("call it once more with ref_limit=200"));
            assert!(instructions.contains("Use scip_find for fragments"));
            assert!(instructions.contains("Prefer rg for simple text or definition search"));
            assert!(instructions.contains("Do not require architecture before every edit"));
            assert!(instructions.contains("stop equivalent retries"));
            assert!(instructions.contains("at most two index calls before answering"));
            assert_eq!(instructions.contains("Call scip_expand"), expansion_hint);
            assert_eq!(
                response.pointer("/result/capabilities/tools/listChanged"),
                Some(&json!(true))
            );
        }
    }

    #[test]
    fn tools_list_json_stays_within_each_profile_budget() {
        let slim = rpc_success(json!(1), json!({"tools": tool_definitions(false)}));
        let full = rpc_success(json!(1), json!({"tools": tool_definitions(true)}));

        // The compact schemas keep the persistent tool list small.
        // The margin is small: 4 bytes in slim and 11 bytes in full.
        // Trim a description before you add a schema field.
        assert!(
            slim.len() <= 3_500,
            "slim tools/list JSON is {} characters",
            slim.len()
        );
        assert!(
            full.len() <= 5_700,
            "full tools/list JSON is {} characters",
            full.len()
        );
        assert!(full.len() > slim.len());
    }

    #[test]
    fn scip_expand_reveals_narrow_tools_notifies_once_and_is_idempotent() {
        let mut server = Server::new(Profile::Slim);
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "scip_expand", "arguments": {}}
        })
        .to_string();

        let messages = server.dispatch_messages(&request);
        assert_eq!(messages.len(), 2);
        let response: Value = serde_json::from_str(&messages[0]).expect("expand response");
        assert_eq!(
            response.pointer("/result/content/0/text"),
            Some(&json!(EXPANDED_TOOLS))
        );
        let notification: Value =
            serde_json::from_str(&messages[1]).expect("list changed notification");
        assert_eq!(
            notification.get("method"),
            Some(&json!("notifications/tools/list_changed"))
        );
        assert!(notification.get("id").is_none());

        let tools = server
            .dispatch_line(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string())
            .expect("expanded tools list");
        let tools: Value = serde_json::from_str(&tools).expect("valid tools response");
        let names = tools
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 11);
        for name in [
            "scip_search",
            "scip_def",
            "scip_refs",
            "scip_callers",
            "scip_dead",
        ] {
            assert!(names.contains(&name));
        }

        let messages = server.dispatch_messages(&request);
        assert_eq!(messages.len(), 1);
        let response: Value = serde_json::from_str(&messages[0]).expect("second expand response");
        assert_eq!(
            response.pointer("/result/content/0/text"),
            Some(&json!("already expanded"))
        );
    }

    #[test]
    fn full_profile_advertises_all_tools_from_start() {
        let mut server = Server::new(Profile::Full);
        let response = server
            .dispatch_line(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string())
            .expect("tools response");
        let response: Value = serde_json::from_str(&response).expect("valid tools response");
        assert_eq!(
            response
                .pointer("/result/tools")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(11)
        );

        let messages = server.dispatch_messages(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "scip_expand", "arguments": {}}
            })
            .to_string(),
        );
        assert_eq!(messages.len(), 1);
        let response: Value = serde_json::from_str(&messages[0]).expect("expand response");
        assert_eq!(
            response.pointer("/result/content/0/text"),
            Some(&json!("already expanded"))
        );
    }

    #[test]
    fn profile_flag_overrides_environment_profile() {
        let (profile, arguments) = resolve_profile(
            vec!["--profile".to_string(), "slim".to_string()],
            Some("invalid"),
        )
        .expect("valid profiles");
        assert_eq!(profile, Profile::Slim);
        assert!(arguments.is_empty());

        let (profile, arguments) =
            resolve_profile(Vec::new(), Some("full")).expect("valid environment profile");
        assert_eq!(profile, Profile::Full);
        assert!(arguments.is_empty());
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
        for (tool, arguments) in [
            (
                "scip_find",
                json!({"project_root": project.root, "name": "format"}),
            ),
            ("scip_architecture", json!({"project_root": project.root})),
        ] {
            let result = server.call_tool(tool, &arguments);
            assert!(result.is_error, "{tool}");
            assert!(result.text.contains("call scip_index first"), "{tool}");
        }
        assert!(!project.root.join(".scip-nav/index.scip").exists());
        assert!(!project.root.join(".crux").exists());
    }

    #[test]
    fn architecture_dispatch_validates_arguments_and_direction() {
        let mut server = Server::default();
        for arguments in [
            json!({"project_root": "/tmp", "direction": "sideways"}),
            json!({"project_root": "/tmp", "unknown": true}),
            json!({"scope": "src"}),
        ] {
            let result = server.call_tool("scip_architecture", &arguments);
            assert!(result.is_error);
            assert!(result.text.contains("scip_architecture: invalid arguments"));
            assert!(result.text.contains("Required: project_root"));
        }
    }

    #[test]
    fn architecture_projection_cache_reuses_and_invalidates_with_the_index() {
        let project = TestProject::new();
        let other_project = TestProject::new();
        write_fixture(&project, false);
        write_fixture(&other_project, false);
        let mut server = Server::default();
        let overview = json!({"project_root": project.root});
        let other_overview = json!({"project_root": other_project.root});
        let scoped = json!({
            "project_root": project.root,
            "scope": "src/lib"
        });

        let first = server.call_tool("scip_architecture", &overview);
        let second = server.call_tool("scip_architecture", &scoped);
        assert!(!first.is_error);
        assert!(!second.is_error);
        assert_eq!(server.architecture_build_count, 1);

        let other = server.call_tool("scip_architecture", &other_overview);
        let original_again = server.call_tool("scip_architecture", &overview);
        assert!(!other.is_error);
        assert!(!original_again.is_error);
        assert_eq!(server.architecture_build_count, 2);

        std::thread::sleep(std::time::Duration::from_millis(20));
        write_fixture(&project, true);
        let refreshed = server.call_tool("scip_architecture", &overview);
        assert!(!refreshed.is_error);
        assert!(refreshed.text.contains("modules 7"));
        assert_eq!(server.architecture_build_count, 3);
    }

    #[test]
    fn absorbed_tool_names_are_not_callable() {
        let mut server = Server::default();
        for name in [
            "scip_search",
            "scip_def",
            "scip_refs",
            "scip_callers",
            "scip_dead",
        ] {
            let result = server.call_tool(name, &json!({}));
            assert!(result.is_error);
            assert_eq!(result.text, format!("unknown tool: {name}"));
        }
    }

    fn dispatch_json(server: &mut Server, line: &str) -> Value {
        let response = server.dispatch_line(line).expect("response line");
        serde_json::from_str(&response).expect("valid response json")
    }

    #[test]
    fn initialize_answers_only_a_supported_protocol_version() {
        let mut server = Server::default();
        for (requested, expected) in [
            (json!("9999-01-01"), "2025-06-18"),
            (json!("2024-11-05"), "2024-11-05"),
            (json!("2025-03-26"), "2025-03-26"),
            (json!("2025-06-18"), "2025-06-18"),
            (json!(20241105), "2024-11-05"),
            (Value::Null, "2024-11-05"),
        ] {
            let request = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": requested, "capabilities": {}}
            });
            let response = dispatch_json(&mut server, &request.to_string());
            assert_eq!(
                response.pointer("/result/protocolVersion"),
                Some(&json!(expected)),
                "requested {requested}"
            );
        }
    }

    #[test]
    fn json_rpc_batch_returns_one_array_and_skips_notifications() {
        let mut server = Server::default();
        let batch = json!([
            {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "ping"},
            42
        ]);
        let response = dispatch_json(&mut server, &batch.to_string());
        let responses = response.as_array().expect("batch response array");
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0]["id"], json!(1));
        assert!(responses[0].pointer("/result/tools").is_some());
        assert_eq!(responses[1]["id"], json!(2));
        assert_eq!(responses[1]["result"], json!({}));
        assert_eq!(responses[2]["id"], Value::Null);
        assert_eq!(responses[2].pointer("/error/code"), Some(&json!(-32600)));

        let notifications = json!([
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "method": "unknown-notification"}
        ]);
        assert!(server.dispatch_line(&notifications.to_string()).is_none());
    }

    #[test]
    fn json_rpc_rejects_empty_batches_and_non_object_requests() {
        let mut server = Server::default();
        for line in ["[]", "42", "null", "\"x\"", "true"] {
            let response = dispatch_json(&mut server, line);
            assert_eq!(response["id"], Value::Null, "{line}");
            assert_eq!(
                response.pointer("/error/code"),
                Some(&json!(-32600)),
                "{line}"
            );
        }
    }

    #[test]
    fn json_rpc_reports_parse_errors_missing_tool_names_and_answers_ping() {
        let mut server = Server::default();
        let response = dispatch_json(&mut server, "{\"jsonrpc\": \"2.0\", \"id\": 1,");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32700)));

        let response = dispatch_json(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {}}).to_string(),
        );
        assert_eq!(response["id"], json!(2));
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
        assert_eq!(
            response.pointer("/error/message"),
            Some(&json!("missing tool name"))
        );

        let response = dispatch_json(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}).to_string(),
        );
        assert_eq!(response["id"], json!(3));
        assert_eq!(response["result"], json!({}));
    }

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn savings_ledger_line_uses_one_write_call() {
        let mut writer = RecordingWriter::default();
        append_line(&mut writer, "{\"a\":1}".to_string()).expect("append line");
        assert_eq!(writer.writes, vec![b"{\"a\":1}\n".to_vec()]);
    }

    #[test]
    fn scip_expand_does_not_book_savings() {
        let project = TestProject::new();
        let ledger_path = project.root.join("ledger/savings.jsonl");
        let mut server = Server::new(Profile::Slim);
        server.savings_ledger = Some(SavingsLedger {
            path: ledger_path.clone(),
        });
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "scip_expand", "arguments": {}}
        })
        .to_string();
        for _ in 0..2 {
            let response = dispatch_json(&mut server, &request);
            assert_eq!(response.pointer("/result/isError"), Some(&json!(false)));
        }
        assert!(!ledger_path.exists());
    }

    #[test]
    fn tool_schemas_reject_unknown_fields_and_describe_inputs() {
        for tool in tool_definitions(true) {
            let name = tool["name"].as_str().expect("tool name");
            let schema = &tool["inputSchema"];
            let properties = schema["properties"].as_object().expect("properties");
            if properties.is_empty() {
                continue;
            }
            assert_eq!(schema["additionalProperties"], json!(false), "{name}");
            assert_eq!(
                properties["project_root"]["description"],
                json!("Absolute root."),
                "{name}"
            );
            for (field, property) in properties {
                if field == "limit" || field == "ref_limit" {
                    assert!(property.get("default").is_some(), "{name}.{field}");
                    assert_eq!(property["minimum"], json!(1), "{name}.{field}");
                    assert_eq!(property["maximum"], json!(MAX_LIMIT), "{name}.{field}");
                }
                if field == "offset" {
                    assert!(property["description"].is_string(), "{name}.{field}");
                }
            }
            let description = tool["description"].as_str().expect("tool description");
            assert!(
                description.starts_with(|first: char| first.is_uppercase()),
                "{name}"
            );
        }
    }

    #[test]
    fn scip_index_schema_lists_every_parsed_language() {
        let tools = tool_definitions(false);
        let index = tools
            .iter()
            .find(|tool| tool["name"] == "scip_index")
            .expect("scip_index tool");
        let properties = &index["inputSchema"]["properties"];
        assert_eq!(properties["language"]["enum"], json!(INDEX_LANGUAGES));
        assert_eq!(
            properties["languages"]["items"]["enum"],
            json!(INDEX_LANGUAGES)
        );

        let project = TestProject::new();
        let result = Server::default().call_tool(
            "scip_index",
            &json!({"project_root": project.root, "languages": ["notalanguage"]}),
        );
        assert!(result.is_error);
        assert!(
            result
                .text
                .ends_with(&format!("(expected {})", INDEX_LANGUAGES.join("|"))),
            "{}",
            result.text
        );
    }

    #[test]
    fn subcommand_help_flags_select_the_subcommand_usage() {
        let arguments = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            requested_subcommand_help(&arguments(&["prepare", "--help"])),
            Some("prepare")
        );
        assert_eq!(
            requested_subcommand_help(&arguments(&["setup", "claude", "-h"])),
            Some("setup")
        );
        assert_eq!(requested_subcommand_help(&arguments(&["--help"])), None);
        assert_eq!(
            requested_subcommand_help(&arguments(&["unknown", "--help"])),
            None
        );
        assert_eq!(
            requested_subcommand_help(&arguments(&["check", "/tmp/project"])),
            None
        );

        for command in SUBCOMMANDS {
            let usage = subcommand_usage(command).expect("usage text");
            let lines = usage.lines().collect::<Vec<_>>();
            assert_eq!(lines[0], "Usage:");
            assert!(lines.len() > 1, "{command}");
            for line in &lines[1..] {
                assert!(line.starts_with(&format!("  crux {command}")), "{line}");
            }
        }
        assert_eq!(
            subcommand_usage("check").expect("check usage"),
            "Usage:\n  crux check --index <file>\n  crux check <absolute-project-root>"
        );
        assert_eq!(
            subcommand_usage("setup").expect("setup usage"),
            "Usage:\n  crux setup <codex|claude> [--project <dir>]"
        );
    }
}

use crate::index::{run_indexer, IndexCache, IndexStats};
use crate::queries::{
    callers, dead_exports, definitions, find, map_symbols, outline, references, search,
    MAX_MAP_NAMES,
};
use crate::setup;
use crate::update;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const FULL_INSTRUCTIONS: &str = "Who calls X? What references X? What breaks if X changes? Is X dead code? — the index answers each in ONE call; grep answers them incompletely. Use scip_map directly with the symbol name; it resolves or lists candidates. Use scip_find for fragments, browsing, and unreferenced audits. Plain text search is cheaper for a simple 'where is X defined' question. If the index is missing or stale, call scip_index once. Make at most two index calls before answering.";
const SLIM_INSTRUCTIONS: &str = "Who calls X? What references X? What breaks if X changes? Is X dead code? — the index answers each in ONE call; grep answers them incompletely. Use scip_map directly with the symbol name; it resolves or lists candidates. Use scip_find for fragments, browsing, and unreferenced audits. Plain text search is cheaper for a simple 'where is X defined' question. If the index is missing or stale, call scip_index once. Make at most two index calls before answering. Need finer-grained tools? Call scip_expand.";
const EXPANDED_TOOLS: &str = "expanded: scip_search, scip_def, scip_refs, scip_callers, scip_dead";
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
pub(crate) const MAX_LIMIT: usize = 200;
const MAX_CALLER_DEPTH: usize = 3;
const HELP_TEXT: &str = "Usage:\n  crux [--profile slim|full]\n  crux [--profile slim|full] --version\n  crux self-update [--check]\n  crux check <absolute-project-root>\n  crux setup <codex|claude> [--project <dir>]\n  crux unsetup <codex|claude> [--project <dir>]";

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
    max_file_mb: Option<u64>,
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

fn default_true() -> bool {
    true
}

fn bounded_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_LIMIT)
}

fn required_fields(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "scip_index" | "scip_dead" => &["project_root"],
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
    profile: Profile,
    expanded: bool,
    list_changed_pending: bool,
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
            profile,
            expanded: profile == Profile::Full,
            list_changed_pending: false,
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
                run_indexer(
                    Path::new(&arguments.project_root),
                    arguments.language.as_deref(),
                    arguments.max_file_mb,
                    &mut self.cache,
                )
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
                rpc_success(id, self.call_tool(name, &arguments).into_json())
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
            "description": "Builds or refreshes the code index for a project root — call once if another tool reports a missing index.",
            "inputSchema": object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute root."
                    },
                    "language": {
                        "type": "string",
                        "description": "Optional language override."
                    },
                    "max_file_mb": {
                        "type": "integer",
                        "description": "TypeScript file limit in MB."
                    }
                }),
                required_fields("scip_index")
            )
        }),
        json!({
            "name": "scip_find",
            "description": "Answers 'which symbol matches this name or fragment?' — use to find or disambiguate symbols; set unreferenced=true to list dead symbols.",
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
                        "description": "Candidate limit."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0
                    },
                    "unreferenced": {
                        "type": "boolean",
                        "description": "Require no inbound references."
                    }
                }),
                required_fields("scip_find")
            )
        }),
        json!({
            "name": "scip_map",
            "description": "Answers 'who calls X?', 'what references X?', and 'where is X defined?' completely in ONE call — use before any text search for symbol questions.",
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
                        "description": "Symbol names, bare or qualified, up to eight."
                    },
                    "ref_limit": {
                        "type": "integer",
                        "default": DEFAULT_MAP_REF_LIMIT,
                        "description": "Per-symbol reference and caller limit."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0
                    }
                }),
                required_fields("scip_map")
            )
        }),
        json!({
            "name": "scip_outline",
            "description": "Answers 'what symbols are defined in this file?' — use for file structure at a glance.",
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
                        "minimum": 0
                    }
                }),
                required_fields("scip_outline")
            )
        }),
        json!({
            "name": "scip_expand",
            "description": "Need finer-grained search, definition, references, callers, or dead-code tools? Call this to reveal them.",
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
            "description": "Search symbol display names and return disambiguated definition locations.",
            "inputSchema": narrow_object_schema(
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
                required_fields("scip_search")
            )
        }),
        json!({
            "name": "scip_def",
            "description": "Find definition sites with signatures and source lines for one name; prefer scip_map for multi-symbol navigation.",
            "inputSchema": narrow_object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
                    },
                    "name": {"type": "string"}
                }),
                required_fields("scip_def")
            )
        }),
        json!({
            "name": "scip_refs",
            "description": "Find reference lines grouped by file for one name; prefer scip_map for source context.",
            "inputSchema": narrow_object_schema(
                json!({
                    "project_root": {
                        "type": "string",
                        "description": "Absolute project path"
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
                        "minimum": 0
                    }
                }),
                required_fields("scip_refs")
            )
        }),
        json!({
            "name": "scip_callers",
            "description": "who calls X (transitive) — resolve references to their enclosing functions or module",
            "inputSchema": narrow_object_schema(
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
                required_fields("scip_callers")
            )
        }),
        json!({
            "name": "scip_dead",
            "description": "find exports nothing uses — audit before deleting; separates fully unused from file-local symbols",
            "inputSchema": narrow_object_schema(
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
                required_fields("scip_dead")
            )
        }),
    ]
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required
    })
}

fn narrow_object_schema(properties: Value, required: &[&str]) -> Value {
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

pub fn run() -> Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let environment_profile = env::var("CRUX_PROFILE").ok();
    let (profile, arguments) = resolve_profile(arguments, environment_profile.as_deref())?;
    match arguments.as_slice() {
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
    fn empty_index_rejects_every_query_tool_and_warns_during_check() {
        let project = TestProject::new();
        write_test_index(&project, &Index::default());
        let expected =
            "index is empty (0 documents) — likely a crashed indexer; run scip_index to rebuild";
        let cases = vec![
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
        assert_eq!(tools.len(), 5);
        assert_eq!(
            tools
                .iter()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            vec![
                "scip_index",
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
        let required_lead = "Who calls X? What references X? What breaks if X changes? Is X dead code? — the index answers each in ONE call; grep answers them incompletely.";
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
            assert!(instructions.split_whitespace().count() < 120);
            assert!(instructions.contains("Use scip_map directly with the symbol name"));
            assert!(instructions
                .contains("Use scip_find for fragments, browsing, and unreferenced audits"));
            assert!(instructions.contains("Plain text search is cheaper"));
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

        // Trigger-worded SLIM descriptions spend more of both budgets to improve organic adoption.
        assert!(
            slim.len() <= 2_600,
            "slim tools/list JSON is {} characters",
            slim.len()
        );
        assert!(
            full.len() <= 4_600,
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
        assert_eq!(names.len(), 10);
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
            Some(10)
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
        let result = server.call_tool(
            "scip_find",
            &json!({
                "project_root": project.root,
                "name": "format"
            }),
        );
        assert!(result.is_error);
        assert!(result.text.contains("call scip_index first"));
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
}

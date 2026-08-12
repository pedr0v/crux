use crate::index::{run_indexer, IndexCache, IndexStats};
use crate::queries::{
    callers, dead_exports, definitions, map_bundle, outline, references, search, MAX_MAP_NAMES,
};
use crate::update;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_INSTRUCTIONS: &str = "crux serves a code index for this machine's projects. For code navigation (find a definition, list references, map callers, audit dead exports), call scip_map first — one call returns definitions, signatures, and reference sites with source lines for up to 8 symbols. Use scip_callers for call graphs and scip_outline for file structure. If a tool reports a missing index, call scip_index once with the project root. Prefer these tools over file reads and text search when the question is about symbols; fall back to reading files when you need full context or the index lacks a symbol (for example, closure-local functions).";
const DEFAULT_SEARCH_LIMIT: usize = 20;
pub(crate) const DEFAULT_REFS_LIMIT: usize = 50;
pub(crate) const DEFAULT_MAP_REFS_LIMIT: usize = 40;
pub(crate) const DEFAULT_CALLERS_LIMIT: usize = 40;
pub(crate) const DEFAULT_DEAD_LIMIT: usize = 100;
pub(crate) const MAX_LIMIT: usize = 200;
const MAX_CALLER_DEPTH: usize = 3;
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
                        "instructions": SERVER_INSTRUCTIONS,
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

pub fn run() -> Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => run_stdio(),
        [flag] if flag == "--version" || flag == "-V" => {
            println!("crux {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [command] if command == "self-update" => update::run_self_update(false),
        [command, flag] if command == "self-update" && flag == "--check" => {
            update::run_self_update(true)
        }
        [command, project_root] if command == "check" => {
            let mut cache = IndexCache::default();
            let loaded = cache.load(Path::new(project_root))?;
            println!("{}", IndexStats::from_loaded(&loaded).compact());
            Ok(())
        }
        _ => {
            bail!("usage: crux [--version | self-update [--check] | check <absolute-project-root>]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

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
    fn initialize_response_includes_non_empty_instructions() {
        let mut server = Server::default();
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

        assert!(!instructions.is_empty());
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
}

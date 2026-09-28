use protobuf::{EnumOrUnknown, Message};
use scip::types::{symbol_information, Document, Index, Occurrence, SymbolInformation, SymbolRole};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const API: &str = "rust-analyzer cargo eval 1.0.0 api().";
const SERVICE: &str = "rust-analyzer cargo eval 1.0.0 service().";
const DATABASE: &str = "rust-analyzer cargo eval 1.0.0 database().";
const ISOLATED: &str = "rust-analyzer cargo eval 1.0.0 isolated().";

struct ToolMeasurement {
    text: String,
    bytes: usize,
    latency: Duration,
}

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    calls: usize,
}

impl McpClient {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crux"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start crux MCP server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            calls: 0,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params
        });
        self.next_id += 1;
        writeln!(self.stdin, "{request}").expect("write MCP request");
        self.stdin.flush().expect("flush MCP request");
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .expect("read MCP response");
        serde_json::from_str(&response).expect("parse MCP response")
    }

    fn call(&mut self, name: &str, arguments: Value) -> ToolMeasurement {
        self.calls += 1;
        let started = Instant::now();
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let latency = started.elapsed();
        assert_eq!(response.pointer("/result/isError"), Some(&json!(false)));
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .expect("tool text")
            .to_string();
        ToolMeasurement {
            bytes: text.len(),
            text,
            latency,
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn occurrence(symbol: &str, line: i32, definition: bool) -> Occurrence {
    Occurrence {
        range: vec![line, 0, 8],
        symbol: symbol.to_string(),
        symbol_roles: if definition {
            SymbolRole::Definition as i32
        } else {
            SymbolRole::ReadAccess as i32
        },
        ..Default::default()
    }
}

fn information(symbol: &str, display_name: &str) -> SymbolInformation {
    SymbolInformation {
        symbol: symbol.to_string(),
        display_name: display_name.to_string(),
        kind: EnumOrUnknown::new(symbol_information::Kind::Function),
        ..Default::default()
    }
}

fn document(path: &str, definition: (&str, &str), references: &[&str]) -> Document {
    let mut occurrences = vec![occurrence(definition.0, 0, true)];
    occurrences.extend(
        references
            .iter()
            .enumerate()
            .map(|(line, symbol)| occurrence(symbol, i32::try_from(line + 1).unwrap(), false)),
    );
    Document {
        language: "rust".to_string(),
        relative_path: path.to_string(),
        occurrences,
        symbols: vec![information(definition.0, definition.1)],
        ..Default::default()
    }
}

fn write_fixture(root: &std::path::Path) {
    fs::create_dir_all(root.join(".scip-nav")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    let index = Index {
        documents: vec![
            document("src/api.rs", (API, "api"), &[SERVICE]),
            document("src/service.rs", (SERVICE, "service"), &[API, DATABASE]),
            document("src/database.rs", (DATABASE, "database"), &[]),
            document("src/isolated.rs", (ISOLATED, "isolated"), &[]),
        ],
        ..Default::default()
    };
    fs::write(
        root.join(".scip-nav/index.scip"),
        index.write_to_bytes().unwrap(),
    )
    .unwrap();
    fs::write(root.join("src/api.rs"), "fn api() {}\nservice();\n").unwrap();
    fs::write(
        root.join("src/service.rs"),
        "fn service() {}\napi();\ndatabase();\n",
    )
    .unwrap();
    fs::write(root.join("src/database.rs"), "fn database() {}\n").unwrap();
    fs::write(root.join("src/isolated.rs"), "fn isolated() {}\n").unwrap();
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn median_micros(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[test]
fn compare_compact_architecture_with_existing_navigation_tools() {
    let project = tempfile::tempdir().unwrap();
    write_fixture(project.path());
    let root = project.path().to_string_lossy().to_string();
    let mut client = McpClient::start();

    let initialize = client.request(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}}),
    );
    let instructions = initialize
        .pointer("/result/instructions")
        .and_then(Value::as_str)
        .expect("server instructions");
    assert!(instructions.contains("Use scip_architecture once for explicit architecture"));
    assert!(instructions.contains("Use scip_map for named-symbol references and callers"));
    assert!(instructions.contains("Use scip_outline for one file's symbols"));
    assert!(instructions.contains("Prefer rg for simple text or definition search"));

    let tool_list = client.request("tools/list", json!({}));
    let tools = tool_list
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tool list");
    let description = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .and_then(|tool| tool["description"].as_str())
            .expect("tool description")
    };
    assert!(description("scip_architecture").contains("Do not use for named symbols"));
    assert!(description("scip_map").contains("named-symbol reference and caller questions"));
    assert!(description("scip_outline").contains("symbols are defined in this file"));

    let architecture_overview = client.call(
        "scip_architecture",
        json!({"project_root": root, "limit": 20}),
    );
    let architecture_scope = client.call(
        "scip_architecture",
        json!({
            "project_root": root,
            "scope": "src/service.rs",
            "direction": "both",
            "limit": 20
        }),
    );
    let mut cached_latencies = Vec::new();
    for _ in 0..9 {
        cached_latencies.push(micros(
            client
                .call(
                    "scip_architecture",
                    json!({
                        "project_root": root,
                        "scope": "src/service.rs",
                        "direction": "both",
                        "limit": 20
                    }),
                )
                .latency,
        ));
    }

    let calls_before_baseline = client.calls;
    let mut outline_bytes = 0;
    for file in [
        "src/api.rs",
        "src/service.rs",
        "src/database.rs",
        "src/isolated.rs",
    ] {
        outline_bytes += client
            .call(
                "scip_outline",
                json!({"project_root": root, "file": file, "limit": 20}),
            )
            .bytes;
    }
    let baseline_map = client.call(
        "scip_map",
        json!({
            "project_root": root,
            "names": ["api", "service", "database"],
            "ref_limit": 20
        }),
    );
    let baseline_calls = client.calls - calls_before_baseline;
    let baseline_bytes = outline_bytes + baseline_map.bytes;

    assert!(architecture_overview.text.contains("modules 4"));
    assert!(architecture_overview
        .text
        .contains("direct module dependencies 3"));
    assert!(architecture_overview.text.contains("cycles 1"));
    assert!(architecture_overview.text.contains("isolated modules 1"));
    assert!(architecture_scope
        .text
        .contains("src/service.rs -> src/api.rs"));
    assert!(architecture_scope
        .text
        .contains("src/service.rs -> src/database.rs"));
    assert!(architecture_scope
        .text
        .contains("src/api.rs -> src/service.rs"));
    assert!(architecture_scope
        .text
        .contains("relevant dependency cycles:"));
    assert!(baseline_map.text.contains("src/api.rs"));
    assert!(baseline_map.text.contains("src/service.rs"));
    assert!(baseline_map.text.contains("src/database.rs"));
    assert!(!baseline_map.text.contains("dependency cycles"));

    let measurements = json!({
        "fixture_truth": {
            "modules": 4,
            "module_dependencies": 3,
            "cycles": 1,
            "isolated_modules": 1
        },
        "architecture_overview": {
            "calls": 1,
            "output_bytes": architecture_overview.bytes,
            "first_build_latency_us": micros(architecture_overview.latency),
            "truth_checks_passed": true
        },
        "architecture_scoped": {
            "calls": 1,
            "output_bytes": architecture_scope.bytes,
            "cached_latency_us": micros(architecture_scope.latency),
            "cached_repeat_median_latency_us": median_micros(&mut cached_latencies),
            "truth_checks_passed": true
        },
        "scripted_existing_tools_baseline": {
            "scripted_calls": baseline_calls,
            "output_bytes": baseline_bytes,
            "supports_manual_dependency_inference": true,
            "has_explicit_cycle_section": false,
            "minimum_calls_measured": false
        },
        "routing_contract": {
            "initialize_and_tool_description_checks_passed": true,
            "negative_cases": [
                {"question": "who calls service", "documented_tool": "scip_map"},
                {"question": "symbols in src/service.rs", "documented_tool": "scip_outline"},
                {"question": "find literal database", "documented_tool": "rg"}
            ]
        },
        "live_model_adoption_measured": false
    });
    println!(
        "ARCHITECTURE_NAVIGATION_EVAL={}",
        serde_json::to_string(&measurements).unwrap()
    );
}

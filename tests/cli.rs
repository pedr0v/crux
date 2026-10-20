use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const DEFINITION: &str = "rust-analyzer cargo fixture 1.0.0 start().";
const REFERENCE: &str = "rust-analyzer cargo fixture 1.0.0 target().";
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

fn fixture_index() -> Index {
    Index {
        documents: vec![
            Document {
                language: "rust".to_string(),
                relative_path: "src/start.rs".to_string(),
                occurrences: vec![occurrence(DEFINITION, true), occurrence(REFERENCE, false)],
                symbols: vec![SymbolInformation {
                    symbol: DEFINITION.to_string(),
                    display_name: "start".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            Document {
                language: "rust".to_string(),
                relative_path: "src/target.rs".to_string(),
                occurrences: vec![occurrence(REFERENCE, true)],
                symbols: vec![SymbolInformation {
                    symbol: REFERENCE.to_string(),
                    display_name: "target".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

fn write_fixture_index(path: &Path) -> u64 {
    let bytes = fixture_index().write_to_bytes().unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes.len() as u64
}

fn crux(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crux"))
        .args(arguments)
        .env_remove("CRUX_PROFILE")
        .env_remove("CRUX_SAVINGS_LEDGER")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_flags_print_the_package_version() {
    for flag in ["--version", "-V"] {
        let output = crux(&[flag]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            stdout(&output),
            format!("crux {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn top_level_help_prints_the_full_usage() {
    let output = crux(&["--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.starts_with("Usage:\n"));
    for command in SUBCOMMANDS {
        assert!(text.contains(&format!("  crux {command}")), "{command}");
    }
}

#[test]
fn every_subcommand_help_flag_prints_its_usage_and_exits_zero() {
    for command in SUBCOMMANDS {
        for flag in ["--help", "-h"] {
            let output = crux(&[command, flag]);
            assert!(
                output.status.success(),
                "crux {command} {flag}: {}",
                stderr(&output)
            );
            let text = stdout(&output);
            assert!(text.starts_with("Usage:"), "crux {command} {flag}: {text}");
            assert!(
                text.contains(&format!("crux {command}")),
                "crux {command} {flag}: {text}"
            );
            assert!(stderr(&output).is_empty(), "crux {command} {flag}");
        }
    }
}

#[test]
fn subcommand_help_wins_over_other_arguments() {
    let output = crux(&["setup", "claude", "--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Usage:\n  crux setup <codex|claude> [--project <dir>]\n"
    );

    let output = crux(&["--profile", "full", "check", "--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("crux check <absolute-project-root>"));
}

#[test]
fn check_reports_stats_for_a_project_index() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap();
    let size = write_fixture_index(&project.join(".scip-nav/index.scip"));

    let output = crux(&["check", project.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("documents 2 | symbols 2 | occurrences 3 | size {size} B\n")
    );
}

#[test]
fn check_rejects_a_relative_project_root() {
    let output = crux(&["check", "relative/project"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("project_root must be an absolute path"));
}

#[test]
fn check_index_validates_an_explicit_index_file() {
    let root = tempfile::tempdir().unwrap();
    let index = root.path().join("fixture.scip");
    let size = write_fixture_index(&index);

    let output = crux(&["check", "--index", index.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["size"], size);
    assert_eq!(
        report["summary"],
        format!("documents 2 | symbols 2 | occurrences 3 | size {size} B")
    );
}

#[test]
fn capabilities_prints_json_with_the_package_version() {
    let output = crux(&["capabilities"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let capabilities: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(capabilities["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(capabilities["prepare"], true);
    assert!(capabilities["languages"].is_array());
}

#[test]
fn unknown_subcommand_prints_usage_on_stderr_and_exits_one() {
    for arguments in [&["not-a-command"][..], &["not-a-command", "--help"][..]] {
        let output = crux(arguments);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(stdout(&output).is_empty(), "{arguments:?}");
        let error = stderr(&output);
        assert!(error.starts_with("error: Usage:"), "{error}");
        assert!(
            error.contains("crux check <absolute-project-root>"),
            "{error}"
        );
    }
}

#[test]
fn invalid_profile_flag_exits_one() {
    let output = crux(&["--profile", "huge"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("invalid profile 'huge'; expected slim or full"));
}

#[test]
fn stdio_server_negotiates_the_version_and_answers_batches() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crux"))
        .env_remove("CRUX_PROFILE")
        .env_remove("CRUX_SAVINGS_LEDGER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"9999-01-01","capabilities":{}}}"#,
        r#"[{"jsonrpc":"2.0","id":2,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":3,"method":"tools/list"}]"#,
        "42",
        "{not json",
    ]
    .join("\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));

    let responses = stdout(&output)
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
    let batch = responses[1].as_array().expect("batch response array");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0]["id"], 2);
    assert_eq!(batch[1]["id"], 3);
    assert!(batch[1]["result"]["tools"].is_array());
    assert_eq!(responses[2]["id"], Value::Null);
    assert_eq!(responses[2]["error"]["code"], -32600);
    assert_eq!(responses[3]["id"], Value::Null);
    assert_eq!(responses[3]["error"]["code"], -32700);
}

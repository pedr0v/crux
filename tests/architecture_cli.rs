use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};
use serde_json::Value;
use std::fs;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const DEFINITION: &str = "rust-analyzer cargo fixture 1.0.0 start().";
const REFERENCE: &str = "rust-analyzer cargo fixture 1.0.0 target().";

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

#[test]
fn architecture_cli_writes_all_outputs_from_an_explicit_index() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let output = root.path().join("output");
    let index = root.path().join("fixture.scip");
    fs::create_dir(&project).unwrap();
    fs::write(&index, fixture_index().write_to_bytes().unwrap()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_crux"))
        .arg("architecture")
        .arg(&project)
        .arg("--index")
        .arg(&index)
        .arg("--output")
        .arg(&output)
        .env("CRUX_PROFILE", "invalid-for-mcp")
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("2 modules, 1 dependencies"));
    for name in ["graph.html", "GRAPH_REPORT.md", "graph.json"] {
        assert!(output.join(name).is_file(), "{name}");
    }
    let graph: Value =
        serde_json::from_slice(&fs::read(output.join("graph.json")).unwrap()).unwrap();
    assert_eq!(graph["summary"]["module_dependency_count"], 1);
    assert_eq!(
        graph["generator"]["evidence"],
        "SCIP definition and reference occurrences"
    );
}

#[test]
fn architecture_cli_uses_the_local_index_by_default() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let output = root.path().join("output");
    fs::create_dir_all(project.join(".scip-nav")).unwrap();
    fs::write(
        project.join(".scip-nav/index.scip"),
        fixture_index().write_to_bytes().unwrap(),
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_crux"))
        .arg("architecture")
        .arg(&project)
        .arg("--output")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.join("graph.json").is_file());
}

#[cfg(unix)]
#[test]
fn architecture_cli_creates_a_missing_local_index() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let output = root.path().join("output");
    let fixture = root.path().join("fixture.scip");
    let bin = root.path().join("bin");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir(&bin).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    fs::write(project.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(&fixture, fixture_index().write_to_bytes().unwrap()).unwrap();
    let indexer = bin.join("rust-analyzer");
    fs::write(
        &indexer,
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output" ]; then
    shift
    cp "$ARCHITECTURE_FIXTURE_INDEX" "$1"
    exit 0
  fi
  shift
done
exit 2
"#,
    )
    .unwrap();
    fs::set_permissions(&indexer, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));

    let result = Command::new(env!("CARGO_BIN_EXE_crux"))
        .arg("architecture")
        .arg(&project)
        .arg("--output")
        .arg(&output)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("ARCHITECTURE_FIXTURE_INDEX", &fixture)
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(project.join(".scip-nav/index.scip").is_file());
    assert!(output.join("graph.json").is_file());
}

#[test]
fn architecture_cli_reports_an_action_for_a_missing_explicit_index() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing.scip");
    let result = Command::new(env!("CARGO_BIN_EXE_crux"))
        .arg("architecture")
        .arg(root.path())
        .arg("--index")
        .arg(&missing)
        .output()
        .unwrap();

    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("index not found"));
    assert!(error.contains("omit --index"));
    assert!(error.contains(".scip-nav/index.scip"));
}

#[test]
fn top_level_help_lists_the_architecture_command() {
    let result = Command::new(env!("CARGO_BIN_EXE_crux"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout)
        .contains("crux architecture [project-dir] [--index <file>] [--output <directory>]"));
}

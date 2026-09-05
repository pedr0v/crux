#![cfg(unix)]

use protobuf::Message;
use scip::types::{Document, Index, SymbolInformation};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

struct Fixture {
    _root: tempfile::TempDir,
    repo: std::path::PathBuf,
    output: std::path::PathBuf,
    bin: std::path::PathBuf,
    index: std::path::PathBuf,
    calls: std::path::PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let bin = root.path().join("bin");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "Create fixture"]);
        let index = root.path().join("fixture.scip");
        let fixture = Index {
            documents: vec![Document {
                relative_path: "src/lib.rs".into(),
                language: "rust".into(),
                symbols: vec![SymbolInformation {
                    symbol: "rust-analyzer cargo fixture 0.1.0 fixture().".into(),
                    display_name: "fixture".into(),
                    documentation: vec![
                        "Fixture documentation with enough bytes for the minimum index size."
                            .into(),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        fs::write(&index, fixture.write_to_bytes().unwrap()).unwrap();
        let output = root.path().join("cache/index.scip");
        let calls = root.path().join("calls");
        let script = bin.join("rust-analyzer");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'rust-analyzer fixture-1'
  exit 0
fi
echo called >> "$FIXTURE_CALLS"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output" ]; then
    shift
    cp "$FIXTURE_INDEX" "$1"
    exit "${FIXTURE_EXIT:-0}"
  fi
  shift
done
exit 2
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _root: root,
            repo,
            output,
            bin,
            index,
            calls,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crux"));
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        command
            .args(["prepare", "--repo"])
            .arg(&self.repo)
            .arg("--output")
            .arg(&self.output)
            .args(["--format", "json"])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("FIXTURE_INDEX", &self.index)
            .env("FIXTURE_CALLS", &self.calls);
        command
    }

    fn ready(&self) -> Value {
        let output = self.command().output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        report(&output)
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

#[test]
fn cli_prepares_validates_and_reuses_an_exact_revision() {
    let fixture = Fixture::new();
    let result = fixture
        .command()
        .env("CRUX_PROFILE", "invalid-for-mcp")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let first = report(&result);
    assert_eq!(first["status"], "ready");
    assert_eq!(first["model_ready"], true);
    assert_eq!(first["cache_hit"], false);
    assert_eq!(fixture.ready()["cache_hit"], true);
    assert_eq!(
        fs::read_to_string(&fixture.calls).unwrap().lines().count(),
        1
    );
    let check = Command::new(env!("CARGO_BIN_EXE_crux"))
        .args(["check", "--index"])
        .arg(&fixture.output)
        .output()
        .unwrap();
    assert!(check.status.success());
    fs::write(fixture.repo.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "Change source"]);
    let changed = fixture.ready();
    assert_eq!(changed["cache_hit"], false);
    assert_ne!(first["cache_key"], changed["cache_key"]);
    assert_eq!(
        fs::read_to_string(&fixture.calls).unwrap().lines().count(),
        2
    );
    let script = fixture.bin.join("rust-analyzer");
    let updated = fs::read_to_string(&script)
        .unwrap()
        .replace("fixture-1", "fixture-2");
    fs::write(script, updated).unwrap();
    let upgraded = fixture.ready();
    assert_eq!(upgraded["cache_hit"], false);
    assert_ne!(changed["cache_key"], upgraded["cache_key"]);
}

#[test]
fn cli_rebuilds_corrupt_cache_and_rejects_failed_indexer_output() {
    let fixture = Fixture::new();
    fixture.ready();
    fs::write(&fixture.output, b"corrupt cache").unwrap();
    assert_eq!(fixture.ready()["cache_hit"], false);
    fs::remove_file(fixture.output.with_extension("scip.provenance.json")).unwrap();
    let previous = fs::read(&fixture.output).unwrap();
    let result = fixture
        .command()
        .env("FIXTURE_EXIT", "23")
        .output()
        .unwrap();
    assert!(!result.status.success());
    let report = report(&result);
    assert_eq!(report["error_code"], "indexer_failed");
    assert_eq!(report["model_ready"], false);
    assert_eq!(fs::read(&fixture.output).unwrap(), previous);
}

#[test]
fn cli_failure_prevents_a_following_model_command() {
    let fixture = Fixture::new();
    fs::write(fixture.repo.join("dirty.rs"), "// Changed source.\n").unwrap();
    let marker = fixture._root.path().join("model-started");
    let output = Command::new("sh")
        .arg("-c")
        .arg("\"$1\" prepare --repo \"$2\" --output \"$3\" --format json && touch \"$4\"")
        .arg("fixture")
        .arg(env!("CARGO_BIN_EXE_crux"))
        .arg(&fixture.repo)
        .arg(&fixture.output)
        .arg(&marker)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(report(&output)["error_code"], "dirty_repository");
    assert!(!marker.exists());
}

#[test]
fn cli_rejects_an_internal_output_without_creating_source_files() {
    let fixture = Fixture::new();
    let result = Command::new(env!("CARGO_BIN_EXE_crux"))
        .args(["prepare", "--repo"])
        .arg(&fixture.repo)
        .arg("--output")
        .arg(fixture.repo.join("new-cache/index.scip"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(report(&result)["error_code"], "output_inside_repository");
    assert!(!fixture.repo.join("new-cache").exists());
}

#[test]
fn inferred_typescript_configuration_has_stable_cache_provenance() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.repo.join("Cargo.toml")).unwrap();
    fs::remove_file(fixture.repo.join("src/lib.rs")).unwrap();
    fs::write(
        fixture.repo.join("package.json"),
        "{\"name\":\"fixture\",\"version\":\"1.0.0\"}",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("src/lib.ts"),
        "export const fixture = 1;\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "-A"]);
    git(
        &fixture.repo,
        &["commit", "-qm", "Use TypeScript without a root config"],
    );
    let mut index = Index::parse_from_bytes(&fs::read(&fixture.index).unwrap()).unwrap();
    index.documents[0].relative_path = "src/lib.ts".into();
    index.documents[0].language = "typescript".into();
    fs::write(&fixture.index, index.write_to_bytes().unwrap()).unwrap();
    for program in ["node", "npx"] {
        let script = fixture.bin.join(program);
        fs::copy(fixture.bin.join("rust-analyzer"), &script).unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let first = fixture.ready();
    assert_eq!(first["cache_hit"], false);
    assert_eq!(first["inferred_configs"].as_array().unwrap().len(), 1);
    let second = fixture.ready();
    assert_eq!(second["cache_hit"], true);
    assert_eq!(first["cache_key"], second["cache_key"]);
    assert_eq!(
        fs::read_to_string(&fixture.calls).unwrap().lines().count(),
        1
    );
    assert!(!fixture.repo.join("tsconfig.json").exists());
}

#[test]
fn unsupported_build_is_structured_and_does_not_run_an_indexer() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.repo.join("Cargo.toml")).unwrap();
    fs::write(
        fixture.repo.join("build.xml"),
        "<project name=\"ant-fixture\"/>\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "-A"]);
    git(
        &fixture.repo,
        &["commit", "-qm", "Use an unsupported Java build"],
    );
    let result = fixture.command().output().unwrap();
    assert!(!result.status.success());
    assert_eq!(report(&result)["error_code"], "unsupported_build");
    assert_eq!(report(&result)["model_ready"], false);
    assert!(!fixture.calls.exists());
}

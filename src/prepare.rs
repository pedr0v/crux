use crate::index::{self, IndexStats};
use crate::{prepare_java, prepare_languages};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use protobuf::{Message, MessageField};
use scip::types::{Index, Metadata};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const MIN_INDEX_BYTES: u64 = 100;
const LANGUAGES: &[&str] = &["typescript", "python", "rust", "dart", "java", "cpp", "go"];

#[derive(Debug)]
struct Options {
    repo: PathBuf,
    output: PathBuf,
    min_bytes: u64,
    depth: u32,
    language: Option<String>,
}

fn options(arguments: &[String]) -> Result<Options> {
    let mut result = Options {
        repo: std::env::current_dir()?,
        output: PathBuf::new(),
        min_bytes: MIN_INDEX_BYTES,
        depth: 3,
        language: None,
    };
    let mut args = arguments.iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("invalid_arguments: {flag} requires a value"))?;
        match flag.as_str() {
            "--repo" => result.repo = value.into(),
            "--output" => result.output = value.into(),
            "--format" if value == "json" => {}
            "--min-index-bytes" => {
                result.min_bytes = value.parse().context("invalid_arguments: minimum size")?
            }
            "--discover-depth" => {
                result.depth = value
                    .parse()
                    .context("invalid_arguments: discovery depth")?
            }
            "--language" if LANGUAGES.contains(&value.as_str()) => {
                result.language = Some(value.clone())
            }
            "--language" => bail!("unsupported_language: {value}"),
            _ => bail!("invalid_arguments: unknown option {flag} {value}"),
        }
    }
    if result.output.as_os_str().is_empty() || result.min_bytes == 0 {
        bail!("invalid_arguments: --output and a positive minimum size are required");
    }
    Ok(result)
}

pub(crate) fn run(arguments: &[String]) -> Result<()> {
    let mut report = json!({
        "status":"error", "error_code":null, "cache_hit":false,
        "detected_languages":[], "build_tool":[], "repository_revision":null,
        "index_path":null, "index_size":null, "index_sha256":null,
        "indexers":[], "validation":{"passed":false}, "model_ready":false,
        "skipped_included_builds":[], "inferred_configs":[],
    });
    let result = options(arguments).and_then(|options| prepare(&options, &mut report));
    if let Err(error) = &result {
        let message = format!("{error:#}");
        report["error_code"] = json!(error_code(&message));
        report["message"] = json!(message);
    }
    println!("{}", serde_json::to_string(&report)?);
    result
}

fn error_code(message: &str) -> &'static str {
    [
        "unsupported_language",
        "unsupported_build",
        "included_build_incompatible",
        "typescript_config_missing",
        "indexer_unavailable",
        "runtime_unavailable",
        "indexer_version_mismatch",
        "invalid_arguments",
        "dirty_repository",
        "repository_changed",
        "invalid_index",
        "index_too_small",
        "index_missing",
        "indexer_failed",
        "repository_unavailable",
        "output_inside_repository",
    ]
    .into_iter()
    .find(|code| message.contains(code))
    .unwrap_or("preparation_failed")
}

fn checked_output(repo: &Path, program: &str, arguments: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(repo)
        .output()
        .with_context(|| format!("runtime_unavailable: {program}"))?;
    if !output.status.success() {
        bail!(
            "runtime_unavailable: {program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .into())
}

fn repository_state(repo: &Path) -> Result<Value> {
    let canonical = repo.canonicalize().context("repository_unavailable")?;
    let repo = canonical.as_path();
    let root = checked_output(repo, "git", &["rev-parse", "--show-toplevel"])
        .context("repository_unavailable")?;
    if fs::canonicalize(root)? != repo {
        bail!("repository_unavailable: --repo must name the Git root");
    }
    let revision = checked_output(repo, "git", &["rev-parse", "HEAD"])?;
    let status = checked_output(
        repo,
        "git",
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if !status.is_empty() {
        bail!("dirty_repository: commit or remove source changes before cache preparation");
    }
    let submodules = checked_output(repo, "git", &["submodule", "status", "--recursive"])?;
    if submodules
        .lines()
        .any(|line| line.starts_with(['-', '+', 'U']))
    {
        bail!("dirty_repository: submodules must match the recorded revision");
    }
    let remote =
        checked_output(repo, "git", &["config", "--get", "remote.origin.url"]).unwrap_or_default();
    Ok(
        json!({"identity":format!("{:x}",Sha256::digest(remote.as_bytes())), "path":repo, "revision":revision, "submodules":submodules}),
    )
}

#[derive(Serialize)]
#[serde(tag = "adapter")]
enum Plan {
    Java(prepare_java::JavaPlan),
    Language(prepare_languages::LanguagePlan),
    Existing {
        language: String,
        build_tool: String,
        build_tool_version: String,
        indexer: String,
        program: String,
        arguments: Vec<String>,
        indexer_version: String,
        runtime_version: String,
    },
}

fn make_plan(language: &str, repo: &Path, scratch: &Path, output: &Path) -> Result<Plan> {
    match language {
        "java" => Ok(Plan::Java(prepare_java::plan(repo, scratch, output)?)),
        "go" | "typescript" => Ok(Plan::Language(prepare_languages::plan(
            repo, scratch, language, output,
        )?)),
        _ => {
            let (program, arguments) = index::preparation_command(language, repo, output)?;
            let (indexer_version, runtime_version) = match language {
                "python" => (
                    "0.6.6".into(),
                    format!(
                        "{}; {}",
                        checked_output(repo, "node", &["--version"])?,
                        checked_output(repo, "python3", &["--version"])?
                    ),
                ),
                "rust" => (
                    checked_output(repo, "rust-analyzer", &["--version"])?,
                    checked_output(repo, "rustc", &["--version", "--verbose"])?,
                ),
                "dart" => (
                    checked_output(repo, "dart", &["pub", "global", "list"])?,
                    checked_output(repo, "dart", &["--version"])?,
                ),
                "cpp" => (
                    checked_output(repo, "scip-clang", &["--version"])?,
                    checked_output(repo, "clang", &["--version"])?,
                ),
                _ => bail!("unsupported_language: {language}"),
            };
            let (build_tool, build_tool_version, indexer) = match language {
                "python" => (
                    "npx",
                    checked_output(repo, "npx", &["--version"])?,
                    "scip-python",
                ),
                "rust" => (
                    "cargo",
                    checked_output(repo, "cargo", &["--version"])?,
                    "rust-analyzer",
                ),
                "dart" => ("pub", runtime_version.clone(), "scip_dart"),
                "cpp" => (
                    "compilation-database",
                    indexer_version.clone(),
                    "scip-clang",
                ),
                _ => unreachable!(),
            };
            Ok(Plan::Existing {
                language: language.into(),
                build_tool: build_tool.into(),
                build_tool_version,
                indexer: indexer.into(),
                program,
                arguments,
                indexer_version,
                runtime_version,
            })
        }
    }
}

fn execute_plan(plan: &Plan, repo: &Path, scratch: &Path, output: &Path) -> Result<()> {
    match plan {
        Plan::Java(plan) => prepare_java::execute(repo, scratch, output, plan),
        Plan::Language(plan) => prepare_languages::execute(repo, scratch, output, plan),
        Plan::Existing {
            program, arguments, ..
        } => {
            let result = Command::new(program)
                .args(arguments)
                .current_dir(repo)
                .output()
                .with_context(|| format!("indexer_unavailable: {program}"))?;
            if !result.status.success() {
                bail!(
                    "indexer_failed: {program}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            Ok(())
        }
    }
}

pub(crate) fn index_adapter(
    language: &str,
    repo: &Path,
    output: &Path,
    max_file_mb: Option<u64>,
) -> Result<()> {
    let scratch = external_scratch(repo)?;
    let mut plan = make_plan(language, repo, scratch.path(), output)?;
    if let (Plan::Language(plan), Some(limit)) = (&mut plan, max_file_mb) {
        if language == "typescript" {
            let position = plan.arguments.len() - plan.project_inputs.len();
            plan.arguments.splice(
                position..position,
                ["--max-file-byte-size".into(), format!("{limit}mb")],
            );
        }
    }
    execute_plan(&plan, repo, scratch.path(), output)
}

fn external_scratch(repo: &Path) -> Result<tempfile::TempDir> {
    let repo = repo.canonicalize()?;
    let base = std::env::temp_dir().canonicalize()?;
    if base.starts_with(&repo) {
        bail!("output_inside_repository: temporary directory must be outside the repository");
    }
    Ok(tempfile::Builder::new()
        .prefix("crux-prepare-")
        .tempdir_in(base)?)
}

fn normalized_plan(plan: &Plan, scratch: &Path) -> Result<Value> {
    fn normalize(value: &mut Value, path: &str) {
        match value {
            Value::String(text) => *text = text.replace(path, "$SCRATCH"),
            Value::Array(values) => values.iter_mut().for_each(|value| normalize(value, path)),
            Value::Object(values) => values.values_mut().for_each(|value| normalize(value, path)),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(plan)?;
    if let Ok(canonical) = scratch.canonicalize() {
        normalize(&mut value, &canonical.to_string_lossy());
    }
    normalize(&mut value, &scratch.to_string_lossy());
    Ok(value)
}

fn build_inputs(repo: &Path, plan: &Value) -> Result<Value> {
    let mut roots = vec![repo.to_path_buf()];
    if let Some(inputs) = plan.get("project_inputs").and_then(Value::as_array) {
        for input in inputs.iter().filter_map(Value::as_str) {
            let path = repo.join(input);
            if path.is_dir() {
                roots.push(path);
            } else if path.is_file() {
                if let Some(parent) = path.parent() {
                    roots.push(parent.to_path_buf());
                }
            }
        }
    }
    let mut hashes = BTreeMap::new();
    for root in roots {
        for name in [
            "pyrightconfig.json",
            "pyproject.toml",
            "requirements.txt",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "pubspec.yaml",
            "pubspec.lock",
            "go.mod",
            "go.sum",
            "go.work",
            "go.work.sum",
            "package.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "bun.lock",
            "tsconfig.json",
            "jsconfig.json",
            "compile_commands.json",
            "build/compile_commands.json",
            "gradle.properties",
            "gradle/wrapper/gradle-wrapper.properties",
            ".mvn/maven.config",
            ".mvn/wrapper/maven-wrapper.properties",
        ] {
            let path = root.join(name);
            if path.is_file() {
                hashes.insert(
                    path.strip_prefix(repo)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                    hash_file(&path)?,
                );
            }
        }
    }
    Ok(json!(hashes))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn validate(path: &Path, min_bytes: u64) -> Result<Value> {
    let size = fs::metadata(path).context("index_missing")?.len();
    if size < min_bytes {
        bail!("index_too_small: {size} bytes, minimum {min_bytes}");
    }
    let loaded = index::load_uncached_index(path).context("invalid_index")?;
    let stats = IndexStats::from_loaded(&loaded);
    if let Some(message) = stats.empty_index_message() {
        bail!("invalid_index: {message}");
    }
    for document in &loaded.index.index().documents {
        let path = Path::new(&document.relative_path);
        if document.relative_path.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("invalid_index: document path escapes the repository");
        }
    }
    Ok(json!({"passed":true,"summary":stats.compact(),"size":size,"sha256":hash_file(path)?}))
}

fn manifest_path(output: &Path) -> PathBuf {
    output.with_extension("scip.provenance.json")
}

fn cache_validation(output: &Path, provenance: &Value, minimum: u64) -> Option<Value> {
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path(output)).ok()?).ok()?;
    if manifest["provenance"] != *provenance {
        return None;
    }
    let validation = validate(output, minimum).ok()?;
    if manifest["sha256"] != validation["sha256"] {
        return None;
    }
    Some(validation)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("output parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn prepare(options: &Options, report: &mut Value) -> Result<()> {
    let repo = options
        .repo
        .canonicalize()
        .context("repository_unavailable")?;
    let output = if options.output.is_absolute() {
        options.output.clone()
    } else {
        std::env::current_dir()?.join(&options.output)
    };
    let parent = output
        .parent()
        .context("invalid_arguments: output parent")?;
    let mut ancestor = parent;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .context("invalid_arguments: output parent")?;
    }
    if ancestor.canonicalize()?.starts_with(&repo) {
        bail!("output_inside_repository: choose an external cache directory");
    }
    fs::create_dir_all(parent)?;
    let output = parent.canonicalize()?.join(
        output
            .file_name()
            .context("invalid_arguments: output filename")?,
    );
    if output.starts_with(&repo) {
        bail!("output_inside_repository: choose an external cache directory");
    }
    report["index_path"] = json!(output);
    let lock_path = output.with_extension("scip.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    let state = repository_state(&repo)?;
    report["repository_revision"] = state["revision"].clone();
    let mut projects = index::preparation_projects(&repo, options.depth)?;
    report["detected_languages"] = json!(projects
        .iter()
        .map(|(language, _)| language)
        .collect::<std::collections::BTreeSet<_>>());
    if let Some(language) = &options.language {
        projects.retain(|(detected, _)| detected == language);
    }
    if projects.is_empty() {
        bail!("unsupported_language: no supported project markers found");
    }
    let scratch = external_scratch(&repo)?;
    let mut plans = Vec::new();
    let mut provenance_plans = Vec::new();
    for (number, (language, relative)) in projects.iter().enumerate() {
        let work = scratch.path().join(format!("project-{number}"));
        fs::create_dir_all(&work)?;
        let pending = work.join("index.scip");
        let plan = make_plan(language, &repo.join(relative), &work, &pending)?;
        let mut serialized = normalized_plan(&plan, scratch.path())?;
        serialized["build_inputs"] = build_inputs(&repo.join(relative), &serialized)?;
        if let Some(values) = serialized["inferred_configs"].as_array() {
            report["inferred_configs"]
                .as_array_mut()
                .expect("array")
                .extend(values.iter().cloned());
        }
        if let Some(tool) = serialized.get("build_tool") {
            report["build_tool"]
                .as_array_mut()
                .expect("array")
                .push(tool.clone());
        }
        provenance_plans.push(json!({"language":language,"root":relative,"plan":serialized}));
        plans.push((plan, work, pending));
    }
    report["indexers"] = json!(provenance_plans);
    let environment = [
        "JAVA_HOME",
        "JAVA_TOOL_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "MAVEN_OPTS",
        "GRADLE_OPTS",
        "GOFLAGS",
        "GOTOOLCHAIN",
        "GOOS",
        "GOARCH",
        "CGO_ENABLED",
        "GOPROXY",
        "GONOSUMDB",
        "GOPRIVATE",
        "NODE_OPTIONS",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
        "CC",
        "CXX",
        "CFLAGS",
        "CXXFLAGS",
    ]
    .into_iter()
    .map(|key| {
        (
            key,
            std::env::var(key)
                .ok()
                .map(|value| format!("{:x}", Sha256::digest(value.as_bytes()))),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let provenance = json!({"schema":1,"repository":state,"crux_version":env!("CARGO_PKG_VERSION"),
        "platform":{"os":std::env::consts::OS,"architecture":std::env::consts::ARCH},
        "plans":provenance_plans,"environment":environment,"minimum_bytes":options.min_bytes,"discover_depth":options.depth});
    report["cache_key"] = json!(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&provenance)?)
    ));
    if let Some(validation) = cache_validation(&output, &provenance, options.min_bytes) {
        if repository_state(&repo)? != state {
            bail!("repository_changed: repository changed during validation");
        }
        let manifest: Value = serde_json::from_slice(&fs::read(manifest_path(&output))?)?;
        if let Some(skipped) = manifest.get("skipped_included_builds") {
            report["skipped_included_builds"] = skipped.clone();
        }
        finish_report(report, validation, true);
        return Ok(());
    }
    let mut merged = Index {
        metadata: MessageField::some(Metadata {
            project_root: format!("file://{}", repo.display()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut documents = HashSet::new();
    let mut symbols = HashSet::new();
    for ((_, relative), (plan, work, pending)) in projects.iter().zip(&plans) {
        let execution = execute_plan(plan, &repo.join(relative), work, pending);
        if matches!(plan, Plan::Java(_)) {
            let skipped = prepare_java::skipped_builds(&repo.join(relative), work)?;
            report["skipped_included_builds"]
                .as_array_mut()
                .expect("array")
                .extend(skipped.into_iter().map(|path| json!(relative.join(path))));
        }
        execution?;
        validate(pending, options.min_bytes)?;
        let mut index = Index::parse_from_bytes(&fs::read(pending)?)?;
        for mut document in index.documents.drain(..) {
            document.relative_path = relative
                .join(&document.relative_path)
                .to_string_lossy()
                .replace('\\', "/");
            if documents.insert(document.relative_path.clone()) {
                merged.documents.push(document);
            }
        }
        for symbol in index.external_symbols.drain(..) {
            if symbols.insert(symbol.symbol.clone()) {
                merged.external_symbols.push(symbol);
            }
        }
    }
    let pending = scratch.path().join("merged.scip");
    let bytes = merged.write_to_bytes()?;
    fs::write(&pending, &bytes)?;
    let validation = validate(&pending, options.min_bytes)?;
    if repository_state(&repo).context("repository_changed")? != state {
        bail!("repository_changed: repository changed during indexing");
    }
    for ((_, relative), previous) in projects.iter().zip(&provenance_plans) {
        if build_inputs(&repo.join(relative), &previous["plan"])?
            != previous["plan"]["build_inputs"]
        {
            bail!("repository_changed: build inputs changed during indexing");
        }
    }
    atomic_write(&output, &bytes)?;
    atomic_write(
        &manifest_path(&output),
        &serde_json::to_vec_pretty(
            &json!({"provenance":provenance,"sha256":validation["sha256"],"skipped_included_builds":report["skipped_included_builds"]}),
        )?,
    )?;
    finish_report(report, validation, false);
    Ok(())
}

fn finish_report(report: &mut Value, validation: Value, cache_hit: bool) {
    report["status"] = json!("ready");
    report["cache_hit"] = json!(cache_hit);
    report["index_size"] = validation["size"].clone();
    report["index_sha256"] = validation["sha256"].clone();
    report["validation"] = validation;
    report["model_ready"] = json!(true);
}

pub(crate) fn capabilities() -> Value {
    json!({"version":env!("CARGO_PKG_VERSION"),"languages":LANGUAGES,
        "prepare":true,"census":true,"structured_errors":true,"exact_provenance_cache":true,
        "java_build_tools":["maven","gradle"],"unsupported_java_build_tools":["bazel","sbt","ant"]})
}

pub(crate) fn census(arguments: &[String]) -> Result<()> {
    let mut repo = std::env::current_dir()?;
    let mut args = arguments.iter();
    while let Some(flag) = args.next() {
        let value = args.next().context("census option requires a value")?;
        match flag.as_str() {
            "--repo" => repo = value.into(),
            "--format" if value == "json" => {}
            _ => bail!("unknown census option: {flag}"),
        }
    }
    let mut counts = BTreeMap::<String, (u64, u64)>::new();
    count_sources(&repo, &mut counts)?;
    let languages = counts.into_iter().map(|(language,(files,lines))| {
        json!({"supported":LANGUAGES.contains(&language.as_str()),"language":language,"files":files,"lines":lines})
    }).collect::<Vec<_>>();
    println!("{}", json!({"languages":languages}));
    Ok(())
}

fn count_sources(root: &Path, counts: &mut BTreeMap<String, (u64, u64)>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.')
                && ![
                    "node_modules",
                    "vendor",
                    "target",
                    "build",
                    "dist",
                    "venv",
                    "__pycache__",
                ]
                .contains(&name.as_ref())
            {
                count_sources(&path, counts)?;
            }
        } else if kind.is_file() {
            let language = match path.extension().and_then(|ext| ext.to_str()) {
                Some("go") => "go",
                Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => "typescript",
                Some("java" | "kt" | "scala") => "java",
                Some("rs") => "rust",
                Some("py") => "python",
                Some("dart") => "dart",
                Some("c" | "cc" | "cpp" | "h" | "hpp") => "cpp",
                Some("rb") => "ruby",
                Some("cs") => "csharp",
                Some("php") => "php",
                Some("swift") => "swift",
                _ => continue,
            };
            let bytes = fs::read(path)?;
            if language == "go"
                && String::from_utf8_lossy(&bytes).lines().any(|line| {
                    line.starts_with("// Code generated ") && line.ends_with(" DO NOT EDIT.")
                })
            {
                continue;
            }
            let count = counts.entry(language.into()).or_default();
            count.0 += 1;
            count.1 += bytes.iter().filter(|byte| **byte == b'\n').count() as u64
                + u64::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture, TestProject};

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

    fn committed_repo() -> TestProject {
        let project = TestProject::new();
        git(&project.root, &["init", "-q"]);
        git(
            &project.root,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&project.root, &["config", "user.name", "Fixture"]);
        fs::write(
            project.root.join("go.mod"),
            "module example.com/fixture\n\ngo 1.22\n",
        )
        .unwrap();
        git(&project.root, &["add", "go.mod"]);
        git(&project.root, &["commit", "-qm", "Create fixture"]);
        project
    }

    #[test]
    fn cache_requires_exact_provenance_and_index_hash() {
        let cache = tempfile::tempdir().unwrap();
        let output = cache.path().join("index.scip");
        let bytes = fixture(false).write_to_bytes().unwrap();
        atomic_write(&output, &bytes).unwrap();
        let provenance = json!({"revision":"one","jdk":"21","strategy":"primary-build"});
        atomic_write(
            &manifest_path(&output),
            &serde_json::to_vec(
                &json!({"provenance":provenance,"sha256":hash_file(&output).unwrap()}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(cache_validation(&output, &provenance, MIN_INDEX_BYTES).is_some());
        for changed in [
            json!({"revision":"two","jdk":"21","strategy":"primary-build"}),
            json!({"revision":"one","jdk":"22","strategy":"primary-build"}),
            json!({"revision":"one","jdk":"21","strategy":"all-builds"}),
        ] {
            assert!(cache_validation(&output, &changed, MIN_INDEX_BYTES).is_none());
        }
        fs::write(&output, fixture(true).write_to_bytes().unwrap()).unwrap();
        assert!(cache_validation(&output, &provenance, MIN_INDEX_BYTES).is_none());
    }

    #[test]
    fn validation_rejects_missing_small_corrupt_and_empty_indexes() {
        let cache = tempfile::tempdir().unwrap();
        let output = cache.path().join("index.scip");
        assert!(validate(&output, MIN_INDEX_BYTES)
            .unwrap_err()
            .to_string()
            .contains("index_missing"));
        fs::write(&output, b"tiny").unwrap();
        assert!(validate(&output, MIN_INDEX_BYTES)
            .unwrap_err()
            .to_string()
            .contains("index_too_small"));
        fs::write(&output, [255; 200]).unwrap();
        assert!(validate(&output, MIN_INDEX_BYTES)
            .unwrap_err()
            .to_string()
            .contains("invalid_index"));
        fs::write(&output, Index::default().write_to_bytes().unwrap()).unwrap();
        assert!(validate(&output, 0)
            .unwrap_err()
            .to_string()
            .contains("invalid_index"));
    }

    #[test]
    fn repository_provenance_rejects_dirty_and_different_revisions() {
        let project = committed_repo();
        let before = repository_state(&project.root).unwrap();
        fs::write(project.root.join("main.go"), "package fixture\n").unwrap();
        assert!(repository_state(&project.root)
            .unwrap_err()
            .to_string()
            .contains("dirty_repository"));
        git(&project.root, &["add", "main.go"]);
        git(&project.root, &["commit", "-qm", "Add source"]);
        assert_ne!(before, repository_state(&project.root).unwrap());
    }

    #[test]
    fn preparation_failure_preserves_existing_cache_and_never_reports_ready() {
        let project = committed_repo();
        fs::write(project.root.join("dirty.go"), "package fixture\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let output = cache.path().join("index.scip");
        fs::write(&output, b"previous index").unwrap();
        let options = Options {
            repo: project.root.clone(),
            output: output.clone(),
            min_bytes: 100,
            depth: 3,
            language: None,
        };
        let mut report = json!({"model_ready":false,"validation":{"passed":false}});
        assert!(prepare(&options, &mut report).is_err());
        assert_eq!(report["model_ready"], false);
        assert_eq!(fs::read(output).unwrap(), b"previous index");
    }

    #[test]
    fn census_counts_go_and_excludes_vendor_and_generated_sources() {
        let project = TestProject::new();
        fs::write(
            project.root.join("main.go"),
            "package main\nfunc main() {}\n",
        )
        .unwrap();
        fs::write(
            project.root.join("generated.go"),
            "// Code generated by fixture. DO NOT EDIT.\npackage main\n",
        )
        .unwrap();
        fs::create_dir(project.root.join("vendor")).unwrap();
        fs::write(project.root.join("vendor/dep.go"), "package dep\n").unwrap();
        let mut counts = BTreeMap::new();
        count_sources(&project.root, &mut counts).unwrap();
        assert_eq!(counts["go"], (1, 2));
        assert!(capabilities()["languages"]
            .as_array()
            .unwrap()
            .contains(&json!("go")));
    }

    #[test]
    fn scratch_paths_do_not_change_cache_provenance() {
        let plan = Plan::Existing {
            language: "rust".into(),
            build_tool: "cargo".into(),
            build_tool_version: "1".into(),
            indexer: "rust-analyzer".into(),
            program: "rust-analyzer".into(),
            arguments: vec!["--output".into(), "/tmp/one/index.scip".into()],
            indexer_version: "1".into(),
            runtime_version: "1".into(),
        };
        let serialized = normalized_plan(&plan, Path::new("/tmp/one")).unwrap();
        assert_eq!(serialized["arguments"][1], "$SCRATCH/index.scip");
    }
}

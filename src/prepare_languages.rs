use anyhow::{anyhow, bail, Context, Result};
use protobuf::Message;
use scip::types::Index;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

const SCIP_GO_VERSION: &str = "0.2.7";
const SCIP_GO_PACKAGE: &str = "github.com/scip-code/scip-go/cmd/scip-go@v0.2.7";
const SCIP_TYPESCRIPT_VERSION: &str = "0.4.0";
const SCIP_TYPESCRIPT_PACKAGE: &str = "@sourcegraph/scip-typescript@0.4.0";
const OUTPUT_PLACEHOLDER: &str = "$OUTPUT";
const REPO_PLACEHOLDER: &str = "$REPO";
const MODULE_ROOT_PLACEHOLDER: &str = "$MODULE_ROOT";

const IGNORED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".scip-nav",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
];

/// A deterministic description of one language preparation strategy.
///
/// Paths in `arguments` use placeholders for values which do not affect the
/// produced index. `project_inputs` and `inferred_configs` are serialized by
/// the caller and therefore form part of cache provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LanguagePlan {
    pub(crate) build_tool: String,
    pub(crate) build_tool_version: String,
    pub(crate) indexer: String,
    pub(crate) indexer_version: String,
    pub(crate) runtime_version: String,
    pub(crate) bootstrap_runtime_version: String,
    pub(crate) strategy: String,
    pub(crate) bootstrap_arguments: Vec<String>,
    pub(crate) arguments: Vec<String>,
    pub(crate) project_inputs: Vec<String>,
    pub(crate) inferred_configs: Vec<String>,
    pub(crate) inferred_config_contents: Vec<String>,
    pub(crate) excluded_paths: Vec<String>,
    pub(crate) environment: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GoWorkInfo {
    #[serde(rename = "Use", default)]
    uses: Vec<GoWorkUse>,
    #[serde(rename = "Replace", default)]
    replacements: Vec<GoWorkReplace>,
}

#[derive(Debug, Deserialize)]
struct GoWorkUse {
    #[serde(rename = "DiskPath")]
    disk_path: String,
}

#[derive(Debug, Deserialize)]
struct GoWorkReplace {
    #[serde(rename = "Old")]
    old: GoModule,
    #[serde(rename = "New")]
    new: GoModule,
}

#[derive(Debug, Deserialize)]
struct GoModule {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Version", default)]
    version: String,
}

pub(crate) fn plan(
    repo: &Path,
    scratch: &Path,
    language: &str,
    _output: &Path,
) -> Result<LanguagePlan> {
    let repo = canonical_directory(repo, "repository")?;
    let scratch = external_scratch(&repo, scratch)?;

    match language {
        "go" => plan_go(&repo, &scratch),
        "typescript" | "javascript" => plan_typescript(&repo, &scratch),
        other => bail!("unsupported_language: no preparation adapter for {other}"),
    }
}

pub(crate) fn execute(
    repo: &Path,
    scratch: &Path,
    output: &Path,
    plan: &LanguagePlan,
) -> Result<()> {
    let repo = canonical_directory(repo, "repository")?;
    let scratch = external_scratch(&repo, scratch)?;

    match (plan.indexer.as_str(), plan.indexer_version.as_str()) {
        ("scip-go", SCIP_GO_VERSION) => execute_go(&repo, &scratch, output, plan),
        ("@sourcegraph/scip-typescript", SCIP_TYPESCRIPT_VERSION) => {
            execute_typescript(&repo, &scratch, output, plan)
        }
        (indexer, version) => {
            bail!("unsupported_build: adapter cannot execute indexer {indexer} version {version}")
        }
    }
}

fn plan_go(repo: &Path, scratch: &Path) -> Result<LanguagePlan> {
    let go_version = command_version("go", &["version"], "Go runtime", repo)?;
    let bootstrap_go_version = command_version("go", &["version"], "Go runtime", scratch)?;
    let bootstrap_toolchain = go_toolchain_name(&bootstrap_go_version)?;
    let (strategy, module_roots) = go_module_roots(repo)?;
    let inherited_go_flags = env::var("GOFLAGS").unwrap_or_default();
    validate_go_flags(&inherited_go_flags)?;
    let go_work = if strategy == "go-work" {
        path_argument(&scratch.join("go.work"))
    } else {
        "off".to_string()
    };
    let mut environment = vec![format!("bootstrap:GOTOOLCHAIN={bootstrap_toolchain}")];
    environment.extend(module_roots.iter().map(|module| {
        let mode_root = if strategy == "go-work" {
            repo.to_path_buf()
        } else {
            repo.join(module)
        };
        let flags = target_go_flags(&inherited_go_flags, &mode_root);
        format!("{module}:GOWORK={go_work};GOFLAGS={flags}")
    }));

    Ok(LanguagePlan {
        build_tool: "go".to_string(),
        build_tool_version: go_version.clone(),
        indexer: "scip-go".to_string(),
        indexer_version: SCIP_GO_VERSION.to_string(),
        runtime_version: go_version,
        bootstrap_runtime_version: bootstrap_go_version,
        strategy,
        bootstrap_arguments: vec!["install".to_string(), SCIP_GO_PACKAGE.to_string()],
        arguments: vec![
            "index".to_string(),
            "--quiet".to_string(),
            "--module-root".to_string(),
            MODULE_ROOT_PLACEHOLDER.to_string(),
            "--output".to_string(),
            OUTPUT_PLACEHOLDER.to_string(),
        ],
        project_inputs: module_roots,
        inferred_configs: Vec::new(),
        inferred_config_contents: Vec::new(),
        excluded_paths: vec![
            "**/vendor/**".to_string(),
            "Go generated source marker: // Code generated ... DO NOT EDIT.".to_string(),
        ],
        environment,
    })
}

fn plan_typescript(repo: &Path, scratch: &Path) -> Result<LanguagePlan> {
    let node_version = command_version("node", &["--version"], "Node.js runtime", repo)?;
    let npx_version = command_version("npx", &["--version"], "npx", repo)?;
    let mut configs = discover_typescript_configs(repo)?;
    let mut inferred_configs = Vec::new();
    let mut inferred_config_contents = Vec::new();

    let strategy = if configs.is_empty() {
        let inferred = scratch.join("typescript-inferred-tsconfig.json");
        let contents = write_inferred_typescript_config(repo, &inferred)?;
        configs.push(path_argument(&inferred));
        inferred_configs.push(path_argument(&inferred));
        inferred_config_contents.push(contents);
        "external-inferred-config".to_string()
    } else if configs.len() == 1 && configs[0] == "tsconfig.json" {
        "root-config".to_string()
    } else {
        "nested-configs".to_string()
    };

    let mut arguments = vec![
        "--yes".to_string(),
        SCIP_TYPESCRIPT_PACKAGE.to_string(),
        "index".to_string(),
        "--cwd".to_string(),
        REPO_PLACEHOLDER.to_string(),
        "--output".to_string(),
        OUTPUT_PLACEHOLDER.to_string(),
        "--no-progress-bar".to_string(),
    ];
    arguments.extend(configs.iter().cloned());

    Ok(LanguagePlan {
        build_tool: "npx".to_string(),
        build_tool_version: npx_version,
        indexer: "@sourcegraph/scip-typescript".to_string(),
        indexer_version: SCIP_TYPESCRIPT_VERSION.to_string(),
        runtime_version: node_version,
        bootstrap_runtime_version: "not applicable".to_string(),
        strategy,
        bootstrap_arguments: Vec::new(),
        arguments,
        project_inputs: configs,
        inferred_configs,
        inferred_config_contents,
        excluded_paths: IGNORED_DIRECTORIES
            .iter()
            .map(|directory| format!("**/{directory}/**"))
            .collect(),
        environment: Vec::new(),
    })
}

fn execute_go(repo: &Path, scratch: &Path, output: &Path, plan: &LanguagePlan) -> Result<()> {
    if plan.project_inputs.is_empty() {
        bail!("unsupported_build: Go plan contains no module roots");
    }

    let tool_directory = scratch.join("bin");
    fs::create_dir_all(&tool_directory)
        .with_context(|| format!("create Go tool directory {}", tool_directory.display()))?;
    let bootstrap_output = Command::new("go")
        .args(&plan.bootstrap_arguments)
        .current_dir(scratch)
        .env("GOBIN", &tool_directory)
        .env("GOWORK", "off")
        .env_remove("GOFLAGS")
        .env("GOTOOLCHAIN", planned_bootstrap_toolchain(plan)?)
        .output()
        .context("unsupported_build: build pinned scip-go with Go")?;
    require_success("scip-go bootstrap", SCIP_GO_VERSION, &bootstrap_output)?;
    let indexer = tool_directory.join(if cfg!(windows) {
        "scip-go.exe"
    } else {
        "scip-go"
    });
    if !indexer.is_file() {
        bail!(
            "indexer_failed: pinned scip-go bootstrap did not create {}",
            indexer.display()
        );
    }

    let external_go_work = if plan.strategy == "go-work" {
        Some(prepare_external_go_workspace(
            repo,
            scratch,
            &plan.project_inputs,
        )?)
    } else {
        None
    };
    let mut indexes = Vec::with_capacity(plan.project_inputs.len());
    for (index, module) in plan.project_inputs.iter().enumerate() {
        let relative = validate_relative_input(module)?;
        let module_root = repo.join(relative);
        if !module_root.join("go.mod").is_file() {
            bail!(
                "unsupported_build: Go module root has no go.mod: {}",
                module_root.display()
            );
        }

        let shard = scratch.join(format!("go-module-{index}.scip"));
        remove_file_if_present(&shard)?;
        let arguments = expand_arguments(&plan.arguments, repo, &module_root, &shard);
        let (planned_go_work, go_flags) = planned_go_environment(plan, module)?;
        let command_output = Command::new(&indexer)
            .args(&arguments)
            .current_dir(repo)
            .env(
                "GOWORK",
                external_go_work
                    .as_deref()
                    .unwrap_or_else(|| Path::new(planned_go_work)),
            )
            .env("GOFLAGS", go_flags)
            .output()
            .context("unsupported_build: start pinned scip-go")?;
        require_success("scip-go", SCIP_GO_VERSION, &command_output)?;
        require_nonempty_output(&shard, "scip-go")?;

        let bytes =
            fs::read(&shard).with_context(|| format!("read scip-go output {}", shard.display()))?;
        let mut index = Index::parse_from_bytes(&bytes)
            .with_context(|| format!("parse scip-go output {}", shard.display()))?;
        filter_and_rebase_go_documents(&mut index, &module_root, relative);
        indexes.push(index);
    }

    let merged = merge_indexes(indexes, repo);
    let bytes = merged
        .write_to_bytes()
        .context("serialize merged Go index")?;
    publish_bytes(output, &bytes)
}

fn execute_typescript(
    repo: &Path,
    scratch: &Path,
    output: &Path,
    plan: &LanguagePlan,
) -> Result<()> {
    if plan.project_inputs.is_empty() {
        bail!("typescript_config_missing: no TypeScript project configuration was planned");
    }

    for inferred in &plan.inferred_configs {
        let inferred = Path::new(inferred);
        if !inferred.starts_with(scratch) || !inferred.is_file() {
            bail!(
                "typescript_config_missing: inferred configuration is unavailable: {}",
                inferred.display()
            );
        }
    }

    let prepared = scratch.join("typescript-index.scip");
    remove_file_if_present(&prepared)?;
    let arguments = expand_arguments(&plan.arguments, repo, repo, &prepared);
    let command_output = Command::new("npx")
        .args(&arguments)
        .current_dir(repo)
        .output()
        .context("unsupported_build: start pinned scip-typescript through npx")?;
    require_success(
        "@sourcegraph/scip-typescript",
        SCIP_TYPESCRIPT_VERSION,
        &command_output,
    )?;
    require_nonempty_output(&prepared, "scip-typescript")?;

    let bytes = fs::read(&prepared)
        .with_context(|| format!("read scip-typescript output {}", prepared.display()))?;
    let mut index = Index::parse_from_bytes(&bytes)
        .with_context(|| format!("parse scip-typescript output {}", prepared.display()))?;
    deduplicate_documents(&mut index);
    let bytes = index
        .write_to_bytes()
        .context("serialize prepared TypeScript index")?;
    publish_bytes(output, &bytes)
}

fn go_module_roots(repo: &Path) -> Result<(String, Vec<String>)> {
    let go_work = repo.join("go.work");
    if go_work.is_file() {
        let uses = read_go_work(repo)?.uses;
        let mut roots = Vec::new();
        for workspace_use in uses {
            let disk_path = workspace_use.disk_path;
            let path = if Path::new(&disk_path).is_absolute() {
                PathBuf::from(&disk_path)
            } else {
                repo.join(&disk_path)
            };
            let canonical = canonical_directory(&path, "go.work module")?;
            let relative = canonical.strip_prefix(repo).map_err(|_| {
                anyhow!(
                    "unsupported_build: go.work module is outside repository: {}",
                    canonical.display()
                )
            })?;
            if !canonical.join("go.mod").is_file() {
                bail!(
                    "unsupported_build: go.work module has no go.mod: {}",
                    canonical.display()
                );
            }
            roots.push(relative_path_argument(relative));
        }
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            bail!("unsupported_build: go.work contains no module roots");
        }
        return Ok(("go-work".to_string(), roots));
    }

    let mut roots = Vec::new();
    discover_named_files(repo, "go.mod", &mut roots)?;
    let mut roots = roots
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .map(|path| {
            path.strip_prefix(repo)
                .map(relative_path_argument)
                .unwrap_or_else(|_| path_argument(&path))
        })
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    if roots.is_empty() {
        bail!("unsupported_build: Go preparation requires go.mod or go.work");
    }
    let strategy = if roots.len() == 1 {
        "go-module"
    } else {
        "go-multi-module"
    };
    Ok((strategy.to_string(), roots))
}

fn read_go_work(repo: &Path) -> Result<GoWorkInfo> {
    let go_work = repo.join("go.work");
    let output = Command::new("go")
        .args(["work", "edit", "-json"])
        .current_dir(repo)
        .env("GOWORK", &go_work)
        .output()
        .context("unsupported_build: inspect go.work")?;
    if !output.status.success() {
        bail!(
            "unsupported_build: cannot read go.work: {}",
            output_summary(&output)
        );
    }
    serde_json::from_slice(&output.stdout)
        .context("unsupported_build: parse `go work edit -json` output")
}

fn prepare_external_go_workspace(
    repo: &Path,
    scratch: &Path,
    module_roots: &[String],
) -> Result<PathBuf> {
    let source = repo.join("go.work");
    let destination = scratch.join("go.work");
    remove_file_if_present(&destination)?;
    fs::copy(&source, &destination).with_context(|| {
        format!(
            "copy Go workspace {} to {}",
            source.display(),
            destination.display()
        )
    })?;

    let workspace = read_go_work(repo)?;
    let mut arguments = vec!["work".to_string(), "edit".to_string()];
    arguments.extend(
        workspace
            .uses
            .into_iter()
            .map(|workspace_use| format!("-dropuse={}", workspace_use.disk_path)),
    );
    for module in module_roots {
        let relative = validate_relative_input(module)?;
        arguments.push(format!("-use={}", repo.join(relative).display()));
    }
    for replacement in workspace.replacements {
        let old = module_spec(&replacement.old);
        let new = replacement_target(repo, &replacement.new)?;
        arguments.push(format!("-dropreplace={old}"));
        arguments.push(format!("-replace={old}={new}"));
    }
    let edit = Command::new("go")
        .args(&arguments)
        .current_dir(scratch)
        .env("GOWORK", &destination)
        .env_remove("GOFLAGS")
        .output()
        .context("unsupported_build: create isolated Go workspace")?;
    if !edit.status.success() {
        bail!(
            "unsupported_build: cannot isolate go.work: {}",
            output_summary(&edit)
        );
    }
    link_workspace_vendor(repo, scratch)?;
    Ok(destination)
}

fn module_spec(module: &GoModule) -> String {
    if module.version.is_empty() {
        module.path.clone()
    } else {
        format!("{}@{}", module.path, module.version)
    }
}

fn replacement_target(repo: &Path, module: &GoModule) -> Result<String> {
    if !module.version.is_empty() || !is_local_go_path(&module.path) {
        return Ok(module_spec(module));
    }
    let repo = canonical_directory(repo, "repository")?;
    let target = if Path::new(&module.path).is_absolute() {
        PathBuf::from(&module.path)
    } else {
        repo.join(&module.path)
    };
    let canonical = target.canonicalize().with_context(|| {
        format!(
            "unsupported_build: resolve local go.work replacement {}",
            target.display()
        )
    })?;
    if !canonical.starts_with(&repo) {
        bail!(
            "unsupported_build: go.work replacement is outside repository: {}",
            canonical.display()
        );
    }
    Ok(path_argument(&canonical))
}

fn is_local_go_path(path: &str) -> bool {
    Path::new(path).is_absolute()
        || path == "."
        || path.starts_with("./")
        || path.starts_with("../")
}

#[cfg(unix)]
fn link_workspace_vendor(repo: &Path, scratch: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let source = repo.join("vendor");
    if !source.join("modules.txt").is_file() {
        return Ok(());
    }
    let destination = scratch.join("vendor");
    if destination.exists() || destination.symlink_metadata().is_ok() {
        bail!(
            "unsupported_build: external workspace vendor path already exists: {}",
            destination.display()
        );
    }
    symlink(&source, &destination).with_context(|| {
        format!(
            "create external workspace vendor link {} -> {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(not(unix))]
fn link_workspace_vendor(repo: &Path, _scratch: &Path) -> Result<()> {
    if repo.join("vendor/modules.txt").is_file() {
        bail!("unsupported_build: Go workspace vendoring requires symbolic-link support");
    }
    Ok(())
}

fn discover_typescript_configs(repo: &Path) -> Result<Vec<String>> {
    fn visit(directory: &Path, repo: &Path, configs: &mut Vec<String>) -> Result<()> {
        let mut entries = fs::read_dir(directory)
            .with_context(|| format!("read directory {}", directory.display()))?
            .collect::<io::Result<Vec<_>>>()
            .with_context(|| format!("read directory entries in {}", directory.display()))?;
        entries.sort_by_key(|entry| entry.file_name());

        let tsconfig = directory.join("tsconfig.json");
        let jsconfig = directory.join("jsconfig.json");
        let selected = if tsconfig.is_file() {
            Some(tsconfig)
        } else if jsconfig.is_file() {
            Some(jsconfig)
        } else {
            None
        };
        if let Some(config) = selected {
            let relative = config
                .strip_prefix(repo)
                .expect("walked TypeScript config is below repository");
            configs.push(relative_path_argument(relative));
        }

        for entry in entries {
            let file_type = entry
                .file_type()
                .with_context(|| format!("read file type for {}", entry.path().display()))?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if ignored_directory(&name) {
                continue;
            }
            visit(&entry.path(), repo, configs)?;
        }
        Ok(())
    }

    let mut configs = Vec::new();
    visit(repo, repo, &mut configs)?;
    configs.sort();
    Ok(configs)
}

fn discover_named_files(directory: &Path, name: &str, found: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read directory {}", directory.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("read directory entries in {}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", entry.path().display()))?;
        if file_type.is_file() && entry.file_name() == name {
            found.push(entry.path());
            continue;
        }
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let entry_name = entry.file_name();
        let entry_name = entry_name.to_string_lossy();
        if ignored_directory(&entry_name) {
            continue;
        }
        discover_named_files(&entry.path(), name, found)?;
    }
    Ok(())
}

fn ignored_directory(name: &str) -> bool {
    name.starts_with('.') || IGNORED_DIRECTORIES.contains(&name)
}

fn write_inferred_typescript_config(repo: &Path, destination: &Path) -> Result<String> {
    let root = path_argument(repo).replace('\\', "/");
    let includes = ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"]
        .map(|extension| format!("{root}/**/*.{extension}"));
    let excludes = IGNORED_DIRECTORIES
        .iter()
        .map(|directory| format!("{root}/**/{directory}/**"))
        .collect::<Vec<_>>();
    let config = serde_json::json!({
        "compilerOptions": {
            "allowJs": true,
            "checkJs": false,
            "noEmit": true,
            "skipLibCheck": true
        },
        "include": includes,
        "exclude": excludes
    });
    let contents = serde_json::to_string_pretty(&config).context("serialize inferred tsconfig")?;
    fs::write(destination, &contents)
        .with_context(|| format!("write inferred config {}", destination.display()))?;
    Ok(contents)
}

fn filter_and_rebase_go_documents(index: &mut Index, module_root: &Path, relative: &Path) {
    index.documents.retain(|document| {
        let document_path = Path::new(&document.relative_path);
        !has_component(document_path, "vendor")
            && !is_generated_go_file(&module_root.join(document_path))
    });
    let prefix = relative_path_argument(relative);
    if !prefix.is_empty() && prefix != "." {
        for document in &mut index.documents {
            document.relative_path = format!("{prefix}/{}", document.relative_path);
        }
    }
    deduplicate_documents(index);
}

fn is_generated_go_file(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("// Code generated ") && trimmed.ends_with(" DO NOT EDIT.") {
            return true;
        }
        if trimmed.starts_with("package ") {
            return false;
        }
    }
    false
}

fn merge_indexes(indexes: Vec<Index>, repo: &Path) -> Index {
    let mut merged = Index::new();
    let mut documents = HashSet::new();
    let mut external_symbols = HashSet::new();
    for mut index in indexes {
        if merged.metadata.is_none() {
            merged.metadata = index.metadata.take().into();
        }
        for document in index.documents.drain(..) {
            if documents.insert(document.relative_path.clone()) {
                merged.documents.push(document);
            }
        }
        for symbol in index.external_symbols.drain(..) {
            if external_symbols.insert(symbol.symbol.clone()) {
                merged.external_symbols.push(symbol);
            }
        }
    }
    if let Some(metadata) = merged.metadata.as_mut() {
        metadata.project_root = format!("file://{}", repo.display());
    }
    merged
}

fn deduplicate_documents(index: &mut Index) {
    let mut paths = HashSet::new();
    index
        .documents
        .retain(|document| paths.insert(document.relative_path.clone()));
}

fn expand_arguments(
    arguments: &[String],
    repo: &Path,
    module_root: &Path,
    output: &Path,
) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| match argument.as_str() {
            REPO_PLACEHOLDER => path_argument(repo),
            MODULE_ROOT_PLACEHOLDER => path_argument(module_root),
            OUTPUT_PLACEHOLDER => path_argument(output),
            _ => argument.clone(),
        })
        .collect()
}

fn require_success(indexer: &str, version: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "indexer_failed: {indexer} {version} exited with {}: {}",
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string()),
        output_summary(output)
    )
}

fn output_summary(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    if message.is_empty() {
        "no diagnostic output".to_string()
    } else {
        message.chars().take(4_000).collect()
    }
}

fn require_nonempty_output(path: &Path, indexer: &str) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| {
        format!(
            "indexer_failed: {indexer} did not create {}",
            path.display()
        )
    })?;
    if metadata.len() == 0 {
        bail!(
            "indexer_failed: {indexer} created an empty index at {}",
            path.display()
        );
    }
    Ok(())
}

fn publish_bytes(output: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create index output directory {}", parent.display()))?;
    }
    fs::write(output, bytes).with_context(|| format!("write prepared index {}", output.display()))
}

fn command_version(program: &str, arguments: &[&str], label: &str, cwd: &Path) -> Result<String> {
    let output = match Command::new(program)
        .args(arguments)
        .current_dir(cwd)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!("unsupported_build: {label} '{program}' is not available")
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {label} version")),
    };
    if !output.status.success() {
        bail!(
            "unsupported_build: cannot determine {label} version: {}",
            output_summary(&output)
        );
    }
    let version = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    if version.is_empty() {
        bail!("unsupported_build: {label} returned an empty version");
    }
    Ok(version.lines().collect::<Vec<_>>().join(" "))
}

fn target_go_flags(inherited: &str, module_root: &Path) -> String {
    if inherited
        .split_whitespace()
        .any(|flag| flag == "-mod" || flag.starts_with("-mod="))
    {
        return inherited.to_string();
    }
    let mode = if module_root.join("vendor/modules.txt").is_file() {
        "-mod=vendor"
    } else {
        "-mod=readonly"
    };
    if inherited.is_empty() {
        mode.to_string()
    } else {
        format!("{inherited} {mode}")
    }
}

fn validate_go_flags(flags: &str) -> Result<()> {
    let tokens = flags.split_whitespace().collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        let mode = if *token == "-mod" {
            tokens.get(index + 1).copied().unwrap_or_default()
        } else if let Some(mode) = token.strip_prefix("-mod=") {
            mode
        } else {
            continue;
        };
        if !matches!(mode, "readonly" | "vendor") {
            bail!(
                "unsupported_build: GOFLAGS uses source-mutating module mode '{mode}'; use -mod=readonly or -mod=vendor"
            );
        }
    }
    Ok(())
}

fn planned_go_environment<'a>(plan: &'a LanguagePlan, module: &str) -> Result<(&'a str, &'a str)> {
    let prefix = format!("{module}:GOWORK=");
    let entry = plan
        .environment
        .iter()
        .find_map(|entry| entry.strip_prefix(&prefix))
        .with_context(|| format!("unsupported_build: Go plan has no environment for {module}"))?;
    entry
        .split_once(";GOFLAGS=")
        .with_context(|| format!("unsupported_build: invalid Go environment planned for {module}"))
}

fn planned_bootstrap_toolchain(plan: &LanguagePlan) -> Result<&str> {
    plan.environment
        .iter()
        .find_map(|entry| entry.strip_prefix("bootstrap:GOTOOLCHAIN="))
        .context("unsupported_build: Go plan has no bootstrap toolchain")
}

fn go_toolchain_name(version: &str) -> Result<String> {
    let toolchain = version
        .split_whitespace()
        .find(|word| {
            word.starts_with("go") && word.as_bytes().get(2).is_some_and(u8::is_ascii_digit)
        })
        .context("unsupported_build: cannot parse Go toolchain version")?;
    let mut numbers = toolchain.trim_start_matches("go").split('.');
    let major = numbers
        .next()
        .and_then(|number| number.parse::<u32>().ok())
        .context("unsupported_build: cannot parse Go toolchain major version")?;
    let minor = numbers
        .next()
        .and_then(|number| number.parse::<u32>().ok())
        .context("unsupported_build: cannot parse Go toolchain minor version")?;
    if (major, minor) < (1, 25) {
        bail!(
            "unsupported_build: scip-go {SCIP_GO_VERSION} requires Go 1.25 or newer; found {toolchain}"
        );
    }
    Ok(toolchain.to_string())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("{label} directory does not exist: {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn external_scratch(repo: &Path, scratch: &Path) -> Result<PathBuf> {
    let repo = canonical_directory(repo, "repository")?;
    let candidate = canonical_candidate(scratch)?;
    if candidate.starts_with(&repo) {
        bail!(
            "scratch_inside_repository: preparation scratch must be outside {}",
            repo.display()
        );
    }
    fs::create_dir_all(scratch)
        .with_context(|| format!("create preparation scratch directory {}", scratch.display()))?;
    let scratch = canonical_directory(scratch, "preparation scratch")?;
    Ok(scratch)
}

fn canonical_candidate(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("resolve preparation scratch path {}", path.display()));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .context("resolve current directory for preparation scratch")?
            .join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().with_context(|| {
            format!(
                "resolve preparation scratch ancestor for {}",
                path.display()
            )
        })?;
        missing.push(name.to_os_string());
        ancestor = ancestor.parent().with_context(|| {
            format!(
                "resolve preparation scratch ancestor for {}",
                path.display()
            )
        })?;
    }
    let mut canonical = ancestor.canonicalize().with_context(|| {
        format!(
            "resolve preparation scratch ancestor for {}",
            path.display()
        )
    })?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn validate_relative_input(input: &str) -> Result<&Path> {
    let path = Path::new(input);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsupported_build: project input escapes repository: {input}");
    }
    Ok(path)
}

fn has_component(path: &Path, expected: &str) -> bool {
    path.components()
        .any(|component| component.as_os_str() == expected)
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove stale {}", path.display())),
    }
}

fn path_argument(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn relative_path_argument(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "crux-prepare-languages-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create fixture");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("fixture file parent"))
                .expect("create fixture parent");
            fs::write(path, contents).expect("write fixture file");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn source_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn visit(root: &Path, directory: &Path, snapshot: &mut Vec<(String, Vec<u8>)>) {
            let mut entries = fs::read_dir(directory)
                .expect("read snapshot directory")
                .collect::<io::Result<Vec<_>>>()
                .expect("read snapshot entries");
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let kind = entry.file_type().expect("snapshot file type");
                if kind.is_dir() {
                    visit(root, &entry.path(), snapshot);
                } else if kind.is_file() {
                    snapshot.push((
                        relative_path_argument(
                            entry
                                .path()
                                .strip_prefix(root)
                                .expect("snapshot relative path"),
                        ),
                        fs::read(entry.path()).expect("read snapshot file"),
                    ));
                }
            }
        }

        let mut snapshot = Vec::new();
        visit(root, root, &mut snapshot);
        snapshot
    }

    #[test]
    fn discovers_multiple_go_modules_but_not_vendor_modules() {
        let fixture = Fixture::new("go-modules");
        fixture.write("go.mod", "module example.com/root\n");
        fixture.write("tools/go.mod", "module example.com/root/tools\n");
        fixture.write("vendor/example.com/dep/go.mod", "module example.com/dep\n");

        let (strategy, roots) = go_module_roots(&fixture.root).expect("discover modules");

        assert_eq!(strategy, "go-multi-module");
        assert_eq!(roots, vec!["".to_string(), "tools".to_string()]);
    }

    #[test]
    fn discovers_nested_typescript_configs_and_prefers_tsconfig() {
        let fixture = Fixture::new("typescript-configs");
        fixture.write("apps/api/tsconfig.json", "{}");
        fixture.write("apps/web/jsconfig.json", "{}");
        fixture.write("apps/web/tsconfig.json", "{}");
        fixture.write("node_modules/pkg/tsconfig.json", "{}");
        fixture.write("dist/tsconfig.json", "{}");

        let configs = discover_typescript_configs(&fixture.root).expect("discover configs");

        assert_eq!(
            configs,
            vec![
                "apps/api/tsconfig.json".to_string(),
                "apps/web/tsconfig.json".to_string()
            ]
        );
    }

    #[test]
    fn inferred_typescript_config_is_external_and_covers_js_and_ts() {
        let fixture = Fixture::new("typescript-inferred");
        let scratch = Fixture::new("typescript-scratch");
        let destination = scratch.root.join("tsconfig.json");

        write_inferred_typescript_config(&fixture.root, &destination).expect("write config");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&destination).expect("read inferred config"))
                .expect("parse inferred config");

        assert!(destination.starts_with(&scratch.root));
        assert!(!destination.starts_with(&fixture.root));
        assert_eq!(value["compilerOptions"]["allowJs"], true);
        let includes = value["include"].as_array().expect("include array");
        assert!(includes
            .iter()
            .any(|value| value.as_str().unwrap().ends_with("/**/*.ts")));
        assert!(includes
            .iter()
            .any(|value| value.as_str().unwrap().ends_with("/**/*.js")));
    }

    #[test]
    fn generated_go_detection_requires_the_standard_marker() {
        let fixture = Fixture::new("generated-go");
        fixture.write(
            "generated.go",
            "// Copyright Example\n// Code generated by tool; DO NOT EDIT.\npackage fixture\n",
        );
        fixture.write("ordinary.go", "// generated helper\npackage fixture\n");

        assert!(is_generated_go_file(&fixture.root.join("generated.go")));
        assert!(!is_generated_go_file(&fixture.root.join("ordinary.go")));
    }

    #[test]
    fn go_flags_prevent_module_file_writes_and_honor_vendoring() {
        let fixture = Fixture::new("go-flags");
        fixture.write("vendor/modules.txt", "# fixture vendor manifest\n");

        assert_eq!(target_go_flags("", &fixture.root), "-mod=vendor");
        assert_eq!(
            target_go_flags("-tags=integration", &fixture.root),
            "-tags=integration -mod=vendor"
        );
        let error = validate_go_flags("-mod=mod -tags=integration")
            .expect_err("mutable module mode should fail preflight");
        assert!(error
            .to_string()
            .starts_with("unsupported_build: GOFLAGS uses source-mutating module mode 'mod'"));
        validate_go_flags("-mod=readonly -tags=integration").expect("readonly flags");
        validate_go_flags("-mod vendor -tags=integration").expect("vendor flags");

        let unvendored = Fixture::new("go-flags-unvendored");
        assert_eq!(target_go_flags("", &unvendored.root), "-mod=readonly");
    }

    #[test]
    fn local_workspace_replacements_are_rebased_and_external_ones_are_rejected() {
        let fixture = Fixture::new("go-work-replacements");
        fixture.write("replacement/go.mod", "module example.com/replacement\n");
        let local = GoModule {
            path: "./replacement".to_string(),
            version: String::new(),
        };
        assert_eq!(
            replacement_target(&fixture.root, &local).expect("rebase local replacement"),
            path_argument(
                &fixture
                    .root
                    .join("replacement")
                    .canonicalize()
                    .expect("canonical replacement")
            )
        );

        let outside = Fixture::new("go-work-external-replacement");
        let external = GoModule {
            path: path_argument(&outside.root),
            version: String::new(),
        };
        let error = replacement_target(&fixture.root, &external)
            .expect_err("external replacement should not enter provenance implicitly");
        assert!(error
            .to_string()
            .starts_with("unsupported_build: go.work replacement is outside repository:"));
    }

    #[test]
    fn scratch_directory_inside_repository_is_rejected() {
        let fixture = Fixture::new("scratch-boundary");
        let error = external_scratch(&fixture.root, &fixture.root.join(".scratch"))
            .expect_err("scratch inside repo should fail");

        assert!(error.to_string().starts_with("scratch_inside_repository:"));
    }

    #[test]
    fn pinned_commands_use_the_documented_upstream_flags() {
        assert_eq!(SCIP_GO_VERSION, "0.2.7");
        assert_eq!(SCIP_TYPESCRIPT_VERSION, "0.4.0");
        assert!(SCIP_GO_PACKAGE.ends_with("@v0.2.7"));
        assert!(SCIP_TYPESCRIPT_PACKAGE.ends_with("@0.4.0"));

        let arguments = expand_arguments(
            &[
                "--module-root".to_string(),
                MODULE_ROOT_PLACEHOLDER.to_string(),
                "--output".to_string(),
                OUTPUT_PLACEHOLDER.to_string(),
            ],
            Path::new("/repo"),
            Path::new("/repo/module"),
            Path::new("/tmp/index.scip"),
        );
        assert_eq!(
            arguments,
            vec![
                "--module-root",
                "/repo/module",
                "--output",
                "/tmp/index.scip"
            ]
        );
    }

    #[test]
    #[ignore = "requires Go 1.25+ and network access to fetch pinned scip-go"]
    fn real_go_workspace_indexes_modules_without_touching_sources() {
        let fixture = Fixture::new("real-go-workspace");
        fixture.write(
            "go.work",
            "go 1.25.0\n\nuse (\n\t./module-a\n\t./module-b\n)\n\nreplace example.com/replacement => ./replacement\n",
        );
        fixture.write(
            "module-a/go.mod",
            "module example.com/module-a\n\ngo 1.25.0\n",
        );
        fixture.write(
            "module-a/value.go",
            "package modulea\nfunc Value() int { return 1 }\n",
        );
        fixture.write(
            "module-a/generated.go",
            "// Code generated by fixture; DO NOT EDIT.\npackage modulea\nfunc Generated() int { return 100 }\n",
        );
        fixture.write(
            "module-b/go.mod",
            "module example.com/module-b\n\ngo 1.25.0\n\nrequire (\n\texample.com/module-a v0.0.0\n\texample.com/replacement v0.0.0\n)\n",
        );
        fixture.write(
            "module-b/value.go",
            "package moduleb\nimport a \"example.com/module-a\"\nimport r \"example.com/replacement\"\nfunc Value() int { return a.Value() + r.Value() }\n",
        );
        fixture.write(
            "replacement/go.mod",
            "module example.com/replacement\n\ngo 1.25.0\n",
        );
        fixture.write(
            "replacement/value.go",
            "package replacement\nfunc Value() int { return 2 }\n",
        );
        let scratch = Fixture::new("real-go-scratch");
        let output = scratch.root.join("index.scip");
        let before = source_snapshot(&fixture.root);

        let plan = plan(&fixture.root, &scratch.root, "go", &output).expect("plan Go index");
        assert_eq!(plan.strategy, "go-work");
        assert_eq!(plan.project_inputs, ["module-a", "module-b"]);
        execute(&fixture.root, &scratch.root, &output, &plan).expect("execute Go index");

        let mut documents = Index::parse_from_bytes(&fs::read(output).expect("read Go index"))
            .expect("parse Go index")
            .documents
            .into_iter()
            .map(|document| document.relative_path)
            .collect::<Vec<_>>();
        documents.sort();
        assert_eq!(documents, ["module-a/value.go", "module-b/value.go"]);
        assert_eq!(source_snapshot(&fixture.root), before);
    }

    #[test]
    #[ignore = "requires Node.js, npx, and network access to fetch pinned scip-typescript"]
    fn real_typescript_indexes_nested_and_external_inferred_configs() {
        let nested = Fixture::new("real-typescript-nested");
        nested.write(
            "package.json",
            "{\"name\":\"fixture\",\"version\":\"1.0.0\"}\n",
        );
        nested.write(
            "apps/api/tsconfig.json",
            "{\"include\":[\"src/**/*.ts\"]}\n",
        );
        nested.write(
            "apps/api/src/api.ts",
            "export function apiValue(): number { return 1 }\n",
        );
        nested.write(
            "apps/web/jsconfig.json",
            "{\"compilerOptions\":{\"allowJs\":true},\"include\":[\"src/**/*.js\"]}\n",
        );
        nested.write(
            "apps/web/src/web.js",
            "export function webValue() { return 2 }\n",
        );
        let nested_scratch = Fixture::new("real-typescript-nested-scratch");
        let nested_output = nested_scratch.root.join("index.scip");
        let nested_before = source_snapshot(&nested.root);
        let nested_plan = plan(
            &nested.root,
            &nested_scratch.root,
            "typescript",
            &nested_output,
        )
        .expect("plan nested TypeScript index");
        assert_eq!(nested_plan.strategy, "nested-configs");
        execute(
            &nested.root,
            &nested_scratch.root,
            &nested_output,
            &nested_plan,
        )
        .expect("execute nested TypeScript index");
        let nested_index = Index::parse_from_bytes(
            &fs::read(&nested_output).expect("read nested TypeScript index"),
        )
        .expect("parse nested TypeScript index");
        assert_eq!(nested_index.documents.len(), 2);
        assert_eq!(source_snapshot(&nested.root), nested_before);

        let inferred = Fixture::new("real-typescript-inferred");
        inferred.write(
            "package.json",
            "{\"name\":\"fixture\",\"version\":\"1.0.0\"}\n",
        );
        inferred.write("src/index.ts", "export const inferredValue: number = 42\n");
        inferred.write(
            "src/helper.js",
            "export function helper(value) { return value }\n",
        );
        let inferred_scratch = Fixture::new("real-typescript-inferred-scratch");
        let inferred_output = inferred_scratch.root.join("index.scip");
        let inferred_before = source_snapshot(&inferred.root);
        let inferred_plan = plan(
            &inferred.root,
            &inferred_scratch.root,
            "typescript",
            &inferred_output,
        )
        .expect("plan inferred TypeScript index");
        assert_eq!(inferred_plan.strategy, "external-inferred-config");
        execute(
            &inferred.root,
            &inferred_scratch.root,
            &inferred_output,
            &inferred_plan,
        )
        .expect("execute inferred TypeScript index");
        let inferred_index = Index::parse_from_bytes(
            &fs::read(&inferred_output).expect("read inferred TypeScript index"),
        )
        .expect("parse inferred TypeScript index");
        assert_eq!(inferred_index.documents.len(), 2);
        assert!(!inferred.root.join("tsconfig.json").exists());
        assert_eq!(source_snapshot(&inferred.root), inferred_before);
    }
}

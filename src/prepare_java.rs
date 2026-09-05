use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SCIP_JAVA_VERSION: &str = "0.13.1";
const SCIP_JAVA_COORDINATE: &str = "org.scip-code:scip-java";
const SCIP_JAVA_MAIN: &str = "org.scip_code.scip_java.ScipJava";
const SCIP_JAVAC_JVM_OPTIONS: &str = "\
-J--add-exports=jdk.compiler/com.sun.tools.javac.api=ALL-UNNAMED\n\
-J--add-exports=jdk.compiler/com.sun.tools.javac.code=ALL-UNNAMED\n\
-J--add-exports=jdk.compiler/com.sun.tools.javac.model=ALL-UNNAMED\n\
-J--add-exports=jdk.compiler/com.sun.tools.javac.tree=ALL-UNNAMED\n\
-J--add-exports=jdk.compiler/com.sun.tools.javac.util=ALL-UNNAMED";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct JavaPlan {
    pub(crate) build_tool: String,
    pub(crate) build_tool_version: String,
    pub(crate) preparation_strategy: String,
    pub(crate) indexer: String,
    pub(crate) indexer_version: String,
    pub(crate) runtime_version: String,
    pub(crate) arguments: Vec<String>,
    pub(crate) skipped_included_builds: Vec<String>,
    pub(crate) build_program: String,
    pub(crate) java_home: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildTool {
    Maven,
    Gradle,
    Bazel,
}

impl BuildTool {
    fn name(self) -> &'static str {
        match self {
            Self::Maven => "maven",
            Self::Gradle => "gradle",
            Self::Bazel => "bazel",
        }
    }
}

pub(crate) fn plan(repo: &Path, scratch: &Path, output: &Path) -> Result<JavaPlan> {
    plan_with_runtime(repo, scratch, output, None)
}

fn plan_with_runtime(
    repo: &Path,
    scratch: &Path,
    output: &Path,
    runtime: Option<(String, PathBuf)>,
) -> Result<JavaPlan> {
    let repo = fs::canonicalize(repo)
        .with_context(|| format!("repository_unavailable: {}", repo.display()))?;
    if !repo.is_dir() {
        bail!(
            "repository_unavailable: {} is not a directory",
            repo.display()
        );
    }
    ensure_external_scratch(&repo, scratch)?;

    let tool = detect_build_tool(&repo)?;
    if tool == BuildTool::Bazel {
        bail!(
            "unsupported_build: Bazel indexing requires a repository-visible scip-java aspect; \
             Crux will not generate preparation files inside the repository"
        );
    }
    let (build_program, build_tool_version) = build_tool_runtime(&repo, tool)?;
    let (runtime_version, java_home) = match runtime {
        Some(runtime) => runtime,
        None => java_runtime(&repo)?,
    };
    let major = java_major_version(&runtime_version).with_context(|| {
        format!("runtime_unavailable: cannot parse JDK version '{runtime_version}'")
    })?;
    if major < 17 {
        bail!(
            "runtime_unavailable: scip-java {SCIP_JAVA_VERSION} requires JDK 17 or newer; found {runtime_version}"
        );
    }

    let work = scratch.join("scip-java");
    let targetroot = scratch.join("scip-targetroot");
    let arguments = index_arguments(tool, &work, &targetroot, output);
    let skipped_included_builds = if tool == BuildTool::Gradle {
        discover_included_builds(&repo)?
    } else {
        Vec::new()
    };

    Ok(JavaPlan {
        build_tool: tool.name().to_string(),
        build_tool_version,
        preparation_strategy: match tool {
            BuildTool::Maven => "scip-java-index".to_string(),
            BuildTool::Gradle => "scip-java-gradle-primary-build-only".to_string(),
            BuildTool::Bazel => unreachable!("Bazel is rejected above"),
        },
        indexer: "scip-java".to_string(),
        indexer_version: SCIP_JAVA_VERSION.to_string(),
        runtime_version,
        arguments,
        skipped_included_builds,
        build_program,
        java_home: path_string(&java_home),
    })
}

pub(crate) fn execute(repo: &Path, scratch: &Path, output: &Path, plan: &JavaPlan) -> Result<()> {
    let repo = fs::canonicalize(repo)
        .with_context(|| format!("repository_unavailable: {}", repo.display()))?;
    ensure_external_scratch(&repo, scratch)?;
    fs::create_dir_all(scratch)
        .with_context(|| format!("indexer_failed: create {}", scratch.display()))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("indexer_failed: create {}", parent.display()))?;
    }
    remove_file_if_present(output)?;

    let launcher = coursier_launcher(&repo)?;
    verify_indexer(&launcher, &repo)?;

    match plan.build_tool.as_str() {
        "maven" => execute_maven(&launcher, &repo, scratch, output, plan),
        "gradle" => execute_gradle(&launcher, &repo, scratch, output, plan),
        "bazel" => bail!(
            "unsupported_build: Bazel indexing cannot keep its generated aspect outside the repository"
        ),
        other => bail!("unsupported_build: unknown Java build tool '{other}'"),
    }
}

pub(crate) fn skipped_builds(repo: &Path, scratch: &Path) -> Result<Vec<String>> {
    let log = scratch.join("scip-java/skipped-included-builds.txt");
    let contents = match fs::read_to_string(&log) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "included_build_incompatible: read skipped build log {}",
                    log.display()
                )
            })
        }
    };
    let repo = fs::canonicalize(repo)
        .with_context(|| format!("repository_unavailable: {}", repo.display()))?;
    let mut skipped = BTreeSet::new();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = PathBuf::from(line);
        let display = path
            .strip_prefix(&repo)
            .map(path_string)
            .unwrap_or_else(|_| path_string(&path));
        skipped.insert(if display.is_empty() {
            ".".to_string()
        } else {
            display
        });
    }
    Ok(skipped.into_iter().collect())
}

fn execute_maven(
    launcher: &str,
    repo: &Path,
    scratch: &Path,
    output: &Path,
    plan: &JavaPlan,
) -> Result<()> {
    let work = scratch.join("scip-java");
    let targetroot = scratch.join("scip-targetroot");
    reset_directory(&work)?;
    reset_directory(&targetroot)?;

    let separator = plan
        .arguments
        .iter()
        .position(|argument| argument == "--")
        .context("indexer_failed: malformed Maven preparation plan")?;
    let mut extraction = plan.arguments[..=separator].to_vec();
    extraction.push("help:effective-pom".to_string());
    let extraction_output = run_scip_java_unchecked(launcher, &extraction, repo)?;
    let javac = work.join("bin/javac");
    if !javac.is_file() {
        bail!(
            "indexer_failed: scip-java did not extract its Maven compiler wrapper: {}",
            output_tail(&extraction_output)
        );
    }
    make_custom_javac_portable(&javac)?;

    let plugin = work.join("scip-plugin.jar");
    if !plugin.is_file() {
        bail!(
            "indexer_failed: scip-java did not extract its compiler plugin at {}",
            plugin.display()
        );
    }
    let error_path = work.join("errorpath.txt");
    let options_prefix = work.join("javac_newarguments");
    let old_options = targetroot.join("javacopts.txt");
    let repo_string = path_string(repo);
    let targetroot_string = path_string(&targetroot);
    let plugin_string = path_string(&plugin);
    let error_string = path_string(&error_path);
    let prefix_string = path_string(&options_prefix);
    let old_options_string = path_string(&old_options);
    let mut maven_arguments = vec![
        "-Dmaven.compiler.useIncrementalCompilation=false".to_string(),
        "-Dmaven.compiler.compilerId=javac".to_string(),
        format!("-Dmaven.compiler.executable={}", javac.display()),
        "-Dmaven.compiler.fork=true".to_string(),
    ];
    maven_arguments.extend_from_slice(&plan.arguments[separator + 1..]);
    let build = run_program(
        &plan.build_program,
        &maven_arguments,
        repo,
        &[
            ("JAVA_HOME", &plan.java_home),
            ("SCIP_ERRORPATH", &error_string),
            ("SCIP_JAVAC_LAUNCHER_JVM_OPTIONS", SCIP_JAVAC_JVM_OPTIONS),
            ("SCIP_JAVAC_OPTIONS_PREFIX", &prefix_string),
            ("SCIP_OLD_JAVAC_OPTS", &old_options_string),
            ("SCIP_PLUGINPATH", &plugin_string),
            ("SCIP_SOURCEROOT", &repo_string),
            ("SCIP_TARGETROOT", &targetroot_string),
        ],
    )?;
    if !build.status.success() {
        return Err(build_failure("maven", &build));
    }
    if error_path.is_file() {
        let errors = fs::read_to_string(&error_path).unwrap_or_default();
        bail!(
            "indexer_failed: scip-java compiler plugin reported errors: {}",
            last_lines(&errors, 20)
        );
    }
    aggregate_targetroot(launcher, repo, output, &targetroot)
}

fn make_custom_javac_portable(path: &Path) -> Result<()> {
    let script = fs::read_to_string(path)
        .with_context(|| format!("indexer_failed: read {}", path.display()))?;
    let patched = script.replacen("set -eu", "set -e", 1);
    if patched == script {
        bail!("indexer_failed: scip-java {SCIP_JAVA_VERSION} compiler wrapper format changed");
    }
    fs::write(path, patched).with_context(|| format!("indexer_failed: patch {}", path.display()))
}

fn detect_build_tool(repo: &Path) -> Result<BuildTool> {
    let mut detected = Vec::new();
    if repo.join("pom.xml").is_file() {
        detected.push(BuildTool::Maven);
    }
    if [
        "settings.gradle",
        "settings.gradle.kts",
        "build.gradle",
        "build.gradle.kts",
        "gradlew",
        "gradlew.bat",
    ]
    .iter()
    .any(|name| repo.join(name).is_file())
    {
        detected.push(BuildTool::Gradle);
    }
    if ["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"]
        .iter()
        .any(|name| repo.join(name).is_file())
    {
        detected.push(BuildTool::Bazel);
    }

    match detected.as_slice() {
        [tool] => Ok(*tool),
        [] => bail!(
            "unsupported_build: no Maven, Gradle, or Bazel marker found in {}",
            repo.display()
        ),
        many => bail!(
            "unsupported_build: multiple Java build tools detected: {}",
            many.iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn build_tool_runtime(repo: &Path, tool: BuildTool) -> Result<(String, String)> {
    let (program, version_arguments): (String, &[&str]) = match tool {
        BuildTool::Maven => {
            let wrapper = repo.join(if cfg!(windows) { "mvnw.cmd" } else { "mvnw" });
            if executable_file(&wrapper) {
                (wrapper.to_string_lossy().into_owned(), &["--version"])
            } else {
                ("mvn".to_string(), &["--version"])
            }
        }
        BuildTool::Gradle => {
            let wrapper = repo.join(if cfg!(windows) {
                "gradlew.bat"
            } else {
                "gradlew"
            });
            if executable_file(&wrapper) {
                (wrapper.to_string_lossy().into_owned(), &["--version"])
            } else {
                ("gradle".to_string(), &["--version"])
            }
        }
        BuildTool::Bazel => {
            if command_exists(repo, "bazelisk", &["version"]) {
                ("bazelisk".to_string(), &["version"])
            } else {
                ("bazel".to_string(), &["version"])
            }
        }
    };
    let output = Command::new(&program)
        .args(version_arguments)
        .current_dir(repo)
        .output()
        .with_context(|| {
            format!(
                "runtime_unavailable: {} executable '{}'",
                tool.name(),
                program
            )
        })?;
    if !output.status.success() {
        bail!(
            "runtime_unavailable: {} version probe failed: {}",
            tool.name(),
            output_tail(&output)
        );
    }
    let combined = combined_output(&output);
    let version = extract_build_tool_version(tool, &combined).with_context(|| {
        format!(
            "runtime_unavailable: could not parse {} version from: {}",
            tool.name(),
            last_lines(&combined, 5)
        )
    })?;
    Ok((program, version))
}

fn extract_build_tool_version(tool: BuildTool, output: &str) -> Option<String> {
    let prefixes: &[&str] = match tool {
        BuildTool::Maven => &["Apache Maven ", "Maven "],
        BuildTool::Gradle => &["Gradle "],
        BuildTool::Bazel => &["bazel ", "Build label: ", "Bazelisk version: "],
    };
    output.lines().find_map(|line| {
        let line = line.trim();
        prefixes.iter().find_map(|prefix| {
            line.strip_prefix(prefix)
                .and_then(|rest| rest.split_whitespace().next())
                .filter(|version| !version.is_empty())
                .map(ToOwned::to_owned)
        })
    })
}

fn java_runtime(repo: &Path) -> Result<(String, PathBuf)> {
    let program = env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| {
            home.join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" })
        })
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("java"));
    let output = Command::new(&program)
        .args(["-XshowSettings:properties", "-version"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("runtime_unavailable: {}", program.display()))?;
    if !output.status.success() {
        bail!(
            "runtime_unavailable: java probe failed: {}",
            output_tail(&output)
        );
    }
    let combined = combined_output(&output);
    let version = java_property(&combined, "java.version")
        .or_else(|| quoted_java_version(&combined))
        .context("runtime_unavailable: java.version was not reported")?;
    let home = java_property(&combined, "java.home")
        .map(PathBuf::from)
        .or_else(|| env::var_os("JAVA_HOME").map(PathBuf::from))
        .context("runtime_unavailable: java.home was not reported")?;
    Ok((version, home))
}

fn java_property(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

fn quoted_java_version(output: &str) -> Option<String> {
    let line = output.lines().find(|line| line.contains(" version \""))?;
    line.split('"').nth(1).map(ToOwned::to_owned)
}

fn java_major_version(version: &str) -> Option<u32> {
    let version = version.strip_prefix("1.").unwrap_or(version);
    version
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn index_arguments(tool: BuildTool, work: &Path, targetroot: &Path, output: &Path) -> Vec<String> {
    let mut arguments = vec![
        "index".to_string(),
        "--build-tool".to_string(),
        title_case(tool.name()).to_string(),
        "--no-cleanup".to_string(),
        "--temporary-directory".to_string(),
        path_string(work),
        "--targetroot".to_string(),
        path_string(targetroot),
        "--output".to_string(),
        path_string(output),
    ];
    match tool {
        BuildTool::Maven => arguments.extend(strings(&[
            "--",
            "--batch-mode",
            "verify",
            "-DskipTests",
            "-Dskip.installnodenpm=true",
            "-Dskip.npm=true",
            "-Dmaven.source.skip=true",
            "-Dshade.skip=true",
            "-Dmaven.javadoc.skip=true",
            "-Dgpg.skip=true",
        ])),
        BuildTool::Gradle => arguments.extend(strings(&[
            "--",
            "--no-configuration-cache",
            "--no-configure-on-demand",
            "scipPrintDependencies",
            "scipCompileAll",
        ])),
        BuildTool::Bazel => {
            let aspect = path_string(&work.join("aspect/scip_java.bzl"));
            let binary = path_string(&work.join("bin/scip-java"));
            arguments.extend(strings(&[
                "--bazel-aspect",
                &aspect,
                "--bazel-scip-java-binary",
                &binary,
                "--",
                "//...",
            ]));
        }
    }
    arguments
}

fn execute_gradle(
    launcher: &str,
    repo: &Path,
    scratch: &Path,
    output: &Path,
    plan: &JavaPlan,
) -> Result<()> {
    let work = scratch.join("scip-java");
    let targetroot = scratch.join("scip-targetroot");
    reset_directory(&work)?;
    reset_directory(&targetroot)?;

    let separator = plan
        .arguments
        .iter()
        .position(|argument| argument == "--")
        .context("indexer_failed: malformed Gradle preparation plan")?;
    let mut extraction = plan.arguments[..=separator].to_vec();
    extraction.push("help".to_string());
    let extraction_output = run_scip_java_unchecked(launcher, &extraction, repo)?;
    let init_script = work.join("init-script.gradle");
    if !init_script.is_file() {
        bail!(
            "indexer_failed: scip-java did not extract its Gradle init script: {}",
            output_tail(&extraction_output)
        );
    }
    let skipped_log = work.join("skipped-included-builds.txt");
    fs::write(&skipped_log, "")?;
    restrict_gradle_init_script(&init_script, repo, &skipped_log)?;

    let mut gradle_arguments = vec![
        "--no-daemon".to_string(),
        "--init-script".to_string(),
        path_string(&init_script),
        "-Pkotlin.compiler.execution.strategy=in-process".to_string(),
        format!("-Dscip.targetroot={}", targetroot.display()),
    ];
    gradle_arguments.extend_from_slice(&plan.arguments[separator + 1..]);
    let build = run_program(
        &plan.build_program,
        &gradle_arguments,
        repo,
        &[("TERM", "dumb"), ("JAVA_HOME", &plan.java_home)],
    )?;
    if !build.status.success() {
        return Err(build_failure("gradle", &build));
    }

    aggregate_targetroot(launcher, repo, output, &targetroot)
}

fn aggregate_targetroot(
    launcher: &str,
    repo: &Path,
    output: &Path,
    targetroot: &Path,
) -> Result<()> {
    let aggregate = vec![
        "aggregate".to_string(),
        "--output".to_string(),
        path_string(output),
        path_string(targetroot),
    ];
    run_scip_java(launcher, &aggregate, repo).map(|_| ())
}

fn restrict_gradle_init_script(path: &Path, repo: &Path, skipped_log: &Path) -> Result<()> {
    let script = fs::read_to_string(path)
        .with_context(|| format!("indexer_failed: read {}", path.display()))?;
    let marker = "allprojects {";
    if !script.contains(marker) {
        bail!(
            "included_build_incompatible: scip-java {SCIP_JAVA_VERSION} init script has no allprojects block"
        );
    }
    let root = groovy_string(&fs::canonicalize(repo)?);
    let skipped_log = groovy_string(skipped_log);
    let guard = format!(
        "{marker}\n  if (project.gradle.parent != null || project.rootProject.projectDir.canonicalFile != new File(\"{root}\").canonicalFile) {{\n    new File(\"{skipped_log}\") << project.rootProject.projectDir.canonicalPath << System.lineSeparator()\n    return\n  }}"
    );
    let mut patched = script.replacen(marker, &guard, 1);
    patched.push_str(&format!(
        "\ngradle.projectsLoaded {{\n  if (gradle.parent == null && gradle.rootProject.projectDir.canonicalFile == new File(\"{root}\").canonicalFile) {{\n    gradle.includedBuilds.each {{ included ->\n      new File(\"{skipped_log}\") << included.projectDir.canonicalPath << System.lineSeparator()\n    }}\n  }}\n}}\n"
    ));
    fs::write(path, patched).with_context(|| format!("indexer_failed: patch {}", path.display()))
}

fn discover_included_builds(repo: &Path) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    let build_src = repo.join("buildSrc");
    if build_src.is_dir() {
        paths.insert("buildSrc".to_string());
    }
    for name in ["settings.gradle", "settings.gradle.kts"] {
        let path = repo.join(name);
        if !path.is_file() {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("repository_unavailable: read {}", path.display()))?;
        for included in included_build_literals(&contents) {
            if included.contains('$') {
                continue;
            }
            let candidate = PathBuf::from(&included);
            let candidate = if candidate.is_absolute() {
                candidate
            } else {
                repo.join(candidate)
            };
            let display = candidate
                .strip_prefix(repo)
                .map(path_string)
                .unwrap_or_else(|_| path_string(&candidate));
            paths.insert(if display.is_empty() {
                ".".to_string()
            } else {
                display
            });
        }
    }
    Ok(paths.into_iter().collect())
}

fn included_build_literals(contents: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = contents;
    while let Some(offset) = rest.find("includeBuild") {
        rest = &rest[offset + "includeBuild".len()..];
        let candidate = rest.trim_start();
        let candidate = candidate
            .strip_prefix('(')
            .map(str::trim_start)
            .unwrap_or(candidate);
        let candidate = candidate
            .strip_prefix("file(")
            .map(str::trim_start)
            .unwrap_or(candidate);
        let Some(quote) = candidate.chars().next().filter(|c| matches!(c, '\'' | '"')) else {
            continue;
        };
        let value = &candidate[quote.len_utf8()..];
        if let Some(end) = value.find(quote) {
            result.push(value[..end].to_string());
            rest = &value[end + quote.len_utf8()..];
        }
    }
    result
}

fn coursier_launcher(repo: &Path) -> Result<String> {
    for candidate in ["cs", "coursier"] {
        match Command::new(candidate)
            .arg("--help")
            .current_dir(repo)
            .output()
        {
            Ok(_) => return Ok(candidate.to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("indexer_unavailable: run {candidate}"))
            }
        }
    }
    bail!("indexer_unavailable: coursier executable 'cs' was not found")
}

fn verify_indexer(launcher: &str, repo: &Path) -> Result<()> {
    let output = run_scip_java_unchecked(launcher, &["--version".to_string()], repo)?;
    if !output.status.success() {
        bail!(
            "indexer_unavailable: scip-java {SCIP_JAVA_VERSION}: {}",
            output_tail(&output)
        );
    }
    let reported = combined_output(&output);
    if !reported.contains(SCIP_JAVA_VERSION) {
        bail!(
            "indexer_version_mismatch: requested {SCIP_JAVA_VERSION}, got {}",
            last_lines(&reported, 3)
        );
    }
    Ok(())
}

fn run_scip_java(launcher: &str, arguments: &[String], repo: &Path) -> Result<Output> {
    let output = run_scip_java_unchecked(launcher, arguments, repo)?;
    if !output.status.success() {
        return Err(build_failure("scip-java", &output));
    }
    Ok(output)
}

fn run_scip_java_unchecked(launcher: &str, arguments: &[String], repo: &Path) -> Result<Output> {
    let coordinate = format!("{SCIP_JAVA_COORDINATE}:{SCIP_JAVA_VERSION}");
    let mut launcher_arguments = vec![
        "launch".to_string(),
        coordinate,
        "-M".to_string(),
        SCIP_JAVA_MAIN.to_string(),
        "--".to_string(),
    ];
    launcher_arguments.extend_from_slice(arguments);
    run_program(
        launcher,
        &launcher_arguments,
        repo,
        &[("NO_PROGRESS_BAR", "true")],
    )
}

fn run_program(
    program: &str,
    arguments: &[String],
    repo: &Path,
    environment: &[(&str, &str)],
) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(arguments).current_dir(repo);
    for (name, value) in environment {
        command.env(name, value);
    }
    let java_options = match env::var("JAVA_TOOL_OPTIONS") {
        Ok(existing) if !existing.trim().is_empty() => format!("{existing} -Xss16m"),
        _ => "-Xss16m".to_string(),
    };
    command.env("JAVA_TOOL_OPTIONS", java_options);
    command
        .output()
        .with_context(|| format!("indexer_unavailable: run '{program}'"))
}

fn build_failure(tool: &str, output: &Output) -> anyhow::Error {
    let complete = combined_output(output);
    let code = if complete.contains("generatePrecompiledScriptPluginAccessors")
        || complete.contains("provider was queried before")
        || complete.contains("Failed to query the value of property 'freeCompilerArgs'")
        || complete.contains("Failed to query the value of property freeCompilerArgs")
    {
        "included_build_incompatible"
    } else {
        "indexer_failed"
    };
    anyhow::anyhow!("{code}: {tool} failed: {}", output_tail(output))
}

fn ensure_external_scratch(repo: &Path, scratch: &Path) -> Result<()> {
    let scratch = if scratch.exists() {
        fs::canonicalize(scratch)?
    } else {
        lexical_absolute(scratch)?
    };
    if scratch.starts_with(repo) {
        bail!(
            "output_inside_repository: Java scratch path must be outside the repository: {}",
            scratch.display()
        );
    }
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn reset_directory(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)
            .with_context(|| format!("indexer_failed: clear {}", path.display()))?;
    }
    fs::create_dir_all(path).with_context(|| format!("indexer_failed: create {}", path.display()))
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("indexer_failed: remove {}", path.display()))
        }
    }
}

fn command_exists(repo: &Path, program: &str, arguments: &[&str]) -> bool {
    Command::new(program)
        .args(arguments)
        .current_dir(repo)
        .output()
        .is_ok()
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

fn groovy_string(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

fn title_case(value: &str) -> &str {
    match value {
        "maven" => "Maven",
        "gradle" => "Gradle",
        "bazel" => "Bazel",
        other => other,
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn output_tail(output: &Output) -> String {
    let combined = combined_output(output);
    let tail = last_lines(&combined, 20);
    if tail.is_empty() {
        output
            .status
            .code()
            .map(|code| format!("exit status {code}"))
            .unwrap_or_else(|| "terminated by signal".to_string())
    } else {
        tail
    }
}

fn last_lines(output: &str, count: usize) -> String {
    let lines = output.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "crux-prepare-java-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn fixture_runtime() -> Option<(String, PathBuf)> {
        Some(("17.0.12".to_string(), PathBuf::from("/fixture/jdk")))
    }

    #[cfg(unix)]
    #[test]
    fn plans_filtered_gradle_with_external_paths_and_current_pin() {
        let root = TestDirectory::new("gradle");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(
            repo.join("settings.gradle.kts"),
            "pluginManagement { includeBuild(\"build-logic\") }\nincludeBuild('../shared')",
        )
        .unwrap();
        executable(&repo.join("gradlew"), "#!/bin/sh\necho 'Gradle 8.10.2'\n");
        let output = scratch.join("index.scip");
        let plan = plan_with_runtime(&repo, &scratch, &output, fixture_runtime()).unwrap();

        assert_eq!(plan.build_tool, "gradle");
        assert_eq!(plan.build_tool_version, "8.10.2");
        assert_eq!(plan.indexer_version, "0.13.1");
        assert_eq!(plan.runtime_version, "17.0.12");
        assert!(plan
            .arguments
            .contains(&"--no-configuration-cache".to_string()));
        assert_eq!(
            plan.skipped_included_builds,
            vec!["../shared".to_string(), "build-logic".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn maven_arguments_do_not_leak_into_gradle_plan() {
        let root = TestDirectory::new("maven");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(repo.join("pom.xml"), "<project/>").unwrap();
        executable(&repo.join("mvnw"), "#!/bin/sh\necho 'Apache Maven 3.9.9'\n");
        let output = scratch.join("index.scip");
        let plan = plan_with_runtime(&repo, &scratch, &output, fixture_runtime()).unwrap();

        assert_eq!(plan.build_tool, "maven");
        assert_eq!(plan.build_tool_version, "3.9.9");
        assert!(plan.arguments.contains(&"--batch-mode".to_string()));
        assert!(!plan.arguments.contains(&"--no-daemon".to_string()));
    }

    #[test]
    fn included_build_parser_handles_groovy_kotlin_and_file_literals() {
        let parsed = included_build_literals(
            "includeBuild 'one'\nincludeBuild(\"two\")\nincludeBuild(file('../three'))",
        );
        assert_eq!(parsed, vec!["one", "two", "../three"]);
    }

    #[test]
    fn gradle_script_filter_keeps_subprojects_and_rejects_other_build_roots() {
        let root = TestDirectory::new("script");
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let script = root.path().join("init-script.gradle");
        fs::write(
            &script,
            "initscript {}\nallprojects {\n  apply plugin: X\n}\n",
        )
        .unwrap();
        let skipped_log = root.path().join("skipped.txt");
        restrict_gradle_init_script(&script, &repo, &skipped_log).unwrap();
        let patched = fs::read_to_string(script).unwrap();
        assert!(patched.contains("project.rootProject.projectDir.canonicalFile"));
        assert!(patched.contains("project.gradle.parent != null"));
        assert!(patched.contains(&groovy_string(&repo.canonicalize().unwrap())));
        assert!(patched.contains(&groovy_string(&skipped_log)));
        assert_eq!(patched.matches("allprojects {").count(), 1);
    }

    #[test]
    fn makes_generated_javac_wrapper_work_with_empty_arrays_on_bash_3() {
        let root = TestDirectory::new("javac-wrapper");
        let script = root.path().join("javac");
        fs::write(&script, "#!/usr/bin/env bash\nset -eu\nLAUNCHER_ARGS=()\n").unwrap();
        make_custom_javac_portable(&script).unwrap();
        assert!(fs::read_to_string(script).unwrap().contains("set -e\n"));
    }

    #[test]
    fn reads_deduplicated_actual_skipped_build_roots() {
        let root = TestDirectory::new("skipped");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        let work = scratch.join("scip-java");
        fs::create_dir_all(repo.join("build-logic")).unwrap();
        fs::create_dir_all(&work).unwrap();
        let included = repo.join("build-logic").canonicalize().unwrap();
        fs::write(
            work.join("skipped-included-builds.txt"),
            format!(
                "{}\n{}\n/opt/shared-build\n",
                included.display(),
                included.display()
            ),
        )
        .unwrap();

        assert_eq!(
            skipped_builds(&repo, &scratch).unwrap(),
            vec!["/opt/shared-build".to_string(), "build-logic".to_string()]
        );
    }

    #[test]
    fn rejects_scratch_inside_repository() {
        let root = TestDirectory::new("inside");
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join("scratch")).unwrap();
        let error = ensure_external_scratch(&repo.canonicalize().unwrap(), &repo.join("scratch"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("output_inside_repository"));
    }

    #[test]
    fn bazel_is_detected_but_rejected_until_external_aspects_are_supported() {
        let root = TestDirectory::new("bazel");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(repo.join("MODULE.bazel"), "").unwrap();

        assert_eq!(detect_build_tool(&repo).unwrap(), BuildTool::Bazel);
        let error = plan_with_runtime(
            &repo,
            &scratch,
            &scratch.join("index.scip"),
            fixture_runtime(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsupported_build"));
    }

    #[test]
    #[ignore = "integration: requires JDK 17+, cs, and Maven on PATH"]
    fn integration_maven_rat_keeps_preparation_files_external() {
        let root = TestDirectory::new("maven-rat-integration");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(repo.join("src/main/java/example")).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(
            repo.join("pom.xml"),
            r#"<!--
Licensed to the Apache Software Foundation (ASF) under one
or more contributor license agreements. See the NOTICE file
distributed with this work for additional information
regarding copyright ownership. The ASF licenses this file
to you under the Apache License, Version 2.0 (the
"License"); you may not use this file except in compliance
with the License. You may obtain a copy of the License at

  http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing,
software distributed under the License is distributed on an
"AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
KIND, either express or implied. See the License for the
specific language governing permissions and limitations
under the License.
-->
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>example</groupId>
  <artifactId>crux-java-rat-fixture</artifactId>
  <version>1.0.0</version>
  <properties>
    <maven.compiler.release>17</maven.compiler.release>
  </properties>
  <build>
    <plugins>
      <plugin>
        <groupId>org.apache.rat</groupId>
        <artifactId>apache-rat-plugin</artifactId>
        <version>0.17</version>
        <executions>
          <execution>
            <phase>verify</phase>
            <goals><goal>check</goal></goals>
          </execution>
        </executions>
      </plugin>
    </plugins>
  </build>
</project>
"#,
        )
        .unwrap();
        fs::write(
            repo.join("src/main/java/example/Hello.java"),
            r#"/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements. See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership. The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License. You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied. See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */
package example;
public final class Hello {
    public static String greet() { return "hello"; }
}
"#,
        )
        .unwrap();
        let output = scratch.join("index.scip");

        let plan = plan(&repo, &scratch, &output).unwrap();
        execute(&repo, &scratch, &output, &plan).unwrap();

        assert!(fs::metadata(output).unwrap().len() > 100);
        assert!(!repo.join(".scip-java-tmp").exists());
        assert!(!repo.join("scip-java").exists());
    }

    #[test]
    #[ignore = "integration: requires JDK 17+, cs, and Gradle on PATH"]
    fn integration_gradle_filters_included_build_and_records_it() {
        let root = TestDirectory::new("gradle-included-build-integration");
        let repo = root.path().join("repo");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(repo.join("app/src/main/java/example")).unwrap();
        fs::create_dir_all(repo.join("build-logic")).unwrap();
        fs::create_dir_all(&scratch).unwrap();
        fs::write(
            repo.join("settings.gradle.kts"),
            "pluginManagement { val logic = \"build-logic\"; includeBuild(logic) }\n\
             // includeBuild(\"not-included\")\n\
             rootProject.name = \"fixture\"\ninclude(\"app\")\n",
        )
        .unwrap();
        fs::write(
            repo.join("build.gradle.kts"),
            "allprojects { repositories { mavenCentral() } }\n",
        )
        .unwrap();
        fs::write(repo.join("app/build.gradle.kts"), "plugins { java }\n").unwrap();
        fs::write(
            repo.join("app/src/main/java/example/Hello.java"),
            "package example;\npublic final class Hello {}\n",
        )
        .unwrap();
        fs::write(
            repo.join("build-logic/settings.gradle.kts"),
            "rootProject.name = \"build-logic\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("build-logic/build.gradle.kts"),
            r#"plugins { id("java-gradle-plugin") }
afterEvaluate {
    if (tasks.findByName("scipCompileAll") != null) {
        throw GradleException("scip-java leaked into included build")
    }
}
"#,
        )
        .unwrap();
        let output = scratch.join("index.scip");

        let plan = plan(&repo, &scratch, &output).unwrap();
        execute(&repo, &scratch, &output, &plan).unwrap();

        assert!(fs::metadata(output).unwrap().len() > 100);
        assert_eq!(
            skipped_builds(&repo, &scratch).unwrap(),
            vec!["build-logic".to_string()]
        );
        assert!(!repo.join(".scip-java-tmp").exists());
    }
}

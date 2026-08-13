use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BEGIN_MARKER: &str = "# --- crux begin ---";
const END_MARKER: &str = "# --- crux end ---";
const ADOPTION_BODY: &str = "## crux code index\n\nThis project may have a crux code index.\n\nWho calls X? What references X? What breaks if X changes? Is X dead code? — the index answers each in ONE call; grep answers them incompletely.\n\nPlain text search is cheaper for a simple 'where is X defined' question.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Client {
    Codex,
    Claude,
}

impl Client {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => bail!("unsupported client '{value}'; expected codex or claude"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct MarkerSpan {
    start: usize,
    end: usize,
    newline: &'static str,
}

#[derive(Debug)]
struct EditReport {
    backup_created: bool,
    changed: bool,
    existed: bool,
}

enum ClaudeCommand {
    Absent,
    Ran(Output),
}

pub(crate) fn parse_client_arguments(arguments: &[String]) -> Result<(Client, Option<PathBuf>)> {
    let Some(client) = arguments.first() else {
        bail!("a client is required; expected codex or claude");
    };
    let client = Client::parse(client)?;
    let project = match &arguments[1..] {
        [] => None,
        [flag, directory] if flag == "--project" => Some(PathBuf::from(directory)),
        [argument] if argument.starts_with("--project=") => {
            let directory = argument
                .strip_prefix("--project=")
                .expect("checked project prefix");
            if directory.is_empty() {
                bail!("--project requires a directory");
            }
            Some(PathBuf::from(directory))
        }
        _ => bail!("expected <client> [--project <dir>]"),
    };
    if let Some(project) = project.as_deref() {
        if !project.is_dir() {
            bail!("project directory does not exist: {}", project.display());
        }
    }
    Ok((client, project))
}

pub(crate) fn run_setup(client: Client, project: Option<&Path>) -> Result<()> {
    let home = home_dir()?;
    let current_exe = env::current_exe().context("find the current executable")?;
    match client {
        Client::Codex => setup_codex(&home, project, &current_exe),
        Client::Claude => setup_claude(&home, project, &current_exe),
    }
}

pub(crate) fn run_unsetup(client: Client, project: Option<&Path>) -> Result<()> {
    let home = home_dir()?;
    match client {
        Client::Codex => unsetup_codex(&home, project),
        Client::Claude => unsetup_claude(&home, project),
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .context("find the user home directory")
}

fn setup_codex(home: &Path, project: Option<&Path>, current_exe: &Path) -> Result<()> {
    let config = home.join(".codex/config.toml");
    let existing = read_optional_utf8(&config)?;
    if has_external_codex_table(&existing)? {
        println!(
            "Left {} unchanged: [mcp_servers.crux] exists outside crux markers.",
            config.display()
        );
    } else {
        let block = codex_server_body(current_exe)?;
        let report = edit_file(&config, |content| insert_or_replace(content, &block))?;
        print_edit(&config, &report, "registered the crux MCP server");
    }

    let instructions = instruction_path(home, project, Client::Codex);
    let report = edit_file(&instructions, |content| {
        insert_or_replace(content, ADOPTION_BODY)
    })?;
    print_edit(&instructions, &report, "installed crux code index guidance");
    Ok(())
}

fn unsetup_codex(home: &Path, project: Option<&Path>) -> Result<()> {
    let config = home.join(".codex/config.toml");
    let report = edit_file(&config, remove_marker_block)?;
    print_removal(&config, &report, "crux MCP server registration");

    let instructions = instruction_path(home, project, Client::Codex);
    let report = edit_file(&instructions, remove_marker_block)?;
    print_removal(&instructions, &report, "crux code index guidance");
    Ok(())
}

fn setup_claude(home: &Path, project: Option<&Path>, current_exe: &Path) -> Result<()> {
    match run_claude(&[
        OsString::from("mcp"),
        OsString::from("add"),
        OsString::from("--scope"),
        OsString::from("user"),
        OsString::from("crux"),
        OsString::from("--"),
        current_exe.as_os_str().to_os_string(),
    ])? {
        ClaudeCommand::Absent => println!(
            "Claude CLI not found. Run: claude mcp add --scope user crux -- {}",
            shell_quote(current_exe)
        ),
        ClaudeCommand::Ran(output) if output.status.success() => {
            println!("Registered the crux MCP server with Claude Code.");
        }
        ClaudeCommand::Ran(output) if output_reports_existing_server(&output) => {
            println!("No change to the Claude Code crux MCP registration.");
        }
        ClaudeCommand::Ran(output) => {
            return Err(claude_failure("register the crux MCP server", &output));
        }
    }

    let instructions = instruction_path(home, project, Client::Claude);
    let report = edit_file(&instructions, |content| {
        insert_or_replace(content, ADOPTION_BODY)
    })?;
    print_edit(&instructions, &report, "installed crux code index guidance");
    Ok(())
}

fn unsetup_claude(home: &Path, project: Option<&Path>) -> Result<()> {
    match run_claude(&[
        OsString::from("mcp"),
        OsString::from("remove"),
        OsString::from("crux"),
        OsString::from("--scope"),
        OsString::from("user"),
    ])? {
        ClaudeCommand::Absent => {
            println!("Claude CLI not found. Run: claude mcp remove crux --scope user")
        }
        ClaudeCommand::Ran(output) if output.status.success() => {
            println!("Removed the crux MCP server from Claude Code.")
        }
        ClaudeCommand::Ran(output) if output_reports_missing_server(&output) => {
            println!("No Claude Code crux MCP registration found.")
        }
        ClaudeCommand::Ran(output) => {
            return Err(claude_failure("remove the crux MCP server", &output));
        }
    }

    let instructions = instruction_path(home, project, Client::Claude);
    let report = edit_file(&instructions, remove_marker_block)?;
    print_removal(&instructions, &report, "crux code index guidance");
    Ok(())
}

fn instruction_path(home: &Path, project: Option<&Path>, client: Client) -> PathBuf {
    match (client, project) {
        (Client::Codex, Some(project)) => project.join("AGENTS.md"),
        (Client::Codex, None) => home.join(".codex/AGENTS.md"),
        (Client::Claude, Some(project)) => project.join("CLAUDE.md"),
        (Client::Claude, None) => home.join(".claude/CLAUDE.md"),
    }
}

fn codex_server_body(current_exe: &Path) -> Result<String> {
    if !current_exe.is_absolute() {
        bail!(
            "the current executable path is not absolute: {}",
            current_exe.display()
        );
    }
    let command = serde_json::to_string(&current_exe.to_string_lossy())
        .context("encode the current executable path")?;
    Ok(format!(
        "[mcp_servers.crux]\ncommand = {command}\nargs = []"
    ))
}

fn has_external_codex_table(content: &str) -> Result<bool> {
    let content_without_markers = match marker_span(content)? {
        Some(span) => format!("{}{}", &content[..span.start], &content[span.end..]),
        None => content.to_string(),
    };
    Ok(content_without_markers.lines().any(|line| {
        let compact = line
            .trim()
            .chars()
            .filter(|character| !matches!(character, ' ' | '\t'))
            .collect::<String>();
        compact == "[mcp_servers.crux]" || compact.starts_with("[mcp_servers.crux]#")
    }))
}

fn insert_or_replace(content: &str, body: &str) -> Result<String> {
    if let Some(span) = marker_span(content)? {
        let block = render_block(body, span.newline);
        return Ok(format!(
            "{}{}{}",
            &content[..span.start],
            block,
            &content[span.end..]
        ));
    }

    let newline = preferred_newline(content);
    let block = render_block(body, newline);
    if content.is_empty() {
        Ok(format!("{block}{newline}"))
    } else {
        Ok(format!("{content}{newline}{block}{newline}"))
    }
}

fn remove_marker_block(content: &str) -> Result<String> {
    let Some(span) = marker_span(content)? else {
        return Ok(content.to_string());
    };
    let start = preceding_newline_start(content, span.start);
    let end = following_newline_end(content, span.end);
    Ok(format!("{}{}", &content[..start], &content[end..]))
}

fn render_block(body: &str, newline: &str) -> String {
    let body = body.replace('\n', newline);
    format!("{BEGIN_MARKER}{newline}{body}{newline}{END_MARKER}")
}

fn marker_span(content: &str) -> Result<Option<MarkerSpan>> {
    let begins = marker_lines(content, BEGIN_MARKER);
    let ends = marker_lines(content, END_MARKER);
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([_], [_]) if begins[0].0 < ends[0].0 => Ok(Some(MarkerSpan {
            start: begins[0].0,
            end: ends[0].0 + END_MARKER.len(),
            newline: begins[0].1,
        })),
        ([], _) => bail!("found a crux end marker without a begin marker"),
        (_, []) => bail!("found a crux begin marker without an end marker"),
        _ => bail!("found malformed or duplicate crux marker blocks"),
    }
}

fn marker_lines(content: &str, marker: &str) -> Vec<(usize, &'static str)> {
    let mut matches = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let (text, newline) = if let Some(text) = line.strip_suffix("\r\n") {
            (text, "\r\n")
        } else if let Some(text) = line.strip_suffix('\n') {
            (text, "\n")
        } else {
            (line, preferred_newline(content))
        };
        if text == marker {
            matches.push((offset, newline));
        }
        offset += line.len();
    }
    matches
}

fn preferred_newline(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn preceding_newline_start(content: &str, offset: usize) -> usize {
    if content[..offset].ends_with("\r\n") {
        offset - 2
    } else if content[..offset].ends_with('\n') {
        offset - 1
    } else {
        offset
    }
}

fn following_newline_end(content: &str, offset: usize) -> usize {
    if content[offset..].starts_with("\r\n") {
        offset + 2
    } else if content[offset..].starts_with('\n') {
        offset + 1
    } else {
        offset
    }
}

fn edit_file(path: &Path, transform: impl FnOnce(&str) -> Result<String>) -> Result<EditReport> {
    let existed = path.exists();
    let content = read_optional_utf8(path)?;
    let updated = transform(&content)?;
    if updated == content {
        return Ok(EditReport {
            backup_created: false,
            changed: false,
            existed,
        });
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let backup_created = create_backup_once(path, content.as_bytes())?;
    fs::write(path, updated.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(EditReport {
        backup_created,
        changed: true,
        existed,
    })
}

fn read_optional_utf8(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".crux-backup");
    PathBuf::from(backup)
}

fn create_backup_once(path: &Path, content: &[u8]) -> Result<bool> {
    let backup = backup_path(path);
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("create {}", backup.display()));
        }
    };
    file.write_all(content)
        .with_context(|| format!("write {}", backup.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", backup.display()))?;
    Ok(true)
}

fn print_edit(path: &Path, report: &EditReport, change: &str) {
    print_backup(path, report);
    if report.changed {
        let verb = if report.existed { "Updated" } else { "Created" };
        println!("{verb} {}: {change}.", path.display());
    } else {
        println!("No change to {}.", path.display());
    }
}

fn print_removal(path: &Path, report: &EditReport, change: &str) {
    print_backup(path, report);
    if report.changed {
        println!("Updated {}: removed {change}.", path.display());
    } else {
        println!("No {change} found in {}.", path.display());
    }
}

fn print_backup(path: &Path, report: &EditReport) {
    if report.backup_created {
        println!("Created backup {}.", backup_path(path).display());
    }
}

fn run_claude(arguments: &[OsString]) -> Result<ClaudeCommand> {
    match Command::new("claude").args(arguments).output() {
        Ok(output) => Ok(ClaudeCommand::Ran(output)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ClaudeCommand::Absent),
        Err(error) => Err(error).context("run the Claude CLI"),
    }
}

fn claude_failure(action: &str, output: &Output) -> anyhow::Error {
    let details = command_output_text(output);
    if details.is_empty() {
        anyhow!("Claude Code could not {action}")
    } else {
        anyhow!("Claude Code could not {action}: {details}")
    }
}

fn command_output_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_reports_missing_server(output: &Output) -> bool {
    text_reports_missing_server(&command_output_text(output))
}

fn text_reports_missing_server(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    output.contains("not found")
        || output.contains("does not exist")
        || output.contains("no mcp server")
}

fn output_reports_existing_server(output: &Output) -> bool {
    text_reports_existing_server(&command_output_text(output))
}

fn text_reports_existing_server(output: &str) -> bool {
    output.to_ascii_lowercase().contains("already exists")
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

#[cfg(not(unix))]
fn shell_quote(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap_or_else(|_| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestProject;

    #[test]
    fn marker_insert_and_remove_round_trip_lf_bytes() {
        for original in ["", "user content", "user content\n", "user content\n\n"] {
            let installed = insert_or_replace(original, "new body").expect("insert marker block");
            let removed = remove_marker_block(&installed).expect("remove marker block");
            assert_eq!(removed, original);
        }
    }

    #[test]
    fn marker_replace_preserves_user_content_before_and_after() {
        let fixture = format!(
            "before\n\n{}\nold body\n{}\n\nafter\n",
            BEGIN_MARKER, END_MARKER
        );
        let replaced = insert_or_replace(&fixture, "new body").expect("replace marker block");
        assert_eq!(
            replaced,
            format!(
                "before\n\n{}\nnew body\n{}\n\nafter\n",
                BEGIN_MARKER, END_MARKER
            )
        );
        let removed = remove_marker_block(&replaced).expect("remove marker block");
        assert_eq!(removed, "before\n\nafter\n");
    }

    #[test]
    fn marker_round_trip_tolerates_crlf() {
        let original = "first\r\nsecond\r\n";
        let installed = insert_or_replace(original, "line one\nline two").expect("insert block");
        assert!(installed.contains(&format!(
            "{BEGIN_MARKER}\r\nline one\r\nline two\r\n{END_MARKER}"
        )));
        let replaced = insert_or_replace(&installed, "replacement").expect("replace block");
        assert!(replaced.contains(&format!("{BEGIN_MARKER}\r\nreplacement\r\n{END_MARKER}")));
        assert_eq!(
            remove_marker_block(&replaced).expect("remove block"),
            original
        );
    }

    #[test]
    fn marker_replacement_is_idempotent() {
        let once = insert_or_replace("user", "body").expect("first insert");
        let twice = insert_or_replace(&once, "body").expect("second insert");
        assert_eq!(twice, once);
    }

    #[test]
    fn backup_is_created_once_and_keeps_the_earliest_bytes() {
        let project = TestProject::new();
        let path = project.root.join("AGENTS.md");
        fs::write(&path, "original").expect("write fixture");

        let first =
            edit_file(&path, |content| insert_or_replace(content, "first")).expect("first edit");
        assert!(first.backup_created);
        assert_eq!(
            fs::read_to_string(backup_path(&path)).expect("read backup"),
            "original"
        );

        let second =
            edit_file(&path, |content| insert_or_replace(content, "second")).expect("second edit");
        assert!(!second.backup_created);
        assert_eq!(
            fs::read_to_string(backup_path(&path)).expect("read backup"),
            "original"
        );
    }

    #[test]
    fn current_executable_path_lands_in_the_codex_block() {
        let current_exe = env::current_exe().expect("current test executable");
        let body = codex_server_body(&current_exe).expect("codex server body");
        let encoded = serde_json::to_string(&current_exe.to_string_lossy()).expect("encoded path");
        assert!(body.contains(&format!("command = {encoded}")));
        assert!(body.contains("args = []"));
    }

    #[test]
    fn unsetup_on_a_clean_file_is_a_no_op() {
        let project = TestProject::new();
        let path = project.root.join("CLAUDE.md");
        fs::write(&path, "user content\r\n").expect("write fixture");

        let report = edit_file(&path, remove_marker_block).expect("clean removal");
        assert!(!report.changed);
        assert!(!backup_path(&path).exists());
        assert_eq!(
            fs::read_to_string(path).expect("read fixture"),
            "user content\r\n"
        );
    }

    #[test]
    fn external_codex_table_is_detected_outside_our_markers() {
        let managed = insert_or_replace("title = 'user'", "[mcp_servers.crux]\ncommand = '/crux'")
            .expect("managed block");
        assert!(!has_external_codex_table(&managed).expect("managed table check"));

        let external = format!("[mcp_servers.crux]\ncommand = '/user'\n{managed}");
        assert!(has_external_codex_table(&external).expect("external table check"));
    }

    #[test]
    fn new_file_gets_an_empty_first_backup() {
        let project = TestProject::new();
        let path = project.root.join("new/AGENTS.md");
        let report =
            edit_file(&path, |content| insert_or_replace(content, "body")).expect("create file");
        assert!(report.backup_created);
        assert_eq!(fs::read(backup_path(&path)).expect("read backup"), b"");
    }

    #[test]
    fn claude_command_results_detect_safe_no_op_states() {
        assert!(text_reports_existing_server(
            "MCP server crux already exists in user config"
        ));
        assert!(text_reports_missing_server("No MCP server named crux."));
        assert!(!text_reports_existing_server("permission denied"));
        assert!(!text_reports_missing_server("permission denied"));
    }
}

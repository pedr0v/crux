use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::prepare::atomic_write_preserving;

const BEGIN_MARKER: &str = "# --- crux begin ---";
const END_MARKER: &str = "# --- crux end ---";
const ADOPTION_BODY: &str = "## crux code index\n\nThis project may have a crux code index.\n\nUse `scip_architecture` once for explicit architecture questions, module or package dependencies, cycles, or unclear scope before a change across files. Pass a known project-relative file or directory as `scope`. Reuse relevant results until the index changes. Use `scip_map` for named-symbol references and callers. Use `scip_outline` for one file's symbols. Use `scip_find` for fragments or unreferenced audits. Prefer `rg` for simple text or definition search. Do not require architecture before every edit or symbol query. If a Crux result is empty, use repository-native search and stop equivalent retries.";
const CODEX_REQUIRED_KEYS: [(&str, &str); 3] = [
    ("required", "required = true"),
    ("startup_timeout_sec", "startup_timeout_sec = 30"),
    ("omit_tools_from", "omit_tools_from = [\"deferred\"]"),
];

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

#[derive(Debug, Eq, PartialEq)]
struct TomlTableSpan {
    start: usize,
    body_start: usize,
    end: usize,
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
    let instructions = instruction_path(&home, project, client);
    match client {
        Client::Codex => setup_codex(
            &codex_config_path(&home, env::var_os("CODEX_HOME")),
            &instructions,
            &current_exe,
        ),
        Client::Claude => setup_claude(&instructions, &current_exe, &claude_programs()),
    }
}

pub(crate) fn run_unsetup(client: Client, project: Option<&Path>) -> Result<()> {
    let home = home_dir()?;
    let instructions = instruction_path(&home, project, client);
    match client {
        Client::Codex => unsetup_codex(
            &codex_config_path(&home, env::var_os("CODEX_HOME")),
            &instructions,
        ),
        Client::Claude => unsetup_claude(&instructions, &claude_programs()),
    }
}

fn home_dir() -> Result<PathBuf> {
    home_from(env::var_os("HOME"), env::var_os("USERPROFILE"))
        .context("find the user home directory")
}

fn home_from(home: Option<OsString>, user_profile: Option<OsString>) -> Option<PathBuf> {
    home.filter(|value| !value.is_empty())
        .or_else(|| user_profile.filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn setup_codex(config: &Path, instructions: &Path, current_exe: &Path) -> Result<()> {
    let report = edit_file(config, |content| install_codex_config(content, current_exe))?;
    print_edit(config, &report, "configured the crux MCP server");

    let report = edit_file(instructions, |content| {
        insert_or_replace(content, ADOPTION_BODY)
    })?;
    print_edit(instructions, &report, "installed crux code index guidance");
    Ok(())
}

fn unsetup_codex(config: &Path, instructions: &Path) -> Result<()> {
    let report = edit_file(config, remove_marker_block)?;
    print_removal(config, &report, "crux MCP server configuration");

    let report = edit_file(instructions, remove_marker_block)?;
    print_removal(instructions, &report, "crux code index guidance");
    Ok(())
}

fn setup_claude(instructions: &Path, current_exe: &Path, programs: &[OsString]) -> Result<()> {
    match run_claude(
        programs,
        &[
            OsString::from("mcp"),
            OsString::from("add"),
            OsString::from("--scope"),
            OsString::from("user"),
            OsString::from("crux"),
            OsString::from("--"),
            current_exe.as_os_str().to_os_string(),
        ],
    )? {
        ClaudeCommand::Absent => println!(
            "Claude CLI not found. Run: claude mcp add --scope user crux -- {}",
            shell_quote(current_exe)
        ),
        ClaudeCommand::Ran(output) if output.status.success() => {
            println!("Registered the crux MCP server with Claude Code.");
        }
        ClaudeCommand::Ran(output) if output_reports_existing_server(&output) => {
            let details = claude_registration_details(programs);
            println!("{}", existing_registration_message(&details, current_exe));
        }
        ClaudeCommand::Ran(output) => {
            return Err(claude_failure("register the crux MCP server", &output));
        }
    }

    let report = edit_file(instructions, |content| {
        insert_or_replace(content, ADOPTION_BODY)
    })?;
    print_edit(instructions, &report, "installed crux code index guidance");
    Ok(())
}

fn unsetup_claude(instructions: &Path, programs: &[OsString]) -> Result<()> {
    match run_claude(
        programs,
        &[
            OsString::from("mcp"),
            OsString::from("remove"),
            OsString::from("crux"),
            OsString::from("--scope"),
            OsString::from("user"),
        ],
    )? {
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

    let report = edit_file(instructions, remove_marker_block)?;
    print_removal(instructions, &report, "crux code index guidance");
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

fn codex_config_path(home: &Path, codex_home: Option<OsString>) -> PathBuf {
    codex_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
        .join("config.toml")
}

/// Encode the executable path as the TOML value of the `command` key.
fn codex_command_value(current_exe: &Path) -> Result<String> {
    if !current_exe.is_absolute() {
        bail!(
            "the current executable path is not absolute: {}",
            current_exe.display()
        );
    }
    serde_json::to_string(&current_exe.to_string_lossy())
        .context("encode the current executable path")
}

fn codex_server_body(current_exe: &Path) -> Result<String> {
    let command = codex_command_value(current_exe)?;
    Ok(format!(
        "[mcp_servers.crux]\ncommand = {command}\nargs = []\n{}",
        CODEX_REQUIRED_KEYS
            .iter()
            .map(|(_, line)| *line)
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn install_codex_config(content: &str, current_exe: &Path) -> Result<String> {
    let marker = marker_span(content)?;
    let Some(table) = codex_table_span(content)? else {
        if marker.is_some() {
            bail!("found crux markers without an [mcp_servers.crux] section");
        }
        return insert_or_replace(content, &codex_server_body(current_exe)?);
    };

    if let Some(marker) = &marker {
        // The block owns the table when the block holds the table header.
        // Crux then keeps the `command` key in step with the current executable.
        if marker.start <= table.start && table.start < marker.end {
            let body = &content[table.body_start..marker.end.min(table.end)];
            let command = codex_command_value(current_exe)?;
            if toml_key_value(body, "command") != Some(command.as_str()) {
                return insert_or_replace(content, &codex_server_body(current_exe)?);
            }
        }
    }

    let missing = missing_codex_keys(&content[table.body_start..table.end]);
    if missing.is_empty() {
        return Ok(content.to_string());
    }
    let body = missing.join("\n");

    if let Some(marker) = marker {
        if marker.start > table.end || marker.end < table.start {
            bail!("found crux markers outside the [mcp_servers.crux] section");
        }
        return insert_into_marker_block(content, &marker, &body);
    }

    Ok(insert_marker_block_at(content, table.end, &body))
}

fn codex_table_span(content: &str) -> Result<Option<TomlTableSpan>> {
    let mut target: Option<TomlTableSpan> = None;
    let mut offset = 0;
    // The compact name of the table that holds the current line.
    // The root table has an empty name. Array tables have no name.
    let mut current_table: Option<String> = Some(String::new());

    for line in content.split_inclusive('\n') {
        let line_end = offset + line.len();
        if let Some(name) = toml_table_name(line) {
            if let Some(table) = target.as_mut() {
                if table.end == content.len() {
                    table.end = offset;
                }
            }
            let compact = compact_toml_name(name);
            if compact == "mcp_servers.crux" {
                if target.is_some() {
                    bail!("found duplicate [mcp_servers.crux] sections");
                }
                target = Some(TomlTableSpan {
                    start: offset,
                    body_start: line_end,
                    end: content.len(),
                });
            }
            current_table = (!name.is_empty()).then_some(compact);
        } else if let Some(table) = current_table.as_deref() {
            if defines_crux_entry(table, line) {
                bail!(
                    "found an existing crux entry under [mcp_servers] (inline table or dotted key); crux cannot edit it; replace it with a [mcp_servers.crux] section or remove it, then run crux setup codex again"
                );
            }
        }
        offset = line_end;
    }

    Ok(target)
}

/// Report a key line that defines `mcp_servers.crux` without a table header.
/// Only the root table and the `[mcp_servers]` table can hold such a line.
fn defines_crux_entry(table: &str, line: &str) -> bool {
    if !table.is_empty() && table != "mcp_servers" {
        return false;
    }
    let line = line.trim();
    if line.starts_with('#') {
        return false;
    }
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    let key = compact_toml_name(key);
    let path = if table.is_empty() {
        key
    } else {
        format!("{table}.{key}")
    };
    if path == "mcp_servers" {
        // A root inline table: mcp_servers = { crux = { ... } }
        return inline_table_defines(value, "crux");
    }
    path == "mcp_servers.crux" || path.starts_with("mcp_servers.crux.")
}

/// Report if an inline table text holds `key` at its top level.
/// The scan is simple: it splits on commas and reads the key before `=`.
fn inline_table_defines(inline: &str, key: &str) -> bool {
    let inline = inline.trim();
    if !inline.starts_with('{') {
        return false;
    }
    inline
        .trim_start_matches('{')
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, _)| compact_toml_name(name))
        .any(|name| name == key || name.starts_with(&format!("{key}.")))
}

fn toml_table_name(line: &str) -> Option<&str> {
    let line = line.trim_end_matches(['\r', '\n']).trim();
    if !line.starts_with('[') {
        return None;
    }
    let closing = line.find(']')?;
    let suffix = line[closing + 1..].trim_start();
    if !suffix.is_empty() && !suffix.starts_with('#') {
        return None;
    }
    let name = &line[1..closing];
    if name.starts_with('[') || name.ends_with(']') {
        return Some("");
    }
    Some(name.trim())
}

/// Remove blanks outside quotes from a dotted TOML key.
/// Also unquote each part that is a valid bare key.
/// With this, `mcp_servers . "crux"` and `mcp_servers.crux` give the same name.
fn compact_toml_name(name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut part = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in name.chars() {
        if let Some(open) = quote {
            part.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' && open == '"' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            ' ' | '\t' => {}
            '"' | '\'' => {
                quote = Some(character);
                part.push(character);
            }
            '.' => parts.push(std::mem::take(&mut part)),
            _ => part.push(character),
        }
    }
    parts.push(part);
    parts
        .iter()
        .map(|part| unquote_bare_key(part))
        .collect::<Vec<_>>()
        .join(".")
}

fn unquote_bare_key(part: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = part
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            let bare = !inner.is_empty()
                && inner.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                });
            if bare {
                return inner;
            }
        }
    }
    part
}

fn missing_codex_keys(table_body: &str) -> Vec<&'static str> {
    CODEX_REQUIRED_KEYS
        .iter()
        .filter_map(|(key, line)| (!toml_key_exists(table_body, key)).then_some(*line))
        .collect()
}

fn toml_key_exists(table_body: &str, key: &str) -> bool {
    toml_key_value(table_body, key).is_some()
}

/// Return the text after `key =` on the first line that sets `key`.
fn toml_key_value<'a>(table_body: &'a str, key: &str) -> Option<&'a str> {
    table_body.lines().find_map(|line| {
        let suffix = line
            .trim_start()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?;
        Some(suffix.trim())
    })
}

fn insert_into_marker_block(content: &str, marker: &MarkerSpan, body: &str) -> Result<String> {
    let end_marker_start = marker
        .end
        .checked_sub(END_MARKER.len())
        .context("find the crux end marker")?;
    let body = body.replace('\n', marker.newline);
    Ok(format!(
        "{}{}{}{}",
        &content[..end_marker_start],
        body,
        marker.newline,
        &content[end_marker_start..]
    ))
}

fn insert_marker_block_at(content: &str, offset: usize, body: &str) -> String {
    let newline = preferred_newline(content);
    let block = render_block(body, newline);
    format!(
        "{}{}{}{}{}",
        &content[..offset],
        newline,
        block,
        newline,
        &content[offset..]
    )
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
    atomic_write_preserving(path, updated.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
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

/// List the program names that can start the Claude CLI, in the order to try.
/// On Windows, npm installs the CLI as `claude.cmd`. Rust finds only `.exe` files by bare name.
fn claude_programs() -> Vec<OsString> {
    if cfg!(windows) {
        vec![OsString::from("claude"), OsString::from("claude.cmd")]
    } else {
        vec![OsString::from("claude")]
    }
}

/// Run the first program that exists. Report the CLI as absent only when no program exists.
fn run_claude(programs: &[OsString], arguments: &[OsString]) -> Result<ClaudeCommand> {
    for program in programs {
        match Command::new(program).args(arguments).output() {
            Ok(output) => return Ok(ClaudeCommand::Ran(output)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("run the Claude CLI"),
        }
    }
    Ok(ClaudeCommand::Absent)
}

/// Read the current crux registration. Return an empty text when the CLI cannot show it.
fn claude_registration_details(programs: &[OsString]) -> String {
    let arguments = [
        OsString::from("mcp"),
        OsString::from("get"),
        OsString::from("crux"),
    ];
    match run_claude(programs, &arguments) {
        Ok(ClaudeCommand::Ran(output)) if output.status.success() => command_output_text(&output),
        _ => String::new(),
    }
}

/// Report if the registration details name the current executable.
/// The path must match a whole token. A prefix match is not enough,
/// because `crux-0.10` and `crux.old` share a prefix with `crux`.
fn registration_uses_executable(details: &str, current_exe: &Path) -> bool {
    let current = current_exe.to_string_lossy();
    details
        .split(|character: char| character.is_whitespace() || matches!(character, '"' | '\''))
        .any(|token| token == current)
}

/// Describe an existing registration. Crux keeps it, but tells the user when it is stale.
fn existing_registration_message(details: &str, current_exe: &Path) -> String {
    if details.is_empty() {
        return format!(
            "Claude Code reports an existing crux MCP registration. Crux kept it.\nTo register this binary instead, run these commands:\n  claude mcp remove crux --scope user\n  claude mcp add --scope user crux -- {}",
            shell_quote(current_exe)
        );
    }
    if registration_uses_executable(details, current_exe) {
        return "No change to the Claude Code crux MCP registration.".to_string();
    }
    format!(
        "The Claude Code crux MCP registration does not point to this binary. Crux kept it.\n{details}\nTo use this binary, run these commands:\n  claude mcp remove crux --scope user\n  claude mcp add --scope user crux -- {}",
        shell_quote(current_exe)
    )
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
    fn adoption_guidance_routes_queries_and_stops_empty_retries() {
        assert!(ADOPTION_BODY.contains("Use `scip_architecture` once"));
        assert!(ADOPTION_BODY.contains("Pass a known project-relative file or directory"));
        assert!(ADOPTION_BODY.contains("Use `scip_map` for named-symbol references and callers"));
        assert!(ADOPTION_BODY.contains("Use `scip_outline` for one file's symbols"));
        assert!(ADOPTION_BODY.contains("Use `scip_find` for fragments"));
        assert!(ADOPTION_BODY.contains("Prefer `rg` for simple text or definition search"));
        assert!(ADOPTION_BODY.contains("Do not require architecture before every edit"));
        assert!(ADOPTION_BODY.contains("stop equivalent retries"));
    }

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
        assert!(body.contains("required = true"));
        assert!(body.contains("startup_timeout_sec = 30"));
        assert!(body.contains("omit_tools_from = [\"deferred\"]"));
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
    fn fresh_codex_config_creates_a_complete_managed_section() {
        let current_exe = env::current_exe().expect("current test executable");
        let original = "model = \"gpt-5\"\n";
        let installed = install_codex_config(original, &current_exe).expect("install config");
        let encoded = serde_json::to_string(&current_exe.to_string_lossy()).expect("encoded path");

        assert_eq!(
            installed,
            format!(
                "model = \"gpt-5\"\n\n{BEGIN_MARKER}\n[mcp_servers.crux]\ncommand = {encoded}\nargs = []\nrequired = true\nstartup_timeout_sec = 30\nomit_tools_from = [\"deferred\"]\n{END_MARKER}\n"
            )
        );
        assert_eq!(
            install_codex_config(&installed, &current_exe).expect("repeat config install"),
            installed
        );
    }

    #[test]
    fn existing_codex_section_adds_only_the_missing_key() {
        let current_exe = env::current_exe().expect("current test executable");
        let original = "model = \"gpt-5\"\n\n[mcp_servers.crux]\ncommand = \"/user/crux\"\nargs = [\"--profile\", \"full\"]\nrequired = false\nstartup_timeout_sec = 7\n\n[features]\napps = true\n";
        let installed = install_codex_config(original, &current_exe).expect("install config");

        assert_eq!(
            installed,
            "model = \"gpt-5\"\n\n[mcp_servers.crux]\ncommand = \"/user/crux\"\nargs = [\"--profile\", \"full\"]\nrequired = false\nstartup_timeout_sec = 7\n\n\n# --- crux begin ---\nomit_tools_from = [\"deferred\"]\n# --- crux end ---\n[features]\napps = true\n"
        );
        assert_eq!(installed.matches("required =").count(), 1);
        assert_eq!(installed.matches("startup_timeout_sec =").count(), 1);
        assert_eq!(installed.matches("omit_tools_from =").count(), 1);
        assert!(installed.contains("command = \"/user/crux\""));
        assert!(installed.contains("args = [\"--profile\", \"full\"]"));
        assert_eq!(
            install_codex_config(&installed, &current_exe).expect("repeat config install"),
            installed
        );
    }

    #[test]
    fn codex_config_unsetup_restores_created_and_extended_sections() {
        let current_exe = env::current_exe().expect("current test executable");
        let originals = [
            "model = \"gpt-5\"\n",
            "[mcp_servers.crux]\r\ncommand = \"C:\\\\crux.exe\"\r\nargs = []\r\nrequired = true\r\nstartup_timeout_sec = 30\r\n",
        ];

        for original in originals {
            let installed =
                install_codex_config(original, &current_exe).expect("install config fixture");
            let restored = remove_marker_block(&installed).expect("unsetup config fixture");
            assert_eq!(restored, original);
        }
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

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn os(value: &str) -> OsString {
        OsString::from(value)
    }

    fn encoded(path: &Path) -> String {
        serde_json::to_string(&path.to_string_lossy()).expect("encoded path")
    }

    #[test]
    fn parse_client_arguments_accepts_each_client_without_a_project() {
        for (name, client) in [("codex", Client::Codex), ("claude", Client::Claude)] {
            assert_eq!(
                parse_client_arguments(&arguments(&[name])).expect("parse client"),
                (client, None)
            );
        }
    }

    #[test]
    fn parse_client_arguments_accepts_both_project_spellings() {
        let project = TestProject::new();
        let directory = project.root.to_string_lossy().into_owned();

        let separate =
            parse_client_arguments(&arguments(&["codex", "--project", &directory])).expect("flag");
        assert_eq!(separate, (Client::Codex, Some(project.root.clone())));

        let joined =
            parse_client_arguments(&arguments(&["claude", &format!("--project={directory}")]))
                .expect("joined flag");
        assert_eq!(joined, (Client::Claude, Some(project.root.clone())));
    }

    #[test]
    fn parse_client_arguments_rejects_bad_input() {
        let project = TestProject::new();
        let missing = project.root.join("missing").to_string_lossy().into_owned();
        let file = project.root.join("file.txt");
        fs::write(&file, "text").expect("write fixture");
        let file = file.to_string_lossy().into_owned();
        let cases = [
            (arguments(&[]), "a client is required"),
            (arguments(&["vim"]), "unsupported client 'vim'"),
            (
                arguments(&["codex", "--project="]),
                "--project requires a directory",
            ),
            (
                arguments(&["codex", "--project", ""]),
                "project directory does not exist",
            ),
            (
                arguments(&["codex", "--project", &missing]),
                "project directory does not exist",
            ),
            (
                arguments(&["codex", &format!("--project={missing}")]),
                "project directory does not exist",
            ),
            (
                arguments(&["codex", "--project", &file]),
                "project directory does not exist",
            ),
            (arguments(&["codex", "--project"]), "expected <client>"),
            (arguments(&["codex", "--other", "x"]), "expected <client>"),
            (arguments(&["codex", "extra"]), "expected <client>"),
        ];
        for (input, message) in cases {
            let error = parse_client_arguments(&input)
                .expect_err("reject input")
                .to_string();
            assert!(error.contains(message), "{input:?}: {error}");
        }
    }

    #[test]
    fn instruction_paths_follow_the_client_and_the_project() {
        let home = Path::new("/home/user");
        let project = Path::new("/work/app");
        assert_eq!(
            instruction_path(home, None, Client::Codex),
            home.join(".codex/AGENTS.md")
        );
        assert_eq!(
            instruction_path(home, None, Client::Claude),
            home.join(".claude/CLAUDE.md")
        );
        assert_eq!(
            instruction_path(home, Some(project), Client::Codex),
            project.join("AGENTS.md")
        );
        assert_eq!(
            instruction_path(home, Some(project), Client::Claude),
            project.join("CLAUDE.md")
        );
    }

    #[test]
    fn codex_config_path_honors_codex_home() {
        let home = Path::new("/home/user");
        let default = home.join(".codex").join("config.toml");
        assert_eq!(codex_config_path(home, None), default);
        assert_eq!(codex_config_path(home, Some(os(""))), default);
        assert_eq!(
            codex_config_path(home, Some(os("/custom"))),
            Path::new("/custom").join("config.toml")
        );
    }

    #[test]
    fn home_directory_falls_back_to_userprofile() {
        assert_eq!(
            home_from(Some(os("/home/user")), Some(os("C:\\Users\\user"))),
            Some(PathBuf::from("/home/user"))
        );
        assert_eq!(
            home_from(None, Some(os("C:\\Users\\user"))),
            Some(PathBuf::from("C:\\Users\\user"))
        );
        assert_eq!(
            home_from(Some(os("")), Some(os("C:\\Users\\user"))),
            Some(PathBuf::from("C:\\Users\\user"))
        );
        assert_eq!(home_from(Some(os("")), None), None);
        assert_eq!(home_from(None, Some(os(""))), None);
    }

    #[test]
    fn rerunning_codex_setup_refreshes_a_stale_executable_path() {
        let project = TestProject::new();
        let first = project.root.join("a/crux");
        let second = project.root.join("b/crux");
        let original = "model = \"gpt-5\"\n";

        let installed = install_codex_config(original, &first).expect("first install");
        let moved = install_codex_config(&installed, &second).expect("second install");

        assert!(moved.contains(&format!("command = {}", encoded(&second))));
        assert!(!moved.contains(&encoded(&first)));
        assert_eq!(moved.matches("[mcp_servers.crux]").count(), 1);
        assert_eq!(moved.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(
            moved,
            install_codex_config(original, &second).expect("fresh install")
        );
        assert_eq!(
            install_codex_config(&moved, &second).expect("repeat install"),
            moved
        );
        assert_eq!(remove_marker_block(&moved).expect("unsetup"), original);
    }

    #[test]
    fn codex_refresh_keeps_line_endings_and_later_content() {
        let project = TestProject::new();
        let first = project.root.join("a/crux");
        let second = project.root.join("b/crux");
        let original = "model = \"gpt-5\"\r\n";
        let installed = install_codex_config(original, &first).expect("first install");
        let installed = format!("{installed}\r\n[features]\r\napps = true\r\n");

        let moved = install_codex_config(&installed, &second).expect("second install");

        assert!(moved.contains(&encoded(&second)));
        assert!(!moved.contains(&encoded(&first)));
        assert!(moved.ends_with("\r\n[features]\r\napps = true\r\n"));
        assert!(!moved.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn codex_refresh_leaves_user_owned_tables_alone() {
        let project = TestProject::new();
        let first = project.root.join("a/crux");
        let second = project.root.join("b/crux");
        let original = "[mcp_servers.crux]\ncommand = \"/user/crux\"\nargs = []\nrequired = true\nstartup_timeout_sec = 30\n";

        let extended = install_codex_config(original, &first).expect("first install");
        let again = install_codex_config(&extended, &second).expect("second install");

        assert_eq!(again, extended);
        assert!(again.contains("command = \"/user/crux\""));
    }

    #[test]
    fn codex_setup_and_unsetup_work_on_explicit_paths() {
        let project = TestProject::new();
        let config = project.root.join("codex-home/config.toml");
        let agents = project.root.join("work/AGENTS.md");
        let first = project.root.join("a/crux");
        let second = project.root.join("b/crux");
        fs::create_dir_all(config.parent().expect("config parent")).expect("create directory");
        fs::write(&config, "model = \"gpt-5\"\n").expect("write fixture");

        setup_codex(&config, &agents, &first).expect("first setup");
        setup_codex(&config, &agents, &second).expect("second setup");

        let text = fs::read_to_string(&config).expect("read config");
        assert!(text.contains(&encoded(&second)));
        assert!(!text.contains(&encoded(&first)));
        assert_eq!(
            fs::read_to_string(backup_path(&config)).expect("read backup"),
            "model = \"gpt-5\"\n"
        );
        assert!(fs::read_to_string(&agents)
            .expect("read agents")
            .contains(BEGIN_MARKER));

        unsetup_codex(&config, &agents).expect("unsetup");
        assert_eq!(
            fs::read_to_string(&config).expect("read config"),
            "model = \"gpt-5\"\n"
        );
        assert_eq!(fs::read_to_string(&agents).expect("read agents"), "");
    }

    #[test]
    fn quoted_crux_table_names_match_the_bare_name() {
        let current_exe = env::current_exe().expect("current test executable");
        let headers = [
            "[mcp_servers.\"crux\"]",
            "[mcp_servers.'crux']",
            "[ mcp_servers . \"crux\" ]",
            "[\"mcp_servers\".crux]",
        ];
        for header in headers {
            let complete = format!(
                "{header}\ncommand = \"/user/crux\"\nargs = []\nrequired = true\nstartup_timeout_sec = 30\nomit_tools_from = [\"deferred\"]\n"
            );
            assert_eq!(
                install_codex_config(&complete, &current_exe).expect("complete table"),
                complete,
                "{header}"
            );

            let partial = format!("{header}\ncommand = \"/user/crux\"\n");
            let installed = install_codex_config(&partial, &current_exe).expect("partial table");
            assert_eq!(installed.matches("mcp_servers").count(), 1, "{header}");
            assert!(installed.starts_with(&partial), "{header}");
            assert!(installed.contains(BEGIN_MARKER), "{header}");
            assert!(installed.contains("required = true"), "{header}");
            assert_eq!(
                install_codex_config(&installed, &current_exe).expect("repeat install"),
                installed,
                "{header}"
            );
        }
    }

    #[test]
    fn defines_crux_entry_finds_a_root_inline_mcp_servers_table() {
        assert!(defines_crux_entry(
            "",
            r#"mcp_servers = { crux = { command = "/old/crux", args = [] } }"#
        ));
        assert!(defines_crux_entry(
            "",
            r#"mcp_servers = { other = { command = "x" }, "crux" = { command = "y" } }"#
        ));
        assert!(!defines_crux_entry(
            "",
            r#"mcp_servers = { other = { command = "x" } }"#
        ));
        assert!(!defines_crux_entry("", r#"mcp_servers = "not a table""#));
    }

    #[test]
    fn compact_toml_name_unquotes_only_bare_key_parts() {
        assert_eq!(compact_toml_name("mcp_servers.crux"), "mcp_servers.crux");
        assert_eq!(
            compact_toml_name(" mcp_servers\t.\t\"crux\" "),
            "mcp_servers.crux"
        );
        assert_eq!(
            compact_toml_name("'mcp_servers'.'crux'"),
            "mcp_servers.crux"
        );
        assert_eq!(
            compact_toml_name("mcp_servers.\"cru x\""),
            "mcp_servers.\"cru x\""
        );
        assert_eq!(
            compact_toml_name("\"mcp_servers.crux\""),
            "\"mcp_servers.crux\""
        );
        assert_eq!(
            compact_toml_name("mcp_servers.\"cr\\\"ux\""),
            "mcp_servers.\"cr\\\"ux\""
        );
        assert_eq!(compact_toml_name("mcp_servers.\"\""), "mcp_servers.\"\"");
    }

    #[test]
    fn codex_setup_refuses_crux_entries_without_a_table_header() {
        let current_exe = env::current_exe().expect("current test executable");
        let cases = [
            "[mcp_servers]\ncrux = { command = \"/old/crux\", args = [] }\n",
            "[mcp_servers]\ncrux.command = \"/old/crux\"\n",
            "[mcp_servers]\n\"crux\" = { command = \"/old/crux\" }\n",
            "[mcp_servers]\n  'crux' . command = \"/old/crux\"\n",
            "[mcp_servers]\nother = { command = \"x\" }\ncrux = {}\n",
            "[mcp_servers]\r\ncrux = {}\r\n",
            "model = \"gpt-5\"\nmcp_servers.crux.command = \"/old/crux\"\n",
            "model = \"gpt-5\"\nmcp_servers.crux = { command = \"/old/crux\" }\n",
        ];
        for content in cases {
            let error = install_codex_config(content, &current_exe)
                .expect_err("refuse existing entry")
                .to_string();
            assert!(
                error.contains("existing crux entry under [mcp_servers]"),
                "{content:?}: {error}"
            );
        }
    }

    #[test]
    fn codex_setup_ignores_keys_that_only_look_like_crux() {
        let current_exe = env::current_exe().expect("current test executable");
        let cases = [
            "[mcp_servers]\nother = { command = \"x\" }\n",
            "[mcp_servers]\n# crux = {}\n",
            "[mcp_servers]\ncrux_old = {}\ncrux-old = 1\n",
            "[other]\ncrux = 1\n",
            "[mcp_servers.other]\ncrux = 1\n",
            "[[mcp_servers.other]]\ncrux = 1\n",
            "crux = 1\n",
        ];
        for content in cases {
            let installed = install_codex_config(content, &current_exe)
                .unwrap_or_else(|error| panic!("{content:?}: {error}"));
            assert!(installed.starts_with(content), "{content:?}");
            assert_eq!(installed.matches("[mcp_servers.crux]").count(), 1);
            assert_eq!(installed.matches(BEGIN_MARKER).count(), 1);
        }
    }

    #[test]
    fn codex_setup_stops_on_an_inline_crux_entry_without_writing() {
        let project = TestProject::new();
        let config = project.root.join("config.toml");
        let agents = project.root.join("AGENTS.md");
        let content = "[mcp_servers]\ncrux = { command = \"/old/crux\", args = [] }\n";
        fs::write(&config, content).expect("write fixture");

        let error = setup_codex(&config, &agents, &project.root.join("crux"))
            .expect_err("stop on inline entry")
            .to_string();

        assert!(error.contains("replace it with a [mcp_servers.crux] section"));
        assert_eq!(fs::read_to_string(&config).expect("read config"), content);
        assert!(!backup_path(&config).exists());
        assert!(!agents.exists());
    }

    #[test]
    fn edit_file_replaces_the_file_instead_of_rewriting_it_in_place() {
        let project = TestProject::new();
        let path = project.root.join("CLAUDE.md");
        let alias = project.root.join("alias.md");
        fs::write(&path, "original").expect("write fixture");
        fs::hard_link(&path, &alias).expect("link fixture");

        edit_file(&path, |content| insert_or_replace(content, "body")).expect("edit file");

        assert_eq!(fs::read_to_string(&alias).expect("read alias"), "original");
        assert!(fs::read_to_string(&path)
            .expect("read file")
            .contains(BEGIN_MARKER));
    }

    #[cfg(unix)]
    #[test]
    fn edit_file_keeps_a_symlinked_file_and_its_mode() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let project = TestProject::new();
        let real = project.root.join("dotfiles/CLAUDE.md");
        let link = project.root.join(".claude/CLAUDE.md");
        fs::create_dir_all(real.parent().expect("real parent")).expect("create directory");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create directory");
        fs::write(&real, "original").expect("write fixture");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).expect("set mode");
        symlink("../dotfiles/CLAUDE.md", &link).expect("create link");

        let report =
            edit_file(&link, |content| insert_or_replace(content, "body")).expect("edit file");

        assert!(report.changed);
        assert!(fs::symlink_metadata(&link)
            .expect("link metadata")
            .file_type()
            .is_symlink());
        assert!(fs::read_to_string(&real)
            .expect("read target")
            .contains(BEGIN_MARKER));
        assert_eq!(
            fs::metadata(&real)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_to_string(backup_path(&link)).expect("read backup"),
            "original"
        );
        let names: Vec<_> = fs::read_dir(real.parent().expect("real parent"))
            .expect("list directory")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![os("CLAUDE.md")]);
    }

    #[test]
    fn claude_programs_start_with_the_plain_command() {
        let programs = claude_programs();
        assert_eq!(programs[0], os("claude"));
        assert_eq!(programs.len(), if cfg!(windows) { 2 } else { 1 });
        if cfg!(windows) {
            assert_eq!(programs[1], os("claude.cmd"));
        }
    }

    #[test]
    fn run_claude_reports_absent_only_when_every_program_is_missing() {
        let project = TestProject::new();
        let programs = vec![
            project.root.join("missing-one").into_os_string(),
            project.root.join("missing-two").into_os_string(),
        ];
        assert!(matches!(
            run_claude(&programs, &[os("mcp"), os("get"), os("crux")]).expect("run claude"),
            ClaudeCommand::Absent
        ));
        assert!(matches!(
            run_claude(&[], &[os("mcp")]).expect("run claude"),
            ClaudeCommand::Absent
        ));
    }

    #[test]
    fn existing_registration_message_flags_a_stale_command() {
        let current_exe = Path::new("/opt/crux/bin/crux");
        let current = "crux:\n  Command: /opt/crux/bin/crux\n  Args:";
        let stale = "crux:\n  Command: /old/crux";

        let unchanged = "No change to the Claude Code crux MCP registration.";
        assert_eq!(
            existing_registration_message(current, current_exe),
            unchanged
        );

        let message = existing_registration_message(stale, current_exe);
        assert!(message.contains("does not point to this binary"));
        assert!(message.contains("Command: /old/crux"));
        assert!(message.contains("claude mcp remove crux --scope user"));
        assert!(message.contains(&format!(
            "claude mcp add --scope user crux -- {}",
            shell_quote(current_exe)
        )));
    }

    #[test]
    fn existing_registration_message_needs_a_whole_token_match() {
        let current_exe = Path::new("/opt/crux/bin/crux");
        let unchanged = "No change to the Claude Code crux MCP registration.";

        let prefix = existing_registration_message("Command: /opt/crux/bin/crux-0.10", current_exe);
        assert!(!prefix.contains(unchanged), "{prefix}");
        assert!(prefix.contains("does not point to this binary"), "{prefix}");

        let quoted = existing_registration_message(
            "Command: \"/opt/crux/bin/crux\" --profile full",
            current_exe,
        );
        assert_eq!(quoted, unchanged);

        let empty = existing_registration_message("", current_exe);
        assert!(!empty.contains(unchanged), "{empty}");
        assert!(empty.contains("Crux kept it"), "{empty}");
        let add_command = format!(
            "claude mcp add --scope user crux -- {}",
            shell_quote(current_exe)
        );
        assert!(empty.contains(&add_command), "{empty}");
    }

    #[test]
    fn setup_claude_continues_when_the_cli_is_absent() {
        let project = TestProject::new();
        let instructions = project.root.join("work/CLAUDE.md");
        let programs = vec![project.root.join("no-such-claude").into_os_string()];

        setup_claude(&instructions, &project.root.join("crux"), &programs).expect("setup claude");

        assert!(fs::read_to_string(&instructions)
            .expect("read instructions")
            .contains(BEGIN_MARKER));
    }

    #[cfg(unix)]
    static FAKE_CLAUDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A shell script that stands in for the Claude CLI.
    /// It logs each call and answers by the second argument: add, get, or remove.
    /// The guard serializes the fake CLI tests in this module.
    #[cfg(unix)]
    struct FakeClaude {
        programs: Vec<OsString>,
        log: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl FakeClaude {
        fn new(directory: &Path, cases: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let guard = FAKE_CLAUDE_LOCK
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let program = directory.join("claude");
            let log = directory.join("claude.log");
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$2\" in\n{cases}\nesac\n",
                log.display()
            );
            fs::write(&program, script).expect("write fake claude");
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
                .expect("make fake claude executable");
            Self {
                programs: vec![program.into_os_string()],
                log,
                _guard: guard,
            }
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_claude_falls_back_to_the_next_program() {
        let project = TestProject::new();
        let fake = FakeClaude::new(&project.root, "get) echo found; exit 0;;");
        let programs = vec![
            project.root.join("missing").into_os_string(),
            fake.programs[0].clone(),
        ];

        let result = run_claude(&programs, &[os("mcp"), os("get"), os("crux")]).expect("run");

        let ClaudeCommand::Ran(output) = result else {
            panic!("expected the fallback program to run");
        };
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "found");
        assert_eq!(fake.calls(), vec!["mcp get crux".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn run_claude_returns_spawn_errors_other_than_not_found() {
        use std::os::unix::fs::PermissionsExt;
        let project = TestProject::new();
        let program = project.root.join("claude");
        fs::write(&program, "not executable").expect("write fixture");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o644)).expect("set mode");

        let result = run_claude(&[program.into_os_string()], &[os("mcp")]);

        let error = result.err().expect("spawn error").to_string();
        assert!(error.contains("run the Claude CLI"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn setup_claude_registers_the_server_with_the_cli() {
        let project = TestProject::new();
        let fake = FakeClaude::new(&project.root, "add) echo registered; exit 0;;");
        let instructions = project.root.join("work/CLAUDE.md");
        let current_exe = project.root.join("bin/crux");

        setup_claude(&instructions, &current_exe, &fake.programs).expect("setup claude");

        assert_eq!(
            fake.calls(),
            vec![format!(
                "mcp add --scope user crux -- {}",
                current_exe.display()
            )]
        );
        assert!(fs::read_to_string(&instructions)
            .expect("read instructions")
            .contains(BEGIN_MARKER));
    }

    #[cfg(unix)]
    #[test]
    fn setup_claude_keeps_an_existing_registration_and_inspects_it() {
        let project = TestProject::new();
        let fake = FakeClaude::new(
            &project.root,
            "add) echo 'MCP server crux already exists in user config' >&2; exit 1;;\nget) echo 'crux:'; echo '  Command: /elsewhere/crux'; exit 0;;",
        );
        let instructions = project.root.join("work/CLAUDE.md");
        let current_exe = project.root.join("bin/crux");

        setup_claude(&instructions, &current_exe, &fake.programs).expect("setup claude");

        assert_eq!(
            fake.calls(),
            vec![
                format!("mcp add --scope user crux -- {}", current_exe.display()),
                "mcp get crux".to_string(),
            ]
        );
        assert!(fs::read_to_string(&instructions)
            .expect("read instructions")
            .contains(BEGIN_MARKER));
    }

    #[cfg(unix)]
    #[test]
    fn setup_claude_reports_cli_failures_and_skips_the_note() {
        let project = TestProject::new();
        let fake = FakeClaude::new(&project.root, "add) echo 'permission denied' >&2; exit 1;;");
        let instructions = project.root.join("work/CLAUDE.md");

        let error = setup_claude(&instructions, &project.root.join("crux"), &fake.programs)
            .expect_err("setup fails")
            .to_string();

        assert!(
            error.contains("could not register the crux MCP server: permission denied"),
            "{error}"
        );
        assert!(!instructions.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unsetup_claude_removes_the_registration_and_the_note() {
        let project = TestProject::new();
        let fake = FakeClaude::new(&project.root, "remove) echo removed; exit 0;;");
        let instructions = project.root.join("CLAUDE.md");
        let installed = insert_or_replace("keep\n", ADOPTION_BODY).expect("insert block");
        fs::write(&instructions, installed).expect("write fixture");

        unsetup_claude(&instructions, &fake.programs).expect("unsetup claude");

        assert_eq!(
            fake.calls(),
            vec!["mcp remove crux --scope user".to_string()]
        );
        assert_eq!(
            fs::read_to_string(&instructions).expect("read instructions"),
            "keep\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsetup_claude_accepts_a_missing_registration() {
        let project = TestProject::new();
        let fake = FakeClaude::new(
            &project.root,
            "remove) echo 'No MCP server found with name: crux' >&2; exit 1;;",
        );
        let instructions = project.root.join("CLAUDE.md");
        let installed = insert_or_replace("keep\n", ADOPTION_BODY).expect("insert block");
        fs::write(&instructions, installed).expect("write fixture");

        unsetup_claude(&instructions, &fake.programs).expect("unsetup claude");

        assert_eq!(
            fs::read_to_string(&instructions).expect("read instructions"),
            "keep\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsetup_claude_reports_cli_failures_and_keeps_the_note() {
        let project = TestProject::new();
        let fake = FakeClaude::new(
            &project.root,
            "remove) echo 'permission denied' >&2; exit 1;;",
        );
        let instructions = project.root.join("CLAUDE.md");
        let installed = insert_or_replace("keep\n", ADOPTION_BODY).expect("insert block");
        fs::write(&instructions, &installed).expect("write fixture");

        let error = unsetup_claude(&instructions, &fake.programs)
            .expect_err("unsetup fails")
            .to_string();

        assert!(
            error.contains("could not remove the crux MCP server: permission denied"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(&instructions).expect("read instructions"),
            installed
        );
    }

    #[test]
    fn unsetup_claude_continues_when_the_cli_is_absent() {
        let project = TestProject::new();
        let instructions = project.root.join("CLAUDE.md");
        let installed = insert_or_replace("keep\n", ADOPTION_BODY).expect("insert block");
        fs::write(&instructions, installed).expect("write fixture");
        let programs = vec![project.root.join("no-such-claude").into_os_string()];

        unsetup_claude(&instructions, &programs).expect("unsetup claude");

        assert_eq!(
            fs::read_to_string(&instructions).expect("read instructions"),
            "keep\n"
        );
    }
}

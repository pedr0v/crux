use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/pedr0v/crux/releases/latest";
const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS";
const UPDATE_CHECK_FILE: &str = "update-check";
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("crux/", env!("CARGO_PKG_VERSION"));
const COPY_BUFFER_SIZE: usize = 64 * 1024;
const COPY_QUEUE_LENGTH: usize = 4;
/// Windows rejects a path of this length or longer unless it has the `\\?\` prefix.
const LEGACY_MAX_PATH: usize = 260;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(tag: &str) -> Option<Self> {
        let tag = tag.trim().strip_prefix('v').unwrap_or(tag.trim());
        let core = tag.split(['-', '+']).next()?;
        let mut numbers = core.split('.');
        let version = Self {
            major: numbers.next()?.parse().ok()?,
            minor: numbers.next()?.parse().ok()?,
            patch: numbers.next()?.parse().ok()?,
        };
        numbers.next().is_none().then_some(version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug)]
struct AvailableUpdate {
    version: Version,
    asset_name: &'static str,
    download_url: String,
    /// URL of the `SHA256SUMS` release asset. It is `None` when the release has no such asset.
    checksums_url: Option<String>,
}

trait HttpClient {
    fn get_text(&self, url: &str, timeout: Duration) -> Result<String>;
    fn download(&self, url: &str, timeout: Duration, writer: &mut dyn Write) -> Result<()>;
}

struct UreqHttpClient;

impl UreqHttpClient {
    /// Start a config that ignores proxy variables, like the old ureq 2 agent did.
    fn base_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
        ureq::Agent::config_builder()
            .proxy(None)
            .user_agent(USER_AGENT)
    }

    /// Build an agent with one deadline for the whole call.
    /// Use it only for small replies.
    fn overall_agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::new_with_config(Self::base_config().timeout_global(Some(timeout)).build())
    }

    /// Build an agent with one timeout for each step before the body and no overall deadline.
    /// A slow transfer can finish. The caller must guard the body read, because ureq 3
    /// has no idle timeout for the body.
    fn step_agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::new_with_config(
            Self::base_config()
                .timeout_resolve(Some(timeout))
                .timeout_connect(Some(timeout))
                .timeout_send_request(Some(timeout))
                .timeout_recv_response(Some(timeout))
                .build(),
        )
    }

    fn request(agent: &ureq::Agent, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        agent
            .get(url)
            .call()
            .map_err(|error| anyhow!(error))
            .with_context(|| format!("get {url}"))
    }
}

impl HttpClient for UreqHttpClient {
    fn get_text(&self, url: &str, timeout: Duration) -> Result<String> {
        Self::request(&Self::overall_agent(timeout), url)?
            .body_mut()
            .read_to_string()
            .map_err(|error| anyhow!(error))
            .with_context(|| format!("read {url}"))
    }

    fn download(&self, url: &str, timeout: Duration, writer: &mut dyn Write) -> Result<()> {
        let response = Self::request(&Self::step_agent(timeout), url)?;
        let reader = response.into_body().into_reader();
        copy_with_idle_timeout(reader, writer, timeout)
            .with_context(|| format!("download {url}"))?;
        Ok(())
    }
}

type Chunk = io::Result<Option<Vec<u8>>>;

/// Copy `reader` to `writer`. Fail when no data arrives for `idle`.
/// The idle timer restarts after each chunk, so a slow transfer that keeps moving can finish.
/// A worker thread reads the data. If the worker blocks, it ends when the process ends.
fn copy_with_idle_timeout<R>(reader: R, writer: &mut dyn Write, idle: Duration) -> io::Result<u64>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel::<Chunk>(COPY_QUEUE_LENGTH);
    thread::spawn(move || read_chunks(reader, &sender));
    let mut total = 0;
    loop {
        match receiver.recv_timeout(idle) {
            Ok(Ok(Some(chunk))) => {
                writer.write_all(&chunk)?;
                total += chunk.len() as u64;
            }
            Ok(Ok(None)) => return Ok(total),
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no data received for {idle:?}"),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("the download reader stopped early"))
            }
        }
    }
}

fn read_chunks(mut reader: impl Read, sender: &SyncSender<Chunk>) {
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    loop {
        let message = match reader.read(&mut buffer) {
            Ok(0) => Ok(None),
            Ok(count) => Ok(Some(buffer[..count].to_vec())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => Err(error),
        };
        let finished = !matches!(message, Ok(Some(_)));
        if sender.send(message).is_err() || finished {
            return;
        }
    }
}

fn check_for_update(
    client: &dyn HttpClient,
    current_version: &str,
    target: &str,
    timeout: Duration,
) -> Result<Option<AvailableUpdate>> {
    let response = client.get_text(LATEST_RELEASE_URL, timeout)?;
    let release: Release = serde_json::from_str(&response).context("parse latest release")?;
    let current = Version::parse(current_version)
        .ok_or_else(|| anyhow!("current version is invalid: {current_version}"))?;
    let latest = Version::parse(&release.tag_name)
        .ok_or_else(|| anyhow!("release tag is invalid: {}", release.tag_name))?;
    if latest <= current {
        return Ok(None);
    }

    let asset_name = asset_name_for_target(target)
        .ok_or_else(|| anyhow!("crux does not publish an asset for {target}"))?;
    let mut download_url = None;
    let mut checksums_url = None;
    for asset in release.assets {
        if asset.name == asset_name {
            download_url = Some(asset.browser_download_url);
        } else if asset.name == CHECKSUMS_ASSET_NAME {
            checksums_url = Some(asset.browser_download_url);
        }
    }
    let download_url =
        download_url.ok_or_else(|| anyhow!("release asset is missing: {asset_name}"))?;
    Ok(Some(AvailableUpdate {
        version: latest,
        asset_name,
        download_url,
        checksums_url,
    }))
}

fn asset_name_for_target(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-apple-darwin" => Some("crux-aarch64-apple-darwin"),
        "x86_64-apple-darwin" => Some("crux-x86_64-apple-darwin"),
        "x86_64-unknown-linux-gnu" => Some("crux-x86_64-unknown-linux-gnu"),
        "aarch64-unknown-linux-gnu" => Some("crux-aarch64-unknown-linux-gnu"),
        "x86_64-pc-windows-msvc" => Some("crux-x86_64-pc-windows-msvc.exe"),
        _ => None,
    }
}

fn current_target_triple() -> Option<&'static str> {
    match (env::consts::ARCH, env::consts::OS) {
        ("aarch64", "macos") if cfg!(target_vendor = "apple") => Some("aarch64-apple-darwin"),
        ("x86_64", "macos") if cfg!(target_vendor = "apple") => Some("x86_64-apple-darwin"),
        ("x86_64", "linux") if cfg!(target_env = "gnu") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") if cfg!(target_env = "gnu") => Some("aarch64-unknown-linux-gnu"),
        ("x86_64", "windows") if cfg!(target_env = "msvc") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

fn format_update_notice(version: &Version) -> String {
    format!("note: crux {version} is available — run crux self-update")
}

pub(crate) fn run_self_update(check_only: bool) -> Result<()> {
    let target = current_target_triple().ok_or_else(|| {
        anyhow!(
            "crux does not publish an asset for {}-{}",
            env::consts::ARCH,
            env::consts::OS
        )
    })?;
    let client = UreqHttpClient;
    if !check_only {
        println!("Check for a crux update.");
    }
    let available = check_for_update(&client, env!("CARGO_PKG_VERSION"), target, COMMAND_TIMEOUT)?;
    let Some(available) = available else {
        println!("crux {} is current.", env!("CARGO_PKG_VERSION"));
        return Ok(());
    };
    if check_only {
        println!("crux {} is available.", available.version);
        return Ok(());
    }

    println!("Download crux {} for {target}.", available.version);
    let current_exe = env::current_exe().context("find the current executable")?;
    install_update(&client, &available, &current_exe)?;
    println!("crux {} is ready.", available.version);
    Ok(())
}

/// Download the update, check its SHA-256 sum, and replace the real executable.
/// A symlink to the executable stays in place.
fn install_update(
    client: &dyn HttpClient,
    available: &AvailableUpdate,
    exe_path: &Path,
) -> Result<()> {
    let current_exe = resolve_executable_path(exe_path);
    let pending = download_update(client, available, &current_exe)?;
    println!("Install crux {}.", available.version);
    replace_executable(&pending.path, &current_exe)
}

/// Return the real path of the executable. Follow every symlink.
/// Keep the given path when the file system cannot resolve it.
fn resolve_executable_path(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .map(|real| strip_verbatim_prefix(&real))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Remove the Windows `\\?\` prefix that `fs::canonicalize` adds.
/// Keep the prefix when the short path would pass the legacy path length limit.
/// Keep every other path as it is.
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let short = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        Some(format!(r"\\{rest}"))
    } else {
        text.strip_prefix(r"\\?\")
            .filter(|rest| {
                let bytes = rest.as_bytes();
                bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && bytes[2] == b'\\'
            })
            .map(str::to_owned)
    };
    match short {
        Some(short) if short.len() < LEGACY_MAX_PATH => PathBuf::from(short),
        _ => path.to_path_buf(),
    }
}

#[derive(Debug)]
struct PendingFile {
    path: PathBuf,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Parse one line of a `sha256sum` file.
/// The line has 64 lowercase hex digits, two spaces, and the file name.
fn parse_checksum_line(line: &str) -> Option<(&str, &str)> {
    let (hash, rest) = line.split_at_checked(64)?;
    let name = rest.strip_prefix("  ")?;
    let is_hex = hash
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    (is_hex && !name.is_empty()).then_some((hash, name))
}

fn find_checksum<'a>(sums: &'a str, asset_name: &str) -> Option<&'a str> {
    sums.lines()
        .filter_map(parse_checksum_line)
        .find(|(_, name)| *name == asset_name)
        .map(|(hash, _)| hash)
}

/// Fetch `SHA256SUMS` and return the expected hash of the asset.
/// Fail when the release has no `SHA256SUMS` asset or the file has no line for the asset.
fn expected_checksum(client: &dyn HttpClient, available: &AvailableUpdate) -> Result<String> {
    let url = available.checksums_url.as_deref().ok_or_else(|| {
        anyhow!(
            "release asset is missing: {CHECKSUMS_ASSET_NAME}; crux does not install an update without a checksum"
        )
    })?;
    let sums = client.get_text(url, COMMAND_TIMEOUT)?;
    find_checksum(&sums, available.asset_name)
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow!(
                "{CHECKSUMS_ASSET_NAME} has no checksum for {}",
                available.asset_name
            )
        })
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_checksum(path: &Path, expected: &str, asset_name: &str) -> Result<()> {
    let actual = hash_file(path)?;
    if actual != expected {
        bail!("checksum mismatch for {asset_name}: expected {expected}, got {actual}");
    }
    Ok(())
}

fn download_update(
    client: &dyn HttpClient,
    available: &AvailableUpdate,
    current_exe: &Path,
) -> Result<PendingFile> {
    let expected = expected_checksum(client, available)?;
    let parent = current_exe
        .parent()
        .ok_or_else(|| anyhow!("the current executable has no parent directory"))?;
    let file_name = current_exe
        .file_name()
        .ok_or_else(|| anyhow!("the current executable has no file name"))?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = parent.join(format!(
        ".{file_name}.update-{}-{nonce}.tmp",
        std::process::id()
    ));
    let pending = PendingFile { path };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending.path)
        .with_context(|| format!("create {}", pending.path.display()))?;
    client.download(&available.download_url, COMMAND_TIMEOUT, &mut file)?;
    file.flush()
        .with_context(|| format!("flush {}", pending.path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", pending.path.display()))?;
    drop(file);
    verify_checksum(&pending.path, &expected, available.asset_name)?;
    set_executable(&pending.path, current_exe)?;
    Ok(pending)
}

#[cfg(unix)]
fn set_executable(path: &Path, current_exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let current_mode = fs::metadata(current_exe)
        .with_context(|| format!("read permissions for {}", current_exe.display()))?
        .permissions()
        .mode();
    let permissions = fs::Permissions::from_mode(current_mode | 0o111);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("set executable permissions for {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _current_exe: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_executable(pending: &Path, current_exe: &Path) -> Result<()> {
    fs::rename(pending, current_exe).with_context(|| {
        format!(
            "replace {} with {}",
            current_exe.display(),
            pending.display()
        )
    })
}

#[cfg(windows)]
fn replace_executable(pending: &Path, current_exe: &Path) -> Result<()> {
    let old_exe = current_exe.with_file_name("crux.old.exe");
    let _ = fs::remove_file(&old_exe);
    fs::rename(current_exe, &old_exe)
        .with_context(|| format!("move {} to {}", current_exe.display(), old_exe.display()))?;
    if let Err(error) = fs::rename(pending, current_exe) {
        let _ = fs::rename(&old_exe, current_exe);
        return Err(error).with_context(|| format!("install {}", current_exe.display()));
    }
    let _ = fs::remove_file(old_exe);
    Ok(())
}

fn update_check_due(last_check: Option<SystemTime>, now: SystemTime) -> bool {
    match last_check {
        None => true,
        Some(last_check) => now
            .duration_since(last_check)
            .is_ok_and(|elapsed| elapsed > UPDATE_CHECK_INTERVAL),
    }
}

fn read_last_check(path: &Path) -> Option<SystemTime> {
    let seconds = fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn claim_update_check(index_dir: &Path, now: SystemTime) -> Result<bool> {
    let path = index_dir.join(UPDATE_CHECK_FILE);
    if !update_check_due(read_last_check(&path), now) {
        return Ok(false);
    }
    let seconds = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    fs::write(&path, seconds.to_string()).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

pub(crate) struct PendingUpdateCheck {
    receiver: Receiver<Option<String>>,
}

impl PendingUpdateCheck {
    pub(crate) fn try_notice(&self) -> Option<String> {
        self.receiver.try_recv().ok().flatten()
    }
}

pub(crate) fn start_background_check(index_dir: &Path) -> Option<PendingUpdateCheck> {
    if !claim_update_check(index_dir, SystemTime::now()).ok()? {
        return None;
    }

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let notice = current_target_triple()
            .and_then(|target| {
                check_for_update(
                    &UreqHttpClient,
                    env!("CARGO_PKG_VERSION"),
                    target,
                    BACKGROUND_TIMEOUT,
                )
                .ok()
                .flatten()
            })
            .map(|available| format_update_notice(&available.version));
        let _ = sender.send(notice);
    });
    Some(PendingUpdateCheck { receiver })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::time::Instant;

    const TEST_TARGET: &str = "x86_64-unknown-linux-gnu";
    const TEST_ASSET: &str = "crux-x86_64-unknown-linux-gnu";
    const TEST_TAG: &str = "v9.9.9";
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// An HTTP client that serves canned text and bytes and records each request.
    #[derive(Default)]
    struct FakeHttpClient {
        texts: HashMap<String, String>,
        downloads: HashMap<String, Vec<u8>>,
        /// Downloads that write half of the bytes and then fail.
        broken_downloads: HashSet<String>,
        requests: RefCell<Vec<String>>,
    }

    impl FakeHttpClient {
        fn downloaded(&self) -> bool {
            self.requests
                .borrow()
                .iter()
                .any(|request| request.starts_with("download "))
        }
    }

    impl HttpClient for FakeHttpClient {
        fn get_text(&self, url: &str, _timeout: Duration) -> Result<String> {
            self.requests.borrow_mut().push(format!("text {url}"));
            self.texts
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow!("no fake text for {url}"))
        }

        fn download(&self, url: &str, _timeout: Duration, writer: &mut dyn Write) -> Result<()> {
            self.requests.borrow_mut().push(format!("download {url}"));
            let bytes = self
                .downloads
                .get(url)
                .ok_or_else(|| anyhow!("no fake download for {url}"))?;
            if self.broken_downloads.contains(url) {
                writer.write_all(&bytes[..bytes.len() / 2])?;
                bail!("connection reset");
            }
            writer.write_all(bytes)?;
            Ok(())
        }
    }

    fn asset_url(name: &str) -> String {
        format!("https://example.test/{TEST_TAG}/{name}")
    }

    fn release_json(tag: &str, asset_names: &[&str]) -> String {
        let assets: Vec<_> = asset_names
            .iter()
            .map(|name| serde_json::json!({"name": name, "browser_download_url": asset_url(name)}))
            .collect();
        serde_json::json!({"tag_name": tag, "assets": assets}).to_string()
    }

    fn checksum_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn client_with_release(tag: &str, asset_names: &[&str]) -> FakeHttpClient {
        let mut client = FakeHttpClient::default();
        client.texts.insert(
            LATEST_RELEASE_URL.to_owned(),
            release_json(tag, asset_names),
        );
        client
    }

    /// A newer release with the binary asset and a matching `SHA256SUMS` asset.
    fn client_with_binary(binary: &[u8]) -> FakeHttpClient {
        let mut client = client_with_release(TEST_TAG, &[TEST_ASSET, CHECKSUMS_ASSET_NAME]);
        client
            .downloads
            .insert(asset_url(TEST_ASSET), binary.to_vec());
        client.texts.insert(
            asset_url(CHECKSUMS_ASSET_NAME),
            format!("{}  {TEST_ASSET}\n", checksum_hex(binary)),
        );
        client
    }

    fn newer_update(client: &FakeHttpClient) -> AvailableUpdate {
        check_for_update(client, "0.11.0", TEST_TARGET, TEST_TIMEOUT)
            .expect("check for update")
            .expect("a newer release")
    }

    fn write_executable(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("set mode");
        }
        path
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn error_text(error: &anyhow::Error) -> String {
        format!("{error:#}")
    }

    #[test]
    fn check_for_update_returns_the_asset_of_a_newer_release() {
        let client = client_with_release(TEST_TAG, &[TEST_ASSET, CHECKSUMS_ASSET_NAME]);
        let update = newer_update(&client);
        assert_eq!(update.version.to_string(), "9.9.9");
        assert_eq!(update.asset_name, TEST_ASSET);
        assert_eq!(update.download_url, asset_url(TEST_ASSET));
        assert_eq!(update.checksums_url, Some(asset_url(CHECKSUMS_ASSET_NAME)));
        assert_eq!(
            *client.requests.borrow(),
            [format!("text {LATEST_RELEASE_URL}")]
        );
    }

    #[test]
    fn check_for_update_returns_none_for_an_equal_or_older_release() {
        for tag in ["v0.11.0", "0.11.0", "v0.10.9", "v0.9.0"] {
            let client = client_with_release(tag, &[TEST_ASSET]);
            let update = check_for_update(&client, "0.11.0", TEST_TARGET, TEST_TIMEOUT)
                .expect("check for update");
            assert!(update.is_none(), "{tag} is not newer than 0.11.0");
        }
    }

    #[test]
    fn check_for_update_keeps_a_release_without_a_checksums_asset() {
        let client = client_with_release(TEST_TAG, &[TEST_ASSET]);
        let update = newer_update(&client);
        assert_eq!(update.download_url, asset_url(TEST_ASSET));
        assert_eq!(update.checksums_url, None);
    }

    #[test]
    fn check_for_update_fails_when_the_asset_is_missing() {
        let client = client_with_release(TEST_TAG, &["crux-aarch64-apple-darwin", "SHA256SUMS"]);
        let error = check_for_update(&client, "0.11.0", TEST_TARGET, TEST_TIMEOUT)
            .expect_err("missing asset");
        assert!(
            error_text(&error).contains(&format!("release asset is missing: {TEST_ASSET}")),
            "{error:#}"
        );
    }

    #[test]
    fn check_for_update_fails_on_malformed_json() {
        for body in [
            "",
            "not json",
            "{\"tag_name\":\"v9.9.9\"}",
            "{\"assets\":[]}",
        ] {
            let mut client = FakeHttpClient::default();
            client
                .texts
                .insert(LATEST_RELEASE_URL.to_owned(), body.to_owned());
            let error = check_for_update(&client, "0.11.0", TEST_TARGET, TEST_TIMEOUT)
                .expect_err("malformed json");
            assert!(
                error_text(&error).contains("parse latest release"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn check_for_update_fails_on_a_bad_tag_or_target() {
        let client = client_with_release("latest", &[TEST_ASSET]);
        let error =
            check_for_update(&client, "0.11.0", TEST_TARGET, TEST_TIMEOUT).expect_err("bad tag");
        assert!(
            error_text(&error).contains("release tag is invalid"),
            "{error:#}"
        );

        let client = client_with_release(TEST_TAG, &[TEST_ASSET]);
        let error = check_for_update(&client, "0.11.0", "x86_64-unknown-linux-musl", TEST_TIMEOUT)
            .expect_err("unsupported target");
        assert!(
            error_text(&error).contains("does not publish an asset"),
            "{error:#}"
        );
    }

    #[test]
    fn check_for_update_fails_when_the_request_fails() {
        let client = FakeHttpClient::default();
        assert!(check_for_update(&client, "0.11.0", TEST_TARGET, TEST_TIMEOUT).is_err());
    }

    #[test]
    fn download_update_writes_a_temp_file_beside_the_executable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = write_executable(dir.path(), "crux", b"old");
        let client = client_with_binary(b"new binary");
        let update = newer_update(&client);

        let pending = download_update(&client, &update, &exe).expect("download");

        assert_eq!(pending.path.parent(), exe.parent());
        let name = pending
            .path
            .file_name()
            .expect("file name")
            .to_string_lossy();
        let prefix = format!(".crux.update-{}-", std::process::id());
        let nonce = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .unwrap_or_else(|| panic!("unexpected temp file name: {name}"));
        assert!(nonce.parse::<u128>().is_ok(), "nonce is a number: {nonce}");
        assert_eq!(fs::read(&pending.path).expect("read temp"), b"new binary");
        assert_eq!(fs::read(&exe).expect("read exe"), b"old");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&pending.path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }

        let temp_path = pending.path.clone();
        drop(pending);
        assert!(!temp_path.exists(), "drop removes the temp file");
        assert_eq!(dir_entries(dir.path()), ["crux"]);
    }

    #[test]
    fn download_update_removes_the_temp_file_after_a_failed_download() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = write_executable(dir.path(), "crux", b"old");
        let mut client = client_with_binary(b"new binary");
        client.broken_downloads.insert(asset_url(TEST_ASSET));
        let update = newer_update(&client);

        let error = download_update(&client, &update, &exe).expect_err("broken download");

        assert!(error_text(&error).contains("connection reset"), "{error:#}");
        assert_eq!(dir_entries(dir.path()), ["crux"]);
        assert_eq!(fs::read(&exe).expect("read exe"), b"old");
    }

    #[test]
    fn download_update_accepts_a_matching_checksum() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = write_executable(dir.path(), "crux", b"old");
        let mut client = client_with_binary(b"new binary");
        // Other lines in the file must not disturb the match.
        let other = "f".repeat(64);
        client.texts.insert(
            asset_url(CHECKSUMS_ASSET_NAME),
            format!(
                "{other}  crux-aarch64-apple-darwin\n{}  {TEST_ASSET}\n{other}  crux-x86_64-pc-windows-msvc.exe\n",
                checksum_hex(b"new binary")
            ),
        );
        let update = newer_update(&client);

        let pending = download_update(&client, &update, &exe).expect("download");

        assert_eq!(fs::read(&pending.path).expect("read temp"), b"new binary");
        assert_eq!(
            *client.requests.borrow(),
            [
                format!("text {LATEST_RELEASE_URL}"),
                format!("text {}", asset_url(CHECKSUMS_ASSET_NAME)),
                format!("download {}", asset_url(TEST_ASSET)),
            ]
        );
    }

    #[test]
    fn download_update_rejects_a_checksum_mismatch_and_removes_the_temp_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = write_executable(dir.path(), "crux", b"old");
        let mut client = client_with_binary(b"new binary");
        client
            .downloads
            .insert(asset_url(TEST_ASSET), b"tampered binary".to_vec());
        let update = newer_update(&client);

        let error = download_update(&client, &update, &exe).expect_err("checksum mismatch");

        let text = error_text(&error);
        assert!(text.contains("checksum mismatch"), "{text}");
        assert!(text.contains(&checksum_hex(b"new binary")), "{text}");
        assert!(text.contains(&checksum_hex(b"tampered binary")), "{text}");
        assert_eq!(dir_entries(dir.path()), ["crux"], "temp file is deleted");
        assert_eq!(fs::read(&exe).expect("read exe"), b"old");
    }

    #[test]
    fn download_update_fails_before_the_download_without_a_checksums_asset() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = write_executable(dir.path(), "crux", b"old");
        let mut client = client_with_release(TEST_TAG, &[TEST_ASSET]);
        client
            .downloads
            .insert(asset_url(TEST_ASSET), b"new binary".to_vec());
        let update = newer_update(&client);

        let error = download_update(&client, &update, &exe).expect_err("no checksums asset");

        let text = error_text(&error);
        assert!(
            text.contains("release asset is missing: SHA256SUMS"),
            "{text}"
        );
        assert!(!client.downloaded(), "no binary download starts");
        assert_eq!(dir_entries(dir.path()), ["crux"]);
    }

    #[test]
    fn download_update_fails_when_no_checksum_line_matches_the_asset() {
        let hash = checksum_hex(b"new binary");
        let bad_sums = [
            String::new(),
            format!("{hash}  crux-aarch64-apple-darwin\n"),
            format!("{hash}  {TEST_ASSET}.exe\n"),
            format!("{hash} {TEST_ASSET}\n"),
            format!("{hash} *{TEST_ASSET}\n"),
            format!("{}  {TEST_ASSET}\n", hash.to_uppercase()),
            format!("{}  {TEST_ASSET}\n", &hash[..63]),
        ];
        for sums in bad_sums {
            let dir = tempfile::tempdir().expect("temp dir");
            let exe = write_executable(dir.path(), "crux", b"old");
            let mut client = client_with_binary(b"new binary");
            client
                .texts
                .insert(asset_url(CHECKSUMS_ASSET_NAME), sums.clone());
            let update = newer_update(&client);

            let error = download_update(&client, &update, &exe).expect_err("no matching line");

            let text = error_text(&error);
            assert!(
                text.contains(&format!("SHA256SUMS has no checksum for {TEST_ASSET}")),
                "{sums:?}: {text}"
            );
            assert!(!client.downloaded(), "{sums:?}: no binary download starts");
            assert_eq!(dir_entries(dir.path()), ["crux"]);
        }
    }

    #[test]
    fn checksum_lines_follow_the_sha256sum_text_format() {
        let hash = "a".repeat(64);
        let other = "b".repeat(64);
        assert_eq!(
            find_checksum(&format!("{hash}  crux\n"), "crux"),
            Some(hash.as_str())
        );
        let crlf = format!("{other}  crux-other\r\n{hash}  crux\r\n");
        assert_eq!(find_checksum(&crlf, "crux"), Some(hash.as_str()));
        assert_eq!(find_checksum("", "crux"), None);
        assert_eq!(
            find_checksum(&format!("{hash}  crux-extra\n"), "crux"),
            None
        );
        assert_eq!(find_checksum(&format!("{hash}  crux\n"), "cru"), None);
        assert_eq!(find_checksum(&format!("{hash} crux\n"), "crux"), None);
        assert_eq!(find_checksum(&format!("{hash}   crux\n"), "crux"), None);
        assert_eq!(find_checksum(&format!("{hash} *crux\n"), "crux"), None);
        assert_eq!(find_checksum(&format!("{hash}  \n"), "crux"), None);
        assert_eq!(
            find_checksum(&format!("{}  crux\n", "A".repeat(64)), "crux"),
            None
        );
        assert_eq!(
            find_checksum(&format!("{}  crux\n", "g".repeat(64)), "crux"),
            None
        );
        // A multi-byte character at the split point must not panic.
        assert_eq!(
            find_checksum(&format!("{}é  crux\n", "a".repeat(63)), "crux"),
            None
        );
    }

    #[test]
    fn hash_file_returns_the_lowercase_sha256_of_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("data");
        fs::write(&path, b"abc").expect("write");
        assert_eq!(
            hash_file(&path).expect("hash"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(hash_file(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn replace_executable_installs_the_new_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exe = write_executable(dir.path(), "crux", b"old");
        let pending = write_executable(dir.path(), ".crux.update-test.tmp", b"new");

        replace_executable(&pending, &exe).expect("replace");

        assert_eq!(fs::read(&exe).expect("read exe"), b"new");
        assert_eq!(dir_entries(dir.path()), ["crux"]);
    }

    #[cfg(unix)]
    #[test]
    fn install_update_keeps_a_symlink_and_replaces_the_real_executable() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp dir");
        let real_dir = dir.path().join("real");
        let link_dir = dir.path().join("link");
        fs::create_dir(&real_dir).expect("create real dir");
        fs::create_dir(&link_dir).expect("create link dir");
        let real_exe = write_executable(&real_dir, "crux", b"old");
        let link_exe = link_dir.join("crux");
        symlink(&real_exe, &link_exe).expect("symlink");
        let client = client_with_binary(b"new binary");
        let update = newer_update(&client);

        install_update(&client, &update, &link_exe).expect("install");

        let link_type = fs::symlink_metadata(&link_exe)
            .expect("link metadata")
            .file_type();
        assert!(link_type.is_symlink(), "the symlink stays a symlink");
        assert_eq!(fs::read(&real_exe).expect("read real exe"), b"new binary");
        assert_eq!(fs::read(&link_exe).expect("read link"), b"new binary");
        assert_eq!(dir_entries(&real_dir), ["crux"]);
        assert_eq!(dir_entries(&link_dir), ["crux"]);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_executable_path_follows_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp dir");
        let real_exe = write_executable(dir.path(), "crux-real", b"old");
        let middle = dir.path().join("crux-middle");
        let link = dir.path().join("crux");
        symlink(&real_exe, &middle).expect("first symlink");
        symlink(&middle, &link).expect("second symlink");

        let resolved = resolve_executable_path(&link);

        assert_eq!(resolved, fs::canonicalize(&real_exe).expect("canonicalize"));
    }

    #[test]
    fn resolve_executable_path_keeps_a_path_that_does_not_resolve() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing");
        assert_eq!(resolve_executable_path(&missing), missing);
    }

    #[test]
    fn verbatim_prefix_is_removed_only_when_the_short_path_is_valid() {
        let strip = |text: &str| strip_verbatim_prefix(Path::new(text));
        assert_eq!(
            strip(r"\\?\C:\Users\me\crux.exe"),
            PathBuf::from(r"C:\Users\me\crux.exe")
        );
        assert_eq!(
            strip(r"\\?\UNC\server\share\crux.exe"),
            PathBuf::from(r"\\server\share\crux.exe")
        );
        let long = format!(r"\\?\C:\{}\crux.exe", "a".repeat(300));
        assert_eq!(strip(&long), PathBuf::from(&long));
        let long_unc = format!(r"\\?\UNC\server\{}\crux.exe", "a".repeat(300));
        assert_eq!(strip(&long_unc), PathBuf::from(&long_unc));
        for unchanged in [
            r"\\?\Volume{1234}\crux.exe",
            r"\\?\C:",
            r"C:\Users\me\crux.exe",
            "/usr/local/bin/crux",
            "crux",
        ] {
            assert_eq!(strip(unchanged), PathBuf::from(unchanged));
        }
    }

    #[test]
    fn claim_update_check_writes_the_time_and_throttles_for_24_hours() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(UPDATE_CHECK_FILE);
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);

        assert!(claim_update_check(dir.path(), now).expect("first claim"));
        assert_eq!(fs::read_to_string(&path).expect("read"), "1000000");

        assert!(!claim_update_check(dir.path(), now).expect("second claim"));
        let later = now + Duration::from_secs(60);
        assert!(!claim_update_check(dir.path(), later).expect("claim a minute later"));
        let edge = now + UPDATE_CHECK_INTERVAL;
        assert!(!claim_update_check(dir.path(), edge).expect("claim at 24 hours"));
        assert_eq!(fs::read_to_string(&path).expect("read"), "1000000");

        let due = now + UPDATE_CHECK_INTERVAL + Duration::from_secs(1);
        assert!(claim_update_check(dir.path(), due).expect("claim after 24 hours"));
        assert_eq!(fs::read_to_string(&path).expect("read"), "1086401");
    }

    #[test]
    fn claim_update_check_treats_a_corrupt_file_as_due() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(UPDATE_CHECK_FILE), "not a number").expect("write");
        let now = UNIX_EPOCH + Duration::from_secs(5);
        assert!(claim_update_check(dir.path(), now).expect("claim"));
        assert_eq!(
            fs::read_to_string(dir.path().join(UPDATE_CHECK_FILE)).expect("read"),
            "5"
        );
    }

    #[test]
    fn claim_update_check_fails_when_the_index_directory_is_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing");
        assert!(claim_update_check(&missing, SystemTime::now()).is_err());
    }

    #[test]
    fn background_check_does_not_start_inside_the_throttle_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(claim_update_check(dir.path(), SystemTime::now()).expect("claim"));
        assert!(start_background_check(dir.path()).is_none());
    }

    /// A reader that waits, then returns the next scripted result.
    struct ScriptedReader {
        steps: VecDeque<(Duration, io::Result<Vec<u8>>)>,
    }

    impl ScriptedReader {
        fn new(steps: Vec<(Duration, io::Result<Vec<u8>>)>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some((delay, result)) = self.steps.pop_front() else {
                return Ok(0);
            };
            thread::sleep(delay);
            let bytes = result?;
            buffer[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    struct PanicReader;

    impl Read for PanicReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            panic!("reader panic for the test");
        }
    }

    #[test]
    fn idle_copy_finishes_a_slow_transfer_that_keeps_moving() {
        let steps = (0..10)
            .map(|index| (Duration::from_millis(150), Ok(vec![index; 10])))
            .collect();
        let mut output = Vec::new();

        // The whole transfer takes about 1.5 s. That is longer than the 1 s idle limit.
        // Each gap is 150 ms, so a slow CI machine has a large margin.
        let copied = copy_with_idle_timeout(
            ScriptedReader::new(steps),
            &mut output,
            Duration::from_secs(1),
        )
        .expect("copy");

        assert_eq!(copied, 100);
        assert_eq!(output.len(), 100);
        assert_eq!(&output[..10], &[0; 10]);
        assert_eq!(&output[90..], &[9; 10]);
    }

    #[test]
    fn idle_copy_fails_when_the_reader_stalls() {
        let steps = vec![
            (Duration::ZERO, Ok(vec![1; 10])),
            (Duration::from_secs(10), Ok(vec![2; 10])),
        ];
        let mut output = Vec::new();
        let started = Instant::now();

        // The idle limit is 1 s, so a slow CI machine still delivers the first chunk in time.
        let error = copy_with_idle_timeout(
            ScriptedReader::new(steps),
            &mut output,
            Duration::from_secs(1),
        )
        .expect_err("stalled reader");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("no data received"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(output, [1; 10]);
    }

    #[test]
    fn idle_copy_passes_on_reader_errors_and_never_hides_a_stopped_reader() {
        let steps = vec![
            (Duration::ZERO, Ok(vec![1; 4])),
            (
                Duration::ZERO,
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset")),
            ),
        ];
        let mut output = Vec::new();
        let error = copy_with_idle_timeout(ScriptedReader::new(steps), &mut output, TEST_TIMEOUT)
            .expect_err("reader error");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);

        let mut output = Vec::new();
        let error = copy_with_idle_timeout(PanicReader, &mut output, TEST_TIMEOUT)
            .expect_err("panicking reader");
        assert!(error.to_string().contains("stopped early"), "{error}");
    }

    struct LoopbackServer {
        url: String,
        // The server keeps the socket open until the test drops this sender.
        _release: mpsc::Sender<()>,
    }

    /// Serve one HTTP response on the loopback interface. The body arrives piece by piece.
    /// The server then keeps the socket open so a short body stalls instead of ending.
    fn serve_slowly(pieces: Vec<(Duration, Vec<u8>)>, content_length: usize) -> LoopbackServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/data", listener.local_addr().expect("address"));
        let (release, released) = mpsc::channel::<()>();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {}
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for (delay, bytes) in pieces {
                thread::sleep(delay);
                if stream
                    .write_all(&bytes)
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
            }
            let _ = released.recv_timeout(Duration::from_secs(10));
        });
        LoopbackServer {
            url,
            _release: release,
        }
    }

    #[test]
    fn ureq_download_finishes_a_slow_transfer_that_keeps_moving() {
        let pieces = (0..10)
            .map(|_| (Duration::from_millis(150), vec![b'x'; 100]))
            .collect();
        let server = serve_slowly(pieces, 1000);
        let mut output = Vec::new();

        // The transfer takes about 1.5 s. An overall 1 s deadline would end it.
        UreqHttpClient
            .download(&server.url, Duration::from_secs(1), &mut output)
            .expect("slow download");

        assert_eq!(output.len(), 1000);
    }

    #[test]
    fn ureq_download_fails_when_the_server_stops_sending() {
        let server = serve_slowly(vec![(Duration::ZERO, vec![b'x'; 100])], 1000);
        let mut output = Vec::new();
        let started = Instant::now();

        // The limit also applies to connect and to the response head.
        // A 1 s limit gives the loopback server time to reply on a slow CI machine.
        let error = UreqHttpClient
            .download(&server.url, Duration::from_secs(1), &mut output)
            .expect_err("stalled download");

        let text = error_text(&error);
        assert!(text.contains("no data received"), "{text}");
        assert!(text.contains(&server.url), "{text}");
        assert_eq!(
            output.len(),
            100,
            "the first piece arrives before the stall"
        );
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn ureq_get_text_keeps_the_overall_deadline() {
        let slow = serve_slowly(
            (0..10)
                .map(|_| (Duration::from_millis(150), b"x".to_vec()))
                .collect(),
            10,
        );
        let error = UreqHttpClient
            .get_text(&slow.url, Duration::from_millis(500))
            .expect_err("the overall deadline ends a slow reply");
        let text = error_text(&error);
        assert!(text.contains("timeout: global"), "{text}");

        let fast = serve_slowly(vec![(Duration::ZERO, b"hello".to_vec())], 5);
        let text = UreqHttpClient
            .get_text(&fast.url, Duration::from_secs(5))
            .expect("fast reply");
        assert_eq!(text, "hello");
    }

    #[test]
    fn version_comparison_accepts_the_v_prefix() {
        let current = Version::parse("0.4.1").expect("current version");
        let newer = Version::parse("v0.5.0").expect("newer version");
        assert!(newer > current);
        assert!(Version::parse("0.10.0") > Version::parse("0.9.0"));
        assert_eq!(Version::parse("v1.2.3"), Version::parse("1.2.3"));
        assert!(Version::parse("v1.2").is_none());
        assert!(Version::parse("release-1.2.3").is_none());
    }

    #[test]
    fn asset_names_match_each_release_target() {
        let expected = [
            ("aarch64-apple-darwin", "crux-aarch64-apple-darwin"),
            ("x86_64-apple-darwin", "crux-x86_64-apple-darwin"),
            ("x86_64-unknown-linux-gnu", "crux-x86_64-unknown-linux-gnu"),
            (
                "aarch64-unknown-linux-gnu",
                "crux-aarch64-unknown-linux-gnu",
            ),
            ("x86_64-pc-windows-msvc", "crux-x86_64-pc-windows-msvc.exe"),
        ];
        for (target, asset) in expected {
            assert_eq!(asset_name_for_target(target), Some(asset));
        }
        assert_eq!(asset_name_for_target("x86_64-unknown-linux-musl"), None);
    }

    #[test]
    fn update_check_throttle_requires_more_than_24_hours() {
        let now = UNIX_EPOCH + Duration::from_secs(200_000);
        assert!(update_check_due(None, now));
        assert!(!update_check_due(Some(now - UPDATE_CHECK_INTERVAL), now));
        assert!(update_check_due(
            Some(now - UPDATE_CHECK_INTERVAL - Duration::from_secs(1)),
            now
        ));
        assert!(!update_check_due(Some(now + Duration::from_secs(1)), now));
    }

    #[test]
    fn update_notice_uses_the_required_format() {
        let version = Version::parse("v0.5.0").expect("version");
        assert_eq!(
            format_update_notice(&version),
            "note: crux 0.5.0 is available — run crux self-update"
        );
    }
}

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::cmp::Ordering;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/pedr0v/crux/releases/latest";
const UPDATE_CHECK_FILE: &str = "update-check";
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("crux/", env!("CARGO_PKG_VERSION"));

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

struct AvailableUpdate {
    version: Version,
    download_url: String,
}

trait HttpClient {
    fn get_text(&self, url: &str, timeout: Duration) -> Result<String>;
    fn download(&self, url: &str, timeout: Duration, writer: &mut dyn Write) -> Result<()>;
}

struct UreqHttpClient;

impl UreqHttpClient {
    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::AgentBuilder::new().timeout(timeout).build()
    }

    fn request(&self, url: &str, timeout: Duration) -> Result<ureq::Response> {
        Self::agent(timeout)
            .get(url)
            .set("User-Agent", USER_AGENT)
            .call()
            .map_err(|error| anyhow!(error))
            .with_context(|| format!("get {url}"))
    }
}

impl HttpClient for UreqHttpClient {
    fn get_text(&self, url: &str, timeout: Duration) -> Result<String> {
        self.request(url, timeout)?
            .into_string()
            .with_context(|| format!("read {url}"))
    }

    fn download(&self, url: &str, timeout: Duration, writer: &mut dyn Write) -> Result<()> {
        let response = self.request(url, timeout)?;
        io::copy(&mut response.into_reader(), writer).with_context(|| format!("download {url}"))?;
        Ok(())
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
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| anyhow!("release asset is missing: {asset_name}"))?;
    Ok(Some(AvailableUpdate {
        version: latest,
        download_url: asset.browser_download_url,
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
    let pending = download_update(&client, &available, &current_exe)?;
    println!("Install crux {}.", available.version);
    replace_executable(&pending.path, &current_exe)?;
    println!("crux {} is ready.", available.version);
    Ok(())
}

struct PendingFile {
    path: PathBuf,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn download_update(
    client: &dyn HttpClient,
    available: &AvailableUpdate,
    current_exe: &Path,
) -> Result<PendingFile> {
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

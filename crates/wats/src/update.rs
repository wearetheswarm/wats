//! Self-upgrade (`wats upgrade`) and the node's update check.
//!
//! Releases are GitHub releases whose archives are signed with minisign
//! (docs/release-signing.md). An upgrade downloads the archive of this
//! target with its `.sha256` and both `.minisig`, checks both signatures
//! with the public key compiled in below, checks that each trusted comment
//! names the expected file, checks the SHA-256, then swaps the binary in
//! with a rename, keeping the previous one as `<exe>.old`.
//!
//! The node checks the latest release at most once a day
//! (`update.check`), caches what it found in `<data dir>/update.json`,
//! logs a newer version once, and reports it in the MCP `status` tool.
//! With `update.auto`, it installs a release once it is `update.delay` old
//! (7 days by default), and asks for a restart: it never restarts itself.
//! With `update.security_auto`, it does the same for security releases
//! only, once they are `update.security_delay` old (ADR 0030). Neither
//! applies to a Homebrew install, which Homebrew upgrades. A pending
//! security release gets a louder notice ([`security_notice`]) in `wats
//! status`, the MCP `status` tool and once at node start.
//!
//! A hermetic or loopback node never checks or downloads anything.

use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// The minisign public key of releases, the one published in
/// docs/release-signing.md and the README. Rotating it is a release that
/// ships the new key (docs/release-signing.md, "Rotating the key").
pub const RELEASE_PUBLIC_KEY: &str = "RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci";

/// Where releases are looked up: a GitHub API repository URL, or a mirror
/// serving the same JSON (`/releases/latest`, `/releases/tags/vX.Y.Z`).
pub const DEFAULT_URL: &str = "https://api.github.com/repos/wearetheswarm/wats";

/// Overrides `update.url`.
pub const URL_ENV: &str = "WATS_UPDATE_URL";

/// The cache of the last check, in the data directory.
pub const CACHE_FILE: &str = "update.json";

/// At most one check a day.
pub const CHECK_EVERY_SECS: u64 = 24 * 3600;

/// Default of `update.delay`: how old a release must be before
/// `update.auto` installs it.
pub const DEFAULT_DELAY: &str = "7d";

/// How often the node looks at its cache (a check happens only when due).
const LOOP_EVERY: Duration = Duration::from_secs(3600);
/// The first look, after the node has started.
const FIRST_LOOK_AFTER: Duration = Duration::from_secs(60);

const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SMALL_BYTES: usize = 64 * 1024;
const MAX_API_BYTES: usize = 4 * 1024 * 1024;

/// The release target of this binary, if releases ship one. A glibc Linux
/// build also runs the static musl release.
pub fn target() -> Option<&'static str> {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

/// A release version `X.Y.Z`. Prereleases (`1.2.0-rc.1`) are not versions
/// here: they are never offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `X.Y.Z` or `vX.Y.Z`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let mut next = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let v = Version(next()?, next()?, next()?);
        parts.next().is_none().then_some(v)
    }

    /// The version of this binary.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version(0, 0, 0))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// A release, as the GitHub API describes it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Asset {
    pub name: String,
    /// The API URL of the asset (works for private repositories).
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

impl Release {
    /// Its version; none for drafts, prereleases and odd tags.
    pub fn version(&self) -> Option<Version> {
        if self.draft || self.prerelease {
            return None;
        }
        Version::parse(&self.tag_name)
    }

    pub fn is_security(&self) -> bool {
        is_security_release(
            self.name.as_deref().unwrap_or(""),
            self.body.as_deref().unwrap_or(""),
        )
    }

    /// Publication time, in seconds since the Unix epoch.
    pub fn published_secs(&self) -> Option<u64> {
        self.published_at.as_deref().and_then(parse_timestamp)
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// A security release says so in its name or notes: `[security]`
/// anywhere (any case), or a line of the notes starting with `Security:`.
/// docs/release-signing.md, "Releases and upgrades".
pub fn is_security_release(name: &str, body: &str) -> bool {
    let tagged = |s: &str| s.to_ascii_lowercase().contains("[security]");
    tagged(name)
        || tagged(body)
        || body.lines().any(|line| {
            line.trim_start()
                .get(..9)
                .is_some_and(|p| p.eq_ignore_ascii_case("security:"))
        })
}

/// `YYYY-MM-DDTHH:MM:SSZ` (what GitHub returns) to Unix seconds.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let secs = days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs).ok()
}

/// Unix seconds to `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_timestamp(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Howard Hinnant's algorithms (public domain).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `wats-X.Y.Z-<target>.tar.gz`.
pub fn archive_name(version: Version, target: &str) -> String {
    format!("wats-{version}-{target}.tar.gz")
}

// ---------------------------------------------------------------------
// Verification

/// Check a minisign signature of `data` with `public_key`, and that its
/// trusted comment names `file_name`: an older signed archive served under
/// a new name is refused. Only prehashed (`ED`) signatures are accepted,
/// what minisign 0.12 makes by default.
pub fn verify_signature(
    public_key: &str,
    data: &[u8],
    signature: &str,
    file_name: &str,
) -> Result<()> {
    let key = minisign_verify::PublicKey::from_base64(public_key)
        .map_err(|e| anyhow!("invalid release public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| anyhow!("{file_name}.minisig is not a minisign signature: {e}"))?;
    key.verify(data, &sig, false).map_err(|e| {
        anyhow!("the signature of {file_name} does not verify with the release key: {e}")
    })?;
    let comment = sig.trusted_comment();
    let expected = format!("file:{file_name}");
    ensure!(
        comment.split_whitespace().any(|t| t == expected),
        "the signature of {file_name} was made for another file (trusted comment {comment:?})"
    );
    Ok(())
}

/// The release asset holding the signed security floor (ADR 0030), and its
/// signature `security.json.minisig`.
pub const SECURITY_FILE: &str = "security.json";

/// `security.json`: `{"version":"X.Y.Z","security_floor":"A.B.C"}`.
#[derive(Debug, Deserialize)]
struct SecurityFile {
    version: String,
    security_floor: String,
}

/// Check the `security.json` of release `version`: its minisign signature
/// by `public_key`, made for `file:security.json` (as the archives'), and
/// that it names `version`, so that the file of an older release cannot be
/// replayed to lower the floor. Returns the floor, which is at most
/// `version`.
pub fn verify_security_floor(
    public_key: &str,
    json: &[u8],
    signature: &str,
    version: Version,
) -> Result<Version> {
    verify_signature(public_key, json, signature, SECURITY_FILE)?;
    let file: SecurityFile = serde_json::from_slice(json)
        .context("security.json is not {\"version\", \"security_floor\"}")?;
    let named = Version::parse(&file.version)
        .with_context(|| format!("security.json: version {:?} is not X.Y.Z", file.version))?;
    ensure!(
        named == version,
        "security.json is for {named}, not for the release {version}"
    );
    let floor = Version::parse(&file.security_floor).with_context(|| {
        format!(
            "security.json: security_floor {:?} is not X.Y.Z",
            file.security_floor
        )
    })?;
    ensure!(
        floor <= version,
        "security.json: security_floor {floor} is newer than the release {version}"
    );
    Ok(floor)
}

/// The signed security floor of `release` (ADR 0030): its `security.json`
/// and `security.json.minisig` assets, fetched with `download` and checked
/// with [`verify_security_floor`]. None, never an error, when they are
/// missing (logged at debug: releases before the floor have none), or
/// unsigned, mismatched or unreadable (logged at info): the notes'
/// `Security:` line then decides.
pub async fn security_floor<'a, D, Fut>(
    release: &'a Release,
    public_key: &str,
    download: D,
) -> Option<Version>
where
    D: Fn(&'a Asset) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let version = release.version()?;
    let sig_name = format!("{SECURITY_FILE}.minisig");
    let (Some(json), Some(sig)) = (release.asset(SECURITY_FILE), release.asset(&sig_name)) else {
        tracing::debug!(
            "release v{version} has no signed {SECURITY_FILE}: its notes say whether it is a security release"
        );
        return None;
    };
    let checked = async {
        let json = download(json).await?;
        let sig = String::from_utf8(download(sig).await?)
            .map_err(|_| anyhow!("{sig_name} is not text"))?;
        verify_security_floor(public_key, &json, &sig, version)
    }
    .await;
    match checked {
        Ok(floor) => Some(floor),
        Err(e) => {
            tracing::info!(
                "release v{version}: {e:#}; its notes say whether it is a security release"
            );
            None
        }
    }
}

/// Check `data` against a `shasum -a 256` line naming `file_name`.
pub fn verify_sha256(data: &[u8], sha_file: &str, file_name: &str) -> Result<()> {
    let mut parts = sha_file.lines().next().unwrap_or("").split_whitespace();
    let (Some(expected), Some(name)) = (parts.next(), parts.next()) else {
        bail!("{file_name}.sha256 is not a checksum line");
    };
    ensure!(
        name.trim_start_matches('*') == file_name,
        "{file_name}.sha256 is the checksum of another file ({name})"
    );
    let actual = hex::encode(sha2::Sha256::digest(data));
    ensure!(
        expected.eq_ignore_ascii_case(&actual),
        "SHA-256 mismatch for {file_name}: expected {expected}, got {actual}"
    );
    Ok(())
}

/// The four files of a release archive.
#[derive(Debug, Clone, Default)]
pub struct ReleaseFiles {
    pub archive: Vec<u8>,
    pub archive_sig: String,
    pub sha256: String,
    pub sha256_sig: String,
}

/// Check both signatures, their file names, and the checksum.
pub fn verify_release_files(public_key: &str, name: &str, files: &ReleaseFiles) -> Result<()> {
    verify_signature(public_key, &files.archive, &files.archive_sig, name)?;
    let sha_name = format!("{name}.sha256");
    verify_signature(
        public_key,
        files.sha256.as_bytes(),
        &files.sha256_sig,
        &sha_name,
    )?;
    verify_sha256(&files.archive, &files.sha256, name)
}

/// The `wats` binary out of a verified release archive.
pub fn extract_binary(archive: &[u8], archive_name: &str) -> Result<Vec<u8>> {
    let dir = archive_name.strip_suffix(".tar.gz").unwrap_or(archive_name);
    let wanted = format!("{dir}/wats");
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("read the archive")? {
        let mut entry = entry.context("read the archive")?;
        if entry.path().context("read the archive")?.as_ref() != Path::new(&wanted) {
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file(),
            "{wanted} is not a file"
        );
        let mut binary = Vec::new();
        entry
            .by_ref()
            .take(MAX_BINARY_BYTES + 1)
            .read_to_end(&mut binary)
            .context("read the archive")?;
        ensure!(
            (binary.len() as u64) <= MAX_BINARY_BYTES,
            "{wanted} is too large"
        );
        ensure!(!binary.is_empty(), "{wanted} is empty");
        return Ok(binary);
    }
    bail!("the archive has no {wanted}")
}

// ---------------------------------------------------------------------
// Installation

/// The running executable, symlinks resolved.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("find the running executable")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// Whether Homebrew installed this binary (`…/Cellar/wats/<version>/…`):
/// then Homebrew upgrades it (`brew upgrade wats`), not `wats upgrade`, so
/// that it keeps track of the installed version.
pub fn homebrew_managed(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts.windows(2).any(|w| w[0] == "Cellar" && w[1] == "wats")
}

/// Whether Homebrew installed the running binary ([`homebrew_managed`]).
pub fn homebrew_install() -> bool {
    current_exe().is_ok_and(|exe| homebrew_managed(&exe))
}

/// Where releases are published, for a platform with no prebuilt one.
pub const RELEASES_PAGE: &str = "https://github.com/wearetheswarm/wats/releases";

/// How this binary is updated: by `wats upgrade` (a prebuilt release of
/// this platform), by Homebrew, or by a build from source (no prebuilt
/// release for this platform, [`target`]). Only the first one lets the
/// node install a release by itself (ADR 0030).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    Release,
    Homebrew,
    Source,
}

impl Install {
    /// This binary's.
    pub fn detect() -> Install {
        if homebrew_install() {
            Install::Homebrew
        } else if target().is_none() {
            Install::Source
        } else {
            Install::Release
        }
    }

    /// What updates it, for `status` (`upgrade`): a command, or how to
    /// build one.
    pub fn hint(self) -> &'static str {
        match self {
            Install::Release => "wats upgrade",
            Install::Homebrew => "brew upgrade wats",
            Install::Source => "build it from source",
        }
    }

    /// The end of "wats X is available, this is Y: …".
    fn how(self) -> String {
        match self {
            Install::Source => format!(
                "no prebuilt release for this platform: build it from source ({RELEASES_PAGE})"
            ),
            install => format!("`{}` installs it", install.hint()),
        }
    }
}

/// The command that upgrades this binary: `brew upgrade wats` for a
/// Homebrew install, `wats upgrade` otherwise, or that it is built from
/// source ([`Install::hint`]).
pub fn upgrade_hint() -> &'static str {
    Install::detect().hint()
}

/// `<exe>.old`: the previous binary, kept for `wats upgrade --rollback`.
pub fn old_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

/// Replace `exe` by `binary` atomically: the new binary is written next to
/// it, `check`ed, then renamed over it, so `exe` is always either the old or
/// the new binary. The old one is kept as `<exe>.old`. Returns that path.
pub fn install_binary(
    exe: &Path,
    binary: &[u8],
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    let dir = exe
        .parent()
        .with_context(|| format!("{} has no directory", exe.display()))?;
    let name = exe.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.upgrade-{}", std::process::id()));
    let cannot = |what: &str| {
        format!(
            "cannot {what} in {}: upgrade needs write access to the directory of the wats binary",
            dir.display()
        )
    };
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<PathBuf> {
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o755);
            }
            let mut file = options
                .open(&tmp)
                .with_context(|| cannot("write the new binary"))?;
            file.write_all(binary)
                .and_then(|()| file.sync_all())
                .with_context(|| cannot("write the new binary"))?;
        }
        check(&tmp)?;
        let old = old_path(exe);
        match std::fs::remove_file(&old) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| cannot("replace the previous .old binary")),
        }
        // A hard link keeps the old binary at no cost; a copy otherwise.
        if std::fs::hard_link(exe, &old).is_err() {
            std::fs::copy(exe, &old).with_context(|| cannot("keep the current binary as .old"))?;
        }
        std::fs::rename(&tmp, exe).with_context(|| cannot("swap in the new binary"))?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(old)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Put `<exe>.old` back in place of `exe`. Returns the restored path.
pub fn rollback(exe: &Path) -> Result<PathBuf> {
    let old = old_path(exe);
    ensure!(
        old.is_file(),
        "no previous binary to restore: {} does not exist",
        old.display()
    );
    std::fs::rename(&old, exe)
        .with_context(|| format!("restore {} over {}", old.display(), exe.display()))?;
    Ok(old)
}

/// Keys of config.toml added after 0.3.3, with the first release that
/// knows them: an older wats refuses a config.toml that sets one (unknown
/// keys are errors). A release adding a key lists it here; a test checks
/// that every key not in `tests/fixtures/keys-0.3.3.txt` is listed.
pub const KEYS_SINCE: &[(&str, Version)] = &[
    ("ask.cap_grace", Version(0, 4, 0)),
    ("ask.max_responders", Version(0, 4, 0)),
    ("reputation.threshold", Version(0, 4, 0)),
    ("limits.get_per_min", Version(0, 4, 1)),
    ("infra_relays", Version(0, 4, 1)),
    ("infra_dns", Version(0, 4, 1)),
    ("storage.events_share", Version(0, 4, 1)),
    ("sync.initial_max", Version(0, 4, 1)),
    ("update.security_auto", Version(0, 4, 3)),
    ("agent.max_minutes_per_day", Version(0, 4, 5)),
    ("agent.max_minutes_per_month", Version(0, 4, 5)),
    ("agent.max_tokens_per_day", Version(0, 4, 5)),
    ("agent.max_tokens_per_month", Version(0, 4, 5)),
];

/// The keys set in `data_dir`'s config.toml that wats `version` does not
/// know ([`KEYS_SINCE`]). None when config.toml is absent or unreadable.
/// A key of one agent (`agent.<leaf>`) is looked for in each
/// `[agents.<name>]` table, and named so.
pub fn keys_unknown_to(data_dir: &Path, version: Version) -> Vec<String> {
    let Ok(file) = crate::settings::ConfigFile::open(data_dir) else {
        return Vec::new();
    };
    let agents = file.agent_names();
    let mut keys = Vec::new();
    for (key, since) in KEYS_SINCE {
        if version >= *since {
            continue;
        }
        match key.strip_prefix("agent.") {
            Some(leaf) if crate::settings::is_agent_leaf(leaf) && !agents.is_empty() => {
                for name in &agents {
                    let named = format!("agents.{name}.{leaf}");
                    if file.get(&named).is_some() {
                        keys.push(named);
                    }
                }
            }
            _ if file.get(key).is_some() => keys.push(key.to_string()),
            _ => {}
        }
    }
    keys
}

/// How long `<binary> --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `<binary> --version` with no environment and stdin closed, for at
/// most [`VERSION_TIMEOUT`] (then it is killed): its exit status and
/// stdout.
fn run_version(binary: &Path) -> Result<(std::process::ExitStatus, String)> {
    use std::process::Stdio;
    let mut child = std::process::Command::new(binary)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("run {}", binary.display()))?;
    let deadline = std::time::Instant::now() + VERSION_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{} --version` did not answer within {} s",
                binary.display(),
                VERSION_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout)?;
    }
    Ok((status, stdout))
}

/// The version `<binary> --version` prints (`wats X.Y.Z`), run as
/// [`check_version_output`] runs a new binary. None, logged, when it does
/// not run, fails or prints something else.
pub fn binary_version(binary: &Path) -> Option<Version> {
    match run_version(binary) {
        Ok((status, stdout)) if status.success() => {
            let version = stdout.trim().strip_prefix("wats ").and_then(Version::parse);
            if version.is_none() {
                tracing::warn!(
                    "{} --version printed {:?}, not `wats X.Y.Z`",
                    binary.display(),
                    stdout.trim()
                );
            }
            version
        }
        Ok((status, _)) => {
            tracing::warn!("{} --version failed ({status})", binary.display());
            None
        }
        Err(e) => {
            tracing::warn!("{e:#}");
            None
        }
    }
}

/// Before `wats upgrade --rollback`: when the previous binary (`old`) is
/// older than `current`, refuse while config.toml sets keys it would
/// refuse, and name the `wats config unset` commands to run first, with
/// this binary (the older one cannot unset a key it does not know, nor
/// start with it). A previous binary that does not say its version is not
/// checked.
pub fn check_rollback(old: &Path, data_dir: &Path, current: Version) -> Result<()> {
    let Some(version) = binary_version(old).filter(|v| *v < current) else {
        return Ok(());
    };
    let keys = keys_unknown_to(data_dir, version);
    if keys.is_empty() {
        return Ok(());
    }
    let dir = shell_word(&data_dir.display().to_string());
    let commands: Vec<String> = keys
        .iter()
        .map(|key| format!("  wats --dir {dir} config unset {key}"))
        .collect();
    bail!(
        "the previous binary is wats {version}, which refuses {} in {}: it would not start. \
         Unset {} first, with this wats:\n{}\nthen run `wats upgrade --rollback` again",
        keys.join(", "),
        data_dir.join(crate::settings::FILE_NAME).display(),
        if keys.len() == 1 { "it" } else { "them" },
        commands.join("\n")
    )
}

/// `s` as one shell word: as is when plain, else single-quoted.
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%=,".contains(&b));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Run `<binary> --version` and expect `wats <version>`: the new binary
/// runs on this machine before it replaces the current one.
pub fn check_version_output(binary: &Path, version: Version) -> Result<()> {
    let (status, stdout) = run_version(binary).context("run the new binary")?;
    ensure!(
        status.success() && stdout.split_whitespace().nth(1) == Some(&version.to_string()),
        "the new binary does not run as wats {version} on this machine (got {:?})",
        stdout.trim()
    );
    Ok(())
}

// ---------------------------------------------------------------------
// Where releases come from

/// The release source: `WATS_UPDATE_URL`, else `update.url`, else GitHub.
/// A token from `GH_TOKEN` or `GITHUB_TOKEN`, optional (it only raises
/// GitHub's rate limits), is sent to api.github.com only, never to a
/// mirror.
#[derive(Debug, Clone)]
pub struct Source {
    base: String,
    token: Option<String>,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Source {
    pub fn new(base: &str, token: Option<String>) -> Source {
        let base = base.trim().trim_end_matches('/').to_string();
        let token = token.filter(|_| Self::is_github(&base));
        Source { base, token }
    }

    pub fn from_env(setting: Option<&str>) -> Source {
        let base = env_nonempty(URL_ENV)
            .or_else(|| setting.map(str::to_string))
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        let token = env_nonempty("GH_TOKEN").or_else(|| env_nonempty("GITHUB_TOKEN"));
        Source::new(&base, token)
    }

    fn is_github(base: &str) -> bool {
        url::Url::parse(base).is_ok_and(|u| {
            u.scheme() == "https" && u.host_str() == Some("api.github.com") && u.port().is_none()
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn client() -> Result<reqwest::Client> {
        // reqwest uses the process-wide rustls provider: ring, like iroh.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .user_agent(concat!("wats/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build()
            .context("set up HTTPS")
    }

    async fn get(&self, url: &str, accept: &str, limit: usize) -> Result<Vec<u8>> {
        let mut request = Self::client()?
            .get(url)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| anyhow!("cannot reach {url}: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let rate_limited = matches!(status.as_u16(), 403 | 429);
            let hint = if rate_limited && self.token.is_none() && Self::is_github(&self.base) {
                " (GitHub's rate limit? set GH_TOKEN or GITHUB_TOKEN to raise it)"
            } else {
                ""
            };
            bail!("{url} answered HTTP {}{hint}", status.as_u16());
        }
        if response.content_length().is_some_and(|n| n > limit as u64) {
            bail!("{url} is larger than {limit} bytes");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| anyhow!("download {url}: {e}"))?
        {
            ensure!(
                body.len() + chunk.len() <= limit,
                "{url} is larger than {limit} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn release(&self, path: &str) -> Result<Release> {
        let url = format!("{}/{path}", self.base);
        let body = self
            .get(&url, "application/vnd.github+json", MAX_API_BYTES)
            .await?;
        serde_json::from_slice(&body).with_context(|| format!("{url}: not a release"))
    }

    /// The latest release (GitHub never returns drafts or prereleases here).
    pub async fn latest(&self) -> Result<Release> {
        self.release("releases/latest").await
    }

    /// The release of tag `vX.Y.Z`.
    pub async fn tagged(&self, version: Version) -> Result<Release> {
        self.release(&format!("releases/tags/v{version}")).await
    }

    /// The latest release and its signed security floor
    /// ([`Source::security_floor`]), for the daily check.
    pub async fn latest_with_floor(&self) -> Result<(Release, Option<Version>)> {
        let release = self.latest().await?;
        let floor = security_floor(&release, RELEASE_PUBLIC_KEY, |asset| {
            self.download(asset, MAX_SMALL_BYTES)
        })
        .await;
        Ok((release, floor))
    }

    async fn download(&self, asset: &Asset, limit: usize) -> Result<Vec<u8>> {
        if Self::is_github(&self.base) && !asset.url.is_empty() {
            self.get(&asset.url, "application/octet-stream", limit)
                .await
        } else {
            ensure!(
                !asset.browser_download_url.is_empty(),
                "{} has no download URL",
                asset.name
            );
            self.get(
                &asset.browser_download_url,
                "application/octet-stream",
                limit,
            )
            .await
        }
    }

    /// Download the archive of `target` and its checksum and signatures,
    /// and verify them. Refuses when one is missing.
    pub async fn fetch_verified(
        &self,
        release: &Release,
        version: Version,
        target: &str,
        say: &(dyn Fn(String) + Sync),
    ) -> Result<Vec<u8>> {
        let name = archive_name(version, target);
        let find = |file: String| -> Result<&Asset> {
            release.asset(&file).with_context(|| {
                format!("release v{version} has no {file}: refusing to install it")
            })
        };
        let archive = find(name.clone())?;
        let archive_sig = find(format!("{name}.minisig"))?;
        let sha256 = find(format!("{name}.sha256"))?;
        let sha256_sig = find(format!("{name}.sha256.minisig"))?;
        say(format!(
            "downloading {name} ({})",
            crate::node::format_bytes(archive.size)
        ));
        let text = |bytes: Vec<u8>, what: &str| {
            String::from_utf8(bytes).map_err(|_| anyhow!("{what} is not text"))
        };
        let files = ReleaseFiles {
            archive: self.download(archive, MAX_ARCHIVE_BYTES).await?,
            archive_sig: text(
                self.download(archive_sig, MAX_SMALL_BYTES).await?,
                &archive_sig.name,
            )?,
            sha256: text(self.download(sha256, MAX_SMALL_BYTES).await?, &sha256.name)?,
            sha256_sig: text(
                self.download(sha256_sig, MAX_SMALL_BYTES).await?,
                &sha256_sig.name,
            )?,
        };
        verify_release_files(RELEASE_PUBLIC_KEY, &name, &files)?;
        say(format!(
            "signatures verified with the release key {RELEASE_PUBLIC_KEY}, file name and SHA-256 match"
        ));
        extract_binary(&files.archive, &name)
    }
}

// ---------------------------------------------------------------------
// `wats upgrade`

pub struct UpgradeOptions {
    pub check: bool,
    pub version: Option<String>,
    pub rollback: bool,
    pub hermetic: bool,
    pub url: Option<String>,
    pub data_dir: PathBuf,
}

/// `wats upgrade`: prints what it does.
pub async fn upgrade_command(opts: UpgradeOptions) -> Result<()> {
    let current = Version::current();
    let restart_note = |version: &str| {
        if crate::node::node_running(&opts.data_dir) {
            println!(
                "a wats node is running on {}: restart it to use {version}",
                opts.data_dir.display()
            );
        }
    };
    let brew = current_exe().is_ok_and(|exe| homebrew_managed(&exe));
    ensure!(
        !brew || opts.check,
        "this wats was installed by Homebrew: upgrade it with `brew upgrade wats`"
    );
    if opts.rollback {
        let exe = current_exe()?;
        check_rollback(&old_path(&exe), &opts.data_dir, current)?;
        let old = rollback(&exe)?;
        println!("restored {} from {}", exe.display(), old.display());
        restart_note("the restored binary");
        return Ok(());
    }
    ensure!(
        !opts.hermetic,
        "--hermetic: wats upgrade never checks or downloads anything"
    );
    let wanted = opts
        .version
        .as_deref()
        .map(|v| Version::parse(v).with_context(|| format!("{v:?} is not a version X.Y.Z")))
        .transpose()?;
    let target = target().context(
        "no prebuilt release for this platform (releases ship x86_64 Linux and Apple silicon macOS); build from source",
    )?;
    let source = Source::from_env(opts.url.as_deref());
    println!("looking up releases at {}", source.base());
    let release = match wanted {
        Some(v) => source.tagged(v).await?,
        None => source.latest().await?,
    };
    let version = release.version().with_context(|| {
        format!(
            "release {} is a draft, a prerelease or not X.Y.Z: not installing it",
            release.tag_name
        )
    })?;
    if let Some(v) = wanted {
        ensure!(
            v == version,
            "asked for v{v}, the release is {}",
            release.tag_name
        );
    }
    let security = if release.is_security() {
        " (security release)"
    } else {
        ""
    };
    let published = release
        .published_at
        .as_deref()
        .map(|p| format!(", published {p}"))
        .unwrap_or_default();
    println!("this is wats {current}; release {version}{security}{published}");
    if opts.check {
        if version > current {
            let how = if brew {
                "brew upgrade wats"
            } else {
                "wats upgrade"
            };
            println!("wats {version} is available: `{how}` installs it");
        } else {
            println!("wats is up to date");
        }
        return Ok(());
    }
    // An explicit --version installs that release, even the same one (a
    // repair) or an older one.
    if wanted.is_none() && version <= current {
        println!("wats is up to date");
        return Ok(());
    }
    let exe = current_exe()?;
    let binary = source
        .fetch_verified(&release, version, target, &|line| println!("{line}"))
        .await?;
    let old = tokio::task::spawn_blocking(move || {
        install_binary(&exe, &binary, |tmp| check_version_output(tmp, version))
            .map(|old| (exe, old))
    })
    .await
    .context("install task")??;
    println!(
        "installed wats {version} at {}; the previous binary is {} (`wats upgrade --rollback` restores it)",
        old.0.display(),
        old.1.display()
    );
    restart_note(&version.to_string());
    Ok(())
}

// ---------------------------------------------------------------------
// The node's check

/// `[update]` of config.toml, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// Look for a newer release once a day (`update.check`).
    pub check: bool,
    /// Install it by itself (`update.auto`).
    pub auto: bool,
    /// Install a security release by itself, even without `auto`
    /// (`update.security_auto`, ADR 0030).
    pub security_auto: bool,
    /// How old a release must be before `auto` installs it.
    pub delay: Duration,
    /// The same for a security release; unset means `delay`.
    pub security_delay: Option<Duration>,
    /// `update.url`.
    pub url: Option<String>,
}

impl Default for UpdatePolicy {
    /// Off: a node built from the library checks nothing unless told to;
    /// `wats` turns the check on from config.toml (default true).
    fn default() -> Self {
        UpdatePolicy {
            check: false,
            auto: false,
            security_auto: false,
            delay: Duration::from_secs(7 * 86400),
            security_delay: None,
            url: None,
        }
    }
}

impl UpdatePolicy {
    /// The delay before `auto` (or `security_auto`) installs a release.
    pub fn delay_for(&self, security: bool) -> Duration {
        match (security, self.security_delay) {
            (true, Some(d)) => d,
            _ => self.delay,
        }
    }

    /// The key that lets the node install a release by itself, if any:
    /// `update.auto` for any release, `update.security_auto` for a
    /// security one.
    pub fn auto_key(&self, security: bool) -> Option<&'static str> {
        if self.auto {
            Some("update.auto")
        } else if self.security_auto && security {
            Some("update.security_auto")
        } else {
            None
        }
    }
}

/// `<data dir>/update.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    /// Last check, Unix seconds (also after a failed one).
    pub checked_at: u64,
    pub latest: Option<String>,
    /// The latest release's notes mark it security (`Security:`), the
    /// fallback when no signed floor is known.
    #[serde(default)]
    pub security: bool,
    /// The latest release's signed security floor (`security.json`, ADR
    /// 0030), when it had a valid one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_floor: Option<String>,
    pub published_at: Option<u64>,
    /// The newer version already logged.
    pub notified: Option<String>,
    /// The version `update.auto` installed.
    pub installed: Option<String>,
}

impl Cache {
    /// Whether the latest release counts as a security release for a
    /// node running `current`: by the signed floor when there is one
    /// (`current` older than the floor), else by the notes.
    pub fn is_security(&self, current: Version) -> bool {
        match self.security_floor.as_deref().and_then(Version::parse) {
            Some(floor) => current < floor,
            None => self.security,
        }
    }

    pub fn load(data_dir: &Path) -> Option<Cache> {
        let text = std::fs::read(data_dir.join(CACHE_FILE)).ok()?;
        serde_json::from_slice(&text).ok()
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = data_dir.join(CACHE_FILE);
        let tmp = data_dir.join(format!("{CACHE_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .with_context(|| format!("write {}", path.display()))
    }

    pub fn latest(&self) -> Option<Version> {
        self.latest.as_deref().and_then(Version::parse)
    }
}

/// Whether a check is due: none yet, a day since the last, or a clock that
/// went back more than a day.
pub fn check_due(cache: Option<&Cache>, now: u64) -> bool {
    match cache {
        None => true,
        Some(c) => {
            now >= c.checked_at.saturating_add(CHECK_EVERY_SECS)
                || c.checked_at > now.saturating_add(CHECK_EVERY_SECS)
        }
    }
}

/// Whether this node may check at all: never in loopback (and so hermetic)
/// mode, nor with `WATS_HERMETIC` set.
pub fn may_check(policy: &UpdatePolicy, loopback: bool) -> bool {
    policy.check && !loopback && !hermetic_env()
}

fn hermetic_env() -> bool {
    std::env::var("WATS_HERMETIC").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The version the node should install by itself now, if any: one that
/// `update.auto` (any release) or `update.security_auto` (a security
/// release) allows ([`UpdatePolicy::auto_key`]), newer than this one, not
/// installed already, and old enough. Only for an [`Install::Release`]:
/// never for a Homebrew install (Homebrew upgrades it) nor a platform
/// with no prebuilt release.
pub fn auto_target(
    policy: &UpdatePolicy,
    cache: &Cache,
    current: Version,
    now: u64,
    install: Install,
) -> Option<Version> {
    let security = cache.is_security(current);
    if install != Install::Release || policy.auto_key(security).is_none() {
        return None;
    }
    let latest = cache.latest()?;
    if latest <= current || cache.installed.as_deref() == Some(&latest.to_string()) {
        return None;
    }
    let published = cache.published_at?;
    let delay = policy.delay_for(security).as_secs();
    (now >= published.saturating_add(delay)).then_some(latest)
}

/// One look: check the latest release and its signed security floor if
/// due (`fetch`, [`Source::latest_with_floor`]), record the result in the
/// cache, and return the cache. Without `policy.check`, nothing is read,
/// fetched or written.
pub async fn tick<F, Fut>(
    policy: &UpdatePolicy,
    data_dir: &Path,
    now: u64,
    fetch: F,
) -> Result<Option<Cache>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(Release, Option<Version>)>>,
{
    if !policy.check {
        return Ok(None);
    }
    let mut cache = Cache::load(data_dir).unwrap_or_default();
    if !check_due(Some(&cache).filter(|c| c.checked_at > 0), now) {
        return Ok(Some(cache));
    }
    cache.checked_at = now;
    let fetched = fetch().await;
    if let Ok((release, floor)) = &fetched
        && let Some(version) = release.version()
    {
        cache.latest = Some(version.to_string());
        cache.security = release.is_security();
        cache.security_floor = floor.map(|f| f.to_string());
        cache.published_at = release.published_secs();
    }
    cache.save(data_dir)?;
    fetched.map(|_| Some(cache))
}

/// The louder notice of a security release newer than `current` (ADR
/// 0030), from the cache of the daily check: that it is a security
/// release, its version and the command that installs it; or, once the
/// node installed it by itself, that a restart is needed. When the node
/// will install it by itself (`update.auto` or `update.security_auto`,
/// an [`Install::Release`] only), from when. Without a prebuilt release
/// for this platform, where to get the source. Nothing for a normal
/// release.
pub fn security_notice(
    cache: &Cache,
    current: Version,
    policy: &UpdatePolicy,
    install: Install,
) -> Option<String> {
    let latest = cache.latest()?;
    if latest <= current || !cache.is_security(current) {
        return None;
    }
    if cache.installed.as_deref() == Some(&latest.to_string()) {
        return Some(format!(
            "SECURITY RELEASE: wats {latest} is installed, but this node still runs {current}: \
             restart wats to use it."
        ));
    }
    let how = match install {
        Install::Source => format!(
            "build wats {latest} from source (no prebuilt release for this platform: \
             {RELEASES_PAGE})"
        ),
        install => format!("`{}`", install.hint()),
    };
    let mut notice = format!(
        "SECURITY RELEASE: wats {latest} is out and fixes a security issue; this node runs \
         {current}. Update now: {how}, then restart wats."
    );
    if let Some(key) = policy
        .auto_key(true)
        .filter(|_| policy.check && install == Install::Release)
        && let Some(published) = cache.published_at
    {
        let from = published.saturating_add(policy.delay_for(true).as_secs());
        notice.push_str(&format!(
            " Otherwise {key} installs it by itself from {}.",
            format_timestamp(from)
        ));
    }
    Some(notice)
}

/// What the MCP `status` tool and `wats status` say about updates, from
/// the cache; with `notice` for a security release ([`security_notice`]).
pub fn status_json(data_dir: &Path, policy: &UpdatePolicy) -> Option<serde_json::Value> {
    let cache = Cache::load(data_dir)?;
    status_json_of(&cache, Version::current(), policy, Install::detect())
}

/// Whether `wats status` and the MCP `status` tool show the cache of the
/// daily check: only when the node looks for releases (`update.check`;
/// off for hermetic and loopback nodes), so that a stale `update.json`
/// says nothing. `WATS_TEST_UPDATE_STATUS=1` shows it anyway, for tests,
/// and only with `WATS_HERMETIC` set (which never checks or downloads).
pub fn shows_status(policy: &UpdatePolicy) -> bool {
    policy.check
        || (hermetic_env() && std::env::var("WATS_TEST_UPDATE_STATUS").as_deref() == Ok("1"))
}

/// [`status_json`] of a given cache, version and install.
fn status_json_of(
    cache: &Cache,
    current: Version,
    policy: &UpdatePolicy,
    install: Install,
) -> Option<serde_json::Value> {
    let latest = cache.latest()?;
    let mut status = serde_json::json!({
        "current": current.to_string(),
        "latest": latest.to_string(),
        "available": latest > current,
        "security": latest > current && cache.is_security(current),
        "checked_at": format_timestamp(cache.checked_at),
        "upgrade": install.hint(),
    });
    if let Some(notice) = security_notice(cache, current, policy, install) {
        status["notice"] = notice.into();
    }
    Some(status)
}

/// The notice of a pending security release at node start (once per
/// start): from the cache, without checking or installing anything. None
/// when the node does not look for releases (`update.check` off).
pub fn start_notice(data_dir: &Path, policy: &UpdatePolicy) -> Option<String> {
    if !policy.check {
        return None;
    }
    security_notice(
        &Cache::load(data_dir)?,
        Version::current(),
        policy,
        Install::detect(),
    )
}

/// The node's update loop: at most one check a day, a log line per new
/// version, and `update.auto`. Runs until the task is dropped.
pub async fn run(policy: UpdatePolicy, data_dir: PathBuf) {
    let source = Source::from_env(policy.url.as_deref());
    let current = Version::current();
    let mut failed_auto: Option<(Version, u64)> = None;
    tokio::time::sleep(FIRST_LOOK_AFTER).await;
    loop {
        let now = now_secs();
        match tick(&policy, &data_dir, now, || source.latest_with_floor()).await {
            Ok(Some(mut cache)) => {
                if let Some(latest) = cache.latest()
                    && latest > current
                    && cache.notified.as_deref() != Some(&latest.to_string())
                {
                    match security_notice(&cache, current, &policy, Install::detect()) {
                        Some(notice) => tracing::warn!("{notice}"),
                        None => tracing::warn!(
                            "wats {latest} is available, this is {current}: {}",
                            Install::detect().how()
                        ),
                    }
                    cache.notified = Some(latest.to_string());
                    let _ = cache.save(&data_dir);
                }
                if let Some(version) = auto_target(&policy, &cache, current, now, Install::detect())
                    && let Some(key) = policy.auto_key(cache.is_security(current))
                    && failed_auto
                        .is_none_or(|(v, at)| v != version || now >= at + CHECK_EVERY_SECS)
                {
                    tracing::info!("{key}: installing wats {version}");
                    match auto_install(&source, version, key).await {
                        Ok(exe) => {
                            tracing::warn!(
                                "wats {version} installed at {} ({key}): restart wats to use it",
                                exe.display()
                            );
                            cache.installed = Some(version.to_string());
                            let _ = cache.save(&data_dir);
                        }
                        Err(e) => {
                            tracing::warn!("{key}: installing wats {version} failed: {e:#}");
                            failed_auto = Some((version, now));
                        }
                    }
                }
            }
            Ok(None) => return,
            Err(e) => tracing::info!("update check failed: {e:#}"),
        }
        tokio::time::sleep(LOOP_EVERY).await;
    }
}

/// Install `version` with the checks of `wats upgrade` (both signatures,
/// the SHA-256, the new binary runs as that version); `key`, the setting
/// that allowed it, for the log.
async fn auto_install(source: &Source, version: Version, key: &str) -> Result<PathBuf> {
    let target = target().context("no prebuilt release for this platform")?;
    let release = source.tagged(version).await?;
    ensure!(
        release.version() == Some(version),
        "release {} is not v{version}",
        release.tag_name
    );
    let binary = source
        .fetch_verified(&release, version, target, &|line| {
            tracing::info!("{key}: {line}")
        })
        .await?;
    let exe = current_exe()?;
    tokio::task::spawn_blocking(move || {
        install_binary(&exe, &binary, |tmp| check_version_output(tmp, version)).map(|_| exe)
    })
    .await
    .context("install task")?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Made with the minisign 0.12 CLI and a throwaway password-less key
    /// (`test.pub`; the secret key was not kept), like release.yml does:
    /// `minisign -S` of the archive and of its `shasum -a 256` line, plus a
    /// signature by another key (`.other-key.minisig`) and one of the same
    /// archive under another name (`.wrong-file.minisig`). The archive holds
    /// `wats-9.9.9-x86_64-unknown-linux-musl/wats`, a script printing
    /// `wats 9.9.9`.
    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/upgrade/");
    const NAME: &str = "wats-9.9.9-x86_64-unknown-linux-musl.tar.gz";

    fn fixture(file: &str) -> Vec<u8> {
        std::fs::read(format!("{FIX}{file}")).unwrap()
    }
    fn fixture_text(file: &str) -> String {
        String::from_utf8(fixture(file)).unwrap()
    }
    fn test_key() -> String {
        fixture_text("test.pub").trim().to_string()
    }
    fn files() -> ReleaseFiles {
        ReleaseFiles {
            archive: fixture(NAME),
            archive_sig: fixture_text(&format!("{NAME}.minisig")),
            sha256: fixture_text(&format!("{NAME}.sha256")),
            sha256_sig: fixture_text(&format!("{NAME}.sha256.minisig")),
        }
    }

    #[test]
    fn compiled_key_is_the_published_one() {
        let doc = include_str!("../../../docs/release-signing.md");
        let section = doc.split("## Public key").nth(1).unwrap();
        let published = section
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("RW"))
            .unwrap();
        assert_eq!(published, RELEASE_PUBLIC_KEY);
        let readme = include_str!("../../../README.md");
        assert!(readme.contains(RELEASE_PUBLIC_KEY));
        minisign_verify::PublicKey::from_base64(RELEASE_PUBLIC_KEY).unwrap();
    }

    #[test]
    fn accepts_a_good_release() {
        verify_release_files(&test_key(), NAME, &files()).unwrap();
        let binary = extract_binary(&files().archive, NAME).unwrap();
        assert!(String::from_utf8_lossy(&binary).contains("wats 9.9.9"));
    }

    #[test]
    fn refuses_the_wrong_key() {
        // The real release key did not sign the fixture.
        let err = verify_release_files(RELEASE_PUBLIC_KEY, NAME, &files()).unwrap_err();
        assert!(err.to_string().contains("does not verify"), "{err}");
        // Another key's signature.
        let mut f = files();
        f.archive_sig = fixture_text(&format!("{NAME}.other-key.minisig"));
        assert!(verify_release_files(&test_key(), NAME, &f).is_err());
    }

    #[test]
    fn refuses_a_tampered_archive() {
        let mut f = files();
        let last = f.archive.len() - 1;
        f.archive[last] ^= 1;
        let err = verify_release_files(&test_key(), NAME, &f).unwrap_err();
        assert!(err.to_string().contains("does not verify"), "{err}");
    }

    #[test]
    fn refuses_a_signature_for_another_file() {
        let mut f = files();
        f.archive_sig = fixture_text(&format!("{NAME}.wrong-file.minisig"));
        let err = verify_release_files(&test_key(), NAME, &f).unwrap_err();
        assert!(err.to_string().contains("another file"), "{err}");
        // The right signature, asked under another name.
        let other = "wats-9.9.8-x86_64-unknown-linux-musl.tar.gz";
        let err =
            verify_signature(&test_key(), &f.archive, &files().archive_sig, other).unwrap_err();
        assert!(err.to_string().contains("another file"), "{err}");
    }

    #[test]
    fn refuses_a_missing_or_broken_signature() {
        let mut f = files();
        f.archive_sig = String::new();
        assert!(verify_release_files(&test_key(), NAME, &f).is_err());
        let mut f = files();
        f.sha256_sig = String::new();
        assert!(verify_release_files(&test_key(), NAME, &f).is_err());
        let mut f = files();
        f.sha256_sig = f.archive_sig.clone();
        assert!(verify_release_files(&test_key(), NAME, &f).is_err());
    }

    #[test]
    fn refuses_a_sha256_mismatch() {
        let f = files();
        let bad = format!("{}  {NAME}\n", "00".repeat(32));
        let err = verify_sha256(&f.archive, &bad, NAME).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
        let other = f.sha256.replace(NAME, "other.tar.gz");
        assert!(verify_sha256(&f.archive, &other, NAME).is_err());
        assert!(verify_sha256(&f.archive, "", NAME).is_err());
        verify_sha256(&f.archive, &f.sha256, NAME).unwrap();
        // A signed but mismatching checksum is still refused: the
        // signature check of the archive comes first, so tamper only the
        // data the checksum covers.
        let mut tampered = f.clone();
        tampered.archive.push(0);
        assert!(verify_release_files(&test_key(), NAME, &tampered).is_err());
    }

    #[test]
    fn extract_needs_the_binary() {
        let err = extract_binary(&files().archive, "wats-1.0.0-x.tar.gz").unwrap_err();
        assert!(err.to_string().contains("has no"), "{err}");
        assert!(extract_binary(b"not gzip", NAME).is_err());
    }

    #[test]
    fn replaces_atomically_and_keeps_the_old_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("wats");
        std::fs::write(&exe, b"old binary").unwrap();
        let old = install_binary(&exe, b"new binary", |tmp| {
            // The new binary sits next to the old one before the swap.
            assert_eq!(tmp.parent(), exe.parent());
            assert_eq!(std::fs::read(tmp).unwrap(), b"new binary");
            assert_eq!(std::fs::read(&exe).unwrap(), b"old binary");
            Ok(())
        })
        .unwrap();
        assert_eq!(old, dir.path().join("wats.old"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        assert_eq!(std::fs::read(&old).unwrap(), b"old binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        // A second upgrade replaces .old.
        install_binary(&exe, b"newer", |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), b"new binary");
        // Rollback.
        rollback(&exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        assert!(!old.exists());
        assert!(rollback(&exe).is_err());
        // A failed check leaves everything as it was, and no temp file.
        let err = install_binary(&exe, b"broken", |_| bail!("does not run")).unwrap_err();
        assert!(err.to_string().contains("does not run"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("wats")]);
    }

    #[cfg(unix)]
    #[test]
    fn checks_that_the_new_binary_runs() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("wats");
        std::fs::write(&exe, b"old").unwrap();
        let binary = extract_binary(&files().archive, NAME).unwrap();
        install_binary(&exe, &binary, |tmp| {
            check_version_output(tmp, Version(9, 9, 9))
        })
        .unwrap();
        std::fs::write(&exe, b"old").unwrap();
        let err = install_binary(&exe, &binary, |tmp| {
            check_version_output(tmp, Version(9, 9, 8))
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("does not run as wats 9.9.8"),
            "{err}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0"), Some(Version(0, 10, 0)));
        for bad in [
            "1.2",
            "1.2.3.4",
            "v1.2.3-rc.1",
            "1.2.x",
            "",
            "v",
            "1..3",
            "+1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(Version(0, 10, 0) > Version(0, 9, 12));
        assert!(Version(1, 0, 0) > Version(0, 99, 99));
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
        let release = |tag: &str, draft, prerelease| Release {
            tag_name: tag.into(),
            draft,
            prerelease,
            ..Default::default()
        };
        assert_eq!(
            release("v1.0.0", false, false).version(),
            Some(Version(1, 0, 0))
        );
        assert_eq!(release("v1.0.0", true, false).version(), None);
        assert_eq!(release("v1.0.0", false, true).version(), None);
        assert_eq!(release("v1.0.0-beta", false, false).version(), None);
    }

    #[test]
    fn security_releases() {
        assert!(is_security_release("v1.2.3 [security]", ""));
        assert!(is_security_release("v1.2.3", "Fixes.\n\n[SECURITY]"));
        assert!(is_security_release(
            "v1.2.3",
            "Notes\n  Security: fixes a crash\n"
        ));
        assert!(!is_security_release("v1.2.3", "Improves security docs"));
        assert!(!is_security_release("v1.2.3", "No security: none"));
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2026-10-18T12:34:56Z"), Some(1792326896));
        assert_eq!(format_timestamp(1792326896), "2026-10-18T12:34:56Z");
        assert_eq!(parse_timestamp("2024-02-29T00:00:00Z"), Some(1709164800));
        assert_eq!(parse_timestamp("2026-10-18 12:34:56Z"), None);
        assert_eq!(parse_timestamp("2026-13-18T12:34:56Z"), None);
    }

    const DAY: u64 = 86400;

    fn cache(latest: &str, security: bool, published_at: u64) -> Cache {
        Cache {
            checked_at: published_at,
            latest: Some(latest.into()),
            security,
            published_at: Some(published_at),
            ..Default::default()
        }
    }

    #[test]
    fn auto_upgrade_waits_for_the_delay() {
        let current = Version(1, 0, 0);
        let on = UpdatePolicy {
            check: true,
            auto: true,
            ..Default::default()
        };
        let t0 = 1_000 * DAY;
        let normal = cache("1.1.0", false, t0);
        assert_eq!(
            auto_target(&on, &normal, current, t0 + 6 * DAY, Install::Release),
            None
        );
        assert_eq!(
            auto_target(&on, &normal, current, t0 + 7 * DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        // Off by default.
        let off = UpdatePolicy {
            auto: false,
            ..on.clone()
        };
        assert_eq!(
            auto_target(&off, &normal, current, t0 + 30 * DAY, Install::Release),
            None
        );
        // Not older, not twice.
        assert_eq!(
            auto_target(
                &on,
                &cache("1.0.0", false, t0),
                current,
                t0 + 30 * DAY,
                Install::Release
            ),
            None
        );
        let installed = Cache {
            installed: Some("1.1.0".into()),
            ..normal.clone()
        };
        assert_eq!(
            auto_target(&on, &installed, current, t0 + 30 * DAY, Install::Release),
            None
        );
        // A security release waits update.delay too, unless security_delay
        // is set.
        let security = cache("1.1.0", true, t0);
        assert_eq!(
            auto_target(&on, &security, current, t0 + DAY, Install::Release),
            None
        );
        assert_eq!(
            auto_target(&on, &security, current, t0 + 7 * DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        let fast = UpdatePolicy {
            security_delay: Some(Duration::from_secs(DAY)),
            ..on.clone()
        };
        assert_eq!(
            auto_target(&fast, &security, current, t0 + DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        assert_eq!(
            auto_target(&fast, &security, current, t0 + DAY - 1, Install::Release),
            None
        );
        // security_delay does not shorten a normal release.
        assert_eq!(
            auto_target(&fast, &normal, current, t0 + DAY, Install::Release),
            None
        );
        // Unknown publication time: never.
        let unknown = Cache {
            published_at: None,
            ..normal
        };
        assert_eq!(
            auto_target(&on, &unknown, current, t0 + 30 * DAY, Install::Release),
            None
        );
    }

    /// `update.security_auto` (ADR 0030): security releases only, after
    /// `update.security_delay`; normal releases follow `update.auto`.
    #[test]
    fn security_auto_installs_security_releases_only() {
        let current = Version(1, 0, 0);
        let t0 = 1_000 * DAY;
        let normal = cache("1.1.0", false, t0);
        let security = cache("1.1.0", true, t0);
        let late = t0 + 30 * DAY;
        // Off (the default): neither.
        let off = UpdatePolicy {
            check: true,
            ..Default::default()
        };
        assert!(!off.security_auto);
        assert_eq!(off.auto_key(true), None);
        assert_eq!(
            auto_target(&off, &security, current, late, Install::Release),
            None
        );
        assert_eq!(
            auto_target(&off, &normal, current, late, Install::Release),
            None
        );
        // On, update.auto off: the security release, not the normal one.
        let sec = UpdatePolicy {
            security_auto: true,
            security_delay: Some(Duration::from_secs(DAY)),
            ..off.clone()
        };
        assert_eq!(sec.auto_key(true), Some("update.security_auto"));
        assert_eq!(sec.auto_key(false), None);
        assert_eq!(
            auto_target(&sec, &security, current, t0 + DAY - 1, Install::Release),
            None
        );
        assert_eq!(
            auto_target(&sec, &security, current, t0 + DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        assert_eq!(
            auto_target(&sec, &normal, current, late, Install::Release),
            None
        );
        // Without security_delay: update.delay.
        let slow = UpdatePolicy {
            security_delay: None,
            ..sec.clone()
        };
        assert_eq!(
            auto_target(&slow, &security, current, t0 + DAY, Install::Release),
            None
        );
        assert_eq!(
            auto_target(&slow, &security, current, t0 + 7 * DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        // Both on: update.auto covers normal releases, and names the key.
        let both = UpdatePolicy {
            auto: true,
            ..sec.clone()
        };
        assert_eq!(both.auto_key(false), Some("update.auto"));
        assert_eq!(
            auto_target(&both, &normal, current, t0 + 7 * DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        // Not twice, not older.
        let installed = Cache {
            installed: Some("1.1.0".into()),
            ..security.clone()
        };
        assert_eq!(
            auto_target(&sec, &installed, current, late, Install::Release),
            None
        );
        assert_eq!(
            auto_target(&sec, &security, Version(1, 1, 0), late, Install::Release),
            None
        );
    }

    /// A Homebrew install is never upgraded by the node, whatever the
    /// settings: Homebrew does it, and the notice names its command.
    #[test]
    fn homebrew_is_never_auto_upgraded() {
        let current = Version(1, 0, 0);
        let t0 = 1_000 * DAY;
        let late = t0 + 30 * DAY;
        let all = UpdatePolicy {
            check: true,
            auto: true,
            security_auto: true,
            ..Default::default()
        };
        for security in [false, true] {
            let c = cache("1.1.0", security, t0);
            assert_eq!(
                auto_target(&all, &c, current, late, Install::Homebrew),
                None
            );
            assert_eq!(
                auto_target(&all, &c, current, late, Install::Release),
                Some(Version(1, 1, 0))
            );
        }
        let notice =
            security_notice(&cache("1.1.0", true, t0), current, &all, Install::Homebrew).unwrap();
        assert!(notice.contains("`brew upgrade wats`"), "{notice}");
        assert!(!notice.contains("installs it by itself"), "{notice}");
        let status =
            status_json_of(&cache("1.1.0", true, t0), current, &all, Install::Homebrew).unwrap();
        assert_eq!(status["upgrade"], "brew upgrade wats");
    }

    /// A platform with no prebuilt release: never installed by the node,
    /// and the notice points to the source instead of `wats upgrade`.
    #[test]
    fn no_prebuilt_release_is_never_auto_upgraded() {
        let current = Version(1, 0, 0);
        let t0 = 1_000 * DAY;
        let all = UpdatePolicy {
            check: true,
            auto: true,
            security_auto: true,
            ..Default::default()
        };
        let security = cache("1.1.0", true, t0);
        assert_eq!(
            auto_target(&all, &security, current, t0 + 30 * DAY, Install::Source),
            None
        );
        let notice = security_notice(&security, current, &all, Install::Source).unwrap();
        assert!(notice.contains("build wats 1.1.0 from source"), "{notice}");
        assert!(notice.contains(RELEASES_PAGE), "{notice}");
        assert!(!notice.contains("wats upgrade"), "{notice}");
        assert!(!notice.contains("by itself"), "{notice}");
        let status = status_json_of(&security, current, &all, Install::Source).unwrap();
        assert_eq!(status["upgrade"], "build it from source");
        assert!(Install::Source.how().contains(RELEASES_PAGE));
        assert_eq!(Install::Release.how(), "`wats upgrade` installs it");
    }

    /// The louder notice: a security release only, with its version and
    /// the command; in the status JSON (`wats status`, MCP `status`).
    #[test]
    fn security_notice_names_version_and_command() {
        let current = Version(1, 0, 0);
        let t0 = 1_000 * DAY;
        let off = UpdatePolicy {
            check: true,
            ..Default::default()
        };
        let security = cache("1.1.0", true, t0);
        let notice = security_notice(&security, current, &off, Install::Release).unwrap();
        assert!(
            notice.starts_with("SECURITY RELEASE: wats 1.1.0"),
            "{notice}"
        );
        assert!(notice.contains("this node runs 1.0.0"), "{notice}");
        assert!(notice.contains("`wats upgrade`"), "{notice}");
        assert!(!notice.contains("by itself"), "{notice}");
        let status = status_json_of(&security, current, &off, Install::Release).unwrap();
        assert_eq!(status["security"], true);
        assert_eq!(status["notice"], notice.as_str());
        // A normal release, or none newer: no notice.
        let normal = cache("1.1.0", false, t0);
        assert_eq!(
            security_notice(&normal, current, &off, Install::Release),
            None
        );
        let status = status_json_of(&normal, current, &off, Install::Release).unwrap();
        assert_eq!(status["available"], true);
        assert!(status.get("notice").is_none(), "{status}");
        assert_eq!(
            security_notice(&security, Version(1, 1, 0), &off, Install::Release),
            None
        );
        // With security_auto: when the node installs it by itself.
        let sec = UpdatePolicy {
            security_auto: true,
            security_delay: Some(Duration::from_secs(DAY)),
            ..off.clone()
        };
        let notice = security_notice(&security, current, &sec, Install::Release).unwrap();
        assert!(
            notice.ends_with(&format!(
                "Otherwise update.security_auto installs it by itself from {}.",
                format_timestamp(t0 + DAY)
            )),
            "{notice}"
        );
        // Installed by the node: restart.
        let installed = Cache {
            installed: Some("1.1.0".into()),
            ..security
        };
        let notice = security_notice(&installed, current, &sec, Install::Release).unwrap();
        assert!(notice.contains("restart wats to use it"), "{notice}");
    }

    /// The signed security floor (ADR 0030). The fixtures are signed with
    /// a throwaway key (`security-floor/test.pub`, its secret not kept):
    /// release 1.1.0 with floor 1.0.5, the file of release 1.0.5 (floor
    /// 1.0.0) to replay, and the good content signed under another name.
    const FLOOR_FIX: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/security-floor/"
    );

    fn floor_fixture(file: &str) -> Vec<u8> {
        std::fs::read(format!("{FLOOR_FIX}{file}")).unwrap()
    }

    fn floor_key() -> String {
        String::from_utf8(floor_fixture("test.pub"))
            .unwrap()
            .lines()
            .last()
            .unwrap()
            .trim()
            .to_string()
    }

    fn floor_sig(file: &str) -> String {
        String::from_utf8(floor_fixture(file)).unwrap()
    }

    #[test]
    fn verifies_the_signed_floor() {
        let key = floor_key();
        let json = floor_fixture("security.json");
        let sig = floor_sig("security.json.minisig");
        let v110 = Version(1, 1, 0);
        assert_eq!(
            verify_security_floor(&key, &json, &sig, v110).unwrap(),
            Version(1, 0, 5)
        );
        // A bad signature: tampered content, another key.
        let mut tampered = json.clone();
        tampered[30] ^= 1;
        let err = verify_security_floor(&key, &tampered, &sig, v110).unwrap_err();
        assert!(err.to_string().contains("does not verify"), "{err:#}");
        assert!(verify_security_floor(RELEASE_PUBLIC_KEY, &json, &sig, v110).is_err());
        // Signed for another file name.
        let err = verify_security_floor(
            &key,
            &json,
            &floor_sig("security.json.wrong-file.minisig"),
            v110,
        )
        .unwrap_err();
        assert!(err.to_string().contains("another file"), "{err:#}");
        // An older release's file, replayed under the latest one: refused.
        let err = verify_security_floor(
            &key,
            &floor_fixture("replayed.json"),
            &floor_sig("replayed.json.minisig"),
            v110,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("security.json is for 1.0.5, not for the release 1.1.0"),
            "{err:#}"
        );
        // It is fine for its own release.
        assert_eq!(
            verify_security_floor(
                &key,
                &floor_fixture("replayed.json"),
                &floor_sig("replayed.json.minisig"),
                Version(1, 0, 5)
            )
            .unwrap(),
            Version(1, 0, 0)
        );
    }

    fn release_with_floor_assets(tag: &str, with: bool) -> Release {
        let mut r = release(tag, "");
        if with {
            for name in ["security.json", "security.json.minisig"] {
                r.assets.push(Asset {
                    name: name.into(),
                    ..Default::default()
                });
            }
        }
        r
    }

    /// `security_floor`: the floor when the assets verify; none (the
    /// notes decide) when they are missing, badly signed or for another
    /// release. No network: the download is a fake.
    #[tokio::test]
    async fn the_floor_falls_back_to_the_notes() {
        let key = floor_key();
        let files = |json: &'static str, sig: &'static str| {
            move |asset: &Asset| {
                let file = if asset.name == "security.json" {
                    json
                } else {
                    sig
                };
                let bytes = floor_fixture(file);
                async move { Ok::<_, anyhow::Error>(bytes) }
            }
        };
        let good = release_with_floor_assets("v1.1.0", true);
        assert_eq!(
            security_floor(&good, &key, files("security.json", "security.json.minisig")).await,
            Some(Version(1, 0, 5))
        );
        // Missing assets: nothing is downloaded.
        let bare = release_with_floor_assets("v1.1.0", false);
        let never = |_: &Asset| async { panic!("nothing to download") };
        assert_eq!(security_floor(&bare, &key, never).await, None);
        // A bad signature (another key's), a replayed file, a failed
        // download, or another file under its name.
        assert_eq!(
            security_floor(
                &good,
                RELEASE_PUBLIC_KEY,
                files("security.json", "security.json.minisig")
            )
            .await,
            None
        );
        assert_eq!(
            security_floor(&good, &key, files("replayed.json", "replayed.json.minisig")).await,
            None
        );
        let offline = |_: &Asset| async { Err::<Vec<u8>, _>(anyhow!("offline")) };
        assert_eq!(security_floor(&good, &key, offline).await, None);
        assert_eq!(
            security_floor(&good, &key, files("test.pub", "security.json.minisig")).await,
            None
        );
    }

    /// Below the floor, the latest release counts as a security release
    /// even when its notes do not say so: the notice and the
    /// `security_auto` install. At or above the floor it does not. No
    /// floor: the notes' `Security:` line decides.
    #[test]
    fn below_the_floor_is_a_security_release() {
        let t0 = 1_000 * DAY;
        let late = t0 + 30 * DAY;
        let sec = UpdatePolicy {
            check: true,
            security_auto: true,
            security_delay: Some(Duration::from_secs(DAY)),
            ..Default::default()
        };
        // Latest 1.1.0, a normal release by its notes, floor 1.0.5.
        let floored = Cache {
            security_floor: Some("1.0.5".into()),
            ..cache("1.1.0", false, t0)
        };
        let below = Version(1, 0, 4);
        assert!(floored.is_security(below));
        assert!(security_notice(&floored, below, &sec, Install::Release).is_some());
        assert_eq!(
            auto_target(&sec, &floored, below, t0 + DAY, Install::Release),
            Some(Version(1, 1, 0))
        );
        assert_eq!(
            status_json_of(&floored, below, &sec, Install::Release).unwrap()["security"],
            true
        );
        for at_or_above in [Version(1, 0, 5), Version(1, 0, 6)] {
            assert!(!floored.is_security(at_or_above));
            assert_eq!(
                security_notice(&floored, at_or_above, &sec, Install::Release),
                None
            );
            assert_eq!(
                auto_target(&sec, &floored, at_or_above, late, Install::Release),
                None
            );
        }
        // A valid floor wins over the notes: marked security, but this
        // node is at the floor.
        let marked = Cache {
            security: true,
            ..floored.clone()
        };
        assert!(!marked.is_security(Version(1, 0, 5)));
        // No floor (missing or invalid): the notes decide.
        assert!(cache("1.1.0", true, t0).is_security(Version(1, 0, 9)));
        assert!(!cache("1.1.0", false, t0).is_security(Version(1, 0, 0)));
        // A cache written before the floor existed still loads.
        let old: Cache = serde_json::from_str(
            r#"{"checked_at":1,"latest":"1.1.0","security":true,"published_at":1,"notified":null,"installed":null}"#,
        )
        .unwrap();
        assert_eq!(old.security_floor, None);
        assert!(old.is_security(Version(1, 0, 0)));
    }

    /// The daily check records the floor next to the notes' flag.
    #[tokio::test]
    async fn tick_records_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let policy = UpdatePolicy {
            check: true,
            ..Default::default()
        };
        let t0 = 1_800_000_000;
        let c = tick(&policy, dir.path(), t0, || async {
            Ok((release("v1.1.0", ""), Some(Version(1, 0, 5))))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(c.security_floor.as_deref(), Some("1.0.5"));
        assert!(!c.security);
        assert!(c.is_security(Version(1, 0, 0)));
        // The next check without a valid floor clears it.
        let c = tick(&policy, dir.path(), t0 + DAY, || async {
            Ok((release("v1.1.0", "Security: fixes"), None))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(c.security_floor, None);
        assert!(c.is_security(Version(1, 0, 9)));
    }

    #[test]
    fn checks_at_most_once_a_day() {
        let t0 = 1_000 * DAY;
        assert!(check_due(None, t0));
        let c = Cache {
            checked_at: t0,
            ..Default::default()
        };
        assert!(!check_due(Some(&c), t0 + DAY - 1));
        assert!(check_due(Some(&c), t0 + DAY));
        // A clock that went back more than a day.
        assert!(check_due(Some(&c), t0 - 2 * DAY));
        assert!(!check_due(Some(&c), t0 - 60));
    }

    fn release(tag: &str, body: &str) -> Release {
        Release {
            tag_name: tag.into(),
            body: Some(body.into()),
            published_at: Some("2026-10-18T12:34:56Z".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tick_caches_for_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let policy = UpdatePolicy {
            check: true,
            ..Default::default()
        };
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((release("v99.0.0", "Security: fixes"), None))
        };
        let t0 = 1_800_000_000;
        let c = tick(&policy, dir.path(), t0, fetch).await.unwrap().unwrap();
        assert_eq!(c.latest.as_deref(), Some("99.0.0"));
        assert!(c.security);
        assert_eq!(c.published_at, Some(1792326896));
        assert_eq!(c.checked_at, t0);
        assert_eq!(Cache::load(dir.path()), Some(c.clone()));
        // Within the day: from the cache.
        let again = tick(&policy, dir.path(), t0 + 3600, fetch).await.unwrap();
        assert_eq!(again, Some(c));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A day later: checked again.
        tick(&policy, dir.path(), t0 + DAY, fetch).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A failed check counts as a check: no retry before a day.
        let failing = || async { Err::<(Release, Option<Version>), _>(anyhow!("offline")) };
        assert!(
            tick(&policy, dir.path(), t0 + 2 * DAY, failing)
                .await
                .is_err()
        );
        let kept = Cache::load(dir.path()).unwrap();
        assert_eq!(kept.checked_at, t0 + 2 * DAY);
        assert_eq!(kept.latest.as_deref(), Some("99.0.0"));
        tick(&policy, dir.path(), t0 + 2 * DAY + 60, fetch)
            .await
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let status = status_json(dir.path(), &policy).unwrap();
        assert_eq!(status["latest"], "99.0.0");
        assert_eq!(status["available"], true);
        assert_eq!(status["security"], true);
        assert_eq!(status["current"], env!("CARGO_PKG_VERSION"));
        // A prerelease or draft never becomes `latest`.
        let mut pre = release("v100.0.0", "");
        pre.prerelease = true;
        let c = tick(&policy, dir.path(), t0 + 4 * DAY, || async {
            Ok((pre, None))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(c.latest.as_deref(), Some("99.0.0"));
    }

    #[tokio::test]
    async fn hermetic_never_checks() {
        let dir = tempfile::tempdir().unwrap();
        // `wats` turns `check` off for --hermetic and --loopback
        // (main.rs), and a node in loopback mode never runs the loop.
        let off = UpdatePolicy::default();
        assert!(!off.check);
        let out = tick(&off, dir.path(), 1_800_000_000, || async {
            panic!("a disabled check must not fetch")
        })
        .await
        .unwrap();
        assert_eq!(out, None);
        assert!(!dir.path().join(CACHE_FILE).exists());
        assert!(status_json(dir.path(), &off).is_none());
        let on = UpdatePolicy {
            check: true,
            ..Default::default()
        };
        assert!(!may_check(&on, true));
        assert!(!may_check(&off, false));
    }

    #[test]
    fn keys_since_are_real_keys() {
        for (key, _) in KEYS_SINCE {
            crate::settings::find_key(key).unwrap();
        }
        assert!(
            KEYS_SINCE.contains(&("update.security_auto", Version(0, 4, 3))),
            "ADR 0030"
        );
    }

    /// `KEYS_SINCE` is complete: every key that wats 0.3.3 did not know
    /// (the committed list of its keys) is in it, with a version after
    /// 0.3.3, and no key of 0.3.3 is. A new key without an entry fails
    /// here.
    #[test]
    fn keys_since_is_complete() {
        let old: Vec<&str> = include_str!("../tests/fixtures/keys-0.3.3.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(old.len(), 69);
        let now: Vec<&str> = crate::settings::KEYS
            .iter()
            .chain(crate::settings::HIDDEN_KEYS)
            .map(|k| k.name)
            .collect();
        for key in &old {
            assert!(now.contains(key), "{key}: a key of 0.3.3 was removed");
            assert!(
                !KEYS_SINCE.iter().any(|(k, _)| k == key),
                "{key} is a key of 0.3.3"
            );
        }
        for key in now.iter().filter(|k| !old.contains(k)) {
            let since = KEYS_SINCE
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("{key} is new since 0.3.3: add it to KEYS_SINCE"));
            assert!(since > Version(0, 3, 3), "{key}");
        }
    }

    /// `wats upgrade --rollback` to an older binary refuses while
    /// config.toml sets keys that binary refuses, with the exact commands
    /// to run first (ADR 0030); otherwise it goes ahead.
    #[cfg(unix)]
    #[test]
    fn rollback_names_the_keys_an_older_wats_refuses() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("wats.old");
        let fake = |version: &str| {
            std::fs::write(&old, format!("#!/bin/sh\necho 'wats {version}'\n")).unwrap();
            std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let data = dir.path().join("my data");
        std::fs::create_dir_all(&data).unwrap();
        let config = data.join(crate::settings::FILE_NAME);
        std::fs::write(
            &config,
            "[update]\nsecurity_auto = true\nsecurity_delay = \"1d\"\n",
        )
        .unwrap();
        let current = Version(0, 4, 3);
        fake("0.4.2");
        assert_eq!(binary_version(&old), Some(Version(0, 4, 2)));
        assert_eq!(
            keys_unknown_to(&data, Version(0, 4, 2)),
            ["update.security_auto"]
        );
        let err = check_rollback(&old, &data, current)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("wats 0.4.2, which refuses update.security_auto"),
            "{err}"
        );
        assert!(
            err.contains(&format!(
                "  wats --dir '{}' config unset update.security_auto\n",
                data.display()
            )),
            "{err}"
        );
        // 0.4.3 or newer knows it; so does this binary's own version.
        fake("0.4.3");
        check_rollback(&old, &data, Version(0, 4, 4)).unwrap();
        fake("0.4.2");
        check_rollback(&old, &data, Version(0, 4, 2)).unwrap();
        // 0.4.0 also refuses the keys of 0.4.1.
        std::fs::write(&config, "[sync]\ninitial_max = \"0\"\n").unwrap();
        fake("0.4.0");
        let err = check_rollback(&old, &data, current)
            .unwrap_err()
            .to_string();
        assert!(err.contains("config unset sync.initial_max"), "{err}");
        // The budget keys of 0.4.5 (ADR 0032), in an agent's table: named
        // with the table.
        std::fs::write(
            &config,
            "[agents.claude]\nruntime = \"claude\"\nmax_minutes_per_day = 120\n\n\
             [agents.codex]\nruntime = \"codex\"\nmax_tokens_per_day = 5000000\n",
        )
        .unwrap();
        assert_eq!(
            keys_unknown_to(&data, Version(0, 4, 3)),
            [
                "agents.claude.max_minutes_per_day",
                "agents.codex.max_tokens_per_day"
            ]
        );
        fake("0.4.4");
        let err = check_rollback(&old, &data, Version(0, 4, 5))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("config unset agents.claude.max_minutes_per_day"),
            "{err}"
        );
        fake("0.4.0");
        // No such key set, no config.toml, or an old binary that does not
        // say its version: go ahead.
        std::fs::write(&config, "[update]\nauto = true\n").unwrap();
        check_rollback(&old, &data, current).unwrap();
        std::fs::remove_file(&config).unwrap();
        check_rollback(&old, &data, current).unwrap();
        std::fs::write(&old, "#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(binary_version(&old), None);
        check_rollback(&old, &data, current).unwrap();
        assert_eq!(shell_word("/home/me/.wats"), "/home/me/.wats");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }

    #[test]
    fn homebrew_installs_are_left_to_homebrew() {
        assert!(homebrew_managed(Path::new(
            "/opt/homebrew/Cellar/wats/0.3.1/bin/wats"
        )));
        assert!(homebrew_managed(Path::new(
            "/home/linuxbrew/.linuxbrew/Cellar/wats/0.3.1/bin/wats"
        )));
        assert!(!homebrew_managed(Path::new("/Users/me/.local/bin/wats")));
        assert!(!homebrew_managed(Path::new(
            "/opt/homebrew/Cellar/other/1/bin/wats"
        )));
    }

    #[test]
    fn tokens_only_go_to_github() {
        let s = Source::new(
            "https://api.github.com/repos/wearetheswarm/wats/",
            Some("t".into()),
        );
        assert_eq!(s.base(), "https://api.github.com/repos/wearetheswarm/wats");
        assert!(s.token.is_some());
        let mirror = Source::new("https://mirror.example/wats", Some("t".into()));
        assert!(mirror.token.is_none());
        let plain = Source::new("http://api.github.com/repos/x/y", Some("t".into()));
        assert!(plain.token.is_none());
    }

    #[test]
    fn archive_names() {
        assert_eq!(
            archive_name(Version(0, 2, 0), "aarch64-apple-darwin"),
            "wats-0.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            old_path(Path::new("/usr/local/bin/wats")),
            Path::new("/usr/local/bin/wats.old")
        );
    }
}

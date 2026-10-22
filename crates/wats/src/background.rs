//! Running a node in the background (ADR 0023): `wats --detach`, and the
//! node `wats mcp` starts for its sessions.
//!
//! - **The log.** A detached node's stdout goes to `/dev/null` and its
//!   stderr to `<data dir>/logs/node.log`, opened in append mode. The node
//!   itself rotates that file by size: it copies it to `node.log.1` (the
//!   older ones shift to `.2`, `.3`, …) and truncates it, so its stderr
//!   keeps writing to the same file without reopening it (Rust has no safe
//!   `dup2`). [`Follower`] reads it across a rotation, for `wats attach`.
//! - **The registry.** Every node records its data directory in a per-user
//!   directory (`~/.wats/run`), one small file per data directory, so that
//!   `wats ps` and `wats stop --all` find nodes outside `~/.wats`. An entry
//!   says where to look, never whether a node runs: the data directory
//!   lock does (a stale entry is removed by `wats ps`). A second index,
//!   `known/`, keeps every data directory a node ever ran in, stopped ones
//!   too, so that a node name (`-n NAME`) finds a stopped node; an entry
//!   goes once its directory is no data directory any more.
//! - **Signals.** `wats stop` asks the node through its socket first, and
//!   signals it with `kill(1)` only when it does not answer or stop in
//!   time (the crate forbids `unsafe`, so no `libc::kill`).
//! - **The stop record.** A node stopped on purpose (`wats stop`, or
//!   handing over to a daemon) says so in `<data dir>/stopped.json`
//!   before it lets go of its sessions; the next node to start removes it.
//!   An MCP session that loses its node reads it to tell an intended stop
//!   (do not start the node again) from a crash (start it again).

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// The directory of a data directory's log files.
pub const LOG_DIR: &str = "logs";
/// The log of the node running in the background.
pub const LOG_NAME: &str = "node.log";
/// What a detached node printed on start (its banner), for `wats attach`.
pub const BANNER_NAME: &str = "banner.txt";
/// The size at which `node.log` is rotated.
pub const DEFAULT_LOG_MAX: u64 = 10 * 1024 * 1024;
/// Rotated files kept: `node.log.1` to `node.log.3`.
pub const DEFAULT_LOG_KEEP: usize = 3;
/// Names the registry directory instead of `~/.wats/run` (tests).
pub const RUN_DIR_ENV: &str = "WATS_RUN_DIR";

/// `<data dir>/logs/node.log`.
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_DIR).join(LOG_NAME)
}

/// `<data dir>/logs/banner.txt`.
pub fn banner_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOG_DIR).join(BANNER_NAME)
}

/// The `n`-th rotated file of `path`: `node.log.1`, `node.log.2`, …
pub fn rotated_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Create the directory of `path` (owner-only on Unix).
fn create_parent(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Open a log file for appending, creating it (and its directory)
/// owner-only.
pub fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    create_parent(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Write `text` to `path` (the banner), replacing it, owner-only.
pub fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    create_parent(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// Rotate `path`: drop `.keep`, shift `.1`… up by one, copy `path` to
/// `.1` and truncate `path`. Writers that opened `path` in append mode
/// carry on at its new end. With `keep` 0, only truncate.
pub fn rotate(path: &Path, keep: usize) -> std::io::Result<()> {
    if keep > 0 {
        let _ = std::fs::remove_file(rotated_path(path, keep));
        for n in (1..keep).rev() {
            let from = rotated_path(path, n);
            if from.exists() {
                std::fs::rename(&from, rotated_path(path, n + 1))?;
            }
        }
        std::fs::copy(path, rotated_path(path, 1))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(0)
}

/// Rotation of this process's stderr, set by [`rotate_stderr`].
struct Rotation {
    path: PathBuf,
    max: u64,
    keep: usize,
    /// Bytes in the file, as far as this process knows; the lock also
    /// keeps a write from landing between the copy and the truncation.
    size: Mutex<u64>,
}

static ROTATION: OnceLock<Rotation> = OnceLock::new();

/// This process's stderr is `path` (a detached node): rotate it at `max`
/// bytes, keeping `keep` older files. Log lines go through
/// [`write_stderr`].
pub fn rotate_stderr(path: &Path, max: u64, keep: usize) {
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    let _ = ROTATION.set(Rotation {
        path: path.to_path_buf(),
        max: max.max(1),
        keep,
        size: Mutex::new(size),
    });
}

/// Write log bytes to stderr, rotating its file when it is full (see
/// [`rotate_stderr`]).
pub fn write_stderr(buf: &[u8]) -> std::io::Result<usize> {
    let Some(rotation) = ROTATION.get() else {
        return std::io::stderr().write(buf);
    };
    let mut size = rotation
        .size
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let written = std::io::stderr().write(buf)?;
    *size = size.saturating_add(written as u64);
    if *size >= rotation.max {
        // Other writes (a panic message) may have grown it too: the file
        // says how big it is.
        let actual = std::fs::metadata(&rotation.path).map_or(*size, |m| m.len());
        if actual >= rotation.max {
            *size = 0;
            if let Err(e) = rotate(&rotation.path, rotation.keep) {
                let _ = writeln!(
                    std::io::stderr(),
                    "could not rotate {}: {e}",
                    rotation.path.display()
                );
            }
        } else {
            *size = actual;
        }
    }
    Ok(written)
}

/// An identity for the file at `path` that changes when the file is
/// replaced: its inode and change time on Unix (an inode can be reused
/// once the oldest file is deleted).
fn file_id(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let changed = (meta.ctime() as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(meta.ctime_nsec() as u64);
        Some((meta.ino(), changed))
    }
    #[cfg(not(unix))]
    {
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some((modified.as_secs(), u64::from(modified.subsec_nanos())))
    }
}

/// The bytes of `path` from `offset` to its end.
fn read_from(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Follows a log file written by [`write_stderr`], across rotations.
#[derive(Debug)]
pub struct Follower {
    path: PathBuf,
    offset: u64,
    /// The identity of `.1`: a new one means a rotation happened.
    rotated: Option<(u64, u64)>,
}

impl Follower {
    /// From the current end of `path`.
    pub fn at_end(path: &Path) -> Self {
        Follower {
            path: path.to_path_buf(),
            offset: std::fs::metadata(path).map_or(0, |m| m.len()),
            rotated: file_id(&rotated_path(path, 1)),
        }
    }

    /// What was written since the last call. After a rotation, the end of
    /// the old file is in `.1` at the same offsets: that first, then the
    /// new file from its start.
    pub fn read_new(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let rotated = file_id(&rotated_path(&self.path, 1));
        let len = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if rotated != self.rotated || len < self.offset {
            if rotated != self.rotated {
                out = read_from(&rotated_path(&self.path, 1), self.offset)?;
            }
            self.rotated = rotated;
            self.offset = 0;
        }
        let new = read_from(&self.path, self.offset)?;
        self.offset = self.offset.saturating_add(new.len() as u64);
        out.extend(new);
        Ok(out)
    }
}

/// The last `n` lines of `path` (at most its last 256 KiB).
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    const WINDOW: u64 = 256 * 1024;
    let len = std::fs::metadata(path).map_or(0, |m| m.len());
    let start = len.saturating_sub(WINDOW);
    let Ok(bytes) = read_from(path, start) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    // A partial first line when the window cut it.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// The registry of the data directories nodes run in: [`RUN_DIR_ENV`],
/// else `~/.wats/run`. A hermetic process (tests) uses one only when
/// [`RUN_DIR_ENV`] names it: it never writes to the real home.
pub fn registry_dir(hermetic: bool) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(RUN_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if hermetic {
        return None;
    }
    let home = std::env::home_dir()?;
    #[cfg(test)]
    crate::naming::assert_test_home(&home);
    Some(home.join(".wats").join("run"))
}

/// The registry entry of `data_dir`.
fn entry_path(registry: &Path, data_dir: &Path) -> PathBuf {
    let absolute = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let hash = blake3::hash(absolute.as_os_str().as_encoded_bytes()).to_hex();
    registry.join(format!("{}.json", &hash[..16]))
}

/// Record that a node runs in `data_dir` (best effort), and that the
/// machine knows it ([`known`]).
pub fn register(registry: &Path, data_dir: &Path) -> std::io::Result<()> {
    let absolute = std::path::absolute(data_dir)?;
    let entry = serde_json::json!({ "data_dir": absolute }).to_string();
    let path = entry_path(registry, &absolute);
    create_parent(&path)?;
    std::fs::write(path, &entry)?;
    // Best effort: `wats ps` and `stop` need the entry above only.
    let known = entry_path(&registry.join(KNOWN_DIR), &absolute);
    if let Err(e) = create_parent(&known).and_then(|()| std::fs::write(&known, &entry)) {
        tracing::debug!(
            "could not record {} in {}: {e}",
            absolute.display(),
            known.display()
        );
    }
    Ok(())
}

/// Remove the entry of `data_dir`.
pub fn unregister(registry: &Path, data_dir: &Path) {
    let _ = std::fs::remove_file(entry_path(registry, data_dir));
}

/// The data directories in the registry.
pub fn registered(registry: &Path) -> Vec<PathBuf> {
    entries(registry).into_iter().map(|(_, dir)| dir).collect()
}

/// The registry's sub-directory of the data directories nodes ever ran
/// in, running or not.
pub const KNOWN_DIR: &str = "known";

/// How long an entry of [`KNOWN_DIR`] whose data directory is gone stays,
/// since its node last started: a volume that is not mounted for a while
/// keeps its nodes known.
pub const KNOWN_KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);

/// The data directories nodes ever ran in on this machine (since 0.4.5),
/// stopped ones too, that are still data directories (a `secret.key`,
/// `config.toml` or `node-name`). [`register`] adds one; it is only read
/// here, never removed ([`prune_known`] does, at a node's start or `wats
/// stop`).
pub fn known(registry: &Path) -> Vec<PathBuf> {
    entries(&registry.join(KNOWN_DIR))
        .into_iter()
        .map(|(_, dir)| dir)
        .filter(|dir| {
            [
                "secret.key",
                crate::settings::FILE_NAME,
                crate::naming::FILE,
            ]
            .iter()
            .any(|f| dir.join(f).exists())
        })
        .collect()
}

/// Remove the entries of [`KNOWN_DIR`] whose data directory is certainly
/// gone (not found: any other error keeps it) and whose node last started
/// more than `keep` ago ([`KNOWN_KEEP`]). Run at a node's start and by
/// `wats stop`, never by a command that only reads.
pub fn prune_known(registry: &Path, keep: std::time::Duration) {
    for (path, dir) in entries(&registry.join(KNOWN_DIR)) {
        let gone = matches!(
            std::fs::metadata(&dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        );
        let old = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= keep);
        if gone && old {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The entries of a registry directory, and the data directory each
/// names, sorted by data directory.
fn entries(registry: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(registry) else {
        return Vec::new();
    };
    let mut dirs: Vec<(PathBuf, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| Some((std::fs::read_to_string(&p).ok()?, p)))
        .filter_map(|(text, p)| Some((serde_json::from_str::<serde_json::Value>(&text).ok()?, p)))
        .filter_map(|(v, p)| v["data_dir"].as_str().map(|d| (p, PathBuf::from(d))))
        .collect();
    dirs.sort_by(|a, b| a.1.cmp(&b.1));
    dirs.dedup_by(|a, b| a.1 == b.1);
    dirs
}

/// The stop record of a data directory (see the module documentation).
pub const STOPPED_NAME: &str = "stopped.json";

/// Why a node stopped on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// `wats stop`, or a `stop` request on its socket: MCP sessions do not
    /// start it again, and say so.
    Stop,
    /// A daemon takes over (ADR 0021 §4): MCP sessions wait for it.
    HandOver,
}

/// What `<data dir>/stopped.json` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopRecord {
    pub reason: StopReason,
    /// Who asked, for people: "`wats stop`", "`wats stop --all`", …
    pub by: String,
    /// When (Unix seconds).
    pub at: u64,
    /// The node that stopped.
    pub pid: u32,
    /// Its profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// `<data dir>/stopped.json`.
pub fn stopped_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STOPPED_NAME)
}

/// Record why the node of `data_dir` stops (through a temporary file and
/// a rename: a session never reads half of it).
pub fn write_stop_record(data_dir: &Path, record: &StopRecord) -> std::io::Result<()> {
    let path = stopped_path(data_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, &path)
}

/// The stop record of `data_dir`, if any (an unreadable one counts as
/// none: the node did not stop on purpose as far as this release knows).
pub fn read_stop_record(data_dir: &Path) -> Option<StopRecord> {
    let text = std::fs::read_to_string(stopped_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remove the stop record (a node starts).
pub fn clear_stop_record(data_dir: &Path) {
    let _ = std::fs::remove_file(stopped_path(data_dir));
}

/// Record that the node of `data_dir` (this process) stops for `reason`,
/// asked by `by`; kept when a stop record of the same reason is there
/// already (`wats stop` writes its own before asking).
pub fn record_stop(data_dir: &Path, reason: StopReason, by: &str, profile: &str) {
    if read_stop_record(data_dir).is_some_and(|r| r.reason == reason) {
        return;
    }
    let record = StopRecord {
        reason,
        by: by.to_string(),
        at: crate::node::now_secs(),
        pid: std::process::id(),
        profile: Some(profile.to_string()),
    };
    if let Err(e) = write_stop_record(data_dir, &record) {
        tracing::warn!("could not record the stop in {}: {e}", data_dir.display());
    }
}

/// Start `wats` with `args` as a detached process: its own process group
/// (no Ctrl-C or terminal hangup from the shell that started it), stdin
/// and stdout to `/dev/null`, stderr appended to `log`. Not waited for.
#[cfg(unix)]
pub fn spawn_detached(
    args: &[std::ffi::OsString],
    log: &Path,
    env_remove: &[&str],
) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let stderr = open_log(log)?;
    let mut command = std::process::Command::new(crate::update::spawn_exe()?);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .process_group(0);
    for name in env_remove {
        command.env_remove(name);
    }
    command.spawn()
}

/// Wait for a process this one started, in a thread of its own: it
/// outlives what started it, and leaves no zombie behind when it exits
/// first.
#[cfg(unix)]
pub fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Send `signal` (`TERM`, `KILL`, or `0` to probe) to `pid` with
/// `kill(1)`. `Ok(false)`: no such process (or not ours).
#[cfg(unix)]
pub fn signal(pid: u32, signal: &str) -> std::io::Result<bool> {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

/// SIGKILL to every process of the process group `pgid` (killpg(2)): an
/// agent run and what it started (MCP servers, shell commands), ADR 0031
/// §3. A group already gone is not an error; any other failure is
/// logged.
#[cfg(unix)]
pub fn kill_group(pgid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let Ok(raw) = i32::try_from(pgid) else {
        tracing::warn!("agent run: process group {pgid} out of range, not killed");
        return;
    };
    // pgid 0 or 1 would be ours or init's: never.
    if raw <= 1 {
        tracing::warn!("agent run: refusing to kill process group {raw}");
        return;
    }
    match killpg(Pid::from_raw(raw), Signal::SIGKILL) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(e) => tracing::warn!("agent run: cannot kill its process group {raw}: {e}"),
    }
}

/// Whether process `pid` exists.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    signal(pid, "0").unwrap_or(false)
}

/// `3s`, `4m`, `2h 5m`, `3d 4h`.
pub fn format_uptime(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_n_files_and_the_follower_reads_across_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join(LOG_NAME);
        let mut file = open_log(&path).unwrap();
        writeln!(file, "one").unwrap();
        let mut follower = Follower::at_end(&path);
        writeln!(file, "two").unwrap();
        assert_eq!(follower.read_new().unwrap(), b"two\n");
        // Written, then rotated before the follower looked: "three" is in
        // `.1`, "four" in the new file.
        writeln!(file, "three").unwrap();
        rotate(&path, 2).unwrap();
        writeln!(file, "four").unwrap();
        assert_eq!(follower.read_new().unwrap(), b"three\nfour\n");
        assert_eq!(
            std::fs::read(rotated_path(&path, 1)).unwrap(),
            b"one\ntwo\nthree\n"
        );
        // The writer kept its file: appends land at the new end.
        assert_eq!(std::fs::read(&path).unwrap(), b"four\n");
        // A rotation the follower missed, with the new file already
        // longer than its offset: still detected.
        writeln!(file, "five").unwrap();
        rotate(&path, 2).unwrap();
        writeln!(file, "six six six six six six").unwrap();
        assert_eq!(
            follower.read_new().unwrap(),
            b"five\nsix six six six six six\n"
        );
        rotate(&path, 2).unwrap();
        rotate(&path, 2).unwrap();
        // Two rotated files at most.
        assert!(rotated_path(&path, 2).exists());
        assert!(!rotated_path(&path, 3).exists());
        assert_eq!(follower.read_new().unwrap(), b"");
        assert_eq!(tail(&path, 5), Vec::<String>::new());
        writeln!(file, "a\nb\nc").unwrap();
        assert_eq!(tail(&path, 2), ["b", "c"]);
    }

    #[test]
    fn keep_zero_only_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_NAME);
        std::fs::write(&path, "x\n").unwrap();
        rotate(&path, 0).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert!(!rotated_path(&path, 1).exists());
    }

    #[test]
    fn the_registry_lists_absolute_data_dirs_once() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("run");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        register(&registry, &a).unwrap();
        register(&registry, &b).unwrap();
        register(&registry, &a).unwrap();
        assert_eq!(registered(&registry), [a.clone(), b.clone()]);
        unregister(&registry, &a);
        assert_eq!(registered(&registry), std::slice::from_ref(&b));
        // Known: those that are data directories; listing removes nothing.
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("secret.key"), "x").unwrap();
        assert_eq!(
            known(&registry),
            std::slice::from_ref(&a),
            "b holds no data"
        );
        assert_eq!(entries(&registry.join(KNOWN_DIR)).len(), 2);
        // Pruned: only a data directory certainly gone, and long enough.
        prune_known(&registry, KNOWN_KEEP);
        assert_eq!(
            entries(&registry.join(KNOWN_DIR)).len(),
            2,
            "b started recently"
        );
        std::fs::create_dir_all(&b).unwrap();
        prune_known(&registry, std::time::Duration::ZERO);
        assert_eq!(entries(&registry.join(KNOWN_DIR)).len(), 2, "b exists");
        std::fs::remove_dir(&b).unwrap();
        prune_known(&registry, std::time::Duration::ZERO);
        let left: Vec<PathBuf> = entries(&registry.join(KNOWN_DIR))
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(left, std::slice::from_ref(&a));
        // `known/` cannot be written: `register` still records the node.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join(KNOWN_DIR), "a file, not a directory").unwrap();
        register(&blocked, &a).unwrap();
        assert_eq!(registered(&blocked), [a]);
    }

    #[test]
    fn a_stop_record_is_kept_until_a_node_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_stop_record(dir.path()), None);
        let by_cli = StopRecord {
            reason: StopReason::Stop,
            by: "`wats stop`".into(),
            at: 1_700_000_000,
            pid: 42,
            profile: Some("alice".into()),
        };
        write_stop_record(dir.path(), &by_cli).unwrap();
        // The node, asked next, keeps the record of the command that asked.
        record_stop(dir.path(), StopReason::Stop, "a stop request", "alice");
        assert_eq!(read_stop_record(dir.path()), Some(by_cli));
        // A hand-over replaces it (a new node ran in between, in practice).
        record_stop(dir.path(), StopReason::HandOver, "a daemon", "alice");
        let record = read_stop_record(dir.path()).unwrap();
        assert_eq!(record.reason, StopReason::HandOver);
        assert_eq!(record.pid, std::process::id());
        let text = std::fs::read_to_string(stopped_path(dir.path())).unwrap();
        assert!(text.contains("\"hand_over\""), "{text}");
        clear_stop_record(dir.path());
        assert_eq!(read_stop_record(dir.path()), None);
        // Unreadable: no stop as far as this release knows.
        std::fs::write(stopped_path(dir.path()), "{\"reason\": \"later\"}").unwrap();
        assert_eq!(read_stop_record(dir.path()), None);
    }

    #[test]
    fn uptime_is_short() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(125), "2m");
        assert_eq!(format_uptime(3 * 3600 + 120), "3h 2m");
        assert_eq!(format_uptime(2 * 86_400 + 3600), "2d 1h");
    }
}

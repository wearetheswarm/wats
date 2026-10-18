//! Crash reports (ADR 0013): when a node panics or stops on an error, it
//! writes a redacted report to `<data dir>/crash/`. The next start says so;
//! `wats report show` prints exactly the bytes that would be sent, and
//! `wats report send` seals them to the maintainers' inbox when its key is
//! known (`crate::inbox`, ADR 0015 §5), and sends them to the collector (our
//! router by default) otherwise. Sending is manual unless
//! `report.crashes = true`.
//!
//! What never leaves the machine: keys and tokens, environment values,
//! configuration values, IP addresses, other nodes' and authors' ids (hashed
//! with a per-report salt), post titles and texts, MCP arguments, agent
//! transcripts and prompts, and paths beyond `~`-relative.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Reports kept on disk.
const KEEP: usize = 20;
/// Log lines kept in memory for the next report.
const RING: usize = 200;
/// Longest line kept from the log.
const LINE: usize = 400;
/// Largest report a collector accepts.
pub const MAX_REPORT_BYTES: usize = 64 * 1024;

/// What the panic hook knows about the node.
#[derive(Debug, Clone, Default)]
pub struct CrashContext {
    pub data_dir: PathBuf,
    pub infra: String,
    pub mode: String,
    pub own_id: Option<String>,
}

use std::sync::atomic::{AtomicU64, Ordering};

static STORAGE: AtomicU64 = AtomicU64::new(0);
static PEERS: AtomicU64 = AtomicU64::new(0);
static EVENTS: AtomicU64 = AtomicU64::new(0);

/// Counts for the next report (updated by the node every 30 s).
pub fn note_counts(storage_bytes: u64, peers: u64, events: u64) {
    STORAGE.store(storage_bytes, Ordering::Relaxed);
    PEERS.store(peers, Ordering::Relaxed);
    EVENTS.store(events, Ordering::Relaxed);
}

struct State {
    context: CrashContext,
    started: Instant,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static LOG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Set or update what reports say about the node.
pub fn set_context(context: CrashContext) {
    let state = STATE.get_or_init(|| {
        Mutex::new(State {
            context: CrashContext::default(),
            started: Instant::now(),
        })
    });
    if let Ok(mut state) = state.lock() {
        state.context = context;
    }
}

/// Update the node's own id once known.
pub fn set_own_id(id: String) {
    if let Some(state) = STATE.get()
        && let Ok(mut state) = state.lock()
    {
        state.context.own_id = Some(id);
    }
}

/// Write a report on panic, then let the default hook print it.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()));
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let _ = write("panic", &message, location.as_deref(), &backtrace);
        previous(info);
    }));
}

/// Write a report for an error that stopped the node.
pub fn record_error(error: &anyhow::Error) -> Option<PathBuf> {
    let backtrace = error.backtrace().to_string();
    write("error", &format!("{error:#}"), None, &backtrace).ok()
}

/// Remove terminal escapes.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Keep a log line for the next report (warnings, errors, and the node's
/// own lifecycle lines).
fn keep_log_line(line: &str) {
    let line = strip_ansi(line);
    let interesting = line.contains(" WARN ")
        || line.contains(" ERROR ")
        || (line.contains(" INFO ") && (line.contains(" wats: ") || line.contains(" wats::node:")));
    if !interesting {
        return;
    }
    if let Ok(mut log) = LOG.lock() {
        if log.len() == RING {
            log.pop_front();
        }
        log.push_back(line.trim_end().to_string());
    }
}

/// A `tracing` writer: stderr, plus the in-memory log tail.
#[derive(Clone, Copy, Default)]
pub struct Tee;

pub struct TeeLine(Vec<u8>);

impl Write for TeeLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        std::io::stderr().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl Drop for TeeLine {
    fn drop(&mut self) {
        for line in String::from_utf8_lossy(&self.0).lines() {
            keep_log_line(line);
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Tee {
    type Writer = TeeLine;
    fn make_writer(&'a self) -> Self::Writer {
        TeeLine(Vec::new())
    }
}

/// Redaction of one report: a fresh salt for the ids it hashes.
pub struct Redactor {
    salt: [u8; 16],
    home: Option<String>,
    own_id: Option<String>,
}

impl Redactor {
    pub fn new(own_id: Option<String>) -> Self {
        let mut salt = [0u8; 16];
        let _ = getrandom::fill(&mut salt);
        Redactor {
            salt,
            home: std::env::home_dir()
                .map(|h| h.display().to_string())
                .filter(|h| h.len() > 1),
            own_id,
        }
    }

    fn hash(&self, id: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.salt);
        hasher.update(id.as_bytes());
        format!("#{}", &hasher.finalize().to_hex()[..8])
    }

    /// One line, redacted.
    pub fn line(&self, line: &str) -> String {
        let mut line = strip_ansi(line);
        // Post titles end these lines.
        if let Some(i) = line.find("held for review: ") {
            line.truncate(i + "held for review: ".len());
            line.push_str("<title>");
        }
        if let Some(home) = &self.home {
            line = line.replace(home.as_str(), "~");
        }
        let mut out = String::with_capacity(line.len());
        let mut token = String::new();
        let is_token = |c: char| c.is_ascii_alphanumeric() || "._:/-[]%~+@".contains(c);
        for c in line.chars() {
            if is_token(c) {
                token.push(c);
            } else {
                out.push_str(&self.token(&token));
                token.clear();
                out.push(c);
            }
        }
        out.push_str(&self.token(&token));
        if out.chars().count() > LINE {
            out = out.chars().take(LINE).collect::<String>() + "…";
        }
        out
    }

    fn token(&self, token: &str) -> String {
        if token.is_empty() {
            return String::new();
        }
        if token.starts_with('/') {
            return path_tail(token);
        }
        let t = replace_runs(
            token,
            |c| c.is_ascii_hexdigit() || c == ':',
            |run| {
                let groups: Vec<&str> = run.split(':').filter(|g| !g.is_empty()).collect();
                let ipv6 = (run.matches(':').count() >= 3 || run.contains("::"))
                    && groups.len() >= 2
                    && groups.iter().all(|g| g.len() <= 4)
                    && run.chars().any(|c| c.is_ascii_digit());
                ipv6.then(|| "<ip>".to_string())
            },
        );
        let t = replace_runs(
            &t,
            |c| c.is_ascii_digit() || c == '.',
            |run| {
                let groups: Vec<&str> = run.split('.').collect();
                (groups.len() == 4
                    && groups
                        .iter()
                        .all(|g| (1..=3).contains(&g.len()) && g.parse::<u8>().is_ok()))
                .then(|| "<ip>".to_string())
            },
        );
        replace_runs(
            &t,
            |c| c.is_ascii_digit() || ('a'..='f').contains(&c),
            |run| {
                let own = self
                    .own_id
                    .as_deref()
                    .is_some_and(|own| !run.is_empty() && own.starts_with(run));
                (run.len() >= 10 && run.chars().any(|c| c.is_ascii_alphabetic()) && !own)
                    .then(|| self.hash(run))
            },
        )
    }
}

/// `/a/b/c/d.rs:12:5` → `…/c/d.rs:12:5`.
fn path_tail(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return format!("/{}", parts.join("/"));
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

/// Replace the maximal runs of `in_run` characters for which `replace`
/// returns a value.
fn replace_runs(
    s: &str,
    in_run: impl Fn(char) -> bool,
    replace: impl Fn(&str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        match replace(run) {
            Some(r) => out.push_str(&r),
            None => out.push_str(run),
        }
        run.clear();
    };
    for c in s.chars() {
        if in_run(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

pub fn crash_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("crash")
}

fn state_path(data_dir: &Path, id: &str) -> PathBuf {
    crash_dir(data_dir).join(format!("{id}.state"))
}

pub fn report_path(data_dir: &Path, id: &str) -> PathBuf {
    crash_dir(data_dir).join(format!("{id}.json"))
}

/// Build and write a report. Returns its path.
fn write(kind: &str, message: &str, location: Option<&str>, backtrace: &str) -> Result<PathBuf> {
    let state = STATE.get().context("no crash context")?;
    let (context, uptime) = {
        let state = state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        (state.context.clone(), state.started.elapsed().as_secs())
    };
    let redactor = Redactor::new(context.own_id.clone());
    let log: Vec<String> = LOG
        .lock()
        .map(|l| l.iter().map(|line| redactor.line(line)).collect())
        .unwrap_or_default();
    let created = crate::node::now_secs();
    let id = format!("{created}-{kind}");
    let report = json!({
        "format": 1,
        "id": id,
        "kind": kind,
        "created": created,
        "version": env!("CARGO_PKG_VERSION"),
        "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        "infra": context.infra,
        "mode": context.mode,
        "uptime_secs": uptime,
        "counts": {
            "storage_bytes": STORAGE.load(Ordering::Relaxed),
            "peers": PEERS.load(Ordering::Relaxed),
            "events": EVENTS.load(Ordering::Relaxed),
        },
        "message": redactor.line(message),
        "location": location.map(|l| redactor.line(l)),
        "backtrace": backtrace.lines().take(120).map(|l| redactor.line(l)).collect::<Vec<_>>(),
        "log_tail": log,
    });
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    // Keep under the collector's limit: the log tail goes first.
    if bytes.len() > MAX_REPORT_BYTES {
        let mut report = report;
        report["log_tail"] = json!([]);
        bytes = serde_json::to_vec_pretty(&report)?;
        bytes.truncate(MAX_REPORT_BYTES);
    }
    let dir = crash_dir(&context.data_dir);
    std::fs::create_dir_all(&dir)?;
    let path = report_path(&context.data_dir, &id);
    std::fs::write(&path, &bytes)?;
    std::fs::write(state_path(&context.data_dir, &id), "new")?;
    prune(&context.data_dir);
    Ok(path)
}

/// A report on disk.
#[derive(Debug, Clone)]
pub struct Stored {
    pub id: String,
    /// `new`, `send` (queued) or `sent`.
    pub state: String,
    pub created: u64,
    pub kind: String,
    pub message: String,
}

/// Reports, oldest first.
pub fn list(data_dir: &Path) -> Vec<Stored> {
    let mut ids: Vec<String> = std::fs::read_dir(crash_dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids.into_iter()
        .filter_map(|id| {
            let report: Value =
                serde_json::from_slice(&std::fs::read(report_path(data_dir, &id)).ok()?).ok()?;
            Some(Stored {
                state: std::fs::read_to_string(state_path(data_dir, &id))
                    .unwrap_or_else(|_| "new".into())
                    .trim()
                    .to_string(),
                created: report["created"].as_u64().unwrap_or(0),
                kind: report["kind"].as_str().unwrap_or("?").to_string(),
                message: report["message"].as_str().unwrap_or("").to_string(),
                id,
            })
        })
        .collect()
}

/// The exact bytes a report sends.
pub fn bytes(data_dir: &Path, id: &str) -> Result<Vec<u8>> {
    std::fs::read(report_path(data_dir, id)).with_context(|| format!("no crash report {id:?}"))
}

pub fn set_state(data_dir: &Path, id: &str, state: &str) -> Result<()> {
    bytes(data_dir, id)?;
    std::fs::write(state_path(data_dir, id), state)?;
    Ok(())
}

pub fn discard(data_dir: &Path, id: &str) -> Result<()> {
    bytes(data_dir, id)?;
    std::fs::remove_file(report_path(data_dir, id))?;
    let _ = std::fs::remove_file(state_path(data_dir, id));
    Ok(())
}

fn prune(data_dir: &Path) {
    let all = list(data_dir);
    for old in all.iter().take(all.len().saturating_sub(KEEP)) {
        let _ = discard(data_dir, &old.id);
    }
}

/// Collector side: store a report from `sender`, with a size cap, at most
/// `per_day` reports per sender and day, and `keep_days` of retention.
pub fn accept(
    data_dir: &Path,
    sender: &str,
    report: &Value,
    per_day: usize,
    keep_days: u64,
) -> Result<PathBuf, String> {
    let bytes = serde_json::to_vec(report).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_REPORT_BYTES {
        return Err(format!("report larger than {MAX_REPORT_BYTES} bytes"));
    }
    let dir = data_dir.join("reports").join(sender);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let now = crate::node::now_secs();
    let today = now - now % 86_400;
    let mut today_count = 0;
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let ts = entry
            .file_name()
            .to_str()
            .and_then(|n| n.split('-').next())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        if ts + keep_days * 86_400 < now {
            let _ = std::fs::remove_file(entry.path());
        } else if ts >= today {
            today_count += 1;
        }
    }
    if today_count >= per_day {
        return Err(format!("at most {per_day} reports a day"));
    }
    let mut random = [0u8; 4];
    let _ = getrandom::fill(&mut random);
    let path = dir.join(format!("{now}-{}.json", hex::encode(random)));
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        let own = "9132c3e3e9a3313114adecc9a45fba05721ec2125fcd3f51494a376acdd603d8";
        let r = Redactor::new(Some(own.into()));
        let line = "\u{1b}[2m2026-10-15T22:09:00.989070Z\u{1b}[0m DEBUG iroh: remote=7aa66a6e4b network_path=Ip(192.168.1.39->[2001:41d0:304:400::54b9]:7842) from 2a02:8428:4343:1e01:6df4:fa16:2cf8:382e";
        let out = r.line(line);
        assert!(!out.contains("192.168"), "{out}");
        assert!(!out.contains("2001:41d0"), "{out}");
        assert!(!out.contains("2a02:8428"), "{out}");
        assert!(!out.contains("7aa66a6e4b"), "{out}");
        assert!(out.contains("22:09:00.989070Z"), "times stay: {out}");
        assert!(out.contains("<ip>") && out.contains('#'), "{out}");
        let mine = r.line(&format!("node started id={own}"));
        assert!(mine.contains(own), "own id stays: {mine}");
        let held = r.line("agent post held for review: My secret plan");
        assert!(held.ends_with("held for review: <title>"), "{held}");
        let path = r.line("at /opt/build/src/registry/crates/wats/src/node.rs:120:5");
        assert!(
            path.contains("…/src/node.rs:120:5") && !path.contains("/opt/build"),
            "{path}"
        );
        assert!(
            r.line("uptime 1792104044 s").contains("1792104044"),
            "numbers stay"
        );
        for rust in [
            "std::backtrace_rs::backtrace::libunwind::trace",
            "INFO wats::node: node started",
            "blake3::hash and core::ops::function::Fn",
        ] {
            assert_eq!(r.line(rust), rust, "Rust paths are not addresses");
        }
        assert!(r.line("peer fe80::1c2b:3a4d").contains("<ip>"));
        // A new report, a new salt: the same id hashes differently.
        let other = Redactor::new(None);
        assert_ne!(other.line("id 7aa66a6e4b0c"), r.line("id 7aa66a6e4b0c"));
    }

    #[test]
    fn collector_limits() {
        let dir = tempfile::tempdir().unwrap();
        let report = json!({"format": 1, "message": "boom"});
        for _ in 0..3 {
            accept(dir.path(), "abc", &report, 3, 90).unwrap();
        }
        assert!(
            accept(dir.path(), "abc", &report, 3, 90).is_err(),
            "rate limit"
        );
        assert!(
            accept(dir.path(), "def", &report, 3, 90).is_ok(),
            "per sender"
        );
        let big = json!({"x": "y".repeat(MAX_REPORT_BYTES)});
        assert!(accept(dir.path(), "ghi", &big, 3, 90).is_err(), "size cap");
    }
}

//! wats's own entries in the agent tools' configs: the `wats` MCP server
//! of Claude Code, Codex, OpenClaw and Mistral Vibe (and OpenClaw's
//! `wats-agent`), and the `wats notify --hook` session hooks of Claude Code
//! and Codex.
//!
//! wats 0.4.3 and older wrote the running binary there with its symlinks
//! resolved: for a Homebrew install, a versioned Cellar directory
//! (`/opt/homebrew/Cellar/wats/0.4.3/bin/wats`), which `brew cleanup`
//! removes after an upgrade. Every session of that tool then loses the
//! swarm, silently. Setup now writes a stable path
//! ([`crate::update::stable_exe`]).
//!
//! [`check`] finds the entries whose wats path is stale ([`stale`]): gone,
//! or in a versioned Cellar directory. When Homebrew's link replaces it
//! ([`crate::update::homebrew_relink`]), it rewrites that path, and nothing
//! else in the file, after a backup copy; otherwise it reports the `wats
//! setup` command that fixes the entry (ADR 0032).

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::update::{homebrew_managed, homebrew_relink};

/// Set to 1, the check runs whatever the mode (tests, with a throwaway
/// HOME). Otherwise only a node on the real network checks.
pub const CHECK_ENV: &str = "WATS_CHECK_REGISTRATIONS";

/// How long `openclaw mcp show` may take before it is killed.
const OPENCLAW_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `openclaw mcp set` may take: longer, so that a cold Node.js
/// start is not killed while it writes openclaw.json.
const OPENCLAW_SET_TIMEOUT: Duration = Duration::from_secs(20);

/// Backups kept per file by repairs.
const MAX_BACKUPS: usize = 3;

/// Whether this process checks the registrations: only a node on the real
/// network does, never in hermetic or loopback mode (`local`: tests and
/// local demos) nor under `cargo test` or `cargo run` of wats itself
/// (`CARGO_PKG_NAME=wats` in the environment), unless [`CHECK_ENV`] is 1.
pub fn enabled(local: bool) -> bool {
    if std::env::var(CHECK_ENV).is_ok_and(|v| v.trim() == "1") {
        return true;
    }
    !local && std::env::var_os("CARGO_PKG_NAME").is_none_or(|name| name != "wats")
}

/// An agent tool wats registers itself in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Codex,
    OpenClaw,
    Vibe,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenClaw => "OpenClaw",
            Self::Vibe => "Mistral Vibe",
        }
    }

    /// Its name for `wats setup`.
    fn setup(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenClaw => "openclaw",
            Self::Vibe => "vibe",
        }
    }
}

/// Where the tools keep the entries [`check`] reads.
#[derive(Debug, Clone, Default)]
pub struct Locations {
    /// Claude Code's user config, with its user-scope MCP servers:
    /// `$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`.
    pub claude_json: Option<PathBuf>,
    /// Claude Code's user settings, with the hooks.
    pub claude_settings: Option<PathBuf>,
    /// Codex's config.toml: its MCP servers and hooks.
    pub codex_config: Option<PathBuf>,
    /// Vibe's config.toml.
    pub vibe_config: Option<PathBuf>,
    /// OpenClaw's openclaw.json: `$OPENCLAW_CONFIG_PATH`, else
    /// `$OPENCLAW_STATE_DIR/openclaw.json`, else `~/.openclaw/openclaw.json`.
    /// Only read: wats never writes it itself (ADR 0017).
    pub openclaw_config: Option<PathBuf>,
    /// OpenClaw's CLI, which repairs its entries (`openclaw mcp set`):
    /// `$WATS_OPENCLAW`, else `openclaw`, when found on PATH.
    pub openclaw_cli: Option<PathBuf>,
}

impl Locations {
    /// This user's, from the environment.
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = std::env::home_dir();
        Self {
            claude_json: var("CLAUDE_CONFIG_DIR")
                .or_else(|| home.clone())
                .map(|d| d.join(".claude.json")),
            claude_settings: crate::hook::claude_settings_path(
                "user",
                std::env::var_os("CLAUDE_CONFIG_DIR"),
                home.clone(),
                Path::new("/"),
            ),
            codex_config: crate::hook::codex_config_path(
                std::env::var_os("CODEX_HOME"),
                home.clone(),
            ),
            vibe_config: crate::vibe::home(std::env::var_os("VIBE_HOME"), home.clone())
                .map(|d| d.join("config.toml")),
            openclaw_config: var("OPENCLAW_CONFIG_PATH")
                .or_else(|| var("OPENCLAW_STATE_DIR").map(|d| d.join("openclaw.json")))
                .or_else(|| home.map(|h| h.join(".openclaw").join("openclaw.json"))),
            openclaw_cli: on_path(&var("WATS_OPENCLAW").unwrap_or_else(|| "openclaw".into())),
        }
    }
}

/// `command` as a file: itself when it is a path, else its first PATH
/// entry. Nothing is run.
fn on_path(command: &Path) -> Option<PathBuf> {
    if command.components().count() > 1 {
        return command.is_file().then(|| command.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(command))
        .find(|p| p.is_file())
}

/// Whether `command`, the wats an entry runs, is stale: an absolute path
/// to a `wats` that no longer exists, or that is in a versioned Homebrew
/// Cellar directory (gone at the next upgrade and `brew cleanup`).
pub fn stale(command: &Path) -> bool {
    command.is_absolute()
        && command.file_name().is_some_and(|n| n == "wats")
        && (!command.exists() || homebrew_managed(command))
}

/// What became of a stale entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its wats path, and only it, now `to`; the file was copied to
    /// `backup` first.
    Repaired {
        to: PathBuf,
        backup: Option<PathBuf>,
    },
    /// Left as it is: `fix` is the command that fixes it; `error`, why the
    /// repair failed, if it was tried; `auto`, the node repairs it at its
    /// next start (only checked, as by `wats status`).
    Left {
        fix: String,
        error: Option<String>,
        auto: bool,
    },
}

/// A wats entry with a stale path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub tool: Tool,
    pub file: PathBuf,
    /// "the wats MCP server", "the wats session hooks"…
    pub entry: String,
    /// The stale wats path it runs (ran).
    pub path: String,
    pub outcome: Outcome,
}

impl Finding {
    pub fn repaired(&self) -> bool {
        matches!(self.outcome, Outcome::Repaired { .. })
    }

    /// Why the path is stale.
    fn why(&self) -> &'static str {
        if Path::new(&self.path).exists() {
            "a versioned Homebrew path, removed by `brew cleanup` after an upgrade"
        } else {
            "which no longer exists"
        }
    }

    /// One line for the log or the terminal.
    pub fn line(&self) -> String {
        let (tool, entry, file, path) = (
            self.tool.name(),
            &self.entry,
            self.file.display(),
            &self.path,
        );
        match &self.outcome {
            Outcome::Repaired { to, backup } => {
                let backup = backup
                    .as_ref()
                    .map(|b| format!(", backup {}", b.display()))
                    .unwrap_or_default();
                let review = if self.tool == Tool::Codex && entry.contains("hook") {
                    "; open /hooks in Codex once to review it"
                } else {
                    ""
                };
                format!(
                    "{tool}: fixed {entry} in {file}: it ran {path}, {}; now {}{backup}{review}",
                    self.why(),
                    to.display()
                )
            }
            Outcome::Left { fix, error, auto } => {
                let error = error
                    .as_ref()
                    .map(|e| format!(" (the repair failed: {e})"))
                    .unwrap_or_default();
                let what = if *auto {
                    format!("the node repairs it at its next start, or run `{fix}`")
                } else {
                    format!("run `{fix}` to fix it")
                };
                format!(
                    "{tool}: {entry} in {file} runs {path}, {}{error}: {what}",
                    self.why()
                )
            }
        }
    }

    /// For `wats status --json` and the MCP `status` tool.
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "tool": self.tool.setup(),
            "file": self.file.display().to_string(),
            "entry": self.entry,
            "path": self.path,
            "notice": self.line(),
        });
        match &self.outcome {
            Outcome::Repaired { to, backup } => {
                value["repaired_to"] = json!(to.display().to_string());
                if let Some(backup) = backup {
                    value["backup"] = json!(backup.display().to_string());
                }
            }
            Outcome::Left { fix, auto, .. } => {
                value["fix"] = json!(fix);
                value["repaired_at_next_start"] = json!(auto);
            }
        }
        value
    }
}

/// The findings of this process's last [`check`] that are left to the
/// user, for the MCP `status` tool.
static LEFT: Mutex<Vec<Finding>> = Mutex::new(Vec::new());

/// Keep the findings left to the user ([`remembered`]).
pub fn remember(findings: &[Finding]) {
    let left = findings.iter().filter(|f| !f.repaired()).cloned().collect();
    *LEFT.lock().unwrap_or_else(|e| e.into_inner()) = left;
}

/// What [`remember`] kept, but for the entries fixed since (their file no
/// longer has the stale path: `wats setup`).
pub fn remembered() -> Vec<Finding> {
    let mut left = LEFT.lock().unwrap_or_else(|e| e.into_inner());
    left.retain(|f| read(&f.file).is_some_and(|text| text.contains(&f.path)));
    left.clone()
}

/// Check wats's entries in every tool's config at `at`, and repair the
/// stale ones when `repair` ([`Outcome`]). `data_dir` is this node's (for
/// OpenClaw's runner token); `now`, the time of the backups' names.
pub fn check(at: &Locations, data_dir: &Path, repair: bool, now: u64) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(file) = &at.claude_json {
        out.extend(claude_json(file, repair, now));
    }
    if let Some(file) = &at.claude_settings {
        out.extend(claude_settings(file, repair, now));
    }
    if let Some(file) = &at.codex_config {
        out.extend(codex(file, repair, now));
    }
    if let Some(file) = &at.vibe_config {
        out.extend(vibe(file, repair, now));
    }
    if let Some(file) = &at.openclaw_config {
        out.extend(openclaw(
            file,
            at.openclaw_cli.as_deref(),
            data_dir,
            repair,
            now,
        ));
    }
    out
}

// ---------------------------------------------------------------------
// The entries.

/// One wats entry's stale command string.
struct Site {
    entry: String,
    /// The command that fixes it.
    fix: String,
    /// The stale wats path.
    exe: String,
    /// The whole string value (the path, or a hook's command line).
    value: String,
    /// Homebrew's link, and the value with it.
    relink: Option<(PathBuf, String)>,
}

/// `wats [--dir <dir>] setup <tool> <flag>`.
fn fix_command(tool: Tool, flag: &str, dir: Option<&str>) -> String {
    match dir {
        Some(dir) => format!(
            "wats --dir {} setup {} {flag}",
            crate::hook::shell_quote(dir),
            tool.setup()
        ),
        None => format!("wats setup {} {flag}", tool.setup()),
    }
}

/// The value of `--dir` in `args`.
fn dir_arg<'a>(mut args: impl Iterator<Item = &'a str>) -> Option<String> {
    args.by_ref().find(|a| *a == "--dir")?;
    args.next().map(str::to_string)
}

/// The site of an MCP server entry running `command`, if stale.
fn mcp_site(tool: Tool, entry: &str, flag: &str, command: &str, args: &[&str]) -> Option<Site> {
    if !stale(Path::new(command)) {
        return None;
    }
    let dir = dir_arg(args.iter().copied());
    Some(Site {
        entry: entry.to_string(),
        fix: fix_command(tool, flag, dir.as_deref()),
        exe: command.to_string(),
        value: command.to_string(),
        relink: homebrew_relink(Path::new(command))
            .map(|link| (link.clone(), link.to_string_lossy().into_owned())),
    })
}

/// The site of a session hook running the shell line `command`, if its
/// wats is stale.
fn hook_site(tool: Tool, command: &str) -> Option<Site> {
    let (exe, rest) = first_word(command)?;
    if !stale(Path::new(&exe)) {
        return None;
    }
    let mut words = Vec::new();
    let mut tail = rest;
    while let Some((word, more)) = first_word(tail.trim_start()) {
        words.push(word);
        tail = more;
    }
    let dir = dir_arg(words.iter().map(String::as_str));
    Some(Site {
        entry: "the wats session hooks".into(),
        fix: fix_command(tool, "--mcp", dir.as_deref()),
        relink: homebrew_relink(Path::new(&exe)).map(|link| {
            let line = format!(
                "{}{rest}",
                crate::hook::shell_quote(&link.to_string_lossy())
            );
            (link, line)
        }),
        exe,
        value: command.to_string(),
    })
}

/// The first POSIX shell word of `line` (plain, or in single quotes as
/// [`crate::hook::shell_quote`] writes it), and what follows it. None for
/// anything else.
fn first_word(line: &str) -> Option<(String, &str)> {
    if line.is_empty() {
        return None;
    }
    if !line.starts_with('\'') {
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        let word = line.get(..end)?;
        if word.contains(['\\', '"', '\'', '$', '`']) {
            return None;
        }
        return Some((word.to_string(), line.get(end..)?));
    }
    let mut word = String::new();
    let mut rest = line;
    loop {
        let body = rest.strip_prefix('\'')?;
        let close = body.find('\'')?;
        word.push_str(body.get(..close)?);
        rest = body.get(close + 1..)?;
        match rest.strip_prefix("\\'") {
            Some(more) if more.starts_with('\'') => {
                word.push('\'');
                rest = more;
            }
            _ => break,
        }
    }
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some((word, rest))
}

/// Repair `sites` of `file`, read as `text` (when `repair` and Homebrew's
/// link replaces each), with `rewrite` making the new text from a text and
/// the map of old values to new ones ([`commit`]); one finding per entry.
fn settle(
    tool: Tool,
    file: &Path,
    text: &str,
    sites: Vec<Site>,
    repair: bool,
    now: u64,
    rewrite: impl Fn(&str, &HashMap<String, String>) -> Result<String>,
) -> Vec<Finding> {
    if sites.is_empty() {
        return Vec::new();
    }
    let all = sites.iter().all(|s| s.relink.is_some());
    let mut result = None;
    if repair && all {
        let map: HashMap<String, String> = sites
            .iter()
            .filter_map(|s| Some((s.value.clone(), s.relink.as_ref()?.1.clone())))
            .collect();
        result = Some(
            commit(file, text, &|base| rewrite(base, &map), now).map_err(|e| format!("{e:#}")),
        );
    }
    let mut out: Vec<Finding> = Vec::new();
    for site in sites {
        if out.iter().any(|f| f.entry == site.entry) {
            continue;
        }
        let outcome = match (&result, site.relink) {
            (Some(Ok(backup)), Some((to, _))) => Outcome::Repaired {
                to,
                backup: backup.clone(),
            },
            (result, _) => Outcome::Left {
                fix: site.fix,
                error: result.as_ref().and_then(|r| r.as_ref().err().cloned()),
                auto: all && result.is_none(),
            },
        };
        out.push(Finding {
            tool,
            file: file.to_path_buf(),
            entry: site.entry,
            path: site.exe,
            outcome,
        });
    }
    out
}

/// A config file's text; None when missing or unreadable.
fn read(file: &Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

/// Replace `file`, read as `text`, with `edit(text)`, atomically: a backup
/// ([`backup`]), then a temporary file next to it (same mode and owner,
/// synced) renamed over it. A symlinked file is resolved first, so that the
/// rename replaces its target, not the link. When the file changed since it
/// was read (Claude Code rewrites `~/.claude.json` often), the edit is
/// redone once on the fresh text; a second change gives up. Returns the
/// backup.
fn commit(
    file: &Path,
    text: &str,
    edit: &dyn Fn(&str) -> Result<String>,
    now: u64,
) -> Result<Option<PathBuf>> {
    let target = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let mut base = text.to_string();
    for _ in 0..2 {
        let new = edit(&base)?;
        if new == base {
            // Nothing left to change: fixed meanwhile.
            return Ok(None);
        }
        let backup = backup(&target, &base, now)?;
        let temp = write_temp(&target, &new)?;
        let current = std::fs::read_to_string(&target);
        match current {
            Ok(current) if current == base => {
                let renamed = std::fs::rename(&temp, &target);
                if renamed.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                renamed.with_context(|| format!("replace {}", target.display()))?;
                return Ok(backup);
            }
            Ok(current) => {
                let _ = std::fs::remove_file(&temp);
                base = current;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(e).with_context(|| format!("read {}", target.display()));
            }
        }
    }
    bail!("{} kept changing while being repaired", file.display())
}

/// `text` in a new temporary file next to `target`, with its mode (and
/// owner, when allowed), synced to disk.
fn write_temp(target: &Path, text: &str) -> Result<PathBuf> {
    use std::io::Write;
    let name = target
        .file_name()
        .context("no file name")?
        .to_string_lossy()
        .into_owned();
    let temp = target.with_file_name(format!(".{name}.wats-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| -> std::io::Result<()> {
        let mut out = options.open(&temp)?;
        if let Ok(meta) = std::fs::metadata(target) {
            out.set_permissions(meta.permissions())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let _ = std::os::unix::fs::fchown(&out, Some(meta.uid()), Some(meta.gid()));
            }
        }
        out.write_all(text.as_bytes())?;
        out.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("write {}", temp.display()));
    }
    Ok(temp)
}

/// A copy of `text`, the content of `target` before a repair, as
/// `<target>.wats-repair.bak-<UTC timestamp>` with its mode: none when the
/// newest such copy already holds it, and only the newest
/// [`MAX_BACKUPS`] are kept. Backups by `wats setup` (`<target>.bak-…`) are
/// never touched.
fn backup(target: &Path, text: &str, now: u64) -> Result<Option<PathBuf>> {
    let name = target
        .file_name()
        .context("no file name")?
        .to_string_lossy()
        .into_owned();
    let stem = target.with_file_name(format!("{name}.wats-repair"));
    let prefix = format!("{name}.wats-repair.bak-");
    let dir = target.parent().context("no directory")?;
    let mut existing: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    existing.sort();
    if let Some(newest) = existing.last()
        && std::fs::read_to_string(newest).is_ok_and(|t| t == text)
    {
        return Ok(Some(newest.clone()));
    }
    let path = crate::openclaw::backup_path(&stem, now);
    let temp = write_temp(target, text)?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("back up {} to {}", target.display(), path.display()))?;
    existing.push(path.clone());
    let extra = existing.len().saturating_sub(MAX_BACKUPS);
    for old in existing.drain(..extra) {
        let _ = std::fs::remove_file(old);
    }
    Ok(Some(path))
}

// ---------------------------------------------------------------------
// Claude Code: JSON, rewritten in place (only the string's bytes change).

/// `~/.claude.json`: `mcpServers.wats`.
fn claude_json(file: &Path, repair: bool, now: u64) -> Vec<Finding> {
    let Some(text) = read(file) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let server = &config["mcpServers"]["wats"];
    let Some(command) = server["command"].as_str() else {
        return Vec::new();
    };
    let args: Vec<&str> = server["args"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let sites = mcp_site(Tool::Claude, "the wats MCP server", "--mcp", command, &args)
        .into_iter()
        .collect();
    let at = |path: &[Seg]| {
        matches!(path, [Seg::Key(a), Seg::Key(b), Seg::Key(c)]
            if a == "mcpServers" && b == "wats" && c == "command")
    };
    settle(
        Tool::Claude,
        file,
        &text,
        sites,
        repair,
        now,
        |text, map| replace_json_strings(text, &at, map),
    )
}

/// Claude Code's settings.json: our `hooks.<event>[].hooks[].command`.
fn claude_settings(file: &Path, repair: bool, now: u64) -> Vec<Finding> {
    let Some(text) = read(file) else {
        return Vec::new();
    };
    if serde_json::from_str::<Value>(&text).is_err() {
        return Vec::new();
    }
    let at = |path: &[Seg]| {
        matches!(path, [Seg::Key(h), Seg::Key(_), Seg::Index(_), Seg::Key(h2), Seg::Index(_), Seg::Key(c)]
            if h == "hooks" && h2 == "hooks" && c == "command")
    };
    let Some(strings) = json_strings(&text, &at) else {
        return Vec::new();
    };
    let sites = strings
        .iter()
        .filter(|(_, command)| crate::hook::is_ours(command))
        .filter_map(|(_, command)| hook_site(Tool::Claude, command))
        .collect();
    settle(
        Tool::Claude,
        file,
        &text,
        sites,
        repair,
        now,
        |text, map| replace_json_strings(text, &at, map),
    )
}

/// A step of a path into a JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

/// `text` with each string value at a path `at` accepts that is a key of
/// `map` replaced by its value: every other byte stays.
fn replace_json_strings(
    text: &str,
    at: &dyn Fn(&[Seg]) -> bool,
    map: &HashMap<String, String>,
) -> Result<String> {
    let strings = json_strings(text, at).context("not valid JSON")?;
    let mut out = text.to_string();
    for (range, value) in strings.into_iter().rev() {
        if let Some(new) = map.get(&value) {
            out.replace_range(range, &serde_json::to_string(new)?);
        }
    }
    Ok(out)
}

/// The string values of the JSON `text` at a path `at` accepts, with
/// their byte ranges (quotes included), in order. None for invalid JSON.
fn json_strings(text: &str, at: &dyn Fn(&[Seg]) -> bool) -> Option<Vec<(Range<usize>, String)>> {
    // serde_json checks the syntax (and its nesting limit) first.
    serde_json::from_str::<serde::de::IgnoredAny>(text).ok()?;
    let mut scan = Scan {
        text,
        pos: 0,
        path: Vec::new(),
        out: Vec::new(),
        at,
    };
    scan.value()?;
    Some(scan.out)
}

struct Scan<'a> {
    text: &'a str,
    pos: usize,
    path: Vec<Seg>,
    out: Vec<(Range<usize>, String)>,
    at: &'a dyn Fn(&[Seg]) -> bool,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, b: u8) -> Option<()> {
        self.ws();
        (self.peek() == Some(b)).then(|| self.pos += 1)
    }

    fn string(&mut self) -> Option<(Range<usize>, String)> {
        let start = self.pos;
        self.pos += 1;
        loop {
            match self.peek()? {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    break;
                }
                _ => self.pos += 1,
            }
        }
        let value = serde_json::from_str(self.text.get(start..self.pos)?).ok()?;
        Some((start..self.pos, value))
    }

    fn value(&mut self) -> Option<()> {
        self.ws();
        match self.peek()? {
            b'{' => {
                self.pos += 1;
                self.ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return Some(());
                }
                loop {
                    self.ws();
                    let (_, key) = self.string()?;
                    self.eat(b':')?;
                    self.path.push(Seg::Key(key));
                    self.value()?;
                    self.path.pop();
                    self.ws();
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b'}' => {
                            self.pos += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                self.ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Some(());
                }
                for i in 0.. {
                    self.path.push(Seg::Index(i));
                    self.value()?;
                    self.path.pop();
                    self.ws();
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
                None
            }
            b'"' => {
                let (range, value) = self.string()?;
                if (self.at)(&self.path) {
                    self.out.push((range, value));
                }
                Some(())
            }
            _ => {
                while self.peek().is_some_and(|b| !b",]} \t\r\n".contains(&b)) {
                    self.pos += 1;
                }
                Some(())
            }
        }
    }
}

// ---------------------------------------------------------------------
// Codex and Vibe: TOML, edited in place (toml_edit keeps the rest).

/// A string value's new text, its decor kept.
fn set_str(value: &mut toml_edit::Value, new: &str) {
    let decor = value.decor().clone();
    *value = new.into();
    *value.decor_mut() = decor;
}

/// Codex's config.toml: `[mcp_servers.wats]` and our hooks.
fn codex(file: &Path, repair: bool, now: u64) -> Vec<Finding> {
    let Some(text) = read(file) else {
        return Vec::new();
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let mut sites = Vec::new();
    codex_visit(&mut doc, &mut |hook, value| {
        let Some(command) = value.as_str() else {
            return;
        };
        let site = match hook {
            Some(()) => hook_site(Tool::Codex, command),
            None => mcp_site(Tool::Codex, "the wats MCP server", "--mcp", command, &[]),
        };
        sites.extend(site);
    });
    // The MCP server's --dir, for the fix.
    let args: Vec<String> = doc
        .get("mcp_servers")
        .and_then(|s| s.get("wats"))
        .and_then(|w| w.get("args"))
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if let Some(dir) = dir_arg(args.iter().map(String::as_str)) {
        for site in sites.iter_mut().filter(|s| s.entry.contains("MCP")) {
            site.fix = fix_command(Tool::Codex, "--mcp", Some(&dir));
        }
    }
    settle(Tool::Codex, file, &text, sites, repair, now, |text, map| {
        let mut doc: toml_edit::DocumentMut = text.parse().context("not valid TOML")?;
        codex_visit(&mut doc, &mut |_, value| {
            if let Some(new) = value.as_str().and_then(|v| map.get(v)).cloned() {
                set_str(value, &new);
            }
        });
        Ok(doc.to_string())
    })
}

/// Call `f` with `mcp_servers.wats.command` (`None`) and the command of
/// each of our hooks (`Some(())`) in a Codex config.
fn codex_visit(
    doc: &mut toml_edit::DocumentMut,
    f: &mut dyn FnMut(Option<()>, &mut toml_edit::Value),
) {
    if let Some(command) = doc
        .get_mut("mcp_servers")
        .and_then(|s| s.get_mut("wats"))
        .and_then(|w| w.get_mut("command"))
        .and_then(|c| c.as_value_mut())
    {
        f(None, command);
    }
    let Some(hooks) = doc.get_mut("hooks").and_then(|h| h.as_table_like_mut()) else {
        return;
    };
    let mut ours = |command: &mut toml_edit::Value| {
        if command.as_str().is_some_and(crate::hook::is_ours) {
            f(Some(()), command);
        }
    };
    for (_, list) in hooks.iter_mut() {
        let mut groups: Vec<&mut dyn toml_edit::TableLike> = Vec::new();
        if list.is_array_of_tables() {
            if let Some(tables) = list.as_array_of_tables_mut() {
                groups.extend(
                    tables
                        .iter_mut()
                        .map(|t| t as &mut dyn toml_edit::TableLike),
                );
            }
        } else if let Some(array) = list.as_array_mut() {
            groups.extend(
                array
                    .iter_mut()
                    .filter_map(|v| v.as_inline_table_mut())
                    .map(|t| t as &mut dyn toml_edit::TableLike),
            );
        }
        for group in groups {
            let Some(items) = group.get_mut("hooks") else {
                continue;
            };
            if let Some(tables) = items.as_array_of_tables_mut() {
                for table in tables.iter_mut() {
                    if let Some(command) = table.get_mut("command").and_then(|c| c.as_value_mut()) {
                        ours(command);
                    }
                }
            } else if let Some(array) = items.as_array_mut() {
                for table in array.iter_mut().filter_map(|v| v.as_inline_table_mut()) {
                    if let Some(command) = table.get_mut("command") {
                        ours(command);
                    }
                }
            }
        }
    }
}

/// Vibe's config.toml: the `[[mcp_servers]]` whose `name = "wats"`.
fn vibe(file: &Path, repair: bool, now: u64) -> Vec<Finding> {
    let Some(text) = read(file) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let ours = |table: &toml_edit::Table| {
        table.get("name").and_then(|n| n.as_str()) == Some(crate::vibe::SERVER)
    };
    let mut sites = Vec::new();
    if let Some(servers) = doc.get("mcp_servers").and_then(|s| s.as_array_of_tables()) {
        for table in servers.iter().filter(|t| ours(t)) {
            let args: Vec<&str> = table
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            if let Some(command) = table.get("command").and_then(|c| c.as_str()) {
                sites.extend(mcp_site(
                    Tool::Vibe,
                    "the wats MCP server",
                    "--mcp",
                    command,
                    &args,
                ));
            }
        }
    }
    settle(Tool::Vibe, file, &text, sites, repair, now, |text, map| {
        let mut doc: toml_edit::DocumentMut = text.parse().context("not valid TOML")?;
        let servers = doc
            .get_mut("mcp_servers")
            .and_then(|s| s.as_array_of_tables_mut())
            .context("no [[mcp_servers]]")?;
        for table in servers.iter_mut().filter(|t| ours(t)) {
            if let Some(command) = table.get_mut("command").and_then(|c| c.as_value_mut())
                && let Some(new) = command.as_str().and_then(|v| map.get(v)).cloned()
            {
                set_str(command, &new);
            }
        }
        Ok(doc.to_string())
    })
}

// ---------------------------------------------------------------------
// OpenClaw: openclaw.json is only read; `openclaw mcp set` repairs.

/// The quoted strings of a JSON5 `text` (`"…"` or `'…'`), as written.
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' && c != '\'' {
            continue;
        }
        let mut s = String::new();
        while let Some(d) = chars.next() {
            match d {
                '\\' => {
                    if let Some(e) = chars.next() {
                        s.push(e);
                    }
                }
                d if d == c => break,
                d => s.push(d),
            }
        }
        out.push(s);
    }
    out
}

/// The JSON object `openclaw mcp show <name>` printed: the whole output,
/// else from its first `{` to its last `}`.
fn shown_entry(output: &str) -> Option<serde_json::Map<String, Value>> {
    let parse = |s: &str| serde_json::from_str::<Value>(s).ok();
    let value = parse(output.trim())
        .or_else(|| parse(output.get(output.find('{')?..=output.rfind('}')?)?))?;
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Whether a value of an entry `openclaw mcp show` printed looks redacted
/// (`***`, `<redacted>`…): setting such an entry again would write the
/// mask instead of the secret.
fn redacted(value: &Value) -> bool {
    match value {
        Value::String(s) => {
            let lower = s.to_ascii_lowercase();
            s.contains("***") || s.contains('•') || lower.contains("redacted")
        }
        Value::Array(items) => items.iter().any(redacted),
        Value::Object(map) => map.values().any(redacted),
        _ => false,
    }
}

/// Run `cli args`, stdin closed, stderr dropped; killed after `limit`
/// (None then, or when it cannot start), so that a hung OpenClaw never
/// holds up the node or `wats status`. Its stdout is awaited within the
/// same deadline: a descendant left holding it does not hang us either.
fn run_for(cli: &Path, args: &[&str], limit: Duration) -> Option<std::process::Output> {
    use std::io::Read;
    let mut child = std::process::Command::new(cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sent, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = sent.send(buf);
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let left = deadline.saturating_duration_since(Instant::now());
    let stdout = read.recv_timeout(left).ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// OpenClaw's `wats` and `wats-agent` entries, when openclaw.json has a
/// stale wats path at all. Only checked (`repair` false): from the file
/// alone, no CLI is run. Repaired: through `openclaw mcp show` and `mcp
/// set` ([`OPENCLAW_TIMEOUT`], [`OPENCLAW_SET_TIMEOUT`]), when the entry
/// shows no redacted value.
fn openclaw(
    file: &Path,
    cli: Option<&Path>,
    data_dir: &Path,
    repair: bool,
    now: u64,
) -> Vec<Finding> {
    use crate::openclaw as oc;
    let Some(text) = read(file) else {
        return Vec::new();
    };
    let Some(any) = quoted_strings(&text)
        .into_iter()
        .find(|s| stale(Path::new(s)))
    else {
        return Vec::new();
    };
    let finding = |entry: &str, path: String, outcome: Outcome| Finding {
        tool: Tool::OpenClaw,
        file: file.to_path_buf(),
        entry: entry.to_string(),
        path,
        outcome,
    };
    let cli = match cli {
        Some(cli) if repair => cli,
        cli => {
            let auto = cli.is_some() && homebrew_relink(Path::new(&any)).is_some();
            return vec![finding(
                "a wats MCP server",
                any,
                Outcome::Left {
                    fix: fix_command(Tool::OpenClaw, "--mcp", None),
                    error: repair.then(|| "no openclaw CLI on PATH".to_string()),
                    auto: auto && !repair,
                },
            )];
        }
    };
    let run = |args: &[&str]| run_for(cli, args, OPENCLAW_TIMEOUT);
    let run_set = |args: &[&str]| run_for(cli, args, OPENCLAW_SET_TIMEOUT);
    let mut out = Vec::new();
    let mut copied: Option<Option<PathBuf>> = None;
    for (name, flag) in [("wats", "--mcp"), (oc::AGENT_SERVER, "--agent")] {
        let Some(shown) = run(&["mcp", "show", name]).filter(|o| o.status.success()) else {
            continue;
        };
        let Some(mut entry) = shown_entry(&String::from_utf8_lossy(&shown.stdout)) else {
            continue;
        };
        let Some(command) = entry
            .get("command")
            .and_then(Value::as_str)
            .map(String::from)
        else {
            continue;
        };
        if !stale(Path::new(&command)) {
            continue;
        }
        let args: Vec<String> = entry
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let dir = dir_arg(args.iter().map(String::as_str));
        let label = format!("the {name} MCP server");
        let fix = fix_command(Tool::OpenClaw, flag, dir.as_deref());
        // Set again only what `mcp show` shows in full: no masked value,
        // and for the agent's entry, the runner token wats keeps.
        let masked = entry.values().any(redacted);
        let token_ok = name != oc::AGENT_SERVER || {
            let own = dir
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.to_path_buf());
            let shown_token = entry
                .get("env")
                .and_then(|e| e.get("WATS_RUNNER_TOKEN"))
                .and_then(Value::as_str);
            oc::read_token(&oc::runner_token_path(&own))
                .is_ok_and(|t| Some(t.as_str()) == shown_token)
        };
        let Some(link) = homebrew_relink(Path::new(&command)) else {
            out.push(finding(
                &label,
                command,
                Outcome::Left {
                    fix,
                    error: None,
                    auto: false,
                },
            ));
            continue;
        };
        if masked || !token_ok {
            let error = if masked {
                "`openclaw mcp show` masks a value of it"
            } else {
                "`openclaw mcp show` does not show its runner token"
            };
            out.push(finding(
                &label,
                command,
                Outcome::Left {
                    fix,
                    error: Some(error.into()),
                    auto: false,
                },
            ));
            continue;
        }
        // A copy of openclaw.json first, once.
        let target = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let backup = copied
            .get_or_insert_with(|| backup(&target, &text, now).ok().flatten())
            .clone();
        entry.insert("command".into(), json!(link.to_string_lossy()));
        let set = run_set(&["mcp", "set", name, &Value::Object(entry).to_string()]);
        let outcome = match set {
            Some(o) if o.status.success() => Outcome::Repaired { to: link, backup },
            Some(o) => Outcome::Left {
                fix,
                error: Some(format!("`openclaw mcp set {name}` failed ({})", o.status)),
                auto: false,
            },
            None => Outcome::Left {
                fix,
                error: Some(format!(
                    "{} did not answer within {}s",
                    cli.display(),
                    OPENCLAW_SET_TIMEOUT.as_secs()
                )),
                auto: false,
            },
        };
        out.push(finding(&label, command, outcome));
    }
    out
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A throwaway HOME with a fake Homebrew prefix in it: 0.4.4 in the
    /// Cellar and linked (`bin/wats`), 0.4.3 gone (`brew cleanup`).
    struct Fixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        /// The 0.4.3 path the tools still run.
        old: String,
        /// `<prefix>/bin/wats`.
        link: String,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            let prefix = tmp.path().join("homebrew");
            let bin = prefix.join("Cellar/wats/0.4.4/bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(bin.join("wats"), "#!/bin/sh\n").unwrap();
            std::fs::create_dir_all(prefix.join("bin")).unwrap();
            std::os::unix::fs::symlink("../Cellar/wats/0.4.4/bin/wats", prefix.join("bin/wats"))
                .unwrap();
            Self {
                old: prefix
                    .join("Cellar/wats/0.4.3/bin/wats")
                    .display()
                    .to_string(),
                link: prefix.join("bin/wats").display().to_string(),
                home,
                _tmp: tmp,
            }
        }

        fn at(&self) -> Locations {
            Locations {
                claude_json: Some(self.home.join(".claude.json")),
                claude_settings: Some(self.home.join(".claude/settings.json")),
                codex_config: Some(self.home.join(".codex/config.toml")),
                vibe_config: Some(self.home.join(".vibe/config.toml")),
                openclaw_config: Some(self.home.join(".openclaw/openclaw.json")),
                openclaw_cli: None,
            }
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.home.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }

        fn backups(&self, rel: &str) -> Vec<PathBuf> {
            let path = self.home.join(rel);
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(&format!("{name}.wats-repair.bak-"))
                })
                .collect()
        }
    }

    #[test]
    fn stale_paths_are_gone_or_in_a_versioned_cellar() {
        let f = Fixture::new();
        assert!(stale(Path::new(&f.old)));
        // In the Cellar, still there: stale too (the next cleanup).
        assert!(stale(&PathBuf::from(&f.link).canonicalize().unwrap()));
        assert!(!stale(Path::new(&f.link)));
        assert!(!stale(Path::new("/nonexistent/bin/other-tool")));
        assert!(!stale(Path::new("wats")));
        assert!(stale(Path::new("/nonexistent/bin/wats")));
    }

    #[test]
    fn claude_code_entries_are_repaired_in_place() {
        let f = Fixture::new();
        let old = &f.old;
        // Another server running the same path is not ours: kept.
        let config = format!(
            "{{\n  \"numStartups\": 12,\n  \"mcpServers\": {{\n    \"other\": {{\n      \
             \"command\": \"{old}\",\n      \"args\": []\n    }},\n    \"wats\": {{\n      \
             \"type\": \"stdio\",\n      \"command\": \"{old}\",\n      \"args\": [\"mcp\", \
             \"--profile\", \"alice\"],\n      \"env\": {{}}\n    }}\n  }},\n  \"tipsHistory\": \
             {{\"a\": 1.50}}\n}}\n"
        );
        let file = f.write(".claude.json", &config);
        let hook = |exe: &str, event: &str| {
            format!("{exe} notify --hook --host claude --event {event} --profile alice")
        };
        let settings = format!(
            "{{\n  \"hooks\": {{\n    \"UserPromptSubmit\": [\n      {{\n        \"hooks\": [\n          \
             {{ \"type\": \"command\", \"command\": \"{}\", \"timeout\": 10 }},\n          \
             {{ \"type\": \"command\", \"command\": \"{old} lint\" }}\n        ]\n      }}\n    ],\n    \
             \"SessionStart\": [{{\"hooks\": [{{\"type\": \"command\", \"command\": \"{}\"}}]}}]\n  \
             }},\n  \"model\": \"opus\"\n}}\n",
            hook(old, "user-prompt-submit"),
            hook(old, "session-start")
        );
        let settings_file = f.write(".claude/settings.json", &settings);

        // Detection alone changes nothing.
        let found = check(&f.at(), &f.home, false, 0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|x| !x.repaired()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), config);
        let Outcome::Left { fix, .. } = &found[0].outcome else {
            panic!()
        };
        assert_eq!(fix, "wats setup claude --mcp");
        assert!(found[0].line().contains("run `wats setup claude --mcp`"));

        let found = check(&f.at(), &f.home, true, 0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(Finding::repaired), "{found:?}");
        assert!(
            found[0]
                .line()
                .starts_with("Claude Code: fixed the wats MCP server in ")
        );
        let wanted = config.replacen(
            &format!("\"type\": \"stdio\",\n      \"command\": \"{old}\""),
            &format!("\"type\": \"stdio\",\n      \"command\": \"{}\"", f.link),
            1,
        );
        assert_ne!(wanted, config);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), wanted);
        let wanted = settings
            .replace(
                &hook(old, "user-prompt-submit"),
                &hook(&f.link, "user-prompt-submit"),
            )
            .replace(&hook(old, "session-start"), &hook(&f.link, "session-start"));
        assert_eq!(std::fs::read_to_string(&settings_file).unwrap(), wanted);
        assert!(wanted.contains(&format!("{old} lint")));
        assert_eq!(f.backups(".claude.json").len(), 1);
        assert_eq!(f.backups(".claude/settings.json").len(), 1);
        // Nothing left.
        assert!(check(&f.at(), &f.home, true, 0).is_empty());
    }

    #[test]
    fn codex_entries_are_repaired_in_place() {
        let f = Fixture::new();
        let old = &f.old;
        let config = format!(
            "# mine\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"{old}\"\n\n\
             [mcp_servers.wats]\ncommand = \"{old}\" # wats\nargs = [\"--dir\", \"/d\", \"mcp\", \
             \"--profile\", \"alice\"]\ndefault_tools_approval_mode = \"approve\"\n\n\
             [features]\nhooks = true\n\n[[hooks.SessionStart]]\nhooks = [{{ type = \"command\", \
             command = \"{old} notify --hook --host codex --event session-start --profile alice \
             --dir /d\", timeout = 10 }}, {{ type = \"command\", command = \"{old} lint\" }}]\n"
        );
        let file = f.write(".codex/config.toml", &config);
        let found = check(&f.at(), &f.home, false, 0);
        assert_eq!(found.len(), 2, "{found:?}");
        for x in &found {
            let Outcome::Left { fix, .. } = &x.outcome else {
                panic!()
            };
            assert_eq!(fix, "wats --dir /d setup codex --mcp");
        }
        let found = check(&f.at(), &f.home, true, 0);
        assert!(found.iter().all(Finding::repaired), "{found:?}");
        assert!(found[1].line().contains("open /hooks in Codex"));
        let wanted = config
            .replace(
                &format!("command = \"{old}\" # wats"),
                &format!("command = \"{}\" # wats", f.link),
            )
            .replace(&format!("\"{old} notify"), &format!("\"{} notify", f.link));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), wanted);
        assert!(wanted.contains(&format!("[mcp_servers.other]\ncommand = \"{old}\"")));
        assert!(wanted.contains(&format!("\"{old} lint\"")));
    }

    #[test]
    fn vibe_entries_are_repaired_in_place() {
        let f = Fixture::new();
        let old = &f.old;
        let config = format!(
            "active_model = \"devstral\"\n\n[[mcp_servers]]\nname = \"other\"\ntransport = \
             \"stdio\"\ncommand = \"{old}\"\n\n[[mcp_servers]]\nname = \"wats\"\ntransport = \
             \"stdio\"\ncommand = \"{old}\"\nargs = [\"mcp\", \"--profile\", \"alice\"]\n"
        );
        let file = f.write(".vibe/config.toml", &config);
        let found = check(&f.at(), &f.home, true, 0);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].repaired());
        let wanted = format!(
            "active_model = \"devstral\"\n\n[[mcp_servers]]\nname = \"other\"\ntransport = \
             \"stdio\"\ncommand = \"{old}\"\n\n[[mcp_servers]]\nname = \"wats\"\ntransport = \
             \"stdio\"\ncommand = \"{}\"\nargs = [\"mcp\", \"--profile\", \"alice\"]\n",
            f.link
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), wanted);
    }

    #[test]
    fn a_path_homebrew_cannot_replace_is_left_with_the_fix() {
        let f = Fixture::new();
        // Gone, but no Homebrew install: which wats to run is the user's
        // choice.
        let config = "{\"mcpServers\": {\"wats\": {\"command\": \"/gone/bin/wats\", \"args\": \
                      [\"--dir\", \"/my data\", \"mcp\"]}}}";
        let file = f.write(".claude.json", config);
        let found = check(&f.at(), &f.home, true, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].outcome,
            Outcome::Left {
                fix: "wats --dir '/my data' setup claude --mcp".into(),
                error: None,
                auto: false
            }
        );
        assert!(found[0].line().contains("which no longer exists"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), config);
        assert!(f.backups(".claude.json").is_empty());
        // A live path: nothing to say.
        f.write(
            ".claude.json",
            &format!(
                "{{\"mcpServers\": {{\"wats\": {{\"command\": \"{}\"}}}}}}",
                f.link
            ),
        );
        assert!(check(&f.at(), &f.home, true, 0).is_empty());
    }

    #[test]
    fn openclaw_entries_are_repaired_through_its_cli() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let old = &f.old;
        let state = f.home.join("fake-openclaw");
        std::fs::create_dir_all(&state).unwrap();
        // A fake CLI: `mcp show <name>` and `mcp set <name> <json>`.
        let cli = state.join("openclaw");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\necho \"$*\" >> {d}/calls\ncase \"$1 $2\" in\n'mcp show') [ -f {d}/entry-$3.json ] || exit 1; \
                 cat {d}/entry-$3.json;;\n'mcp set') printf '%s\\n' \"$4\" > {d}/entry-$3.json;;\n\
                 *) exit 2;;\nesac\n",
                d = state.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let entry = json!({"command": old, "args": ["mcp", "--profile", "alice"],
                           "env": {"WATS_MCP_CLIENT": "openclaw"}});
        std::fs::write(state.join("entry-wats.json"), entry.to_string()).unwrap();
        // wats-agent, whose token `mcp show` redacts: left to setup.
        let data = f.home.join(".wats");
        crate::openclaw::ensure_token(&crate::openclaw::runner_token_path(&data)).unwrap();
        let agent = json!({"command": old, "args": ["mcp", "--profile", "alice-agent"],
                           "env": {"WATS_RUNNER_TOKEN": "***"}});
        std::fs::write(state.join("entry-wats-agent.json"), agent.to_string()).unwrap();
        let json5 = format!(
            "{{\n  // mine\n  mcp: {{ servers: {{ wats: {{ command: '{old}' }}, other: {{ \
             command: \"/bin/sh\" }} }} }},\n}}\n"
        );
        let file = f.write(".openclaw/openclaw.json", &json5);
        let mut at = f.at();

        // No CLI: the fix.
        let found = check(&at, &data, true, 0);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].repaired());
        assert!(
            found[0].line().contains("wats setup openclaw --mcp"),
            "{}",
            found[0].line()
        );

        at.openclaw_cli = Some(cli);
        // Only checked: from the file alone, the CLI never runs.
        let found = check(&at, &data, false, 0);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0]
                .line()
                .contains("the node repairs it at its next start")
        );
        assert!(!state.join("calls").exists());

        // A token that is not wats's, shown in full: left to setup.
        let mut other = agent.clone();
        other["env"]["WATS_RUNNER_TOKEN"] = json!("someone-else");
        std::fs::write(state.join("entry-wats-agent.json"), other.to_string()).unwrap();
        let found = check(&at, &data, true, 0);
        let Outcome::Left { error, .. } = &found[1].outcome else {
            panic!("{found:?}")
        };
        assert!(
            error.as_deref().unwrap().contains("runner token"),
            "{error:?}"
        );
        std::fs::write(state.join("entry-wats.json"), entry.to_string()).unwrap();
        std::fs::write(state.join("entry-wats-agent.json"), agent.to_string()).unwrap();

        let found = check(&at, &data, true, 0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].repaired(), "{found:?}");
        let Outcome::Left { fix, error, .. } = &found[1].outcome else {
            panic!("{found:?}")
        };
        assert_eq!(fix, "wats setup openclaw --agent");
        assert!(
            error.as_deref().unwrap().contains("masks a value"),
            "{error:?}"
        );
        let set: Value =
            serde_json::from_str(&std::fs::read_to_string(state.join("entry-wats.json")).unwrap())
                .unwrap();
        let mut wanted = entry.clone();
        wanted["command"] = json!(f.link);
        assert_eq!(set, wanted);
        // openclaw.json itself is never written; it was copied first.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), json5);
        assert_eq!(f.backups(".openclaw/openclaw.json").len(), 1);

        // With its real token shown, wats-agent is repaired too.
        let token =
            crate::openclaw::read_token(&crate::openclaw::runner_token_path(&data)).unwrap();
        let mut agent = agent;
        agent["env"]["WATS_RUNNER_TOKEN"] = json!(token);
        std::fs::write(state.join("entry-wats-agent.json"), agent.to_string()).unwrap();
        let found = check(&at, &data, true, 1);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].repaired(), "{found:?}");
        assert_eq!(found[0].entry, "the wats-agent MCP server");
        // openclaw.json did not change: no second backup.
        assert_eq!(f.backups(".openclaw/openclaw.json").len(), 1);
    }

    #[test]
    fn a_hung_cli_is_killed() {
        let started = Instant::now();
        let out = run_for(Path::new("/bin/sleep"), &["10"], Duration::from_millis(200));
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        let out = run_for(Path::new("/bin/echo"), &["hi"], Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, b"hi\n");
        // A descendant left holding stdout does not hang us past the deadline.
        let started = Instant::now();
        let out = run_for(
            Path::new("/bin/sh"),
            &["-c", "sleep 10 & echo started"],
            Duration::from_millis(300),
        );
        assert!(out.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn redacted_values_are_seen() {
        assert!(redacted(&json!({"env": {"T": "***"}})));
        assert!(redacted(&json!(["<REDACTED>"])));
        assert!(!redacted(&json!({"env": {"T": "abc"}, "args": ["mcp"]})));
    }

    #[test]
    fn repairs_write_atomically_and_keep_few_backups() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // A symlinked dotfile (a dotfiles repo): the target is replaced,
        // the link stays.
        let real = dir.join("dotfiles/claude.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "one").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.join(".claude.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let backups = || -> Vec<PathBuf> {
            let mut found: Vec<PathBuf> = std::fs::read_dir(real.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.to_string_lossy().contains(".wats-repair.bak-"))
                .collect();
            found.sort();
            found
        };
        let upper = |t: &str| Ok(t.to_uppercase());
        let backup = commit(&link, "one", &upper, 0).unwrap().unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "ONE");
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "one");
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o640
        );
        // No temporary file left.
        assert!(
            std::fs::read_dir(real.parent().unwrap())
                .unwrap()
                .all(|e| !e.unwrap().path().to_string_lossy().ends_with(".tmp"))
        );

        // The same content again: the newest backup serves; at most 3.
        for (i, text) in ["one", "two", "three", "four", "five"].iter().enumerate() {
            std::fs::write(&real, text).unwrap();
            commit(&link, text, &upper, i as u64 * 60).unwrap();
            std::fs::write(&real, text).unwrap();
            commit(&link, text, &upper, i as u64 * 60 + 1).unwrap();
        }
        let kept = backups();
        assert_eq!(kept.len(), MAX_BACKUPS, "{kept:?}");
        assert_eq!(
            std::fs::read_to_string(kept.last().unwrap()).unwrap(),
            "five"
        );

        // Changed under us once: redone on the fresh text.
        std::fs::write(&real, "six").unwrap();
        let changed = std::cell::Cell::new(false);
        let edit = |t: &str| {
            if !changed.replace(true) {
                std::fs::write(&real, "seven").unwrap();
            }
            Ok(t.to_uppercase())
        };
        commit(&link, "six", &edit, 1000).unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "SEVEN");
        // Changed under us every time: given up, the file as the other
        // writer left it.
        let n = std::cell::Cell::new(0);
        let edit = |t: &str| {
            n.set(n.get() + 1);
            std::fs::write(&real, format!("w{}", n.get())).unwrap();
            Ok(format!("{t}!"))
        };
        let err = commit(&link, "SEVEN", &edit, 2000).unwrap_err();
        assert!(format!("{err:#}").contains("kept changing"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "w2");
    }

    #[test]
    fn shell_words_are_read_as_written() {
        assert_eq!(
            first_word("/bin/wats notify --hook"),
            Some(("/bin/wats".into(), " notify --hook"))
        );
        let quoted = crate::hook::shell_quote("/Users/it's me/bin/wats");
        let line = format!("{quoted} notify");
        assert_eq!(
            first_word(&line),
            Some(("/Users/it's me/bin/wats".into(), " notify"))
        );
        assert_eq!(first_word("\"/a b/wats\" x"), None);
        assert_eq!(first_word(""), None);
        assert_eq!(
            dir_arg(["mcp", "--dir", "/d", "x"].into_iter()),
            Some("/d".into())
        );
    }
}

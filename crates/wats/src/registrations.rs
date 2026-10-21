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

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::update::{homebrew_managed, homebrew_relink};

/// Set to 1, the check runs in hermetic mode too (tests, with a throwaway
/// HOME). Otherwise hermetic mode never reads the tools' configs.
pub const CHECK_ENV: &str = "WATS_CHECK_REGISTRATIONS";

/// Whether this process checks the registrations: never in hermetic mode,
/// unless [`CHECK_ENV`] is 1.
pub fn enabled(hermetic: bool) -> bool {
    !hermetic || std::env::var(CHECK_ENV).is_ok_and(|v| v.trim() == "1")
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
    /// repair failed, if it was tried.
    Left { fix: String, error: Option<String> },
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
            Outcome::Left { fix, error } => {
                let error = error
                    .as_ref()
                    .map(|e| format!(" (the repair failed: {e})"))
                    .unwrap_or_default();
                format!(
                    "{tool}: {entry} in {file} runs {path}, {}{error}: run `{fix}` to fix it",
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
            Outcome::Left { fix, .. } => value["fix"] = json!(fix),
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
/// longer has the stale path: `wats setup`, `wats status`).
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

/// Repair `sites` of `file` (when `repair` and Homebrew's link replaces
/// each), with `rewrite` making the new text from the map of old values to
/// new ones; one finding per entry.
fn settle(
    tool: Tool,
    file: &Path,
    sites: Vec<Site>,
    repair: bool,
    now: u64,
    rewrite: impl FnOnce(&HashMap<String, String>) -> Result<String>,
) -> Vec<Finding> {
    if sites.is_empty() {
        return Vec::new();
    }
    let mut result = None;
    if repair && sites.iter().all(|s| s.relink.is_some()) {
        let map: HashMap<String, String> = sites
            .iter()
            .filter_map(|s| Some((s.value.clone(), s.relink.as_ref()?.1.clone())))
            .collect();
        result = Some(
            rewrite(&map)
                .and_then(|text| crate::vibe::write_with_backup(file, &text, now))
                .map_err(|e| format!("{e:#}")),
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
    settle(Tool::Claude, file, sites, repair, now, |map| {
        replace_json_strings(&text, &at, map)
    })
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
    settle(Tool::Claude, file, sites, repair, now, |map| {
        replace_json_strings(&text, &at, map)
    })
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
    settle(Tool::Codex, file, sites, repair, now, |map| {
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
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
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
    settle(Tool::Vibe, file, sites, repair, now, |map| {
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

/// OpenClaw's `wats` and `wats-agent` entries. Looked at through its CLI
/// only when openclaw.json has a stale wats path at all.
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
    let Some(cli) = cli else {
        return vec![finding(
            "a wats MCP server",
            any,
            Outcome::Left {
                fix: fix_command(Tool::OpenClaw, "--mcp", None),
                error: repair.then(|| "no openclaw CLI on PATH".to_string()),
            },
        )];
    };
    let run = |args: &[&str]| {
        std::process::Command::new(cli)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
    };
    let mut out = Vec::new();
    let mut backup: Option<Option<PathBuf>> = None;
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
        let relink = homebrew_relink(Path::new(&command));
        // The agent's entry carries the runner token: re-set only when
        // `mcp show` shows it as it is (not redacted).
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
        let (Some(link), true, true) = (relink, repair, token_ok) else {
            let error = (repair && !token_ok)
                .then(|| "`openclaw mcp show` does not show its runner token".to_string());
            out.push(finding(&label, command, Outcome::Left { fix, error }));
            continue;
        };
        // A copy of openclaw.json first, once.
        let copied = backup
            .get_or_insert_with(|| {
                let path = oc::backup_path(file, now);
                std::fs::copy(file, &path).ok().map(|_| path)
            })
            .clone();
        entry.insert("command".into(), json!(link.to_string_lossy()));
        let set = run(&["mcp", "set", name, &Value::Object(entry).to_string()]);
        let outcome = match set {
            Some(o) if o.status.success() => Outcome::Repaired {
                to: link,
                backup: copied,
            },
            Some(o) => Outcome::Left {
                fix,
                error: Some(format!("`openclaw mcp set {name}` failed ({})", o.status)),
            },
            None => Outcome::Left {
                fix,
                error: Some(format!("could not run {}", cli.display())),
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
                        .starts_with(&format!("{name}.bak-"))
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
                error: None
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
                "#!/bin/sh\ncase \"$1 $2\" in\n'mcp show') [ -f {d}/entry-$3.json ] || exit 1; \
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
        let found = check(&at, &data, true, 0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].repaired(), "{found:?}");
        let Outcome::Left { fix, error } = &found[1].outcome else {
            panic!("{found:?}")
        };
        assert_eq!(fix, "wats setup openclaw --agent");
        assert!(error.as_deref().unwrap().contains("runner token"));
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

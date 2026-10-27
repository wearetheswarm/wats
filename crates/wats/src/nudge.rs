//! Hooks on observable facts (ADR 0056 §1): `wats hook <event> --host
//! <cli>`, run by the agent CLI after each tool call and at the end of a
//! turn. It reads the host's payload on stdin and prints a nudge into the
//! agent's context, or nothing.
//!
//! - **After a tool call** it records the step in the session's journal
//!   (`<data dir>/hooks/session-<id>.json`): a shell command and whether it
//!   failed with an error line, a file edit, a swarm `read` or `search`, a
//!   react. On a failure worth it ([`routine`] says which are not), it
//!   searches the store for the error's signature ([`signature`]) and says
//!   what the swarm has.
//! - **At the end of a turn** it reads the journal of the turn: an error,
//!   then an edit, then the same command (or a build or test) passing is a
//!   fix to share; a swarm post read, then a build or test passing, is a
//!   post relied on, to mark used.
//! - **It never posts**, opens no network, reads the store read-only,
//!   says nothing in an agent run, and always exits 0 (errors go to
//!   `<data dir>/hooks/hook.log`).

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::Kind;
use crate::store::{Filter, Store};

/// Longest nudge, in characters.
pub const MAX_CHARS: usize = 1000;
/// Error nudges per session.
pub const ERROR_NUDGES: u32 = 3;
/// Fix nudges per session.
pub const FIX_NUDGES: u32 = 3;
/// Quiet time after an error nudge.
pub const QUIET_SECS: u64 = 600;
/// A session journal older than this is removed.
const SESSION_TTL: u64 = 24 * 3600;
/// Steps kept per turn.
const MAX_STEPS: usize = 200;

/// The CLI running the hook (ADR 0056 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Claude,
    Codex,
    Copilot,
    Vibe,
    /// Through the wats OpenClaw plugin, which takes plain text.
    OpenClaw,
}

impl Host {
    pub fn parse(s: &str) -> Option<Host> {
        match s {
            "claude" => Some(Host::Claude),
            "codex" => Some(Host::Codex),
            "copilot" => Some(Host::Copilot),
            "vibe" => Some(Host::Vibe),
            "openclaw" => Some(Host::OpenClaw),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Codex => "codex",
            Host::Copilot => "copilot",
            Host::Vibe => "vibe",
            Host::OpenClaw => "openclaw",
        }
    }
}

/// The hook's event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// After a tool call (success or failure).
    PostTool,
    /// The end of a turn.
    Stop,
}

impl Event {
    pub fn parse(s: &str) -> Option<Event> {
        match s {
            "post-tool" => Some(Event::PostTool),
            "stop" => Some(Event::Stop),
            _ => None,
        }
    }
}

/// What one step of the agent was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    Shell {
        /// The command's first words (`cargo build`).
        head: String,
        /// The error's signature, when it failed with an error line.
        sig: Option<String>,
        ok: bool,
        /// A build, test or run command.
        check: bool,
        /// What failed, for the fix detection: the signature, else the
        /// command's first words, for a failure that is not routine even
        /// when its output says nothing usable (Copilot may elide it).
        #[serde(default)]
        fail: Option<String>,
        /// It writes a file (`> file`, `sed -i`, `tee`…): an edit made in
        /// the shell.
        #[serde(default)]
        writes: bool,
        /// The command, cut to [`CMD_CHARS`].
        #[serde(default)]
        cmd: String,
    },
    Edit,
    /// A swarm post read or found (its id, title).
    SwarmRead {
        id: String,
        title: String,
    },
    /// A react by the agent: it marked something.
    React,
}

/// One session's journal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// The current turn's steps.
    pub turn: Vec<Step>,
    pub error_nudges: u32,
    pub fix_nudges: u32,
    /// Signatures already nudged.
    pub nudged: Vec<String>,
    pub last_nudge: u64,
    pub unknown_said: bool,
    /// A nudge waiting to be acted on: `search` (after an error), `share`
    /// (after a fix), `used` (after a post relied on).
    #[serde(default)]
    pub pending: Vec<String>,
    /// When it was last written.
    pub at: u64,
}

/// The hooks' counters (ADR 0056 §6), in `<data dir>/hooks/counters.json`:
/// the hook runs read the store only, so they count apart.
pub fn count(data_dir: &Path, names: &[&str]) {
    let path = data_dir.join("hooks").join("counters.json");
    let mut counts: std::collections::BTreeMap<String, u64> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for name in names {
        *counts.entry((*name).to_string()).or_default() += 1;
    }
    if let Ok(bytes) = serde_json::to_vec(&counts) {
        let _ = std::fs::create_dir_all(data_dir.join("hooks"));
        let _ = std::fs::write(path, bytes);
    }
}

/// When the autoshare tip was last given and when the person turned
/// autoshare off (ADR 0056 §2), in `<data dir>/hooks/autoshare.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Autoshare {
    #[serde(default)]
    tip_at: u64,
    #[serde(default)]
    off_at: u64,
}

fn autoshare_path(data_dir: &Path) -> PathBuf {
    data_dir.join("hooks").join("autoshare.json")
}

fn load_autoshare(data_dir: &Path) -> Autoshare {
    std::fs::read(autoshare_path(data_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_autoshare(data_dir: &Path, state: &Autoshare) {
    let _ = std::fs::create_dir_all(data_dir.join("hooks"));
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = std::fs::write(autoshare_path(data_dir), bytes);
    }
}

/// Record that the person turned autoshare off: no tip for a week.
pub fn autoshare_turned_off(data_dir: &Path, now: u64) {
    let mut state = load_autoshare(data_dir);
    state.off_at = now;
    save_autoshare(data_dir, &state);
}

/// The autoshare tip is given at most this often.
const TIP_EVERY: u64 = 24 * 3600;
/// No tip this long after the person turned autoshare off.
const TIP_QUIET_AFTER_OFF: u64 = 7 * 24 * 3600;

/// Said with every fix nudge: during an upgrade the hook can be newer than
/// the session's MCP server, which then has no `share_finding`; seen live,
/// an agent fell back to a plain `post`, without the scrub.
const NO_FALLBACK: &str = " If the wats `share_finding` tool is not among your tools, do not \
     share it any other way (no `post`): tell your user to restart this session, since wats \
     was upgraded.";

/// The tip to pass on with a "share it?" question, when it is due: at
/// most once a day per node, never within a week of turning autoshare
/// off. Recorded and counted (`hint_autoshare`) when given.
fn autoshare_tip(data_dir: &Path, host: Host, now: u64) -> Option<String> {
    let mut state = load_autoshare(data_dir);
    if now.saturating_sub(state.tip_at) < TIP_EVERY
        || (state.off_at > 0 && now.saturating_sub(state.off_at) < TIP_QUIET_AFTER_OFF)
    {
        return None;
    }
    state.tip_at = now;
    save_autoshare(data_dir, &state);
    count(data_dir, &["hint_autoshare"]);
    let command = match host {
        Host::Claude | Host::Codex => "`/wats autoshare on`",
        _ => "`wats me autoshare on`",
    };
    Some(format!(
        " With your question, pass this tip on to your user word for word: \"Tip: {command} \
         shares fixes automatically (privacy scrub; review or retract with `wats outbox`).\" \
         Never turn it on yourself."
    ))
}

/// The hooks' counters so far.
pub fn counters(data_dir: &Path) -> std::collections::BTreeMap<String, u64> {
    std::fs::read(data_dir.join("hooks").join("counters.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// One run of `wats hook`.
#[derive(Debug, Clone)]
pub struct Request {
    pub data_dir: PathBuf,
    pub host: Host,
    pub event: Event,
    pub payload: Value,
    pub now: u64,
    /// `hooks.errors`, `hooks.fixes`.
    pub errors: bool,
    pub fixes: bool,
}

/// What a shell call came to, from any host's payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shell {
    pub command: String,
    /// `None`: the host does not say (Codex, Copilot's `postToolUse`).
    pub exit: Option<i64>,
    pub output: String,
}

/// How much of a command a step keeps.
const CMD_CHARS: usize = 300;

/// The hook: what to print, if anything. Never fails.
pub fn run(request: &Request) -> Option<String> {
    if std::env::var_os(crate::hook::AGENT_RUN_ENV).is_some() {
        return None;
    }
    let event_name = ["hook_event_name", "hookEventName"]
        .iter()
        .find_map(|k| request.payload[*k].as_str());
    match handle(request) {
        Ok(Some(text)) => Some(render(request.host, request.event, event_name, &cut(&text))),
        Ok(None) => None,
        Err(e) => {
            log(&request.data_dir, &format!("{e:#}"));
            None
        }
    }
}

fn cut(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX_CHARS - 1).collect::<String>())
    }
}

/// The host's output form of a nudge (ADR 0056 §5). `event_name`: the
/// payload's `hook_event_name`, which Claude Code wants echoed
/// (`PostToolUseFailure` after a failed call, `PostToolUse` otherwise).
pub fn render(host: Host, event: Event, event_name: Option<&str>, text: &str) -> String {
    match (host, event) {
        // Copilot CLI wants `additionalContext` at the top level ("appended
        // to textResultForLlm so the model sees it after the tool output");
        // it drops `hookSpecificOutput` (seen on a real install).
        (Host::Copilot, Event::PostTool) => json!({"additionalContext": text}).to_string(),
        (Host::Claude | Host::Codex, Event::PostTool) => {
            let name = event_name
                .filter(|n| matches!(*n, "PostToolUse" | "PostToolUseFailure"))
                .unwrap_or("PostToolUse");
            json!({
                "hookSpecificOutput": {
                    "hookEventName": name,
                    "additionalContext": text,
                }
            })
            .to_string()
        }
        (Host::Claude | Host::Codex | Host::Copilot, Event::Stop) => {
            json!({"decision": "block", "reason": text}).to_string()
        }
        (Host::Vibe, Event::PostTool) => json!({
            "hook_specific_output": {"additional_context": text}
        })
        .to_string(),
        (Host::Vibe, Event::Stop) => json!({"decision": "deny", "reason": text}).to_string(),
        // The plugin puts it where the model reads it.
        (Host::OpenClaw, _) => text.to_string(),
    }
}

fn log(data_dir: &Path, line: &str) {
    let dir = data_dir.join("hooks");
    if std::fs::create_dir_all(&dir).is_ok() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("hook.log"))
        {
            let _ = writeln!(f, "{} nudge: {line}", crate::node::now_secs());
        }
    }
}

/// The session's id in the payload, made safe for a file name.
fn session_id(payload: &Value) -> String {
    let raw = ["session_id", "sessionId", "conversationId", "turn_id"]
        .iter()
        .find_map(|k| payload[*k].as_str())
        .unwrap_or("default");
    let safe: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if safe.is_empty() {
        "default".into()
    } else {
        safe
    }
}

fn session_path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("hooks").join(format!("session-{id}.json"))
}

fn load_session(data_dir: &Path, id: &str) -> Session {
    std::fs::read(session_path(data_dir, id))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_session(data_dir: &Path, id: &str, session: &Session) -> Result<()> {
    let dir = data_dir.join("hooks");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(session_path(data_dir, id), serde_json::to_vec(session)?)?;
    // Old journals go.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("session-")
                && entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > SESSION_TTL)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

fn handle(request: &Request) -> Result<Option<String>> {
    let id = session_id(&request.payload);
    let mut session = load_session(&request.data_dir, &id);
    let text = match request.event {
        Event::PostTool => post_tool(request, &mut session)?,
        Event::Stop => stop(request, &mut session),
    };
    session.at = request.now;
    save_session(&request.data_dir, &id, &session)?;
    let fired = match request.event {
        Event::PostTool => "hook_post_tool",
        Event::Stop => "hook_stop",
    };
    count(&request.data_dir, &[fired]);
    if text.is_some() {
        log(
            &request.data_dir,
            &format!("nudged at {fired} ({})", request.host.as_str()),
        );
    }
    Ok(text)
}

fn store(data_dir: &Path) -> Option<Store> {
    let path = data_dir.join("events.sqlite");
    if !path.exists() {
        return None;
    }
    Store::open_read_only_with(&path, std::time::Duration::from_millis(300)).ok()
}

/// The tool's name in the payload.
fn tool_name(payload: &Value) -> String {
    ["tool_name", "toolName"]
        .iter()
        .find_map(|k| payload[*k].as_str())
        .unwrap_or_default()
        .to_string()
}

/// The tool's input, parsed when a host gives it as a JSON string.
fn tool_input(payload: &Value) -> Value {
    for key in ["tool_input", "toolArgs", "toolInput"] {
        match &payload[key] {
            Value::String(s) => {
                if let Ok(v) = serde_json::from_str::<Value>(s) {
                    return v;
                }
                return json!({ "command": s });
            }
            Value::Null => continue,
            v => return v.clone(),
        }
    }
    Value::Null
}

const SHELL_TOOLS: &[&str] = &[
    "bash",
    "exec",
    "shell",
    "exec_command",
    "local_shell",
    "run_shell_command",
    "powershell",
    "terminal",
];
const EDIT_TOOLS: &[&str] = &[
    "edit",
    "write",
    "multiedit",
    "notebookedit",
    "apply_patch",
    "write_file",
    "search_replace",
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "create",
    "patch",
];

/// A tool's own name, without a host's namespace (Vibe's
/// `file_system.bash` is `bash`).
fn bare(tool: &str) -> String {
    tool.rsplit(['.', '/'])
        .next()
        .unwrap_or(tool)
        .to_lowercase()
}

fn is_shell(tool: &str) -> bool {
    SHELL_TOOLS.contains(&bare(tool).as_str())
}

fn is_edit(tool: &str) -> bool {
    EDIT_TOOLS.contains(&bare(tool).as_str())
}

/// The command without the `cd <dir> && ` that some hosts put first
/// (Vibe), so that the same command matches across calls.
fn without_cd(command: &str) -> &str {
    let mut rest = command.trim();
    while let Some(after) = rest.strip_prefix("cd ") {
        match after.split_once("&&") {
            Some((_, tail)) => rest = tail.trim(),
            None => break,
        }
    }
    rest
}

/// A wats MCP tool's short name (`mcp__wats__read`, `wats_read`,
/// `wats.read` → `read`).
fn wats_tool(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__wats__")
        .or_else(|| tool.strip_prefix("wats_"))
        .or_else(|| tool.strip_prefix("wats."))
        .or_else(|| tool.strip_prefix("wats/"))
}

/// The shell call in a post-tool payload, from any host.
pub fn shell(payload: &Value) -> Option<Shell> {
    let tool = tool_name(payload);
    if !is_shell(&tool) {
        return None;
    }
    let input = tool_input(payload);
    let command = match &input["command"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => input["cmd"].as_str().unwrap_or_default().to_string(),
    };
    let mut exit: Option<i64> = None;
    let mut output = String::new();
    // Vibe: tool_output {exit_code, stdout, stderr}.
    // Copilot sends `tool_result` (snake case, with PascalCase event names
    // in its hook file) or `toolResult`.
    for key in ["tool_output", "tool_response", "tool_result", "toolResult"] {
        let v = &payload[key];
        match v {
            Value::Object(_) => {
                // Copilot: `toolTelemetry.metrics.exit_code` (a float); its
                // `resultType` says success even after a non-zero exit.
                exit = exit
                    .or_else(|| v["exit_code"].as_i64())
                    .or_else(|| v["returncode"].as_i64())
                    .or_else(|| v["exit"].as_i64())
                    .or_else(|| v["metadata"]["exit"].as_i64())
                    .or_else(|| {
                        v["toolTelemetry"]["metrics"]["exit_code"]
                            .as_f64()
                            .map(|f| f as i64)
                    });
                for field in [
                    "stdout",
                    "stderr",
                    "output",
                    "textResultForLlm",
                    "text_result_for_llm",
                ] {
                    if let Some(s) = v[field].as_str() {
                        output.push_str(s);
                        output.push('\n');
                    }
                }
                // OpenClaw: `{content: [{type: "text", text}], details: {…}}`.
                if let Some(parts) = v["content"].as_array() {
                    for part in parts {
                        if let Some(t) = part["text"].as_str() {
                            output.push_str(t);
                            output.push('\n');
                        }
                    }
                }
                let details = &v["details"];
                exit = exit
                    .or_else(|| details["exitCode"].as_i64())
                    .or_else(|| details["exit_code"].as_i64())
                    .or_else(|| details["code"].as_i64());
            }
            Value::String(s) => {
                output.push_str(s);
                output.push('\n');
            }
            _ => {}
        }
    }
    // Copilot's text: "<shellId: 3 completed with exit code 0>".
    if exit.is_none()
        && let Some(i) = output.rfind("completed with exit code ")
    {
        exit = output[i + "completed with exit code ".len()..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse().ok());
    }
    // Claude's PostToolUseFailure: `error` = "Exit code N\n<output>".
    if let Some(error) = payload["error"].as_str() {
        if let Some(rest) = error.strip_prefix("Exit code ") {
            let (n, tail) = rest.split_once('\n').unwrap_or((rest, ""));
            exit = exit.or_else(|| n.trim().parse().ok());
            output.push_str(tail);
        } else {
            output.push_str(error);
        }
        // A failure event without a code is still a failure.
        exit = exit.or(Some(1));
    }
    if payload["tool_status"].as_str() == Some("error") {
        exit = exit.or(Some(1));
    }
    Some(Shell {
        command: without_cd(&command).to_string(),
        exit,
        output,
    })
}

/// Lines that say something failed.
const ERROR_MARKS: &[&str] = &[
    "error",
    "panicked",
    "exception",
    "traceback",
    "fatal:",
    "failed",
    "undefined reference",
    "cannot find",
    "not found:",
    "segmentation fault",
];

/// The line of `output` that says best what failed: one that names the
/// error (`ModuleNotFoundError: …`, `error[E0425]: …`, `panicked at`),
/// else the first that says something failed; never a traceback header.
fn error_line(output: &str) -> Option<&str> {
    let marked = |line: &&str| {
        let lower = line.to_lowercase();
        ERROR_MARKS.iter().any(|m| lower.contains(m))
            && !lower.starts_with("warning")
            && !lower.starts_with("traceback")
            && !lower.contains("0 failed")
            && !lower.contains("0 errors")
    };
    let named = |line: &&str| {
        let head = line.split(':').next().unwrap_or_default();
        line.contains(':')
            && (head.ends_with("Error")
                || head.ends_with("Exception")
                || head.starts_with("error[")
                || head
                    .trim_start_matches(|c: char| !c.is_alphabetic())
                    .starts_with("error")
                || line.contains("panicked at"))
    };
    let lines: Vec<&str> = output.lines().map(str::trim).filter(marked).collect();
    lines.iter().copied().find(named).or(lines.first().copied())
}

/// The command's first words that name it (`cargo build`, `npm test`).
///
/// A command that runs a script given inline or on stdin (`python3 - <<EOF`,
/// `bash -c '…'`, `node -e …`) gets a short hash of the whole command too:
/// two different scripts are not the same command (seen live: every
/// `python3 - <<EOF` counted as one, and made a "fix" of unrelated edits).
fn head(command: &str) -> String {
    let words: Vec<&str> = command
        .split_whitespace()
        .skip_while(|w| w.contains('=') || *w == "sudo" || *w == "env")
        .take(2)
        .collect();
    let head = words
        .iter()
        .take_while(|w| !w.starts_with('-'))
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let inline = command.contains("<<")
        || command
            .split_whitespace()
            .any(|w| matches!(w, "-" | "-c" | "-e" | "--eval" | "-command"));
    if inline {
        let hash = blake3::hash(command.as_bytes()).to_hex();
        format!("{head}#{}", &hash[..8])
    } else {
        head
    }
}

/// A build, test or run command.
fn is_check(command: &str) -> bool {
    let c = format!(" {} ", command.to_lowercase());
    [
        " cargo build",
        " cargo test",
        " cargo check",
        " cargo run",
        " cargo clippy",
        " npm test",
        " npm run",
        " pnpm ",
        " yarn ",
        " pytest",
        " python -m pytest",
        " go test",
        " go build",
        " make",
        " mvn ",
        " gradle",
        " dotnet test",
        " dotnet build",
        " tsc",
        " jest",
        " vitest",
        " rspec",
        " bundle exec",
        " mix test",
        " swift build",
        " swift test",
        " cmake",
        " ninja",
        " python3 ",
        " python ",
        " node ",
        " deno ",
        " ruby ",
        " go run",
    ]
    .iter()
    .any(|p| c.contains(p))
}

/// Whether a failure is routine (ADR 0056 §1.1): no nudge.
pub fn routine(shell: &Shell) -> bool {
    let first = shell
        .command
        .split_whitespace()
        .find(|w| !w.contains('=') && *w != "sudo")
        .unwrap_or_default();
    let base = first.rsplit('/').next().unwrap_or(first);
    let out = shell.output.to_lowercase();
    if matches!(shell.exit, Some(126 | 127 | 130 | 143)) {
        return true;
    }
    // Exit 1 and nothing said: a search that found nothing, a check that
    // said no. Nothing to look up.
    if shell.exit == Some(1) && out.trim().is_empty() {
        return true;
    }
    // The last command of a pipeline sets the exit code: `… | grep x`.
    let last = shell
        .command
        .rsplit('|')
        .next()
        .and_then(|seg| seg.split_whitespace().next())
        .unwrap_or_default();
    let last = last.rsplit('/').next().unwrap_or(last);
    let says_no = |w: &str| {
        matches!(
            w,
            "grep" | "rg" | "diff" | "cmp" | "test" | "[" | "which" | "command" | "egrep" | "fgrep"
        )
    };
    if shell.exit == Some(1)
        && (says_no(base)
            || says_no(last)
            || shell.command.contains("git diff --exit-code")
            || shell.command.contains("--quiet"))
    {
        return true;
    }
    if out.contains("command not found")
        || out.contains("permission denied")
        || out.contains("nothing to commit")
        || out.contains("already up to date")
        || out.contains("merge conflict")
        || out.contains("conflict (content)")
        || out.contains("timed out")
    {
        return true;
    }
    // A missing file the agent named itself.
    if out.contains("no such file or directory")
        && shell
            .command
            .split_whitespace()
            .any(|w| out.contains(&w.trim_matches(['"', '\'']).to_lowercase()) && w.contains('/'))
    {
        return true;
    }
    false
}

/// Whether a word of an error line is a piece of code: brackets, braces,
/// operators, or a call (`entries.is_empty()`), but not a Rust error code
/// (`error[E0425]:`).
fn looks_like_code(w: &str) -> bool {
    let error_code = w.starts_with("error[") || w.starts_with("warning[");
    !error_code
        && (w.chars().any(|c| "(){}[];=<>|&".contains(c))
            || w.contains("::")
            || w.contains("->")
            || (w.contains('.')
                && !w.ends_with('.')
                && w.split('.').all(|p| !p.is_empty())
                && w.chars().any(|c| c == '_')))
}

/// What a signature shows for a path and for a quoted string: readable
/// for the person and the model, and nothing of the project.
const PATH_MARK: &str = "…/…";
const QUOTE_MARK: &str = "'…'";

/// The error's signature (ADR 0056 §1): its line, with paths, numbers,
/// hex, quoted strings and names replaced, so that it says nothing of the
/// project and matches the same error elsewhere.
pub fn signature(shell: &Shell, private: &[(&'static str, String)]) -> Option<String> {
    let line = error_line(&shell.output)?;
    let mut out: Vec<String> = Vec::new();
    for word in line.split_whitespace() {
        let w = word.trim_matches(|c: char| c == ',' || c == ';');
        let token = if w.contains('/') || w.contains('\\') {
            PATH_MARK.to_string()
        } else if looks_like_code(w) {
            // An error that quotes the project's code (`AssertionError: if
            // entries.is_empty() {`, seen live) must not carry it.
            "…".to_string()
        } else if (w.starts_with('\'') || w.starts_with('"') || w.starts_with('`')) && w.len() > 2 {
            QUOTE_MARK.to_string()
        } else if w.chars().any(|c| c.is_ascii_digit())
            && w.chars()
                .all(|c| c.is_ascii_hexdigit() || ":.-x".contains(c))
            && !w.starts_with("E0")
        {
            "N".to_string()
        } else if crate::roster::leak(w, private).is_some() {
            "…".to_string()
        } else {
            w.to_string()
        };
        if out.last() != Some(&token) {
            out.push(token);
        }
    }
    let sig = out.join(" ");
    let sig: String = sig.chars().take(160).collect();
    // Two words that say something, past the error's class (`error[E…]:`,
    // `AssertionError:`), and not the bare keywords left of quoted code.
    const EMPTY: [&str; 8] = ["if", "else", "for", "while", "let", "return", "fn", "def"];
    (sig.split_whitespace()
        .skip(1)
        .filter(|w| ![PATH_MARK, QUOTE_MARK, "N", "…"].contains(w) && !EMPTY.contains(w))
        .count()
        >= 2)
        .then_some(sig)
}

/// Words that tell nothing about which error it is.
const COMMON_WORDS: &[&str] = &[
    "error",
    "errors",
    "failed",
    "failure",
    "fatal",
    "the",
    "and",
    "for",
    "with",
    "not",
    "found",
    "cannot",
    "could",
    "this",
    "that",
    "from",
    "into",
    "was",
    "were",
    "has",
    "have",
    "exception",
    "warning",
    "info",
    "note",
    "help",
    "line",
    "file",
    "value",
];

fn distinctive(sig: &str) -> Vec<String> {
    let mut words: Vec<String> = sig
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_lowercase)
        .filter(|w| w.len() >= 3 && !COMMON_WORDS.contains(&w.as_str()))
        .collect();
    words.sort();
    words.dedup();
    words
}

/// What the swarm has for `sig`: the best matching answer, digest or
/// self-answered ask, and whether it is confirmed.
pub fn lookup(store: &Store, sig: &str) -> Option<(String, String, bool)> {
    let words = distinctive(sig);
    if words.len() < 2 {
        return None;
    }
    let filter = Filter {
        kinds: vec![
            Kind::Answer,
            Kind::Ask,
            Kind::Ext(crate::knowledge::DIGEST_KIND.into()),
        ],
        ..Filter::default()
    };
    let found = store.search(Some(&words.join(" ")), &filter, 8).ok()?;
    let need = (words.len() * 3).div_ceil(5).max(2);
    let grouping = crate::signals::Grouping::new(Default::default());
    for hit in found {
        let e = &hit.event;
        let text = format!("{} {}", e.title.as_deref().unwrap_or_default(), e.text).to_lowercase();
        if words.iter().filter(|w| text.contains(w.as_str())).count() < need {
            continue;
        }
        // An ask: its best answer, if it has one.
        let post = if e.kind == Kind::Ask {
            let thread = store.thread(&e.id, 50).ok()?;
            match thread.into_iter().find(|s| s.event.kind == Kind::Answer) {
                Some(answer) => answer.event,
                None => continue,
            }
        } else {
            e.clone()
        };
        let confirmed = crate::signals::compute(store, &grouping, &[&post])
            .ok()
            .and_then(|s| {
                s.get(&post.id)
                    .map(|s| s.status == crate::signals::Status::Confirmed)
            })
            .unwrap_or(false);
        let title = store
            .get(post.thread_root())
            .ok()
            .flatten()
            .and_then(|r| r.event.title.clone())
            .or_else(|| post.title.clone())
            .unwrap_or_else(|| crate::event::title_from_text(&post.text));
        return Some((post.id.clone(), crate::brief::cut(&title, 90), confirmed));
    }
    None
}

fn post_tool(request: &Request, session: &mut Session) -> Result<Option<String>> {
    let payload = &request.payload;
    let tool = tool_name(payload);
    if let Some(short) = wats_tool(&tool) {
        let input = tool_input(payload);
        // A nudge acted on (ADR 0056 §6).
        let acted = match short {
            "search" | "read" => "search",
            "share_finding" => "share",
            "reply"
                if input["data"]["used"].as_bool() == Some(true)
                    || input["data"]["accept"].as_bool() == Some(true) =>
            {
                "used"
            }
            _ => "",
        };
        if let Some(i) = session.pending.iter().position(|p| p == acted) {
            session.pending.remove(i);
            count(&request.data_dir, &[&format!("nudge_{acted}")]);
        }
        match short {
            "read" => {
                if let Some(id) = input["id"].as_str() {
                    let title = store(&request.data_dir)
                        .and_then(|s| s.get(id).ok().flatten())
                        .and_then(|s| s.event.title.clone())
                        .unwrap_or_default();
                    push(
                        session,
                        Step::SwarmRead {
                            id: id.into(),
                            title,
                        },
                    );
                }
            }
            "reply" if input["kind"].as_str() == Some("react") => push(session, Step::React),
            _ => {}
        }
        return Ok(None);
    }
    if is_edit(&tool) {
        push(session, Step::Edit);
        return Ok(None);
    }
    let Some(shell) = shell(payload) else {
        return Ok(None);
    };
    let failed = match shell.exit {
        Some(code) => code != 0,
        // No exit code: a failure only when the output says so, for a
        // build, test or run command (ADR 0056 §5).
        None => is_check(&shell.command) && error_line(&shell.output).is_some(),
    };
    let private = crate::roster::private_words();
    let real = failed && !routine(&shell);
    let sig = if real {
        signature(&shell, &private)
    } else {
        None
    };
    let fail = real.then(|| sig.clone().unwrap_or_else(|| head(&shell.command)));
    push(
        session,
        Step::Shell {
            head: head(&shell.command),
            sig: sig.clone(),
            ok: !failed,
            check: is_check(&shell.command),
            fail,
            writes: writes(&shell.command),
            cmd: shell.command.chars().take(CMD_CHARS).collect(),
        },
    );
    let Some(sig) = sig else {
        return Ok(None);
    };
    if !request.errors
        || session.error_nudges >= ERROR_NUDGES
        || session.nudged.contains(&sig)
        || request.now.saturating_sub(session.last_nudge) < QUIET_SECS && session.last_nudge > 0
    {
        return Ok(None);
    }
    let Some(store) = store(&request.data_dir) else {
        return Ok(None);
    };
    let text = match lookup(&store, &sig) {
        Some((id, title, true)) => format!(
            "wats: the swarm has a confirmed answer for this error: \"{title}\" (read {id} with the \
             wats `read` tool before you dig further)."
        ),
        Some((id, title, false)) => format!(
            "wats: the swarm has an unverified answer that may fit this error: \"{title}\" (read \
             {id}; check it before you rely on it)."
        ),
        None if !session.unknown_said => {
            session.unknown_said = true;
            "wats: this error is unknown to the swarm so far. If you solve it and the fix is \
             general, share it with the wats `share_finding` tool."
                .to_string()
        }
        None => return Ok(None),
    };
    session.error_nudges += 1;
    session.nudged.push(sig);
    session.last_nudge = request.now;
    session.pending.retain(|p| p != "search");
    session.pending.push("search".into());
    count(&request.data_dir, &["nudge_error"]);
    Ok(Some(text))
}

fn push(session: &mut Session, step: Step) {
    session.turn.push(step);
    let excess = session.turn.len().saturating_sub(MAX_STEPS);
    session.turn.drain(..excess);
}

/// Whether a shell command writes a file: a redirection to a file (not
/// `/dev/null`, not `2>&1`), or a command that edits in place.
pub fn writes(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        let target = if let Some(rest) = w.strip_prefix(">>").or_else(|| w.strip_prefix('>')) {
            if rest.starts_with('&') {
                continue;
            }
            if rest.is_empty() {
                words.get(i + 1).copied().unwrap_or_default()
            } else {
                rest
            }
        } else if let Some((_, rest)) = w.split_once('>') {
            // `x>file` (not `2>`, `&>`).
            if w.starts_with(|c: char| c.is_ascii_digit() || c == '&') || rest.starts_with('&') {
                continue;
            }
            rest
        } else {
            continue;
        };
        if !target.is_empty() && target != "/dev/null" {
            return true;
        }
    }
    let c = format!(" {command} ");
    [
        " sed -i",
        " perl -i",
        " perl -pi",
        " tee ",
        " patch ",
        " git apply",
        " git checkout -- ",
        " truncate ",
        " mv ",
        " cp ",
    ]
    .iter()
    .any(|p| c.contains(p))
}

/// The fix found in a turn (ADR 0056 §1.2): a signature that failed twice
/// or more (or a command that failed twice with it), then an edit (a file
/// tool, or a shell command that writes a file), then the same command,
/// or a build or test, passing. The edit and the pass can be one shell
/// call (`printf … > check.py && python3 check.py`).
pub fn fix_in(turn: &[Step]) -> Option<String> {
    for (i, step) in turn.iter().enumerate() {
        let Step::Shell {
            head,
            fail: Some(sig),
            ok: false,
            ..
        } = step
        else {
            continue;
        };
        let again = turn[i + 1..].iter().position(|s| {
            matches!(s, Step::Shell { fail: Some(s2), ok: false, head: h2, .. } if s2 == sig || h2 == head)
        });
        if again.is_none() {
            continue;
        }
        let edit = turn[i + 1..]
            .iter()
            .position(|s| *s == Step::Edit || matches!(s, Step::Shell { writes: true, .. }))
            .map(|j| i + 1 + j);
        let Some(edit) = edit else {
            continue;
        };
        // The edit's own call counts when it passed and ran the command.
        let from = if turn[edit] == Step::Edit {
            edit + 1
        } else {
            edit
        };
        let passed = turn[from..].iter().any(|s| {
            matches!(s, Step::Shell { ok: true, head: h2, check, cmd, .. }
                if h2 == head || *check || (!head.is_empty() && cmd.contains(head.as_str())))
        });
        if passed {
            return Some(sig.clone());
        }
    }
    None
}

/// The swarm post the turn relied on (ADR 0056 §1.3): read, then a build
/// or test passing, with no react after the read.
pub fn relied_in(turn: &[Step]) -> Option<(String, String)> {
    let (i, (id, title)) = turn.iter().enumerate().rev().find_map(|(i, s)| match s {
        Step::SwarmRead { id, title } => Some((i, (id.clone(), title.clone()))),
        _ => None,
    })?;
    let rest = &turn[i + 1..];
    let passed = rest.iter().any(|s| {
        matches!(
            s,
            Step::Shell {
                ok: true,
                check: true,
                ..
            }
        )
    });
    let reacted = rest.contains(&Step::React);
    (passed && !reacted).then_some((id, title))
}

fn stop(request: &Request, session: &mut Session) -> Option<String> {
    let turn = std::mem::take(&mut session.turn);
    // Claude Code, Codex and Copilot set it on a continuation the hook
    // asked for: never ask twice.
    if request.payload["stop_hook_active"].as_bool() == Some(true) || !request.fixes {
        return None;
    }
    if session.fix_nudges < FIX_NUDGES
        && let Some(sig) = fix_in(&turn)
    {
        session.fix_nudges += 1;
        session.pending.push("share".into());
        count(&request.data_dir, &["nudge_fix"]);
        let auto = crate::settings::Settings::load(&request.data_dir)
            .ok()
            .and_then(|s| s.share.auto)
            .unwrap_or(false);
        return Some(if auto {
            format!(
                "wats: you found a fix for \"{sig}\" after several tries. If it is general (not \
                 specific to this project), share it now with the wats `share_finding` tool: \
                 this node shares at once (share.auto).{NO_FALLBACK}"
            )
        } else {
            let tip =
                autoshare_tip(&request.data_dir, request.host, request.now).unwrap_or_default();
            format!(
                "wats: you found a fix for \"{sig}\" after several tries. If it is general (not \
                 specific to this project), ask your user whether to share it with the swarm, \
                 and if they agree, call the wats `share_finding` tool with approved_by_user \
                 true. It earns +2 when someone uses it, +3 if confirmed.{NO_FALLBACK}{tip}"
            )
        });
    }
    if let Some((id, title)) = relied_in(&turn) {
        session.pending.push("used".into());
        count(&request.data_dir, &["nudge_relied"]);
        let title = if title.is_empty() { id.clone() } else { title };
        return Some(format!(
            "wats: you relied on \"{title}\" ({id}) and your checks passed: mark it used with the \
             wats `reply` tool (kind react, data.used true), or accepted (data.accept true) if \
             it answered your own ask. If it was wrong, say why with data.value -1 and evidence."
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failing(command: &str, output: &str) -> Shell {
        Shell {
            command: command.into(),
            exit: Some(101),
            output: output.into(),
        }
    }

    #[test]
    fn each_host_payload_gives_the_shell_call() {
        // Claude Code, PostToolUseFailure.
        let claude = json!({"tool_name": "Bash", "tool_input": {"command": "cargo build"},
            "error": "Exit code 101\nerror[E0425]: cannot find value `x` in this scope"});
        let s = shell(&claude).unwrap();
        assert_eq!((s.command.as_str(), s.exit), ("cargo build", Some(101)));
        assert!(s.output.contains("E0425"));
        // Claude Code, PostToolUse (success).
        let ok = json!({"tool_name": "Bash", "tool_input": {"command": "cargo build"},
            "tool_response": {"stdout": "Finished", "stderr": ""}});
        assert_eq!(shell(&ok).unwrap().exit, None);
        // Vibe, post_tool.
        let vibe = json!({"tool_name": "bash", "tool_input": {"command": "pytest"},
            "tool_status": "success",
            "tool_output": {"command": "pytest", "exit_code": 1, "stdout": "1 failed", "stderr": ""}});
        assert_eq!(shell(&vibe).unwrap().exit, Some(1));
        // Copilot, postToolUse: success in name, exit 1 in its telemetry.
        let copilot = json!({"toolName": "bash", "toolArgs": {"command": "python3 check.py"},
            "toolResult": {"resultType": "success", "textResultForLlm": "[elided]",
                "toolTelemetry": {"metrics": {"exit_code": 1.0},
                    "properties": {"shell_error_category": "command_nonzero_exit"}}}});
        assert_eq!(shell(&copilot).unwrap().exit, Some(1));
        let done = json!({"toolName": "bash", "toolArgs": {"command": "python3 check.py"},
            "toolResult": {"resultType": "success",
                "textResultForLlm": "ok\n<shellId: 3 completed with exit code 0>"}});
        assert_eq!(shell(&done).unwrap().exit, Some(0));
        // Vibe 3: a namespaced tool, and the command behind a `cd`.
        let vibe3 = json!({"tool_name": "file_system.bash",
            "tool_input": {"timeout": 30, "command": "cd /tmp/p && python3 check.py"},
            "tool_status": "success",
            "tool_output": {"returncode": 1, "stderr": "ModuleNotFoundError: x", "stdout": ""}});
        let s = shell(&vibe3).unwrap();
        assert_eq!((s.command.as_str(), s.exit), ("python3 check.py", Some(1)));
        assert!(is_edit("file_system.search_replace"));
        // OpenClaw, through its plugin: `exec`, a result with content and
        // details.
        let openclaw = json!({"session_id": "o1", "tool_name": "exec",
            "tool_input": {"command": "python3 check.py"},
            "tool_response": {"content": [{"type": "text", "text": "ModuleNotFoundError: No module named 'x'"}],
                "details": {"exitCode": 1}}});
        let s = shell(&openclaw).unwrap();
        assert_eq!(s.exit, Some(1));
        assert!(s.output.contains("ModuleNotFoundError"));
        assert_eq!(
            render(Host::OpenClaw, Event::PostTool, None, "plain"),
            "plain"
        );
        // Codex, PostToolUse: output text only.
        let codex = json!({"tool_name": "shell", "tool_input": {"command": ["bash", "-lc", "npm test"]},
            "tool_response": "FAIL src/a.test.ts\nError: expected 2"});
        let s = shell(&codex).unwrap();
        assert_eq!(s.command, "bash -lc npm test");
        assert_eq!(s.exit, None);
        // Not a shell tool.
        assert!(shell(&json!({"tool_name": "Read"})).is_none());
    }

    #[test]
    fn routine_failures_stay_quiet() {
        let quiet = [
            Shell {
                command: "grep -r foo .".into(),
                exit: Some(1),
                output: String::new(),
            },
            Shell {
                command: "git log --oneline | grep -i frobnicate".into(),
                exit: Some(1),
                output: "warning: refname is ambiguous".into(),
            },
            Shell {
                command: "python3 check.py".into(),
                exit: Some(1),
                output: "\n".into(),
            },
            Shell {
                command: "foo".into(),
                exit: Some(127),
                output: "zsh: command not found: foo".into(),
            },
            Shell {
                command: "git commit -m x".into(),
                exit: Some(1),
                output: "nothing to commit, working tree clean".into(),
            },
            Shell {
                command: "cat ./notes/todo.txt".into(),
                exit: Some(1),
                output: "cat: ./notes/todo.txt: No such file or directory".into(),
            },
            Shell {
                command: "sleep 100".into(),
                exit: Some(130),
                output: String::new(),
            },
        ];
        for s in &quiet {
            assert!(routine(s), "{s:?}");
        }
        assert!(!routine(&failing(
            "cargo build",
            "error[E0425]: cannot find value"
        )));
    }

    #[test]
    fn a_signature_is_generic() {
        let s = failing(
            "cargo build",
            "   Compiling foo v0.1.0\nerror[E0425]: cannot find value `secret_x` in /home/vincentcarluer/proj/src/main.rs:12:5",
        );
        let private = vec![("the OS user name", "vincentcarluer".to_string())];
        assert_eq!(
            signature(&s, &private).as_deref(),
            Some("error[E0425]: cannot find value '…' in …/…")
        );
        // Python: the error, not the traceback's header.
        let py = failing(
            "python3 check.py",
            "Traceback (most recent call last):\n  File \"/home/x/check.py\", line 1\nModuleNotFoundError: No module named 'widgetlib'",
        );
        assert_eq!(
            signature(&py, &private).as_deref(),
            Some("ModuleNotFoundError: No module named '…'")
        );
        // An error quoting the project's code keeps none of it.
        let quoted = failing(
            "python3 x.py",
            "AssertionError: if entries.is_empty() { println!(x) }",
        );
        assert_eq!(
            signature(&quoted, &private),
            None,
            "nothing generic is left"
        );
        let mixed = failing(
            "cargo build",
            "error[E0599]: no method named `frob` found for Vec<u8> in scope",
        );
        assert_eq!(
            signature(&mixed, &private).as_deref(),
            Some("error[E0599]: no method named '…' found for … in scope")
        );
        // Inline scripts are told apart.
        assert_ne!(
            head("python3 - <<'EOF'\nprint(1)\nEOF"),
            head("python3 - <<'EOF'\nprint(2)\nEOF")
        );
        assert_eq!(head("cargo build --release"), "cargo build");
        // Nothing that says what failed: no signature.
        assert_eq!(signature(&failing("x", "all good"), &private), None);
        assert_eq!(
            signature(&failing("x", "error"), &private),
            None,
            "one word says nothing"
        );
    }

    fn sh(head: &str, sig: Option<&str>, ok: bool, check: bool) -> Step {
        Step::Shell {
            head: head.into(),
            sig: sig.map(str::to_string),
            ok,
            check,
            fail: if ok { None } else { sig.map(str::to_string) },
            writes: false,
            cmd: head.into(),
        }
    }

    #[test]
    fn a_fix_needs_two_failures_an_edit_and_a_pass() {
        let e = Some("error[E0425]: cannot find value '…'");
        let fixed = [
            sh("cargo build", e, false, true),
            sh("cargo build", e, false, true),
            Step::Edit,
            sh("cargo build", None, true, true),
        ];
        assert_eq!(fix_in(&fixed).as_deref(), e);
        // A flaky rerun: no edit.
        let flaky = [
            sh("cargo test", e, false, true),
            sh("cargo test", e, false, true),
            sh("cargo test", None, true, true),
        ];
        assert_eq!(fix_in(&flaky), None);
        // One failure only.
        let once = [
            sh("cargo build", e, false, true),
            Step::Edit,
            sh("cargo build", None, true, true),
        ];
        assert_eq!(fix_in(&once), None);
        // Seen live (Claude Code): the edit in the shell, in the same call
        // as the passing run.
        let p = Some("ModuleNotFoundError: No module named '…'");
        let in_shell = [
            sh("python3 check.py", p, false, true),
            sh("python3 check.py", p, false, true),
            sh("cat check.py", None, true, false),
            Step::Shell {
                head: "printf".into(),
                sig: None,
                ok: true,
                check: true,
                fail: None,
                writes: true,
                cmd: "printf 'print(\"ok\")\\n' > check.py && python3 check.py".into(),
            },
        ];
        assert_eq!(fix_in(&in_shell).as_deref(), p);
        assert!(writes("printf 'x' > check.py && python3 check.py"));
        assert!(writes("sed -i 's/a/b/' src/x.rs"));
        assert!(writes("echo x>>notes.txt"));
        assert!(!writes("cargo build 2>&1 | tail"));
        assert!(!writes("ls > /dev/null"));
        assert!(!writes("python3 check.py"));
        // Seen live (Copilot): the output elided, so no signature; the
        // failure still counts, by its command.
        let blind = |ok: bool| Step::Shell {
            head: "python3 check.py".into(),
            sig: None,
            ok,
            check: true,
            fail: (!ok).then(|| "python3 check.py".to_string()),
            writes: false,
            cmd: "python3 check.py".into(),
        };
        let elided = [blind(false), blind(false), Step::Edit, blind(true)];
        assert_eq!(fix_in(&elided).as_deref(), Some("python3 check.py"));
        // Fixed but nothing passed afterwards.
        let open = [
            sh("cargo build", e, false, true),
            sh("cargo build", e, false, true),
            Step::Edit,
        ];
        assert_eq!(fix_in(&open), None);
    }

    #[test]
    fn a_post_relied_on_is_one_read_then_a_passing_check() {
        let read = Step::SwarmRead {
            id: "abc".into(),
            title: "musl needs musl-tools".into(),
        };
        let turn = [
            read.clone(),
            Step::Edit,
            sh("cargo build", None, true, true),
        ];
        assert_eq!(relied_in(&turn).unwrap().0, "abc");
        let reacted = [
            read.clone(),
            sh("cargo build", None, true, true),
            Step::React,
        ];
        assert_eq!(relied_in(&reacted), None);
        let no_check = [read, sh("ls", None, true, false)];
        assert_eq!(relied_in(&no_check), None);
    }

    /// The autoshare tip: once a day, never in the week after turning
    /// autoshare off, with the host's command.
    #[test]
    fn the_autoshare_tip_is_rare() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let first = autoshare_tip(d, Host::Claude, 100_000).unwrap();
        assert!(
            first.contains("`/wats autoshare on`") && first.contains("Never turn it on yourself")
        );
        assert_eq!(
            autoshare_tip(d, Host::Claude, 100_000 + 3600),
            None,
            "once a day"
        );
        assert!(
            autoshare_tip(d, Host::Copilot, 100_000 + TIP_EVERY)
                .unwrap()
                .contains("`wats me autoshare on`")
        );
        autoshare_turned_off(d, 300_000);
        assert_eq!(
            autoshare_tip(d, Host::Claude, 300_000 + 2 * TIP_EVERY),
            None,
            "a week after off"
        );
        assert!(autoshare_tip(d, Host::Claude, 300_000 + TIP_QUIET_AFTER_OFF).is_some());
        assert_eq!(counters(d).get("hint_autoshare"), Some(&3));
    }

    #[test]
    fn nudges_take_each_hosts_form() {
        let claude: Value =
            serde_json::from_str(&render(Host::Claude, Event::PostTool, None, "x")).unwrap();
        assert_eq!(claude["hookSpecificOutput"]["additionalContext"], "x");
        assert_eq!(claude["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        // Seen live: Claude Code refuses an output naming another event.
        let failed: Value = serde_json::from_str(&render(
            Host::Claude,
            Event::PostTool,
            Some("PostToolUseFailure"),
            "x",
        ))
        .unwrap();
        assert_eq!(
            failed["hookSpecificOutput"]["hookEventName"],
            "PostToolUseFailure"
        );
        let stop: Value =
            serde_json::from_str(&render(Host::Codex, Event::Stop, None, "y")).unwrap();
        let copilot: Value =
            serde_json::from_str(&render(Host::Copilot, Event::PostTool, None, "c")).unwrap();
        assert_eq!(copilot, json!({"additionalContext": "c"}));
        assert_eq!(stop["decision"], "block");
        let vibe: Value =
            serde_json::from_str(&render(Host::Vibe, Event::PostTool, None, "z")).unwrap();
        assert_eq!(vibe["hook_specific_output"]["additional_context"], "z");
    }
}

//! The first run of `wats`, and `wats setup` with no tool: the node's main
//! choices, each with a default.
//!
//! A foreground node (`wats`, `wats node`) that finds no `config.toml` in
//! its data directory asks them on a terminal ([`setup`]), or writes the
//! defaults, commented, without one ([`write_default`]: daemons,
//! containers, services). A bare `wats` (no argument at all) on a terminal
//! runs the whole setup wizard instead (ADR 0027, in `main.rs`): these
//! choices ([`setup_for_wizard`]), then the MCP hosts found, then the
//! background or this terminal. `wats mcp` and the background node it starts
//! never do either: an MCP-only user keeps a data directory without
//! `config.toml`.
//!
//! The choices are existing keys only ([`crate::settings::KEYS`]): the
//! agent runtime among the agent CLIs found on `PATH`, `storage.max`, the
//! agent's run time and turn limits, its daily budget (how it is paid,
//! then dollars, minutes and LLM tokens per day: ADR 0034), its access
//! level, and `nets`. Right after the agent, "quick" keeps the
//! recommended (or current) settings and asks only the budget; "detailed"
//! asks each one (ADR 0027, 0.4.5).
//!
//! Each question carries the setup revision that added it
//! ([`SETUP_REVISION`], ADR 0027): `<data dir>/setup-revision` records the
//! revision the last setup completed, so that a release adding questions
//! asks only those ([`catch_up`]) instead of the whole setup again.

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::settings::{self, ConfigFile, FILE_NAME, RUNTIMES, Settings};
use crate::update::Install;

/// The agent CLIs looked for on `PATH`. Only [`RUNTIMES`] can run; any
/// other would be listed as an MCP host only.
pub const AGENT_CLIS: [&str; 7] = [
    "claude", "codex", "openclaw", "opencode", "vibe", "copilot", "agy",
];

/// Shown when the node runs no agent (startup, `status`). A future
/// policy: no quota is enforced today.
pub const NO_AGENT_NOTICE: &str = "This node runs no agent: nodes that don't contribute \
(answers, storage, uptime) will get a limited quota in a future version. `wats setup` \
enables one.";

/// Shown by setup when no supported agent CLI is on `PATH`.
pub const NO_AGENT_FOUND: &str = "No agent CLI found: install Claude Code \
(https://claude.com/claude-code), Codex (https://github.com/openai/codex), OpenCode \
(https://opencode.ai, for your own model or cloud credits) or Mistral Vibe \
(https://github.com/mistralai/mistral-vibe), then run `wats setup` again.";

/// Shown before the model question when OpenCode is the node's agent
/// (ADR 0031 §2).
pub const OPENCODE_MODEL_ABOUT: &str = "OpenCode runs with the models you set up in \
OpenCode (`opencode auth login`, your provider's environment). Its runs do not read \
~/.config/opencode: a custom provider goes in the agent's own OpenCode directory, which \
`wats setup opencode --agent` creates.";

/// The model question for OpenCode: `provider/model`, empty for
/// OpenCode's own choice.
pub const OPENCODE_MODEL_QUESTION: &str =
    "The agent's model, as provider/model, e.g. azure/gpt-5 (empty: OpenCode's default)";

/// Shown by setup when OpenClaw is on `PATH` and not the node's agent: its
/// agents can use the wats tools.
pub const OPENCLAW_MCP_HINT: &str = "OpenClaw found: to reach the swarm from your OpenClaw \
agents, register wats as its MCP server: wats setup openclaw --mcp";

/// Shown by setup when Mistral Vibe is on `PATH` and not the node's agent:
/// an MCP host too.
pub const VIBE_MCP_HINT: &str = "Mistral Vibe found: to reach the swarm from your Vibe \
sessions, add wats as its MCP server: wats setup vibe --mcp";

/// The access question for Mistral Vibe (ADR 0033): no `workspace`.
pub const VIBE_ACCESS_QUESTION: &str = "What the agent may use besides the wats tools: \
swarm-only, or inherit (your whole Vibe environment; Vibe has no workspace level)";

/// Shown by setup when OpenClaw is the node's agent: the gateway side is
/// set up apart (tokens, MCP entries, the config block to paste).
pub const OPENCLAW_AGENT_NEXT: &str = "`wats` now forwards the agent's runs to OpenClaw's \
gateway. Finish with: wats setup openclaw --agent (the hook token, the restricted `wats` \
OpenClaw agent and its MCP entry)";

/// Keys not asked for OpenClaw: it counts its own turns (ADR 0017 §2),
/// and its access level goes with its allow list and review (`wats setup
/// openclaw --agent` checks them together). Its budget is asked, minutes
/// only: it reports no cost nor LLM tokens.
const NOT_FOR_OPENCLAW: [&str; 2] = ["agent.max_turns", "agent.access"];

/// The revision of setup's questions (ADR 0027). A release bumps it only
/// when it adds a question, which then carries the new value as its
/// `since`: `wats` on a terminal asks the questions newer than the
/// revision a data directory last completed ([`catch_up`]), and says so in
/// one log line elsewhere. Not tied to the version number.
pub const SETUP_REVISION: u32 = 2;

/// The revision of a data directory set up before revisions were recorded
/// (wats 0.4.2 and older), or with `wats setup <tool>` only: a config.toml
/// without [`REVISION_FILE`].
pub const BASELINE_REVISION: u32 = 1;

/// The file, in the data directory, that holds the revision the last setup
/// completed. Not a config.toml key: an older wats refuses unknown keys,
/// and `wats upgrade --rollback` must keep working.
pub const REVISION_FILE: &str = "setup-revision";

/// The revision that added the agent runtime question.
const RUNTIME_SINCE: u32 = 1;

/// The revision that added the desktop notification question.
const DESKTOP_SINCE: u32 = 1;

/// The revision that added the security updates question (ADR 0030):
/// part of revision 1, which no release had shipped yet when it was added.
const SECURITY_AUTO_SINCE: u32 = 1;

/// The revision that added the budget question (how the agent is paid,
/// then its daily budget: ADR 0034), in 0.4.5. It replaces the two USD
/// questions of revision 1.
const BUDGET_SINCE: u32 = 2;

/// Shown with the budget question (ADR 0034): what to set it to.
pub const BUDGET_GUIDE: &str = "The agent's budget: set it to what you're willing to give \
the swarm from your subscription, machine or API spend.";

/// The budget question (ADR 0034).
pub const PAID_QUESTION: &str = "How is this agent paid?";

/// The mode question, right after the agent (ADR 0027, 0.4.5).
pub const MODE_QUESTION: &str = "Recommended settings, or choose each one?";

/// Shown before the security updates question (ADR 0030).
pub const SECURITY_AUTO_ABOUT: &str = "Security updates: the node can install a release \
marked security by itself, signature checked, once it is update.security_delay old (a day, \
unless you set another delay). It never restarts itself: restart wats to use it. Other \
releases are only announced (update.auto installs them).";

/// The security updates question (ADR 0030); Yes is its default.
pub const SECURITY_AUTO_QUESTION: &str =
    "Install security updates automatically? (update.security_auto)";

/// Shown instead of the security updates question for a Homebrew install,
/// which the node never upgrades.
pub const SECURITY_AUTO_HOMEBREW: &str = "Security updates: Homebrew installed this wats, so \
`brew upgrade wats` installs its updates; wats tells you when a security release is out.";

/// Shown instead of the security updates question on a platform with no
/// prebuilt release, which the node never upgrades.
pub const SECURITY_AUTO_SOURCE: &str = "Security updates: there is no prebuilt release for \
this platform, so the node cannot install them; build new releases from source. wats tells \
you when a security release is out.";

/// Shown instead of the security updates question when stdin is not a
/// terminal: turning on auto-install takes a person's answer.
pub const SECURITY_AUTO_NO_TERMINAL: &str = "Security updates: not asked without a terminal, \
so update.security_auto stays as it is (off unless set). Run `wats setup` on a terminal to \
choose.";

/// The `update.security_delay` a Yes sets when none is set.
pub const SECURITY_DELAY: &str = "1d";

/// The file, in the data directory, of the setup answers kept outside
/// config.toml (ADR 0030): `security_auto=no` remembers a No, which leaves
/// the key unset so that an older wats still loads config.toml.
/// `name=value` lines.
pub const ANSWERS_FILE: &str = "setup-answers";

/// One question after the agent runtime.
#[derive(Debug, Clone, Copy)]
struct Question {
    key: &'static str,
    label: &'static str,
    /// Only asked when the node runs an agent.
    agent_only: bool,
    /// The setup revision that added it.
    since: u32,
    /// The budget question ([`ask_budget`]): how the agent is paid, then
    /// its daily budget, several keys. Asked in quick mode too.
    budget: bool,
}

const fn question(key: &'static str, label: &'static str, agent_only: bool) -> Question {
    Question {
        key,
        label,
        agent_only,
        since: 1,
        budget: false,
    }
}

/// The questions after the agent runtime, in order: storage, the agent's
/// limits, budget and access (only asked when it runs an agent), networks.
const QUESTIONS: [Question; 6] = [
    question("storage.max", "Disk budget of the data directory", false),
    question("agent.timeout", "Time limit of one agent run", true),
    question("agent.max_turns", "Turns per agent run", true),
    Question {
        since: BUDGET_SINCE,
        budget: true,
        ..question("agent.max_usd_per_day", "The agent's daily budget", true)
    },
    question(
        "agent.access",
        "What the agent may use besides the wats tools: swarm-only, workspace (its own \
         directory) or inherit (your whole environment)",
        true,
    ),
    question("nets", "Networks to join, comma-separated", false),
];

/// The keys of an agent's table in a new config.toml ([`render`]): set
/// when chosen and not the default, else commented with the default.
const AGENT_TABLE_KEYS: [&str; 6] = [
    "agent.timeout",
    "agent.max_turns",
    "agent.max_usd_per_day",
    "agent.max_minutes_per_day",
    "agent.max_tokens_per_day",
    "agent.access",
];

/// The revision the last setup of `data_dir` completed: `None` without a
/// config.toml (never set up, or its config.toml deleted to start over),
/// else its [`REVISION_FILE`], else [`BASELINE_REVISION`] (also for an
/// unreadable file).
pub fn stored_revision(data_dir: &Path) -> Option<u32> {
    if !data_dir.join(FILE_NAME).exists() {
        return None;
    }
    let recorded = std::fs::read_to_string(data_dir.join(REVISION_FILE)).ok();
    Some(
        recorded
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(BASELINE_REVISION),
    )
}

/// Record that setup completed [`SETUP_REVISION`] in `data_dir`.
pub fn record_revision(data_dir: &Path) -> Result<()> {
    let path = data_dir.join(REVISION_FILE);
    std::fs::write(&path, format!("{SETUP_REVISION}\n"))
        .with_context(|| format!("write {}", path.display()))
}

/// How many questions are newer than `revision`, whatever the answers
/// (the agent's questions count even when it runs none).
pub fn new_questions(revision: u32) -> usize {
    [RUNTIME_SINCE, DESKTOP_SINCE, SECURITY_AUTO_SINCE]
        .into_iter()
        .chain(QUESTIONS.iter().map(|q| q.since))
        .filter(|since| *since > revision)
        .count()
}

/// How many of the questions newer than `revision` a catch-up of these
/// settings would ask: the agent's only when it runs one. 0: nothing to
/// ask (the revision can be recorded as is).
pub fn pending_questions(settings: &Settings, revision: u32) -> usize {
    let agent = settings.primary_agent().is_some();
    [RUNTIME_SINCE, DESKTOP_SINCE, SECURITY_AUTO_SINCE]
        .into_iter()
        .filter(|since| *since > revision)
        .count()
        + QUESTIONS
            .iter()
            .filter(|q| q.since > revision && (agent || !q.agent_only))
            .count()
}

/// An agent CLI found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: &'static str,
    pub path: PathBuf,
}

impl Found {
    /// The runtime it is (`agy` is Antigravity CLI's command).
    pub fn runtime(&self) -> &'static str {
        crate::settings::runtime_of_command(self.name)
    }

    /// Whether wats can run it (an agent runtime).
    pub fn supported(&self) -> bool {
        RUNTIMES.contains(&self.runtime())
    }
}

/// The [`AGENT_CLIS`] found in the directories of `path` (a `PATH` value),
/// in the order of [`AGENT_CLIS`].
pub fn detect_agents(path: Option<&OsStr>) -> Vec<Found> {
    let dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    AGENT_CLIS
        .iter()
        .filter_map(|&name| {
            dirs.iter()
                .flat_map(|dir| candidates(dir, name))
                .find(|p| is_executable(p))
                .map(|path| Found { name, path })
        })
        .collect()
}

fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|ext| dir.join(format!("{name}.{ext}")))
            .collect()
    } else {
        vec![dir.join(name)]
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// What setup chose. `values` holds the answered keys only, as typed (e.g.
/// `10G`, `public,acme`). `keep_runtime`: the runtime was not asked (a
/// [`catch_up`]): the agents stay as they are, `runtime` is the current
/// one's. `quick`: the quick mode, which kept the recommended (or
/// current) settings without asking them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choices {
    pub runtime: Option<String>,
    pub values: Vec<(&'static str, String)>,
    pub keep_runtime: bool,
    pub quick: bool,
    /// The budget question was answered (by a person), for this agent.
    pub budget_for: Option<String>,
}

/// The value of `key` in `settings`, as `wats config set` takes it; an
/// agent's key from `agent`, the table setup asks about.
fn current(settings: &Settings, agent: &settings::AgentSettings, key: &str) -> Option<String> {
    match key {
        "storage.max" => settings.storage.max.clone(),
        "agent.timeout" => agent.timeout.clone(),
        "agent.max_turns" => agent.max_turns.map(|n| n.to_string()),
        "agent.max_usd_per_day" => agent.max_usd_per_day.map(|n| n.to_string()),
        "agent.max_usd_per_month" => agent.max_usd_per_month.map(|n| n.to_string()),
        "agent.max_minutes_per_day" => agent.max_minutes_per_day.map(|n| n.to_string()),
        "agent.max_tokens_per_day" => agent.max_tokens_per_day.map(|n| n.to_string()),
        "agent.access" => agent.access.clone(),
        "agent.model" => agent.model.clone(),
        "nets" => (!settings.nets.is_empty()).then(|| settings.nets.join(",")),
        _ => None,
    }
}

/// Whether `raw` is the key's default value.
fn is_default(key: &str, raw: &str) -> bool {
    let Ok(default) = settings::find_key(key) else {
        return false;
    };
    match (
        settings::check_value(key, raw),
        settings::check_value(key, default.default),
    ) {
        (Ok(a), Ok(b)) => a.to_string().trim() == b.to_string().trim(),
        _ => false,
    }
}

/// One line of input: `None` at the end of the input.
fn read_answer(input: &mut impl BufRead, out: &mut impl Write) -> Result<Option<String>> {
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(out)?;
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

/// Ask the agent runtime among the CLIs found. `None`: no agent.
fn ask_runtime(
    input: &mut impl BufRead,
    out: &mut impl Write,
    found: &[Found],
    current: Option<&str>,
) -> Result<Option<String>> {
    let supported: Vec<&Found> = found.iter().filter(|f| f.supported()).collect();
    if !found.is_empty() {
        writeln!(out, "Agent CLIs found on your PATH:")?;
        let mut n = 0;
        for f in found {
            if f.supported() {
                n += 1;
                writeln!(out, "  {n}) {:<9} {}", f.runtime(), f.path.display())?;
            } else {
                writeln!(
                    out,
                    "  -  {:<9} {} (agent: not supported yet; MCP: wats setup {} --mcp)",
                    f.name,
                    f.path.display(),
                    f.name
                )?;
            }
        }
    }
    if supported.is_empty() {
        writeln!(out, "{NO_AGENT_FOUND}")?;
        return Ok(current.map(str::to_string));
    }
    writeln!(out, "  0) none: the node runs no agent")?;
    let default = current
        .map(str::to_string)
        .unwrap_or_else(|| supported[0].runtime().to_string());
    loop {
        write!(out, "Agent the node runs (agent.runtime) [{default}]: ")?;
        let Some(answer) = read_answer(input, out)? else {
            return Ok(Some(default).filter(|r| r != "none"));
        };
        let answer = if answer.is_empty() {
            default.clone()
        } else {
            answer.to_lowercase()
        };
        let pick = match answer.parse::<usize>() {
            Ok(0) => Some("none"),
            Ok(i) => supported.get(i - 1).map(|f| f.runtime()),
            Err(_) if answer == "none" => Some("none"),
            // A custom CLI needs its command line: `wats setup custom`.
            Err(_) => RUNTIMES
                .iter()
                .copied()
                .find(|r| *r == answer && *r != crate::custom::RUNTIME),
        };
        match pick {
            Some("none") => return Ok(None),
            Some(runtime) => return Ok(Some(runtime.to_string())),
            None => writeln!(
                out,
                "  answer a number above, {} or none",
                RUNTIMES.join(", ")
            )?,
        }
    }
}

/// Ask one key, checked like `wats config set`; Enter (or the end of the
/// input) keeps `default`.
fn ask(
    input: &mut impl BufRead,
    out: &mut impl Write,
    key: &str,
    label: &str,
    default: &str,
) -> Result<String> {
    loop {
        write!(out, "{label} ({key}) [{default}]: ")?;
        let answer = match read_answer(input, out)? {
            Some(answer) if !answer.is_empty() => answer,
            _ => return Ok(default.to_string()),
        };
        match settings::check_value(key, &answer) {
            Ok(_) => return Ok(answer),
            Err(e) => writeln!(out, "  {e:#}")?,
        }
    }
}

/// The access level of a Vibe agent: swarm-only (the default, or the
/// current level) or inherit, which is followed by its warning.
fn ask_vibe_access(
    input: &mut impl BufRead,
    out: &mut impl Write,
    settings: &Settings,
    table: &settings::AgentSettings,
) -> Result<String> {
    let key = "agent.access";
    let default = current(settings, table, key)
        .filter(|v| crate::vibe::access(Some(v)).is_ok())
        .unwrap_or_else(|| "swarm-only".to_string());
    let answer = loop {
        write!(out, "{VIBE_ACCESS_QUESTION} ({key}) [{default}]: ")?;
        let answer = match read_answer(input, out)? {
            Some(answer) if !answer.is_empty() => answer,
            _ => break default.clone(),
        };
        match settings::check_value(key, &answer)
            .and_then(|_| crate::vibe::access(Some(&answer)).map(|_| ()))
        {
            Ok(()) => break answer,
            Err(e) => writeln!(out, "  {e:#}")?,
        }
    };
    if crate::vibe::access(Some(&answer))? == crate::access::Access::Inherit {
        writeln!(out, "{}", crate::vibe::INHERIT_WARNING)?;
    }
    Ok(answer)
}

/// A yes/no question; Enter (or the end of the input) keeps `default`.
pub fn ask_yes_no(
    input: &mut impl BufRead,
    out: &mut impl Write,
    question: &str,
    default: bool,
) -> Result<bool> {
    loop {
        write!(
            out,
            "{question} [{}]: ",
            if default { "Y/n" } else { "y/N" }
        )?;
        let answer = match read_answer(input, out)? {
            Some(answer) if !answer.is_empty() => answer.to_lowercase(),
            _ => return Ok(default),
        };
        match answer.as_str() {
            "y" | "yes" | "true" | "on" | "1" => return Ok(true),
            "n" | "no" | "false" | "off" | "0" => return Ok(false),
            _ => writeln!(out, "  answer yes or no")?,
        }
    }
}

/// Step 2 of the wizard (ADR 0027, 0.4.5): set up wats in all the agent
/// CLIs found at once, one by one, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosts {
    All,
    OneByOne,
    None,
}

/// "Set up wats in all the agent CLIs found (MCP + session hook)? [Y =
/// all / d = one by one / n = none]"; Enter is all. `labels`: what each
/// one would do, shown in the question.
pub fn ask_hosts(
    input: &mut impl BufRead,
    out: &mut impl Write,
    labels: &[String],
) -> Result<Hosts> {
    loop {
        write!(
            out,
            "Set up wats in all the agent CLIs found ({}): MCP server + session hook? [Y = all / \
             d = one by one / n = none]: ",
            labels.join(", ")
        )?;
        let answer = match read_answer(input, out)? {
            Some(answer) => answer.to_lowercase(),
            None => return Ok(Hosts::All),
        };
        match answer.as_str() {
            "" | "y" | "yes" | "a" | "all" => return Ok(Hosts::All),
            "d" | "detailed" | "one by one" | "each" => return Ok(Hosts::OneByOne),
            "n" | "no" | "none" => return Ok(Hosts::None),
            _ => writeln!(out, "  answer y (all), d (one by one) or n (none)")?,
        }
    }
}

/// Step 3 of the wizard (ADR 0027, 0.4.5): where the node runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    /// In the background (`wats --detach`), the recommended choice.
    Background,
    /// In this terminal, until Ctrl-C (a bare `wats` only).
    Here,
    /// Not started now (`wats setup`).
    NotNow,
}

/// "Run the node in the background? [Y = background / d = choose]": Enter
/// is the background, recommended; `d` lists the choices: the
/// background, this terminal (`here`: a bare `wats`, which runs the node
/// next), or not now.
pub fn ask_where(input: &mut impl BufRead, out: &mut impl Write, here: bool) -> Result<Where> {
    let other = if here { Where::Here } else { Where::NotNow };
    loop {
        write!(
            out,
            "Run the node in the background (`wats --detach`: it keeps running when this \
             terminal closes; `wats attach` follows it, `wats stop` stops it)? [Y = background, \
             recommended / d = choose]: "
        )?;
        let answer = match read_answer(input, out)? {
            Some(answer) => answer.to_lowercase(),
            None => return Ok(Where::Background),
        };
        match answer.as_str() {
            "" | "y" | "yes" | "b" | "background" => return Ok(Where::Background),
            "n" | "no" => return Ok(other),
            "d" | "detailed" | "c" | "choose" => break,
            _ => writeln!(out, "  answer y (the background) or d (choose)")?,
        }
    }
    let choices: &[(Where, &str)] = if here {
        &[
            (Where::Background, "in the background (`wats --detach`)"),
            (Where::Here, "here, in this terminal, until Ctrl-C"),
            (Where::NotNow, "not now (later: wats --detach)"),
        ]
    } else {
        &[
            (Where::Background, "in the background now (`wats --detach`)"),
            (Where::NotNow, "not now (later: wats --detach)"),
        ]
    };
    for (n, (_, label)) in choices.iter().enumerate() {
        writeln!(out, "  {}) {label}", n + 1)?;
    }
    loop {
        write!(out, "Where the node runs [1]: ")?;
        let answer = read_answer(input, out)?.unwrap_or_default();
        if answer.is_empty() {
            return Ok(Where::Background);
        }
        match answer
            .parse::<usize>()
            .ok()
            .and_then(|n| choices.get(n.wrapping_sub(1)))
        {
            Some((choice, _)) => return Ok(*choice),
            None => writeln!(out, "  answer 1 to {}", choices.len())?,
        }
    }
}

/// Ask the main choices, with the current settings (or the defaults) as
/// defaults. `found`: [`detect_agents`].
pub fn prompt(
    input: &mut impl BufRead,
    out: &mut impl Write,
    found: &[Found],
    settings: &Settings,
) -> Result<Choices> {
    writeln!(out, "{SETUP_HEADER}")?;
    prompt_after(input, out, found, settings, &QUESTIONS, 0, &Ctx::person())
}

/// How setup asks (ADR 0027, ADR 0034).
#[derive(Debug, Clone)]
struct Ctx {
    /// The whole setup, which asks quick or detailed first; a catch-up
    /// asks its few questions.
    full: bool,
    /// A person answers (stdin a terminal): only then are the budget and
    /// the old defaults asked; without one they stay as they are.
    person: bool,
    /// The agents whose USD budget of 0 from before 0.4.5 is not
    /// confirmed, by name (day, month).
    zeros: Vec<(String, (bool, bool))>,
    /// The agents whose budget the budget question already confirmed (ADR
    /// 0034 §5): a catch-up does not ask it again.
    confirmed: Vec<String>,
    /// The API key variable set for a runtime, if any: the budget
    /// question's default then is "API key".
    api_key: fn(&str) -> Option<&'static str>,
}

impl Ctx {
    /// A person, the whole setup, no API key looked up.
    fn person() -> Ctx {
        Ctx {
            full: true,
            person: true,
            zeros: Vec::new(),
            confirmed: Vec::new(),
            api_key: |_| None,
        }
    }
}

/// The API key variable of `runtime` set in this environment: Claude
/// Code's `ANTHROPIC_API_KEY`, Codex's `OPENAI_API_KEY`, Mistral Vibe's
/// `MISTRAL_API_KEY` (OpenCode reaches many providers: none).
fn env_api_key(runtime: &str) -> Option<&'static str> {
    let var = match runtime {
        "claude" => "ANTHROPIC_API_KEY",
        "codex" => "OPENAI_API_KEY",
        "vibe" => "MISTRAL_API_KEY",
        _ => return None,
    };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(|_| var)
}

/// The first line of `wats setup` without its screens.
const SETUP_HEADER: &str = "wats setup: the node's main choices. Enter accepts the [default].";

/// Ask those of `questions` newer than `revision` (0: all of them), after
/// the agent runtime when it is newer too, as `ctx` says.
fn prompt_after(
    input: &mut impl BufRead,
    out: &mut impl Write,
    found: &[Found],
    settings: &Settings,
    questions: &[Question],
    revision: u32,
    ctx: &Ctx,
) -> Result<Choices> {
    let primary = settings
        .primary_agent()
        .and_then(|(_, table)| table.runtime.clone());
    let keep_runtime = RUNTIME_SINCE <= revision;
    let runtime = if keep_runtime {
        primary
    } else {
        ask_runtime(input, out, found, primary.as_deref())?
    };
    let openclaw = runtime.as_deref() == Some("openclaw");
    if openclaw && !keep_runtime {
        writeln!(out, "{}", crate::openclaw::SPEND_CAPS)?;
    }
    // The agent asked about, and its table: the primary agent's when it
    // keeps its runtime; another runtime is the agent named after it, from
    // its own table if there is one, else the defaults.
    let fresh = settings::AgentSettings::default();
    let (target, table) = match settings.primary_agent() {
        Some((name, t)) if t.runtime == runtime => (Some(name), t),
        Some(_) => (
            runtime.clone(),
            runtime
                .as_ref()
                .and_then(|r| settings.agents.get(r))
                .unwrap_or(&fresh),
        ),
        None => (runtime.clone(), &settings.agent),
    };
    let same_agent = settings
        .primary_agent()
        .is_some_and(|(_, t)| t.runtime == runtime);
    let mut values = Vec::new();
    // OpenCode reaches many providers: its model is asked when it is
    // chosen (ADR 0031 §2), in quick mode too (it goes with the agent's
    // choice). Not a revision question: nothing is asked of existing
    // setups.
    if runtime.as_deref() == Some("opencode") && !keep_runtime {
        writeln!(out, "{OPENCODE_MODEL_ABOUT}")?;
        let default = current(settings, table, "agent.model").unwrap_or_default();
        let model = loop {
            write!(out, "{OPENCODE_MODEL_QUESTION} (agent.model) [{default}]: ")?;
            let model = match read_answer(input, out)? {
                Some(answer) if !answer.is_empty() => answer,
                _ => break default.clone(),
            };
            match crate::opencode::check_model(&model) {
                Ok(()) => break model,
                Err(e) => writeln!(out, "  {e:#}")?,
            }
        };
        if !model.is_empty() {
            values.push(("agent.model", model));
        }
    }
    // Quick or detailed: asked with the whole setup only (a catch-up asks
    // its few new questions).
    let quick = ctx.full && ask_mode(input, out, settings, table, runtime.as_deref())?;
    // Values of the old defaults would pin them (ADR 0027, 0.4.5): offered
    // to drop, in both modes, for the agent asked about; asked of a person
    // only, never dropped without an answer.
    let mut dropped = Vec::new();
    if runtime.is_some() && same_agent && ctx.person {
        let old = old_defaults_of(table);
        if !old.is_empty() {
            let question = format!(
                "config.toml sets {}: the defaults of wats 0.4.4 and older, now {}. Drop {}, \
                 to use the new defaults?",
                old.iter()
                    .map(|o| format!("{} = {}", o.key, o.value))
                    .collect::<Vec<_>>()
                    .join(" and "),
                old.iter().map(|o| o.now).collect::<Vec<_>>().join(" and "),
                if old.len() == 1 { "it" } else { "them" },
            );
            if ask_yes_no(input, out, &question, true)? {
                for o in &old {
                    // The default: `apply` unsets the key.
                    values.push((o.key, o.now.to_string()));
                    dropped.push(o.key);
                }
            }
        }
    }
    let vibe = runtime.as_deref() == Some("vibe");
    let mut budget_for = None;
    for q in questions {
        if vibe && q.key == "agent.access" && q.since > revision {
            // Quick: unset is swarm-only for Vibe (it refuses workspace).
            if !quick {
                values.push((q.key, ask_vibe_access(input, out, settings, table)?));
            }
            continue;
        }
        if q.since <= revision
            || (q.agent_only && runtime.is_none())
            || (openclaw && NOT_FOR_OPENCLAW.contains(&q.key))
            || (q.key == "agent.max_turns" && !has_turn_limit(runtime.as_deref()))
        {
            continue;
        }
        if q.budget {
            let runtime = runtime.as_deref().unwrap_or("claude");
            if !ctx.full && target.as_ref().is_some_and(|t| ctx.confirmed.contains(t)) {
                // Already confirmed: a catch-up does not ask it again.
                budget_for = target.clone();
                continue;
            }
            if !ctx.person {
                writeln!(out, "{BUDGET_NOT_ASKED}")?;
                continue;
            }
            let zero = target
                .as_ref()
                .and_then(|name| ctx.zeros.iter().find(|(n, _)| n == name))
                .map_or((false, false), |(_, z)| *z);
            let api_key = (ctx.api_key)(runtime);
            if let Some(budget) = ask_budget(input, out, table, runtime, zero, "agent", api_key)? {
                values.extend(budget);
                budget_for = target.clone();
            }
            continue;
        }
        if quick {
            continue;
        }
        let default = current(settings, table, q.key)
            .filter(|_| !dropped.contains(&q.key))
            .unwrap_or_else(|| {
                settings::find_key(q.key)
                    .map_or("", |k| k.default)
                    .to_string()
            });
        let answer = ask(input, out, q.key, q.label, &default)?;
        values.retain(|(k, _)| *k != q.key);
        values.push((q.key, answer));
    }
    Ok(Choices {
        runtime,
        values,
        keep_runtime,
        quick,
        budget_for,
    })
}

/// Said instead of the budget question without a terminal: the budget, and
/// the pause of a USD budget of 0 from before 0.4.5, need a person.
pub const BUDGET_NOT_ASKED: &str = "Budget: not asked without a terminal, so the agent's \
budget stays as it is (and a USD budget of 0 from before 0.4.5 stays a pause). Run `wats \
setup` on a terminal to set it.";

/// The quick or detailed question (ADR 0027, 0.4.5): Enter is quick.
/// Returns whether quick.
fn ask_mode(
    input: &mut impl BufRead,
    out: &mut impl Write,
    settings: &Settings,
    table: &settings::AgentSettings,
    runtime: Option<&str>,
) -> Result<bool> {
    let value = |key: &str| {
        current(settings, table, key).unwrap_or_else(|| {
            settings::find_key(key)
                .map_or("", |k| k.default)
                .to_string()
        })
    };
    let mut kept = vec![
        format!("disk {}", value("storage.max")),
        format!("networks {}", value("nets")),
    ];
    if runtime.is_some() {
        kept.push(format!(
            "agent runs of at most {}{}",
            value("agent.timeout"),
            if has_turn_limit(runtime) {
                format!(" and {} turns", value("agent.max_turns"))
            } else {
                String::new()
            }
        ));
        if runtime == Some("vibe") {
            // Vibe refuses workspace: unset is swarm-only.
            let access = current(settings, table, "agent.access")
                .filter(|v| crate::vibe::access(Some(v)).is_ok())
                .unwrap_or_else(|| "swarm-only".to_string());
            kept.push(format!("{access} access"));
        } else if runtime != Some("openclaw") {
            kept.push(format!("{} access", value("agent.access")));
        }
    }
    let which = if settings.storage.max.is_some()
        || !settings.nets.is_empty()
        || settings.primary_agent().is_some()
    {
        "your current settings"
    } else {
        "the recommended settings"
    };
    writeln!(
        out,
        "Quick keeps {which} ({}, desktop notifications and security updates as \
         recommended){}. Detailed asks each one, with its [default].",
        kept.join(", "),
        if runtime.is_some() {
            " and asks only the agent's budget"
        } else {
            ""
        }
    )?;
    loop {
        write!(out, "{MODE_QUESTION} [quick/detailed, Enter: quick]: ")?;
        let answer = match read_answer(input, out)? {
            Some(answer) => answer.to_lowercase(),
            None => return Ok(true),
        };
        match answer.as_str() {
            "" | "q" | "quick" | "r" | "recommended" | "y" | "yes" => return Ok(true),
            "d" | "detailed" | "c" | "choose" => return Ok(false),
            _ => writeln!(out, "  answer quick or detailed (q or d)")?,
        }
    }
}

/// How an agent's model is paid (ADR 0034): it picks the budget's metric
/// and its defaults ([`preset`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paid {
    /// Claude Pro/Max, ChatGPT…: a flat fee, its use capped by the
    /// provider. A cost estimate means nothing there: time is the measure.
    Subscription,
    /// An API key, paid per use: dollars (LLM tokens for Codex, which
    /// reports no cost).
    ApiKey,
    /// A self-hosted or free model: the machine's time.
    SelfHosted,
}

/// A daily budget, as the wizard shows and writes it: 0 is no limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Daily {
    pub usd: f64,
    pub minutes: u64,
    pub tokens: u64,
}

/// The daily budget offered for `paid` and `runtime` (ADR 0034): a
/// subscription, 120 minutes; an API key, $5 where the runtime reports a
/// cost, else 5M LLM tokens where it reports them (Codex; cached input
/// counts); self-hosted, 240 minutes of the machine. OpenClaw: no limit
/// (its spend caps are its own).
pub fn preset(paid: Paid, runtime: &str) -> Daily {
    let daily = |usd, minutes, tokens| Daily {
        usd,
        minutes,
        tokens,
    };
    use crate::budget::{reports_cost, reports_tokens};
    match paid {
        // Its time only: OpenClaw reports no cost nor tokens.
        _ if runtime == "openclaw" => daily(0.0, crate::budget::DEFAULT_MINUTES_PER_DAY, 0),
        Paid::Subscription => daily(0.0, 120, 0),
        // Pay per use: dollars where the runtime reports a cost, else LLM
        // tokens where it reports them (Codex), else its time.
        Paid::ApiKey if reports_cost(runtime) => daily(5.0, 0, 0),
        Paid::ApiKey if reports_tokens(runtime) => daily(0.0, 0, 5_000_000),
        Paid::ApiKey => daily(0.0, 120, 0),
        Paid::SelfHosted => daily(0.0, 240, 0),
    }
}

/// `120 min, no USD limit, no LLM token limit`: the metrics `runtime` is
/// held to.
pub fn describe_daily(daily: &Daily, runtime: &str) -> String {
    let mut parts = vec![match daily.minutes {
        0 => "no time limit".to_string(),
        n => format!("{n} min"),
    }];
    if crate::budget::reports_cost(runtime) {
        parts.push(match daily.usd {
            x if x <= 0.0 => "no USD limit".to_string(),
            x if x.fract() == 0.0 => format!("${x:.0}"),
            x => format!("${x:.2}"),
        });
    }
    if crate::budget::reports_tokens(runtime) {
        parts.push(match daily.tokens {
            0 => "no LLM token limit".to_string(),
            n => format!("{} LLM tokens", crate::budget::tokens(n)),
        });
    }
    parts.join(", ")
}

/// Change `daily` with the user's line: `name=value` items, separated by
/// spaces or commas, any order, the others kept: `min=90` (or `2h`),
/// `usd=3` (or `$3`), `tokens=2M` (`k` and `M` suffixes); 0 or `none` is
/// no limit. Only the metrics `runtime` is held to.
pub fn parse_daily(line: &str, mut daily: Daily, runtime: &str) -> Result<Daily, String> {
    let none = |v: &str| matches!(v, "0" | "none" | "no" | "off" | "unlimited");
    for item in line
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|i| !i.is_empty())
    {
        let Some((name, value)) = item.split_once('=') else {
            return Err(format!(
                "{item:?}: write name=value, e.g. min=90 usd=3 tokens=2M (0: no limit)"
            ));
        };
        let value = value.trim().to_lowercase();
        match name.trim().to_lowercase().as_str() {
            "min" | "mins" | "minutes" | "time" => {
                daily.minutes = if none(&value) {
                    0
                } else if let Some(h) = value.strip_suffix('h') {
                    h.parse::<u64>().map_err(|_| {
                        format!("min={value}: a number of minutes, or of hours with h")
                    })? * 60
                } else {
                    value
                        .trim_end_matches("min")
                        .trim_end_matches('m')
                        .parse()
                        .map_err(|_| format!("min={value}: a whole number of minutes"))?
                }
            }
            "usd" | "$" | "dollars" => {
                if !crate::budget::reports_cost(runtime) {
                    return Err(format!(
                        "usd: {runtime} reports no cost, so no dollar budget applies (min= or \
                         tokens= instead)"
                    ));
                }
                daily.usd = if none(&value) {
                    0.0
                } else {
                    let x: f64 = value
                        .trim_start_matches('$')
                        .parse()
                        .map_err(|_| format!("usd={value}: a number of dollars"))?;
                    if !x.is_finite() || x < 0.0 {
                        return Err(format!("usd={value}: out of range"));
                    }
                    x
                }
            }
            "tokens" | "tok" | "llm_tokens" => {
                if !crate::budget::reports_tokens(runtime) {
                    return Err(format!(
                        "tokens: {runtime} reports no LLM tokens, so no token budget applies \
                         (min= instead)"
                    ));
                }
                daily.tokens = if none(&value) {
                    0
                } else {
                    let (digits, scale) = match value.chars().last() {
                        Some('k') => (&value[..value.len() - 1], 1e3),
                        Some('m') => (&value[..value.len() - 1], 1e6),
                        _ => (value.as_str(), 1.0),
                    };
                    let x: f64 = digits
                        .parse()
                        .map_err(|_| format!("tokens={value}: a number, e.g. 500k or 2M"))?;
                    if !x.is_finite() || x < 0.0 {
                        return Err(format!("tokens={value}: out of range"));
                    }
                    (x * scale).round() as u64
                }
            }
            other => {
                return Err(format!(
                    "{other:?}: not a budget; min=, usd= or tokens= (0: no limit)"
                ));
            }
        }
    }
    Ok(daily)
}

/// The daily budget of `table` as config.toml sets it (the defaults for
/// the keys it does not set), and how it is likely paid when it sets one
/// of them.
fn current_daily(table: &settings::AgentSettings) -> (Daily, Option<Paid>) {
    let daily = Daily {
        usd: table
            .max_usd_per_day
            .unwrap_or(crate::budget::DEFAULT_USD_PER_DAY),
        minutes: table
            .max_minutes_per_day
            .unwrap_or(crate::budget::DEFAULT_MINUTES_PER_DAY),
        tokens: table.max_tokens_per_day.unwrap_or(0),
    };
    let set = table.max_usd_per_day.is_some()
        || table.max_minutes_per_day.is_some()
        || table.max_tokens_per_day.is_some();
    let paid = if daily.usd > 0.0 || daily.tokens > 0 {
        Paid::ApiKey
    } else if daily.minutes >= 240 {
        Paid::SelfHosted
    } else {
        Paid::Subscription
    };
    (daily, set.then_some(paid))
}

/// The budget question (ADR 0034), asked in both modes of a person: how
/// the agent is paid, which picks the metric and its defaults, then the
/// daily budget on one line that Enter accepts or the user edits
/// ([`parse_daily`]). How it is paid has no default but the answer
/// config.toml already says, or "API key" when the runtime's key is in the
/// environment (`api_key`). Each month is 30 × its day unless config.toml
/// sets it. `zero`: the table's USD budget of 0 from before 0.4.5, said and
/// not offered as it is. `prefix`: the table's keys (`agent`,
/// `agents.<name>`). Returns the keys to write (those `runtime` is held to),
/// or `None` at the end of the input: not answered.
fn ask_budget(
    input: &mut impl BufRead,
    out: &mut impl Write,
    table: &settings::AgentSettings,
    runtime: &str,
    zero: (bool, bool),
    prefix: &str,
    api_key: Option<&'static str>,
) -> Result<Option<Vec<(&'static str, String)>>> {
    let (now, mut was) = current_daily(table);
    writeln!(out, "{BUDGET_GUIDE}")?;
    // A USD budget of 0 from before 0.4.5 paused the agent: said, and not
    // offered as is (it now means no limit). The answer here confirms the
    // budget (ADR 0034 §5).
    if runtime != "openclaw" && (zero.0 || zero.1) {
        writeln!(out, "{}", zero_line(zero, prefix))?;
        was = None;
    }
    let paid = if runtime == "openclaw" {
        None
    } else {
        let default = was.or_else(|| api_key.map(|_| Paid::ApiKey));
        if was.is_none()
            && let Some(var) = api_key
        {
            writeln!(
                out,
                "{var} is set: {} bills that API key per use, so 2 is the default.",
                runtime_label(runtime)
            )?;
        }
        let number = |p: Paid| match p {
            Paid::Subscription => 1,
            Paid::ApiKey => 2,
            Paid::SelfHosted => 3,
        };
        writeln!(
            out,
            "  1) subscription (Claude Pro/Max, ChatGPT…)\n  2) API key, pay per use\n  3) \
             self-hosted or free model"
        )?;
        let shown = default.map_or("1/2/3".to_string(), |d| number(d).to_string());
        loop {
            write!(out, "{PAID_QUESTION} [{shown}]: ")?;
            let Some(answer) = read_answer(input, out)? else {
                // The end of the input: the default, if there is one; else
                // no answer, and nothing changes.
                match default {
                    Some(_) => break default,
                    None => return Ok(None),
                }
            };
            match answer.to_lowercase().as_str() {
                "" if default.is_some() => break default,
                "1" | "subscription" | "sub" => break Some(Paid::Subscription),
                "2" | "api" | "api key" | "key" => break Some(Paid::ApiKey),
                "3" | "self-hosted" | "self" | "free" | "local" => break Some(Paid::SelfHosted),
                _ => writeln!(out, "  answer 1, 2 or 3")?,
            }
        }
    };
    let mut daily = match paid {
        // The answer config.toml already says: its own values.
        Some(p) if Some(p) == was => now,
        Some(p) => preset(p, runtime),
        None if was.is_some() => now,
        None => preset(Paid::Subscription, runtime),
    };
    // Months set by hand stay: said with the 30×.
    let months: Vec<String> = [
        (
            "max_usd_per_month",
            table.max_usd_per_month.map(|x| x.to_string()),
        ),
        (
            "max_minutes_per_month",
            table.max_minutes_per_month.map(|x| x.to_string()),
        ),
        (
            "max_tokens_per_month",
            table.max_tokens_per_month.map(|x| x.to_string()),
        ),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| format!("{prefix}.{k} = {v}")))
    .collect();
    let month = if months.is_empty() {
        "monthly: 30×".to_string()
    } else {
        format!("monthly: 30×, but config.toml sets {}", months.join(", "))
    };
    let caps = match runtime {
        "openclaw" => "; OpenClaw keeps its own spend caps",
        "codex" => "; Codex reports no cost: its runs per day are capped too",
        _ => "",
    };
    loop {
        write!(
            out,
            "Daily budget: {} ({month}{caps}). Enter to accept, or change it (e.g. min=90 \
             usd=3 tokens=2M; 0: no limit): ",
            describe_daily(&daily, runtime)
        )?;
        let line = read_answer(input, out)?.unwrap_or_default();
        if line.is_empty() {
            break;
        }
        match parse_daily(&line, daily, runtime) {
            Ok(changed) => {
                daily = changed;
                writeln!(out, "  daily budget: {}", describe_daily(&daily, runtime))?;
                break;
            }
            Err(e) => writeln!(out, "  {e}")?,
        }
    }
    let mut values = vec![("agent.max_minutes_per_day", daily.minutes.to_string())];
    if crate::budget::reports_cost(runtime) {
        values.push(("agent.max_usd_per_day", daily.usd.to_string()));
    }
    if crate::budget::reports_tokens(runtime) {
        values.push(("agent.max_tokens_per_day", daily.tokens.to_string()));
    }
    Ok(Some(values))
}

/// Whether `agent.max_turns` means something for `runtime`: Copilot CLI
/// and Antigravity CLI have no turn limit to pass (ADR 0038), OpenClaw
/// keeps its own.
fn has_turn_limit(runtime: Option<&str>) -> bool {
    !matches!(runtime, Some("copilot" | "antigravity" | "openclaw"))
}

/// A runtime as people know it.
fn runtime_label(runtime: &str) -> &str {
    match runtime {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "vibe" => "Mistral Vibe",
        "opencode" => "OpenCode",
        "openclaw" => "OpenClaw",
        "copilot" => "GitHub Copilot CLI",
        "antigravity" => "Antigravity CLI",
        other => other,
    }
}

/// What the budget question says of a USD budget of 0 from before 0.4.5
/// in the table whose keys start with `prefix` (`agent`, `agents.<name>`).
fn zero_line(zero: (bool, bool), prefix: &str) -> String {
    let mut parts = Vec::new();
    if zero.0 {
        parts.push(format!(
            "{prefix}.max_usd_per_day = 0 paused the agent before 0.4.5 and now means no \
             limit: the daily budget below replaces it"
        ));
    }
    if zero.1 {
        parts.push(format!(
            "{prefix}.max_usd_per_month = 0 paused it before 0.4.5 and now means no monthly \
             limit (`wats config unset {prefix}.max_usd_per_month` for 30 × the day)"
        ));
    }
    format!("Note: {}.", parts.join("; "))
}

/// For each agent's table left with an unconfirmed USD budget of 0 from
/// before 0.4.5 (ADR 0034 §5): its own budget question, asked of a person,
/// written to its table and confirmed; without a person, or at the end of
/// the input, a warning that it stays paused. Returns how many are left.
fn confirm_zeros(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    stored: Option<u32>,
    person: bool,
    api_key: fn(&str) -> Option<&'static str>,
) -> Result<usize> {
    let mut left = 0;
    let settings = Settings::load(data_dir)?;
    for z in zero_tables(&settings, data_dir, stored) {
        let answered = if person {
            writeln!(
                out,
                "\nThe agent {} ({}, [{}]) also has a USD budget of 0 from before 0.4.5, which \
                 paused it: its own budget now.",
                z.name,
                runtime_label(&z.runtime),
                z.prefix
            )?;
            ask_budget(
                input,
                out,
                &z.table,
                &z.runtime,
                z.zero,
                &z.prefix,
                api_key(&z.runtime),
            )?
        } else {
            None
        };
        let Some(values) = answered else {
            writeln!(
                out,
                "warning: {}: a USD budget of 0 from before 0.4.5 keeps the agent {} paused, \
                 until `wats setup` on a terminal sets its budget (or {}.enabled = false keeps \
                 it off)",
                z.prefix, z.name, z.prefix
            )?;
            left += 1;
            continue;
        };
        let mut file = ConfigFile::open(data_dir)?;
        for (key, value) in &values {
            let leaf = key.trim_start_matches("agent.");
            let key = format!("{}.{leaf}", z.prefix);
            if is_default(&key, value) {
                if file.get(&key).is_some() {
                    file.unset(&key)?;
                }
            } else {
                file.set(&key, value)?;
            }
        }
        file.save()?;
        crate::budget::confirm(&z.work_dir)
            .with_context(|| format!("write {}", z.work_dir.display()))?;
    }
    Ok(left)
}

/// Whether an agent's table sets any budget key of its own.
pub fn has_budget(table: &settings::AgentSettings) -> bool {
    table.max_usd_per_day.is_some()
        || table.max_usd_per_month.is_some()
        || table.max_minutes_per_day.is_some()
        || table.max_minutes_per_month.is_some()
        || table.max_tokens_per_day.is_some()
        || table.max_tokens_per_month.is_some()
}

/// `wats setup <runtime> --agent` on a terminal (0.4.11): the budget
/// question of the wizard (ADR 0034) for the agent `name`, whose table
/// sets no budget yet. The answer goes to `agents.<name>` and confirms the
/// budget. Returns the line to show, or `None` at the end of the input.
pub fn ask_agent_budget(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    name: &str,
    runtime: &str,
    table: &settings::AgentSettings,
    work_dir: &Path,
) -> Result<Option<String>> {
    let prefix = format!("agents.{name}");
    writeln!(out)?;
    let Some(values) = ask_budget(
        input,
        out,
        table,
        runtime,
        (false, false),
        &prefix,
        env_api_key(runtime),
    )?
    else {
        return Ok(None);
    };
    let mut file = ConfigFile::open(data_dir)?;
    let mut set = Vec::new();
    // Defaults are left out of the table, as the wizard does; the
    // confirmation keeps the question from coming back.
    for (key, value) in &values {
        let leaf = key.trim_start_matches("agent.");
        let key = format!("{prefix}.{leaf}");
        if is_default(&key, value) {
            if file.get(&key).is_some() {
                file.unset(&key)?;
            }
        } else {
            file.set(&key, value)?;
        }
        set.push(format!("{leaf} = {value}"));
    }
    file.save()?;
    crate::budget::confirm(work_dir).with_context(|| format!("write {}", work_dir.display()))?;
    Ok(Some(format!("{prefix}: {}", set.join(", "))))
}

/// An agent's table with a USD budget of 0 from before 0.4.5 that the
/// budget question has not confirmed (ADR 0034 §5): its agent stays paused.
#[derive(Debug, Clone)]
struct ZeroTable {
    /// The agent's name (`agent` for an old `[agent]` table with no
    /// runtime, which `wats --agent <runtime>` runs).
    name: String,
    /// Its keys: `agent` (the old table) or `agents.<name>`.
    prefix: String,
    runtime: String,
    work_dir: PathBuf,
    table: settings::AgentSettings,
    zero: (bool, bool),
}

/// The agents' tables of `settings`, the old `[agent]` one included, whose
/// USD budget of 0 from before 0.4.5 is not confirmed (`stored`: the data
/// directory's setup revision). Not OpenClaw's, whose USD budgets never
/// applied.
fn zero_tables(settings: &Settings, data_dir: &Path, stored: Option<u32>) -> Vec<ZeroTable> {
    let mut tables = Vec::new();
    let members = crate::roster::configured(settings, data_dir, "user").unwrap_or_default();
    for m in members {
        let runtime = m.runtime.clone().unwrap_or_else(|| "claude".to_string());
        let zero = crate::budget::legacy_zero(&m.settings, stored, &m.work_dir);
        if runtime == "openclaw" || !(zero.0 || zero.1) {
            continue;
        }
        tables.push(ZeroTable {
            prefix: if m.legacy {
                "agent".to_string()
            } else {
                format!("agents.{}", m.name)
            },
            name: m.name,
            runtime,
            work_dir: m.work_dir,
            table: m.settings,
            zero,
        });
    }
    // The old [agent] table with no runtime: `wats --agent <runtime>` runs
    // it, in <data dir>/agent.
    if settings.legacy_agent && settings.agent.runtime.is_none() {
        let work_dir = data_dir.join("agent");
        let zero = crate::budget::legacy_zero(&settings.agent, stored, &work_dir);
        if zero.0 || zero.1 {
            tables.push(ZeroTable {
                name: "agent".to_string(),
                prefix: "agent".to_string(),
                runtime: "claude".to_string(),
                work_dir,
                table: settings.agent.clone(),
                zero,
            });
        }
    }
    tables
}

/// A key of an agent's table set to the default of wats 0.4.4 and older,
/// which would pin it (ADR 0027, 0.4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldDefault {
    /// The key, as `wats config unset` takes it.
    pub key: &'static str,
    /// Its value in config.toml.
    pub value: String,
    /// The default now.
    pub now: &'static str,
}

/// `agent.timeout` = 10m and `agent.max_turns` = 10 in `table`.
fn old_defaults_of(table: &settings::AgentSettings) -> Vec<OldDefault> {
    let mut old = Vec::new();
    if let Some(timeout) = &table.timeout
        && settings::parse_duration(timeout).ok() == Some(crate::agent::OLD_DEFAULT_TIMEOUT)
    {
        old.push(OldDefault {
            key: "agent.timeout",
            value: timeout.clone(),
            now: "25m",
        });
    }
    if table.max_turns == Some(crate::agent::OLD_DEFAULT_MAX_TURNS) {
        old.push(OldDefault {
            key: "agent.max_turns",
            value: crate::agent::OLD_DEFAULT_MAX_TURNS.to_string(),
            now: "50",
        });
    }
    old
}

/// The one-line notice of `wats status` and the node's start when an
/// agent's table sets `agent.timeout` or `agent.max_turns` to the default
/// of wats 0.4.4 and older (10m, 10), which would pin it: the new
/// defaults, and how to use them. config.toml is never rewritten for it.
pub fn old_defaults_notice(settings: &Settings) -> Option<String> {
    let several =
        settings.agents.len() > 1 || (settings.legacy_agent && !settings.agents.is_empty());
    let mut tables: Vec<(String, &settings::AgentSettings)> = Vec::new();
    if settings.legacy_agent {
        tables.push(("agent".to_string(), &settings.agent));
    }
    for (name, table) in &settings.agents {
        tables.push((
            if several {
                format!("agents.{name}")
            } else {
                "agent".to_string()
            },
            table,
        ));
    }
    let mut set = Vec::new();
    let mut keys = Vec::new();
    let mut now = Vec::new();
    for (prefix, table) in tables {
        for o in old_defaults_of(table) {
            let leaf = o.key.trim_start_matches("agent.");
            set.push(format!("{prefix}.{leaf} = {}", o.value));
            keys.push(format!("{prefix}.{leaf}"));
            if !now.contains(&o.now) {
                now.push(o.now);
            }
        }
    }
    if set.is_empty() {
        return None;
    }
    let one = set.len() == 1;
    Some(format!(
        "config.toml sets {}: {} of wats 0.4.4 and older; {} now {}. To use {}: wats setup, or \
         wats config unset {}",
        set.join(" and "),
        if one { "the default" } else { "the defaults" },
        if one {
            "the default is"
        } else {
            "the defaults are"
        },
        now.join(" and "),
        if one { "it" } else { "them" },
        keys.join(" ")
    ))
}

const HEADER: &str = "\
# wats node configuration: what a bare `wats` runs with. Flags and WATS_*
# variables override it.
# Commented lines show the defaults: uncomment one to change it, or use
# `wats config set <key> <value>`. `wats config list` shows every key;
# `wats setup` asks the main choices again.
";

/// The text of a new config.toml with these choices: chosen values that
/// are not the default are set, the others are commented.
pub fn render(choices: &Choices) -> Result<String> {
    let chosen = |key: &str| {
        choices
            .values
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    };
    let mut text = HEADER.to_string();
    let line = |text: &mut String, key: &str| -> Result<()> {
        let help = settings::find_key(key)?.help;
        let leaf = key.rsplit('.').next().unwrap_or(key);
        text.push_str(&format!("# {help}\n"));
        match chosen(key).filter(|v| !is_default(key, v)) {
            Some(v) => text.push_str(&format!("{leaf} = {}\n", settings::check_value(key, v)?)),
            None => {
                let default = settings::find_key(key)?.default;
                text.push_str(&format!(
                    "# {leaf} = {}\n",
                    settings::check_value(key, default)?
                ));
            }
        }
        Ok(())
    };
    text.push('\n');
    line(&mut text, "nets")?;
    text.push_str("\n[storage]\n");
    line(&mut text, "storage.max")?;
    text.push_str(
        "\n# The agents the node runs, one [agents.<name>] table each (ADR 0021); the\n\
         # name defaults to the runtime: claude, codex, openclaw, opencode or vibe.\n",
    );
    match &choices.runtime {
        Some(runtime) => {
            crate::roster::check_name(runtime).map_err(anyhow::Error::msg)?;
            text.push_str(&format!(
                "[agents.{runtime}]\nruntime = {}\n",
                settings::check_value("agent.runtime", runtime)?
            ));
            if let Some(model) = chosen("agent.model") {
                text.push_str(&format!(
                    "# {}\nmodel = {}\n",
                    settings::find_key("agent.model")?.help,
                    settings::check_value("agent.model", model)?
                ));
            }
            for key in AGENT_TABLE_KEYS {
                text.push('\n');
                line(&mut text, key)?;
            }
        }
        None => {
            // Commented: an example to uncomment.
            let mut example = String::from("[agents.claude]\nruntime = \"claude\"\n");
            for key in AGENT_TABLE_KEYS {
                example.push('\n');
                line(&mut example, key)?;
            }
            for l in example.lines() {
                match l {
                    "" => text.push('\n'),
                    l if l.starts_with('#') => text.push_str(&format!("{l}\n")),
                    l => text.push_str(&format!("# {l}\n")),
                }
            }
        }
    }
    if let Some(on) = chosen("notify.desktop") {
        text.push_str(&format!(
            "\n[notify]\n# {}\ndesktop = {}\n",
            settings::find_key("notify.desktop")?.help,
            settings::check_value("notify.desktop", on)?
        ));
    }
    // Only a Yes is written: absent is false, and a wats older than 0.4.3
    // refuses the key (ADR 0030).
    if let Some(on) =
        chosen("update.security_auto").filter(|v| !is_default("update.security_auto", v))
    {
        text.push_str(&format!(
            "\n[update]\n# {}\nsecurity_auto = {}\n",
            settings::find_key("update.security_auto")?.help,
            settings::check_value("update.security_auto", on)?
        ));
        if let Some(delay) = chosen("update.security_delay") {
            text.push_str(&format!(
                "# {}\nsecurity_delay = {}\n",
                settings::find_key("update.security_delay")?.help,
                settings::check_value("update.security_delay", delay)?
            ));
        }
    }
    toml::from_str::<Settings>(&text).context("the generated config.toml does not load")?;
    Ok(text)
}

/// Write the choices to `<data dir>/config.toml`: a new file from
/// [`render`], or the answered keys of an existing one (a default value
/// unsets its key), keeping its comments. Returns the file's path.
pub fn apply(data_dir: &Path, choices: &Choices) -> Result<PathBuf> {
    let path = data_dir.join(FILE_NAME);
    if !path.exists() {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        std::fs::write(&path, render(choices)?)
            .with_context(|| format!("write {}", path.display()))?;
        return Ok(path);
    }
    // The agents as they were (ADR 0021).
    let before = Settings::load(data_dir)?;
    let enabled = before.enabled_agents();
    let primary = before.primary_agent();
    let mut file = ConfigFile::open(data_dir)?;
    let agent_key = |agent: &str, key: &str| match key.strip_prefix("agent.") {
        Some(leaf) => format!("agents.{agent}.{leaf}"),
        None => key.to_string(),
    };
    let target = match &choices.runtime {
        // Not asked: the agents stay as they are, and the agent's keys go
        // to the one asked about.
        _ if choices.keep_runtime => primary.map(|(name, _)| name),
        Some(runtime) => {
            // The agent asked about keeps its name when it keeps its
            // runtime; another runtime is the agent named after it.
            let name = match &primary {
                Some((name, table)) if table.runtime.as_deref() == Some(runtime) => name.clone(),
                _ => runtime.clone(),
            };
            file.set(&agent_key(&name, "agent.runtime"), runtime)?;
            if file.get(&agent_key(&name, "agent.enabled")).is_some() {
                file.unset(&agent_key(&name, "agent.enabled"))?;
            }
            // Switching the node's single agent: the previous one is kept,
            // off.
            if let [(old, _)] = enabled.as_slice()
                && *old != name
            {
                file.set(&agent_key(old, "agent.enabled"), "false")?;
            }
            Some(name)
        }
        None => {
            // No agent: every agent is kept, off.
            for (name, _) in &enabled {
                file.set(&agent_key(name, "agent.enabled"), "false")?;
            }
            None
        }
    };
    for (key, value) in &choices.values {
        let key = match &target {
            Some(name) => agent_key(name, key),
            None => key.to_string(),
        };
        // Only what changes: a value the file already has keeps its line
        // and comments, a default unsets its key (ADR 0027, 0.4.5).
        let set = file.get(&key);
        if is_default(&key, value) {
            if set.is_some() {
                file.unset(&key)?;
            }
        } else if set.as_deref()
            != settings::check_value(&key, value)
                .ok()
                .map(|v| v.to_string().trim().to_string())
                .as_deref()
        {
            file.set(&key, value)?;
        }
    }
    file.save()?;
    Ok(path)
}

/// Write the default config.toml (every key commented) if there is none.
/// Returns its path when it wrote it.
pub fn write_default(data_dir: &Path) -> Result<Option<PathBuf>> {
    let path = data_dir.join(FILE_NAME);
    std::fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    let text = render(&Choices::default())?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(text.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            Ok(Some(path))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e).with_context(|| format!("write {}", path.display())),
    }
}

/// `wats setup` with no tool: detect the agent CLIs on `path`, ask the
/// main choices on `input`/`out`, write them. Returns the choices. The
/// security updates question is asked only when stdin is a terminal
/// (ADR 0030).
pub fn setup(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
) -> Result<Choices> {
    use std::io::IsTerminal;
    setup_asking(
        data_dir,
        input,
        out,
        path,
        crate::desktop::this_desktop_session(),
        Asker {
            install: Install::detect(),
            terminal: std::io::stdin().is_terminal(),
            machine: None,
            api_key: env_api_key,
        },
    )
}

/// [`setup`], `desktop` telling whether this machine has a desktop session
/// ([`crate::desktop::desktop_session`]): only then is `notify.desktop`
/// asked (default yes). As if a person answered on a terminal, with no API
/// key in the environment.
pub fn setup_with(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    desktop: bool,
) -> Result<Choices> {
    let asker = Asker {
        machine: Some(crate::naming::Machine::default()),
        api_key: |_| None,
        ..Asker::person()
    };
    setup_asking(data_dir, input, out, path, desktop, asker)
}

fn setup_asking(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    desktop: bool,
    asker: Asker,
) -> Result<Choices> {
    let (choices, found) =
        ask_and_write(data_dir, input, out, path, desktop, asker, 0, Run::Setup)?;
    next_steps(out, &choices, &found, true)?;
    Ok(choices)
}

/// [`setup`] for the setup wizard (ADR 0027), which asks next whether
/// to register wats in the MCP hosts found: the hints to run `wats setup
/// <tool> --mcp` are left out. Returns the choices and the agent CLIs found.
/// The wizard runs on a terminal only (or `WATS_TEST_INTERACTIVE=1` under
/// `--hermetic`), so a person answers.
pub fn setup_for_wizard(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
) -> Result<(Choices, Vec<Found>)> {
    let (choices, found) = ask_and_write(
        data_dir,
        input,
        out,
        path,
        crate::desktop::this_desktop_session(),
        Asker {
            api_key: env_api_key,
            ..Asker::person()
        },
        0,
        Run::Wizard,
    )?;
    next_steps(out, &choices, &found, false)?;
    Ok((choices, found))
}

/// A data directory set up at `revision`, older than [`SETUP_REVISION`]:
/// ask only the questions added since, write them, record the revision.
/// Run by a bare `wats` on a terminal only.
pub fn catch_up(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    revision: u32,
) -> Result<Choices> {
    catch_up_asking(
        data_dir,
        input,
        out,
        path,
        revision,
        crate::desktop::this_desktop_session(),
        Asker {
            api_key: env_api_key,
            ..Asker::person()
        },
    )
}

/// [`catch_up`], `desktop` as in [`setup_with`], with no API key in the
/// environment.
pub fn catch_up_with(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    revision: u32,
    desktop: bool,
) -> Result<Choices> {
    catch_up_asking(
        data_dir,
        input,
        out,
        path,
        revision,
        desktop,
        Asker::person(),
    )
}

fn catch_up_asking(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    revision: u32,
    desktop: bool,
    asker: Asker,
) -> Result<Choices> {
    // Only questions for an agent, and it runs none: nothing to ask. Not
    // with a USD budget of 0 from before 0.4.5 in a table: that pause
    // waits for `wats setup` (ADR 0034 §5).
    let settings = Settings::load(data_dir)?;
    if pending_questions(&settings, revision) == 0
        && zero_tables(&settings, data_dir, stored_revision(data_dir)).is_empty()
    {
        record_revision(data_dir)?;
        return Ok(Choices {
            keep_runtime: true,
            ..Choices::default()
        });
    }
    writeln!(
        out,
        "wats setup has new questions since this data directory was set up (setup revision \
         {revision}, now {SETUP_REVISION}). Enter accepts the [default]."
    )?;
    let (choices, _) = ask_and_write(
        data_dir,
        input,
        out,
        path,
        desktop,
        asker,
        revision,
        Run::CatchUp,
    )?;
    Ok(choices)
}

/// Which setup asks: `wats setup` with its header line, the wizard (its
/// screens instead), or a catch-up of the questions added since a
/// revision (no quick or detailed question).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Setup,
    Wizard,
    CatchUp,
}

/// Who answers the security updates question, and for which install
/// (ADR 0030): asked only of a person (stdin a terminal) and only for an
/// [`Install::Release`], which the node can update by itself.
#[derive(Debug, Clone)]
struct Asker {
    install: Install,
    terminal: bool,
    /// The nodes whose names the node name question refuses (ADR 0023
    /// §8); None: this user's, looked up when asked.
    machine: Option<crate::naming::Machine>,
    /// The API key variable set for a runtime ([`env_api_key`]), for the
    /// budget question's default.
    api_key: fn(&str) -> Option<&'static str>,
}

impl Asker {
    /// A person on a terminal, with this binary's install, no API key
    /// looked up.
    fn person() -> Asker {
        Asker {
            install: Install::detect(),
            terminal: true,
            machine: None,
            api_key: |_| None,
        }
    }
}

/// Ask the main choices newer than `revision` (0: all of them), write
/// them, and record [`SETUP_REVISION`] (and the security updates answer,
/// in [`ANSWERS_FILE`]).
#[allow(clippy::too_many_arguments)]
fn ask_and_write(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    desktop: bool,
    asker: Asker,
    revision: u32,
    run: Run,
) -> Result<(Choices, Vec<Found>)> {
    let settings = Settings::load(data_dir)?;
    let found = detect_agents(path);
    let stored = stored_revision(data_dir);
    if revision == 0 && run == Run::Setup {
        writeln!(out, "{SETUP_HEADER}")?;
    }
    let person = asker.terminal;
    let ctx = Ctx {
        full: run != Run::CatchUp,
        person,
        zeros: zero_tables(&settings, data_dir, stored)
            .into_iter()
            .map(|z| (z.name, z.zero))
            .collect(),
        confirmed: crate::roster::configured(&settings, data_dir, "user")
            .unwrap_or_default()
            .into_iter()
            .filter(|m| crate::budget::confirmed(&m.work_dir))
            .map(|m| m.name)
            .collect(),
        api_key: asker.api_key,
    };
    let mut choices = prompt_after(input, out, &found, &settings, &QUESTIONS, revision, &ctx)?;
    // Quick: notify.desktop stays as it is (unset: on with a desktop
    // session), the security updates as recommended.
    if desktop && DESKTOP_SINCE > revision && !choices.quick {
        let on = ask_yes_no(
            input,
            out,
            "Desktop notification when someone answers or clarifies one of your asks \
             (notify.desktop)",
            settings.notify.desktop.unwrap_or(true),
        )?;
        choices.values.push(("notify.desktop", on.to_string()));
    }
    let security = if SECURITY_AUTO_SINCE > revision {
        ask_security_auto(input, out, &settings, data_dir, asker.clone(), &mut choices)?
    } else {
        None
    };
    // Last, so that the answers before it keep their order (ADR 0023 §8).
    if revision == 0 {
        ask_node_name(input, out, data_dir, asker.machine.as_ref(), &mut choices)?;
    }
    let file = apply(data_dir, &choices)?;
    if let Some(on) = security {
        record_answer(data_dir, "security_auto", if on { "yes" } else { "no" })?;
    }
    // The budget question answered: that agent's budget is confirmed, and a
    // USD budget of 0 from before 0.4.5 no longer pauses it (ADR 0034 §5).
    if let Some(name) = &choices.budget_for {
        let after = Settings::load(data_dir)?;
        let work_dir = crate::roster::configured(&after, data_dir, "user")
            .unwrap_or_default()
            .into_iter()
            .find(|m| &m.name == name)
            .map_or_else(|| data_dir.join("agent").join(name), |m| m.work_dir);
        crate::budget::confirm(&work_dir)
            .with_context(|| format!("write {}", work_dir.display()))?;
    }
    // The other agents with such a 0: each one's budget asked (a person),
    // else it stays paused.
    let left = confirm_zeros(data_dir, input, out, stored, person, asker.api_key)?;
    // Revision 2 once a person answered the budget question (with an agent)
    // and no agent is left with an unconfirmed 0; else as it was, so that
    // a terminal asks it next.
    let budget_due = choices.runtime.is_some() && revision < BUDGET_SINCE;
    if person && left == 0 && (!budget_due || choices.budget_for.is_some()) {
        record_revision(data_dir)?;
    } else if stored.is_some_and(|r| r >= SETUP_REVISION) {
        // Already there: nothing to lower.
    } else if left > 0 || budget_due {
        writeln!(
            out,
            "Setup revision kept at {}: run `wats setup` on a terminal to answer the budget \
             question.",
            stored.unwrap_or(BASELINE_REVISION)
        )?;
    }
    writeln!(out, "Wrote {}", file.display())?;
    Ok((choices, found))
}

/// The security updates question (ADR 0030). Its default: the current
/// value when config.toml sets the key, else the answer recorded in
/// [`ANSWERS_FILE`], else Yes (never answered). Yes sets
/// `update.security_auto` and, when none is set, `update.security_delay`
/// to [`SECURITY_DELAY`]. No unsets a `true` (absent is false, and a wats
/// older than 0.4.3 refuses the key) and keeps a `false` set by hand; the
/// No is remembered in [`ANSWERS_FILE`]. Not asked without a terminal (an
/// opt-in to auto-install needs a person's answer: the key stays as it
/// is), nor for a Homebrew install or a platform with no prebuilt release,
/// which the node never updates. Returns the answer, if asked.
fn ask_security_auto(
    input: &mut impl BufRead,
    out: &mut impl Write,
    settings: &Settings,
    data_dir: &Path,
    asker: Asker,
    choices: &mut Choices,
) -> Result<Option<bool>> {
    match asker.install {
        Install::Homebrew => {
            writeln!(out, "{SECURITY_AUTO_HOMEBREW}")?;
            return Ok(None);
        }
        Install::Source => {
            writeln!(out, "{SECURITY_AUTO_SOURCE}")?;
            return Ok(None);
        }
        Install::Release => {}
    }
    if !asker.terminal {
        writeln!(out, "{SECURITY_AUTO_NO_TERMINAL}")?;
        return Ok(None);
    }
    let current = settings.update.security_auto;
    let default = current
        .or_else(|| recorded_answer(data_dir, "security_auto").map(|a| a == "yes"))
        .unwrap_or(true);
    let on = if choices.quick {
        writeln!(
            out,
            "Security updates: {} (update.security_auto; `wats setup`, detailed, to change).",
            if default {
                "installed automatically, signature checked, a day after their release"
            } else {
                "announced, not installed (as you answered before)"
            }
        )?;
        default
    } else {
        writeln!(out, "{SECURITY_AUTO_ABOUT}")?;
        ask_yes_no(input, out, SECURITY_AUTO_QUESTION, default)?
    };
    if on {
        choices
            .values
            .push(("update.security_auto", "true".to_string()));
        if settings.update.security_delay.is_none() {
            choices
                .values
                .push(("update.security_delay", SECURITY_DELAY.to_string()));
        }
    } else if current == Some(true) {
        // `false` is the default: `apply` unsets the key.
        choices
            .values
            .push(("update.security_auto", "false".to_string()));
    }
    Ok(Some(on))
}

/// This user's nodes, for the node name question: the registry (none in a
/// hermetic process without `WATS_RUN_DIR`) and `~/.wats`.
#[cfg(not(test))]
fn users_machine() -> crate::naming::Machine {
    crate::naming::Machine::of_user(crate::background::registry_dir(
        crate::update::hermetic_env(),
    ))
}

/// Unit tests never look at this user's nodes (the real ~/.wats): none.
#[cfg(test)]
fn users_machine() -> crate::naming::Machine {
    crate::naming::Machine::default()
}

/// The node name question (ADR 0023 §8).
pub const NODE_NAME_QUESTION: &str = "Name of this node";

/// Ask the node's name (ADR 0023 §8), its current one by default, else one
/// drawn now; another node of this machine's name is refused. Written to
/// `<data dir>/node-name`, which older releases ignore (so a rollback still
/// starts), unless config.toml sets `node.name`: then there.
fn ask_node_name(
    input: &mut impl BufRead,
    out: &mut impl Write,
    data_dir: &Path,
    machine: Option<&crate::naming::Machine>,
    choices: &mut Choices,
) -> Result<()> {
    use crate::naming;
    let taken = match machine {
        Some(machine) => machine.taken(data_dir),
        None => users_machine().taken(data_dir),
    };
    let current = naming::stored(data_dir);
    let default = current
        .clone()
        .unwrap_or_else(|| naming::draw_random(&taken));
    // Quick: the current name, or the one drawn, said in one line
    // (ADR 0027 §5).
    let name = if choices.quick {
        writeln!(
            out,
            "Node name: {default} (`wats setup`, detailed, to change)"
        )?;
        default.clone()
    } else {
        loop {
            write!(out, "{NODE_NAME_QUESTION} (node.name) [{default}]: ")?;
            let answer = match read_answer(input, out)? {
                Some(answer) if !answer.is_empty() => answer,
                _ => break default.clone(),
            };
            if let Err(why) = naming::check(&answer) {
                writeln!(out, "  {why}")?;
            } else if taken.contains(&answer) {
                writeln!(out, "  another node of this machine is named {answer}")?;
            } else {
                break answer;
            }
        }
    };
    if naming::configured(data_dir).is_some() {
        choices.values.push(("node.name", name));
    } else if current.as_deref() != Some(name.as_str()) {
        naming::write(data_dir, &name)
            .with_context(|| format!("write {}", data_dir.join(naming::FILE).display()))?;
    }
    Ok(())
}

/// The answer `name` recorded in `data_dir`'s [`ANSWERS_FILE`], if any.
pub fn recorded_answer(data_dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(ANSWERS_FILE)).ok()?;
    text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// Record `name=value` in `data_dir`'s [`ANSWERS_FILE`], keeping the other
/// answers.
fn record_answer(data_dir: &Path, name: &str, value: &str) -> Result<()> {
    let path = data_dir.join(ANSWERS_FILE);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text: String = old
        .lines()
        .filter(|line| line.split_once('=').is_none_or(|(k, _)| k.trim() != name))
        .map(|line| format!("{line}\n"))
        .collect();
    text.push_str(&format!("{name}={value}\n"));
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
}

/// What the node now runs, and with `hints`, how to register wats in the
/// MCP hosts found.
fn next_steps(out: &mut impl Write, choices: &Choices, found: &[Found], hints: bool) -> Result<()> {
    match &choices.runtime {
        Some(runtime) if runtime == "openclaw" => {
            if hints {
                writeln!(out, "{OPENCLAW_AGENT_NEXT}")?;
            }
        }
        Some(runtime) => {
            let name = match runtime.as_str() {
                "codex" => "Codex",
                "opencode" => "OpenCode",
                "vibe" => "Mistral Vibe",
                "copilot" => "GitHub Copilot CLI",
                "antigravity" => "Antigravity CLI",
                _ => "Claude Code",
            };
            write!(out, "`wats` now runs the node with a {name} agent.")?;
            if runtime == "copilot" {
                write!(
                    out,
                    " Its runs have their own Copilot home (not ~/.copilot), the wats tools \
                     and their workspace, the web but no shell (`wats share` changes it)."
                )?;
            }
            if runtime == "vibe" {
                let inherit = choices
                    .values
                    .iter()
                    .any(|(k, v)| *k == "agent.access" && v.trim() == "inherit");
                if inherit {
                    write!(
                        out,
                        " With inherit, its runner starts only once agent.review = true or \
                         agent.allow is set (wats config set agents.vibe.review true, or wats \
                         config set agents.vibe.allow <ids>)."
                    )?;
                } else {
                    write!(
                        out,
                        " Its runs have the wats tools only, with their own Vibe home (not \
                         ~/.vibe); your API key comes from your OS keyring (`vibe --setup`) \
                         or MISTRAL_API_KEY."
                    )?;
                }
            }
            if runtime == "opencode" {
                write!(
                    out,
                    " Its runs read the agent's own OpenCode directory, not ~/.config/opencode \
                     (`wats setup opencode --agent` creates it and says where)."
                )?;
            }
            if hints {
                write!(
                    out,
                    " To reach the swarm from your own {name} sessions too: wats setup \
                     {runtime} --mcp"
                )?;
            }
            writeln!(out)?;
        }
        None => writeln!(out, "{NO_AGENT_NOTICE}")?,
    }
    if !hints {
        return Ok(());
    }
    if found.iter().any(|f| f.name == "openclaw") && choices.runtime.as_deref() != Some("openclaw")
    {
        writeln!(out, "{OPENCLAW_MCP_HINT}")?;
    }
    if found.iter().any(|f| f.name == "vibe") && choices.runtime.as_deref() != Some("vibe") {
        writeln!(out, "{VIBE_MCP_HINT}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_cli(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn detects_executables_on_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let codex = fake_cli(a.path(), "codex");
        let openclaw = fake_cli(b.path(), "openclaw");
        // Not executable: not found.
        std::fs::write(a.path().join("claude"), "").unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        let found = detect_agents(Some(&path));
        assert_eq!(
            found,
            [
                Found {
                    name: "codex",
                    path: codex
                },
                Found {
                    name: "openclaw",
                    path: openclaw
                }
            ]
        );
        assert!(found.iter().all(Found::supported));
        assert!(detect_agents(None).is_empty());
    }

    /// OpenClaw on PATH: setup offers its MCP registration, whatever agent
    /// the node runs.
    #[cfg(unix)]
    #[test]
    fn setup_offers_openclaw_mcp() {
        let bin = tempfile::tempdir().unwrap();
        fake_cli(bin.path(), "codex");
        fake_cli(bin.path(), "openclaw");
        let path = std::env::join_paths([bin.path()]).unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup(data.path(), &mut "".as_bytes(), &mut out, Some(&path)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime.as_deref(), Some("codex"));
        assert!(said.contains("wats setup codex --mcp"), "{said}");
        assert!(said.contains(OPENCLAW_MCP_HINT), "{said}");
        // OpenClaw as the agent: the next step is its own setup.
        let other = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup(other.path(), &mut "2\n".as_bytes(), &mut out, Some(&path)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime.as_deref(), Some("openclaw"));
        assert!(said.contains(OPENCLAW_AGENT_NEXT), "{said}");
        assert!(!said.contains(OPENCLAW_MCP_HINT), "{said}");
        // Without OpenClaw, no hint.
        std::fs::remove_file(bin.path().join("openclaw")).unwrap();
        let mut out = Vec::new();
        setup(data.path(), &mut "".as_bytes(), &mut out, Some(&path)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!said.contains("openclaw"), "{said}");
    }

    /// OpenCode on PATH (looked up, never run) is a runtime (ADR 0031):
    /// choosing it asks its model as provider/model, written to its table.
    #[cfg(unix)]
    #[test]
    fn opencode_is_a_runtime_with_a_model_question() {
        let bin = tempfile::tempdir().unwrap();
        let opencode = fake_cli(bin.path(), "opencode");
        let path = std::env::join_paths([bin.path()]).unwrap();
        let found = detect_agents(Some(&path));
        assert_eq!(
            found,
            [Found {
                name: "opencode",
                path: opencode
            }]
        );
        assert!(found[0].supported());
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup(
            data.path(),
            &mut "opencode\namazon-bedrock/claude-test\n".as_bytes(),
            &mut out,
            Some(&path),
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime.as_deref(), Some("opencode"));
        assert!(said.contains(OPENCODE_MODEL_QUESTION), "{said}");
        assert!(said.contains("wats setup opencode --mcp"), "{said}");
        let config = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(config.contains("[agents.opencode]"), "{config}");
        assert!(
            config.contains("model = \"amazon-bedrock/claude-test\""),
            "{config}"
        );
        // Enter keeps OpenCode's own default: no model written.
        let other = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        setup(other.path(), &mut "\n\n".as_bytes(), &mut out, Some(&path)).unwrap();
        let config = std::fs::read_to_string(other.path().join(FILE_NAME)).unwrap();
        assert!(!config.contains("\nmodel = "), "{config}");
    }

    /// On a desktop session, setup asks notify.desktop (default yes) and
    /// writes the answer; elsewhere it does not ask. Quick keeps it as it
    /// is (unset: on with a desktop session).
    #[test]
    fn setup_asks_desktop_notifications_on_a_desktop() {
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup_with(data.path(), &mut "".as_bytes(), &mut out, None, true).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(choices.quick);
        assert!(!said.contains("(notify.desktop)"), "{said}");
        assert_eq!(Settings::load(data.path()).unwrap().notify.desktop, None);
        let mut out = Vec::new();
        let choices = setup_with(data.path(), &mut "d\n".as_bytes(), &mut out, None, true).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("(notify.desktop) [Y/n]"), "{said}");
        assert!(choices.values.contains(&("notify.desktop", "true".into())));
        let settings = Settings::load(data.path()).unwrap();
        assert_eq!(settings.notify.desktop, Some(true));
        // Asked again (no agent CLI: storage and networks first): "n"
        // turns it off.
        let mut out = Vec::new();
        setup_with(
            data.path(),
            &mut "d\n\n\nn\n".as_bytes(),
            &mut out,
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            Settings::load(data.path()).unwrap().notify.desktop,
            Some(false)
        );
        // Headless: not asked, not written.
        let other = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup_with(other.path(), &mut "".as_bytes(), &mut out, None, false).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("notify.desktop"));
        assert!(choices.values.iter().all(|(k, _)| *k != "notify.desktop"));
        assert_eq!(Settings::load(other.path()).unwrap().notify.desktop, None);
    }

    /// The security updates question (ADR 0030): Enter is Yes, which sets
    /// `update.security_auto` and a one-day `update.security_delay`; a
    /// delay already set is kept.
    #[test]
    fn setup_asks_security_updates() {
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        // No agent CLI: detailed, storage, networks, then this question
        // (Enter).
        let choices =
            setup_with(data.path(), &mut "d\n".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains(SECURITY_AUTO_ABOUT), "{said}");
        assert!(
            said.contains("Install security updates automatically? (update.security_auto) [Y/n]"),
            "{said}"
        );
        assert!(
            choices
                .values
                .contains(&("update.security_auto", "true".into()))
        );
        let text = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\n[update]\n"), "{text}");
        assert!(text.contains("\nsecurity_auto = true\n"), "{text}");
        assert!(text.contains("\nsecurity_delay = \"1d\"\n"), "{text}");
        let policy = Settings::load(data.path())
            .unwrap()
            .update
            .policy()
            .unwrap();
        assert!(policy.security_auto && !policy.auto);
        assert_eq!(
            policy.security_delay,
            Some(std::time::Duration::from_secs(86400))
        );
        assert_eq!(
            recorded_answer(data.path(), "security_auto").as_deref(),
            Some("yes")
        );
        // Yes with a delay of the user's own: kept.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join(FILE_NAME),
            "[update]\nsecurity_delay = \"12h\"\n",
        )
        .unwrap();
        setup_with(
            other.path(),
            &mut "d\n\n\ny\n".as_bytes(),
            &mut Vec::new(),
            None,
            false,
        )
        .unwrap();
        let settings = Settings::load(other.path()).unwrap();
        assert_eq!(settings.update.security_auto, Some(true));
        // Quick: the recommended Yes, said in one line, not asked.
        let quick = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        setup_with(quick.path(), &mut "".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!said.contains(SECURITY_AUTO_QUESTION), "{said}");
        assert!(
            said.contains("Security updates: installed automatically"),
            "{said}"
        );
        assert_eq!(
            Settings::load(quick.path()).unwrap().update.security_auto,
            Some(true)
        );
        assert_eq!(settings.update.security_delay.as_deref(), Some("12h"));
    }

    /// The node's name (ADR 0023 §8), asked last: a drawn default, kept in
    /// `node-name` (never config.toml, which 0.4.4 would refuse); an answer
    /// is checked; with `node.name` already in config.toml, it goes there.
    #[test]
    fn setup_asks_the_node_name() {
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        setup_with(data.path(), &mut "d\n".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        let drawn = crate::naming::generated(data.path()).expect("written");
        assert!(
            said.contains(&format!("{NODE_NAME_QUESTION} (node.name) [{drawn}]: ")),
            "{said}"
        );
        // Quick keeps it, in one line, not asked (ADR 0027 §5).
        let mut out = Vec::new();
        setup_with(data.path(), &mut "".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!said.contains(NODE_NAME_QUESTION), "{said}");
        assert!(
            said.contains(&format!(
                "Node name: {drawn} (`wats setup`, detailed, to change)"
            )),
            "{said}"
        );
        assert_eq!(crate::naming::generated(data.path()), Some(drawn.clone()));
        let fresh = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        setup_with(fresh.path(), &mut "".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        let kept = crate::naming::generated(fresh.path()).expect("drawn and written");
        assert!(said.contains(&format!("Node name: {kept} (")), "{said}");
        let config = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(!config.contains(&drawn), "{config}");
        // Again: the current name is the default. Storage, networks,
        // security updates, then the name: an invalid one, then another.
        let mut out = Vec::new();
        setup_with(
            data.path(),
            &mut "d\n\n\n\nNot Valid\nhive_test\n".as_bytes(),
            &mut out,
            None,
            false,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains(&format!("(node.name) [{drawn}]")), "{said}");
        assert!(said.contains("node name \"Not Valid\""), "{said}");
        assert_eq!(
            crate::naming::generated(data.path()).as_deref(),
            Some("hive_test")
        );
        assert!(Settings::load(data.path()).unwrap().node.name.is_none());
        // Set in config.toml by hand: the answer goes there.
        let mut file = ConfigFile::open(data.path()).unwrap();
        file.set("node.name", "by_hand").unwrap();
        file.save().unwrap();
        setup_with(
            data.path(),
            &mut "d\n\n\n\nrenamed\n".as_bytes(),
            &mut Vec::new(),
            None,
            false,
        )
        .unwrap();
        let settings = Settings::load(data.path()).unwrap();
        assert_eq!(settings.node.name.as_deref(), Some("renamed"));
        assert_eq!(
            crate::naming::stored(data.path()).as_deref(),
            Some("renamed")
        );
    }

    /// The node name question refuses another node's name, among the
    /// machine's directories it is given (temporary ones here: a unit test
    /// never reads the real ~/.wats).
    #[test]
    fn the_node_name_question_refuses_another_nodes_name() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = tmp.path().join("run");
        let other = tmp.path().join("other");
        crate::naming::write(&other, "busy_bee").unwrap();
        crate::background::register(&registry, &other).unwrap();
        let machine = crate::naming::Machine {
            registry: Some(registry),
            default_dir: Some(tmp.path().join("home/.wats")),
        };
        let data = tmp.path().join("data");
        let mut out = Vec::new();
        let mut choices = Choices::default();
        ask_node_name(
            &mut "busy_bee\ncalm_bee\n".as_bytes(),
            &mut out,
            &data,
            Some(&machine),
            &mut choices,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("another node of this machine is named busy_bee"),
            "{said}"
        );
        assert_eq!(crate::naming::generated(&data).as_deref(), Some("calm_bee"));
    }

    /// A No sticks (ADR 0030): it unsets the key (absent is false, and an
    /// older wats refuses it), is remembered in `setup-answers`, and is the
    /// default of the next run, so Enter keeps it off.
    #[test]
    fn a_no_to_security_updates_sticks() {
        let data = tempfile::tempdir().unwrap();
        // Yes first, then No: the key is unset, the delay kept.
        setup_with(
            data.path(),
            &mut "".as_bytes(),
            &mut Vec::new(),
            None,
            false,
        )
        .unwrap();
        let mut out = Vec::new();
        setup_with(
            data.path(),
            &mut "d\n\n\nn\n".as_bytes(),
            &mut out,
            None,
            false,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("(update.security_auto) [Y/n]"), "{said}");
        let text = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains("security_auto ="), "{text}");
        let settings = Settings::load(data.path()).unwrap();
        assert_eq!(settings.update.security_auto, None);
        assert!(!settings.update.policy().unwrap().security_auto);
        assert_eq!(settings.update.security_delay.as_deref(), Some("1d"));
        assert_eq!(
            recorded_answer(data.path(), "security_auto").as_deref(),
            Some("no")
        );
        // Re-run with Enter: the recorded No is the default; still off.
        let mut out = Vec::new();
        setup_with(data.path(), &mut "d\n".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("(update.security_auto) [y/N]"), "{said}");
        let settings = Settings::load(data.path()).unwrap();
        assert_eq!(settings.update.security_auto, None);
        // Quick keeps the No too.
        let mut out = Vec::new();
        setup_with(data.path(), &mut "".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("announced, not installed"), "{said}");
        assert_eq!(
            Settings::load(data.path()).unwrap().update.security_auto,
            None
        );
        // No on a new data directory: nothing in config.toml.
        let other = tempfile::tempdir().unwrap();
        setup_with(
            other.path(),
            &mut "d\n\n\nno\n".as_bytes(),
            &mut Vec::new(),
            None,
            false,
        )
        .unwrap();
        let text = std::fs::read_to_string(other.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains("security_auto"), "{text}");
        assert!(!text.contains("security_delay = "), "{text}");
        assert_eq!(
            recorded_answer(other.path(), "security_auto").as_deref(),
            Some("no")
        );
        // Other answers of the file are kept.
        std::fs::write(
            other.path().join(ANSWERS_FILE),
            "future=1\nsecurity_auto=no\n",
        )
        .unwrap();
        setup_with(
            other.path(),
            &mut "d\n\n\ny\n".as_bytes(),
            &mut Vec::new(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(other.path().join(ANSWERS_FILE)).unwrap(),
            "future=1\nsecurity_auto=yes\n"
        );
    }

    /// A `security_auto = false` set by hand is the default and is kept
    /// as it is by a No (not unset).
    #[test]
    fn a_false_set_by_hand_is_kept() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(
            data.path().join(FILE_NAME),
            "[update]\nsecurity_auto = false\n",
        )
        .unwrap();
        let mut out = Vec::new();
        setup_with(data.path(), &mut "d\n".as_bytes(), &mut out, None, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("(update.security_auto) [y/N]"), "{said}");
        let text = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("security_auto = false"), "{text}");
        assert_eq!(
            Settings::load(data.path()).unwrap().update.security_auto,
            Some(false)
        );
    }

    /// Without a terminal (`wats setup </dev/null`, or an agent piping
    /// answers), the question is not asked: the end of the input never
    /// opts in, and even a piped "y" is not taken. The key stays as it is.
    #[test]
    fn no_terminal_never_opts_in() {
        let data = tempfile::tempdir().unwrap();
        let piped = Asker {
            install: Install::Release,
            terminal: false,
            machine: Some(crate::naming::Machine::default()),
            api_key: |_| None,
        };
        for input in ["", "\n\ny\n"] {
            let mut out = Vec::new();
            let (choices, _) = ask_and_write(
                data.path(),
                &mut input.as_bytes(),
                &mut out,
                None,
                false,
                piped.clone(),
                0,
                Run::Setup,
            )
            .unwrap();
            let said = String::from_utf8(out).unwrap();
            assert!(said.contains(SECURITY_AUTO_NO_TERMINAL), "{said}");
            assert!(!said.contains(SECURITY_AUTO_QUESTION), "{said}");
            assert!(
                choices
                    .values
                    .iter()
                    .all(|(k, _)| !k.starts_with("update."))
            );
            let settings = Settings::load(data.path()).unwrap();
            assert_eq!(settings.update.security_auto, None);
            assert!(!settings.update.policy().unwrap().security_auto);
            assert_eq!(recorded_answer(data.path(), "security_auto"), None);
        }
        // A Yes set before stays as it is.
        let mut file = ConfigFile::open(data.path()).unwrap();
        file.set("update.security_auto", "true").unwrap();
        file.save().unwrap();
        ask_and_write(
            data.path(),
            &mut "".as_bytes(),
            &mut Vec::new(),
            None,
            false,
            piped,
            0,
            Run::Setup,
        )
        .unwrap();
        assert_eq!(
            Settings::load(data.path()).unwrap().update.security_auto,
            Some(true)
        );
    }

    /// A Homebrew install, or a platform with no prebuilt release, is not
    /// asked about security updates: the node never upgrades it. Nor is a
    /// data directory already at revision 1 asked again (part of revision
    /// 1, ADR 0030).
    #[test]
    fn security_updates_question_skips_homebrew_source_and_catch_up() {
        for (install, line) in [
            (Install::Homebrew, SECURITY_AUTO_HOMEBREW),
            (Install::Source, SECURITY_AUTO_SOURCE),
        ] {
            let data = tempfile::tempdir().unwrap();
            let mut out = Vec::new();
            let asker = Asker {
                install,
                terminal: true,
                machine: Some(crate::naming::Machine::default()),
                api_key: |_| None,
            };
            let (choices, _) = ask_and_write(
                data.path(),
                &mut "".as_bytes(),
                &mut out,
                None,
                false,
                asker,
                0,
                Run::Setup,
            )
            .unwrap();
            let said = String::from_utf8(out).unwrap();
            assert!(said.contains(line), "{said}");
            assert!(!said.contains(SECURITY_AUTO_QUESTION), "{said}");
            assert!(
                choices
                    .values
                    .iter()
                    .all(|(k, _)| !k.starts_with("update."))
            );
            assert_eq!(
                Settings::load(data.path()).unwrap().update.security_auto,
                None
            );
        }
        // Revision 2 (0.4.5) adds the budget question only.
        assert_eq!(new_questions(BASELINE_REVISION), 1);
        assert_eq!(new_questions(SETUP_REVISION), 0);
    }

    /// Mistral Vibe on PATH (looked up, never run) is a runtime (ADR
    /// 0033): offered as the agent, with its own access question (no
    /// workspace, swarm-only by default, inherit with its warning).
    #[cfg(unix)]
    #[test]
    fn vibe_is_a_runtime_without_workspace() {
        let bin = tempfile::tempdir().unwrap();
        let vibe = fake_cli(bin.path(), "vibe");
        let path = std::env::join_paths([bin.path()]).unwrap();
        let found = detect_agents(Some(&path));
        assert_eq!(
            found,
            [Found {
                name: "vibe",
                path: vibe
            }]
        );
        assert!(found[0].supported());
        // Enter everywhere: Vibe, quick, so swarm-only without asking.
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup(data.path(), &mut "".as_bytes(), &mut out, Some(&path)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime.as_deref(), Some("vibe"));
        assert!(said.contains("swarm-only access"), "{said}");
        assert!(!said.contains(VIBE_ACCESS_QUESTION), "{said}");
        let config = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(!config.contains("\naccess = "), "{config}");
        assert_eq!(
            crate::vibe::access(
                Settings::load(data.path()).unwrap().agents["vibe"]
                    .access
                    .as_deref()
            )
            .unwrap(),
            crate::access::Access::SwarmOnly
        );
        // Detailed, Enter everywhere: its own access question.
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup(data.path(), &mut "\nd\n".as_bytes(), &mut out, Some(&path)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime.as_deref(), Some("vibe"));
        assert!(said.contains("1) vibe"), "{said}");
        assert!(!said.contains("not supported yet"), "{said}");
        assert!(
            said.contains(&format!(
                "{VIBE_ACCESS_QUESTION} (agent.access) [swarm-only]"
            )),
            "{said}"
        );
        assert!(!said.contains(crate::vibe::INHERIT_WARNING), "{said}");
        assert!(said.contains("wats setup vibe --mcp"), "{said}");
        assert!(!said.contains(VIBE_MCP_HINT), "{said}");
        let config = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(config.contains("[agents.vibe]"), "{config}");
        assert!(config.contains("access = \"swarm-only\""), "{config}");
        // workspace is refused, inherit is taken, with its warning.
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let input = "vibe\nd\n\n\n\n1\n\nworkspace\ninherit\n";
        let choices = setup_with(
            data.path(),
            &mut input.as_bytes(),
            &mut out,
            Some(&path),
            false,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime.as_deref(), Some("vibe"));
        assert!(said.contains("Vibe cannot keep its tools"), "{said}");
        assert!(said.contains(crate::vibe::INHERIT_WARNING), "{said}");
        assert!(said.contains("agent.review = true"), "{said}");
        let config = std::fs::read_to_string(data.path().join(FILE_NAME)).unwrap();
        assert!(config.contains("access = \"inherit\""), "{config}");
    }

    fn run(found: &[Found], settings: &Settings, input: &str) -> (Choices, String) {
        let mut out = Vec::new();
        let choices = prompt(&mut input.as_bytes(), &mut out, found, settings).unwrap();
        (choices, String::from_utf8(out).unwrap())
    }

    fn found(names: &[&'static str]) -> Vec<Found> {
        names
            .iter()
            .map(|&name| Found {
                name,
                path: PathBuf::from(format!("/bin/{name}")),
            })
            .collect()
    }

    /// Enter for each question: the first agent, quick (only the budget
    /// asked: a subscription, 120 minutes a day), and only what differs
    /// from the defaults written.
    #[test]
    fn enter_accepts_every_default() {
        let (choices, said) = run(
            &found(&["claude", "codex"]),
            &Settings::default(),
            "\n\n1\n",
        );
        assert_eq!(choices.runtime.as_deref(), Some("claude"));
        assert!(choices.quick);
        assert_eq!(
            choices.values,
            [
                ("agent.max_minutes_per_day", "120".to_string()),
                ("agent.max_usd_per_day", "0".into()),
                ("agent.max_tokens_per_day", "0".into()),
            ]
        );
        assert!(said.contains("1) claude"), "{said}");
        assert!(
            said.contains(&format!("{MODE_QUESTION} [quick/detailed, Enter: quick]: ")),
            "{said}"
        );
        assert!(
            said.contains(
                "Quick keeps the recommended settings (disk 10G, networks public, agent runs of \
                 at most 25m and 50 turns, workspace access"
            ),
            "{said}"
        );
        assert!(said.contains(BUDGET_GUIDE), "{said}");
        assert!(
            said.contains(&format!("{PAID_QUESTION} [1/2/3]: ")),
            "{said}"
        );
        assert!(
            said.contains(
                "Daily budget: 120 min, no USD limit, no LLM token limit (monthly: 30×). Enter \
                 to accept"
            ),
            "{said}"
        );
        // Not asked: no [default] of the detailed questions.
        assert!(!said.contains("[10G]"), "{said}");
        // Only what differs from the defaults is set; the rest commented.
        let text = render(&choices).unwrap();
        assert!(text.contains("\nruntime = \"claude\"\n"), "{text}");
        assert!(text.contains("# max = \"10G\""), "{text}");
        assert!(text.contains("# timeout = \"25m\""), "{text}");
        assert!(text.contains("# max_turns = 50"), "{text}");
        assert!(text.contains("\nmax_usd_per_day = 0.0\n"), "{text}");
        // 120 minutes, the subscription's, is the default since 0.4.11.
        assert!(text.contains("# max_minutes_per_day = 120"), "{text}");
        assert!(text.contains("# max_tokens_per_day = 0"), "{text}");
        // Detailed, Enter for each (a subscription): every question, the
        // same budget.
        let (choices, said) = run(&found(&["claude"]), &Settings::default(), "\nd\n\n\n\n1\n");
        assert!(!choices.quick);
        let keys: Vec<_> = choices.values.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "storage.max",
                "agent.timeout",
                "agent.max_turns",
                "agent.max_minutes_per_day",
                "agent.max_usd_per_day",
                "agent.max_tokens_per_day",
                "agent.access",
                "nets"
            ]
        );
        assert!(said.contains("(agent.timeout) [25m]"), "{said}");
        assert!(said.contains("(agent.max_turns) [50]"), "{said}");
        let text = render(&choices).unwrap();
        assert!(text.contains("# timeout = \"25m\""), "{text}");
        assert!(!text.contains("\ntimeout ="), "{text}");
    }

    /// The budget question picks the metric and its defaults: a
    /// subscription is time, an API key dollars (LLM tokens for Codex,
    /// which reports no cost), a self-hosted model the machine's time.
    #[test]
    fn the_budget_question_picks_the_metric() {
        let budget = |runtime: &'static str, input: &str| {
            let (choices, said) = run(&found(&[runtime]), &Settings::default(), input);
            let get = |key: &str| {
                choices
                    .values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.clone())
            };
            (
                get("agent.max_usd_per_day"),
                get("agent.max_minutes_per_day"),
                get("agent.max_tokens_per_day"),
                said,
            )
        };
        let some = |s: &str| Some(s.to_string());
        let (usd, min, tok, said) = budget("claude", "\n\n1\n");
        assert_eq!((usd, min, tok), (some("0"), some("120"), some("0")));
        assert!(
            said.contains("1) subscription (Claude Pro/Max, ChatGPT…)"),
            "{said}"
        );
        let (usd, min, tok, said) = budget("claude", "\n\n2\n");
        assert_eq!((usd, min, tok), (some("5"), some("0"), some("0")));
        assert!(
            said.contains("Daily budget: no time limit, $5, no LLM token limit (monthly: 30×)"),
            "{said}"
        );
        let (usd, min, tok, _) = budget("claude", "\n\nself-hosted\n");
        assert_eq!((usd, min, tok), (some("0"), some("240"), some("0")));
        // Codex reports no cost: no dollars, LLM tokens for an API key.
        let (usd, min, tok, said) = budget("codex", "\n\n2\n");
        assert_eq!((usd, min, tok), (None, some("0"), some("5000000")));
        assert!(
            said.contains(
                "Daily budget: no time limit, 5M LLM tokens (monthly: 30×; Codex reports no \
                 cost: its runs per day are capped too)"
            ),
            "{said}"
        );
        // OpenClaw: not asked how it is paid; minutes only, 120 by default
        // since 0.4.11.
        let (usd, min, tok, said) = budget("openclaw", "");
        assert_eq!((usd, min, tok), (None, some("120"), None));
        assert!(!said.contains(PAID_QUESTION), "{said}");
        assert!(
            said.contains("Daily budget: 120 min (monthly: 30×; OpenClaw keeps its own"),
            "{said}"
        );
        // A wrong answer is asked again.
        let (_, min, _, said) = budget("claude", "\n\n4\n3\n");
        assert_eq!(min, some("240"));
        assert!(said.contains("answer 1, 2 or 3"), "{said}");
    }

    /// The daily budget line: Enter accepts it, or `name=value` items
    /// change some of it (0 or none: no limit); a wrong line is asked
    /// again.
    #[test]
    fn the_budget_line_is_editable() {
        let sub = preset(Paid::Subscription, "claude");
        assert_eq!(
            parse_daily("min=90 usd=3 tokens=2M", sub, "claude"),
            Ok(Daily {
                usd: 3.0,
                minutes: 90,
                tokens: 2_000_000
            })
        );
        assert_eq!(
            parse_daily("min=2h, tokens=500k", sub, "claude"),
            Ok(Daily {
                usd: 0.0,
                minutes: 120,
                tokens: 500_000
            })
        );
        assert_eq!(
            parse_daily("usd=$2.50 min=none", sub, "claude"),
            Ok(Daily {
                usd: 2.5,
                minutes: 0,
                tokens: 0
            })
        );
        assert_eq!(parse_daily("", sub, "claude"), Ok(sub));
        assert!(
            parse_daily("90", sub, "claude")
                .unwrap_err()
                .contains("name=value")
        );
        assert!(parse_daily("min=lots", sub, "claude").is_err());
        assert!(parse_daily("usd=-1", sub, "claude").is_err());
        assert!(parse_daily("hours=2", sub, "claude").is_err());
        assert!(
            parse_daily("usd=3", sub, "codex")
                .unwrap_err()
                .contains("reports no cost")
        );
        assert!(
            parse_daily("tokens=1M", sub, "openclaw")
                .unwrap_err()
                .contains("reports no LLM tokens")
        );
        // Through the wizard: an error, then a change.
        let (choices, said) = run(
            &found(&["claude"]),
            &Settings::default(),
            "\n\n1\nmin=lots\nmin=45 usd=1.5\n",
        );
        assert!(
            said.contains("min=lots: a whole number of minutes"),
            "{said}"
        );
        assert!(
            said.contains("  daily budget: 45 min, $1.50, no LLM token limit"),
            "{said}"
        );
        assert!(
            choices
                .values
                .contains(&("agent.max_minutes_per_day", "45".into()))
        );
        assert!(
            choices
                .values
                .contains(&("agent.max_usd_per_day", "1.5".into()))
        );
        assert_eq!(
            describe_daily(&preset(Paid::ApiKey, "claude"), "claude"),
            "no time limit, $5, no LLM token limit"
        );
    }

    /// A config.toml of 0.4.3 that sets the old defaults (timeout 10m,
    /// max_turns 10): the wizard offers to drop them, in quick mode too;
    /// yes unsets them, no keeps them. `wats status` and the node's start
    /// name them in one line, and config.toml is never rewritten for it.
    #[test]
    fn old_defaults_are_offered_to_drop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agents.claude]\nruntime = \"claude\"\ntimeout = \"10m\"\nmax_turns = 10\n\
             max_usd_per_day = 2.0\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        let notice = old_defaults_notice(&settings).unwrap();
        assert_eq!(
            notice,
            "config.toml sets agent.timeout = 10m and agent.max_turns = 10: the defaults of \
             wats 0.4.4 and older; the defaults are now 25m and 50. To use them: wats setup, or \
             wats config unset agent.timeout agent.max_turns"
        );
        // Quick, yes to drop them (Enter), the budget as it was (API key,
        // $2 a day: Enter, Enter).
        let (choices, said) = run(&found(&["claude"]), &settings, "\n\n\n\n\n");
        assert!(
            said.contains(
                "config.toml sets agent.timeout = 10m and agent.max_turns = 10: the defaults of \
                 wats 0.4.4 and older, now 25m and 50. Drop them, to use the new defaults? [Y/n]"
            ),
            "{said}"
        );
        assert!(said.contains(&format!("{PAID_QUESTION} [2]: ")), "{said}");
        // 120 minutes: the default since 0.4.11.
        assert!(said.contains("Daily budget: 120 min, $2,"), "{said}");
        apply(dir.path(), &choices).unwrap();
        let after = Settings::load(dir.path()).unwrap();
        let claude = &after.agents["claude"];
        assert_eq!((claude.timeout.as_deref(), claude.max_turns), (None, None));
        // $2 is the default: not written either.
        assert_eq!(claude.max_usd_per_day, None);
        assert_eq!(old_defaults_notice(&after), None);
        // No: kept as they are; detailed shows them as the defaults.
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agents.claude]\nruntime = \"claude\"\ntimeout = \"10m\"\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        let (choices, said) = run(&found(&["claude"]), &settings, "\nd\nn\n");
        assert!(
            said.contains("Drop it, to use the new defaults? [Y/n]"),
            "{said}"
        );
        assert!(said.contains("(agent.timeout) [10m]"), "{said}");
        apply(dir.path(), &choices).unwrap();
        let after = Settings::load(dir.path()).unwrap();
        assert_eq!(after.agents["claude"].timeout.as_deref(), Some("10m"));
        assert_eq!(
            old_defaults_notice(&after).unwrap(),
            "config.toml sets agent.timeout = 10m: the default of wats 0.4.4 and older; the \
             default is now 25m. To use it: wats setup, or wats config unset agent.timeout"
        );
        // Several agents: named by their table.
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agents.a]\nruntime = \"claude\"\nmax_turns = 10\n\n\
             [agents.b]\nruntime = \"codex\"\nmax_turns = 12\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert!(
            old_defaults_notice(&settings)
                .unwrap()
                .ends_with("wats config unset agents.a.max_turns"),
        );
        // Values other than the old defaults: nothing said.
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agents.claude]\nruntime = \"claude\"\ntimeout = \"15m\"\nmax_turns = 50\n",
        )
        .unwrap();
        assert_eq!(
            old_defaults_notice(&Settings::load(dir.path()).unwrap()),
            None
        );
    }

    /// The wizard writes only what differs from the defaults: a new
    /// config.toml sets the budget it asked and nothing else; detailed
    /// answers equal to the defaults leave their keys commented.
    #[test]
    fn the_wizard_writes_only_changed_values() {
        let dir = tempfile::tempdir().unwrap();
        // Detailed: Enter for everything but the timeout (30m); a
        // subscription.
        let (choices, _) = run(
            &found(&["claude"]),
            &Settings::default(),
            "\nd\n\n30m\n\n1\n\n\n\n",
        );
        apply(dir.path(), &choices).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        let set: Vec<&str> = text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['))
            .collect();
        assert_eq!(
            set,
            [
                "runtime = \"claude\"",
                "timeout = \"30m\"",
                "max_usd_per_day = 0.0",
            ],
            "{text}"
        );
        // Again, quick, with Enter: nothing changes but what is asked
        // (the same budget).
        let before = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        let (choices, said) = run(&found(&["claude"]), &settings, "");
        assert!(said.contains("Quick keeps your current settings"), "{said}");
        apply(dir.path(), &choices).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            before
        );
    }

    /// Step 2: all the agent CLIs found at once (Enter), one by one, or
    /// none; step 3: the background (Enter, recommended), or choose.
    #[test]
    fn steps_two_and_three_are_quick_or_detailed() {
        let labels = ["Claude Code".to_string(), "Codex".to_string()];
        let hosts = |input: &str| {
            let mut out = Vec::new();
            let answer = ask_hosts(&mut input.as_bytes(), &mut out, &labels).unwrap();
            (answer, String::from_utf8(out).unwrap())
        };
        let (all, said) = hosts("");
        assert_eq!(all, Hosts::All);
        assert!(
            said.contains(
                "Set up wats in all the agent CLIs found (Claude Code, Codex): MCP server + \
                 session hook? [Y = all / d = one by one / n = none]: "
            ),
            "{said}"
        );
        assert_eq!(hosts("d\n").0, Hosts::OneByOne);
        assert_eq!(hosts("n\n").0, Hosts::None);
        let (again, said) = hosts("what\ny\n");
        assert_eq!(again, Hosts::All);
        assert!(
            said.contains("answer y (all), d (one by one) or n (none)"),
            "{said}"
        );

        let place = |input: &str, here: bool| {
            let mut out = Vec::new();
            let answer = ask_where(&mut input.as_bytes(), &mut out, here).unwrap();
            (answer, String::from_utf8(out).unwrap())
        };
        let (bg, said) = place("", true);
        assert_eq!(bg, Where::Background);
        assert!(
            said.contains("[Y = background, recommended / d = choose]: "),
            "{said}"
        );
        assert_eq!(place("n\n", true).0, Where::Here);
        assert_eq!(place("n\n", false).0, Where::NotNow);
        let (here, said) = place("d\n2\n", true);
        assert_eq!(here, Where::Here);
        assert!(
            said.contains("  2) here, in this terminal, until Ctrl-C"),
            "{said}"
        );
        assert_eq!(place("d\n3\n", true).0, Where::NotNow);
        assert_eq!(place("d\n\n", true).0, Where::Background);
        let (later, said) = place("d\n5\n2\n", false);
        assert_eq!(later, Where::NotNow);
        assert!(said.contains("answer 1 to 2"), "{said}");
    }

    #[test]
    fn scripted_answers_are_checked() {
        let input = "7\n2\nmaybe\nd\n20G\n1h\nmany\n5\n2\nusd=1\nmin=90 tokens=2M\nall\n\
                     swarm-only\npublic,acme\n";
        let (choices, said) = run(&found(&["claude", "codex"]), &Settings::default(), input);
        assert_eq!(choices.runtime.as_deref(), Some("codex"));
        assert!(said.contains("answer a number above"), "{said}");
        assert!(said.contains("answer quick or detailed"), "{said}");
        assert!(said.contains("not an integer"), "{said}");
        assert!(said.contains("usd: codex reports no cost"), "{said}");
        assert!(said.contains("(agent.access) [workspace]"), "{said}");
        assert!(said.contains("swarm-only, workspace, inherit"), "{said}");
        assert_eq!(
            choices.values,
            [
                ("storage.max", "20G".to_string()),
                ("agent.timeout", "1h".into()),
                ("agent.max_turns", "5".into()),
                ("agent.max_minutes_per_day", "90".into()),
                ("agent.max_tokens_per_day", "2000000".into()),
                ("agent.access", "swarm-only".into()),
                ("nets", "public,acme".into()),
            ]
        );
        let dir = tempfile::tempdir().unwrap();
        apply(dir.path(), &choices).unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        // The new form (ADR 0021): [agents.codex].
        let codex = &settings.agents["codex"];
        assert_eq!(codex.runtime.as_deref(), Some("codex"));
        assert_eq!(settings.storage.max.as_deref(), Some("20G"));
        assert_eq!(codex.max_turns, Some(5));
        assert_eq!(codex.max_minutes_per_day, Some(90));
        assert_eq!(codex.max_tokens_per_day, Some(2_000_000));
        assert_eq!(codex.max_usd_per_day, None);
        assert_eq!(codex.access.as_deref(), Some("swarm-only"));
        assert_eq!(settings.nets, ["public", "acme"]);
        // Again on the same file: the current values are the defaults, and
        // choosing no agent turns it off, keeping the rest.
        let (again, said) = run(&found(&["claude", "codex"]), &settings, "0\nd\n\n\n");
        assert!(said.contains("[codex]") && said.contains("[20G]"), "{said}");
        assert_eq!(again.runtime, None);
        apply(dir.path(), &again).unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert!(settings.enabled_agents().is_empty());
        assert_eq!(settings.agents["codex"].max_turns, Some(5));
        assert_eq!(settings.nets, ["public", "acme"]);
        // Claude this time: codex is kept, off; claude runs.
        let (claude, _) = run(&found(&["claude", "codex"]), &settings, "1\n");
        apply(dir.path(), &claude).unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        assert_eq!(
            settings.enabled_agents(),
            [("claude".to_string(), "claude".to_string())]
        );
        assert_eq!(settings.agents["codex"].enabled, Some(false));
    }

    /// An old `[agent]` file: `wats setup` rewrites it as
    /// `[agents.<runtime>]`, keeping its profile.
    #[test]
    fn setup_rewrites_the_old_agent_table() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agent]\nruntime = \"codex\"\nmax_turns = 7\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        let (choices, _) = run(&found(&["codex"]), &settings, "\n\n\n\n\n\n\n");
        assert_eq!(choices.runtime.as_deref(), Some("codex"));
        apply(dir.path(), &choices).unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        let codex = &settings.agents["codex"];
        assert_eq!(codex.max_turns, Some(7));
        assert_eq!(codex.legacy_dir, Some(true));
        assert!(
            codex
                .profile
                .as_deref()
                .is_some_and(|p| p.ends_with("-agent"))
        );
        assert!(settings.agent.runtime.is_none());
    }

    /// OpenClaw is offered as an agent; its spend caps are its own, so the
    /// USD budgets and turns are not asked.
    #[test]
    fn openclaw_is_offered_as_an_agent() {
        let (choices, said) = run(&found(&["openclaw"]), &Settings::default(), "");
        assert!(said.contains("1) openclaw"), "{said}");
        assert!(!said.contains("not supported yet"), "{said}");
        assert!(said.contains(crate::openclaw::SPEND_CAPS), "{said}");
        assert_eq!(choices.runtime.as_deref(), Some("openclaw"));
        let keys: Vec<_> = choices.values.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["agent.max_minutes_per_day"]);
        let (detailed, _) = run(&found(&["openclaw"]), &Settings::default(), "\nd\n");
        let keys: Vec<_> = detailed.values.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "storage.max",
                "agent.timeout",
                "agent.max_minutes_per_day",
                "nets"
            ]
        );
        let text = render(&choices).unwrap();
        assert!(text.contains("\nruntime = \"openclaw\"\n"), "{text}");
    }

    #[test]
    fn no_supported_agent_found() {
        let (choices, said) = run(&[], &Settings::default(), "");
        assert!(said.contains(NO_AGENT_FOUND), "{said}");
        assert_eq!(choices.runtime, None);
        // No agent, quick: nothing asked; detailed: no agent limits.
        assert!(choices.values.is_empty());
        let (choices, _) = run(&[], &Settings::default(), "d\n");
        let keys: Vec<_> = choices.values.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["storage.max", "nets"]);
    }

    #[test]
    fn default_config_is_commented_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_default(dir.path()).unwrap().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines()
                .all(|l| l.is_empty() || l.starts_with('#') || l.starts_with('[')),
            "{text}"
        );
        assert!(text.contains("# nets = [\"public\"]"), "{text}");
        let settings = Settings::load(dir.path()).unwrap();
        assert!(settings.agent.runtime.is_none() && settings.nets.is_empty());
        // Never overwritten.
        std::fs::write(&path, "profile = \"alice\"\n").unwrap();
        assert_eq!(write_default(dir.path()).unwrap(), None);
        assert!(std::fs::read_to_string(&path).unwrap().contains("alice"));
    }

    /// The setup revision of a data directory: none before a config.toml,
    /// the baseline for one set up before revisions were recorded (an
    /// upgraded 0.4.x node: not asked again), the current one after a
    /// setup; a deleted config.toml is a new start.
    #[test]
    fn setup_revision_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(stored_revision(dir.path()), None);
        write_default(dir.path()).unwrap();
        assert_eq!(stored_revision(dir.path()), Some(BASELINE_REVISION));
        // Revision 1 (0.4.3): the questions of 0.4.2, plus agent.access,
        // whose default every 0.4.x node already runs with. Revision 2
        // (0.4.5): the budget question, asked of nodes with an agent.
        assert_eq!(
            new_questions(BASELINE_REVISION),
            1,
            "0.4.x nodes are asked the budget"
        );
        assert_eq!(
            pending_questions(&Settings::default(), BASELINE_REVISION),
            0
        );
        setup_with(dir.path(), &mut "".as_bytes(), &mut Vec::new(), None, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(REVISION_FILE)).unwrap(),
            format!("{SETUP_REVISION}\n")
        );
        assert_eq!(stored_revision(dir.path()), Some(SETUP_REVISION));
        std::fs::write(dir.path().join(REVISION_FILE), "garbage").unwrap();
        assert_eq!(stored_revision(dir.path()), Some(BASELINE_REVISION));
        std::fs::remove_file(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(stored_revision(dir.path()), None);
        // Every question of today is in revision 1: revision 0 has them all.
        assert_eq!(new_questions(0), QUESTIONS.len() + 3);
    }

    /// A data directory set up at an older revision: only the questions
    /// added since are asked (here a made-up revision 2 question), the
    /// agent runtime is not, and the agents stay as they were.
    #[test]
    fn catch_up_asks_only_the_new_questions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agents.reviewer]\nruntime = \"codex\"\nmax_turns = 7\n\n\
             [agents.claude]\nruntime = \"claude\"\nenabled = false\n",
        )
        .unwrap();
        let settings = Settings::load(dir.path()).unwrap();
        let questions = [
            question("storage.max", "Disk budget", false),
            Question {
                since: 2,
                ..question("agent.timeout", "Time limit of one agent run", true)
            },
            Question {
                since: 2,
                ..question("nets", "Networks", false)
            },
        ];
        let mut out = Vec::new();
        let choices = prompt_after(
            &mut "1h\n".as_bytes(),
            &mut out,
            &found(&["claude", "codex"]),
            &settings,
            &questions,
            1,
            &Ctx {
                full: false,
                ..Ctx::person()
            },
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!said.contains("agent.runtime"), "{said}");
        assert!(!said.contains("storage.max"), "{said}");
        assert!(said.contains("(agent.timeout) [25m]"), "{said}");
        assert!(said.contains("(nets) [public]"), "{said}");
        assert!(!said.contains(MODE_QUESTION), "{said}");
        assert!(choices.keep_runtime);
        assert_eq!(choices.runtime.as_deref(), Some("codex"));
        assert_eq!(
            choices.values,
            [("agent.timeout", "1h".into()), ("nets", "public".into())]
        );
        apply(dir.path(), &choices).unwrap();
        let after = Settings::load(dir.path()).unwrap();
        assert_eq!(after.agents["reviewer"].timeout.as_deref(), Some("1h"));
        assert_eq!(after.agents["reviewer"].max_turns, Some(7));
        assert_eq!(after.agents["reviewer"].enabled, None);
        assert_eq!(after.agents["claude"].enabled, Some(false));
        assert_eq!(
            after.enabled_agents(),
            [("reviewer".to_string(), "codex".to_string())]
        );
        // Revision 2 already: nothing is asked.
        let mut out = Vec::new();
        let none = prompt_after(
            &mut "".as_bytes(),
            &mut out,
            &[],
            &after,
            &questions,
            2,
            &Ctx {
                full: false,
                ..Ctx::person()
            },
        )
        .unwrap();
        assert!(none.values.is_empty() && out.is_empty());
    }

    /// The real catch-up from revision 0 (older than every question): the
    /// main choices, written, and the revision recorded; no hint to run
    /// `wats setup <tool> --mcp`.
    #[test]
    fn catch_up_records_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        write_default(dir.path()).unwrap();
        std::fs::write(dir.path().join(REVISION_FILE), "0\n").unwrap();
        let mut out = Vec::new();
        catch_up_with(
            dir.path(),
            &mut "20G\n".as_bytes(),
            &mut out,
            None,
            0,
            false,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("setup revision 0, now 2"), "{said}");
        assert!(!said.contains(MODE_QUESTION), "{said}");
        assert!(!said.contains("--mcp"), "{said}");
        assert_eq!(
            Settings::load(dir.path()).unwrap().storage.max.as_deref(),
            Some("20G")
        );
        assert_eq!(stored_revision(dir.path()), Some(SETUP_REVISION));
    }

    /// A 0.4.3 data directory with an agent (revision 1): the catch-up
    /// asks the budget question only; without an agent, nothing, and the
    /// revision is recorded as is.
    #[test]
    fn catch_up_from_revision_one_asks_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[agents.claude]\nruntime = \"claude\"\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let choices = catch_up_with(
            dir.path(),
            &mut "3\n\n".as_bytes(),
            &mut out,
            None,
            1,
            false,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("setup revision 1, now 2"), "{said}");
        assert!(said.contains(PAID_QUESTION), "{said}");
        assert!(!said.contains("(storage.max)"), "{said}");
        assert_eq!(
            choices.values,
            [
                ("agent.max_minutes_per_day", "240".to_string()),
                ("agent.max_usd_per_day", "0".into()),
                ("agent.max_tokens_per_day", "0".into()),
            ]
        );
        let after = Settings::load(dir.path()).unwrap();
        assert_eq!(after.agents["claude"].max_minutes_per_day, Some(240));
        assert_eq!(stored_revision(dir.path()), Some(2));
        // No agent: nothing asked, the revision recorded.
        let none = tempfile::tempdir().unwrap();
        write_default(none.path()).unwrap();
        let mut out = Vec::new();
        catch_up_with(none.path(), &mut "".as_bytes(), &mut out, None, 1, false).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        assert_eq!(stored_revision(none.path()), Some(2));
    }

    /// USD budgets of 0 from before 0.4.5 (revision 1), when they paused
    /// their agents, in two agents' tables and the old [agent] one (which
    /// `wats --agent claude` runs): each agent's budget question confirms
    /// that agent only (its marker), and revision 2 is recorded once none
    /// is left. A question left unanswered keeps that agent paused and the
    /// revision as it was.
    #[test]
    fn each_zero_from_before_is_confirmed_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(
            d.join(FILE_NAME),
            "[agent]\nmax_usd_per_day = 0.0\n\n\
             [agents.claude]\nruntime = \"claude\"\nmax_usd_per_day = 0.0\nmax_usd_per_month = 0.0\n\n\
             [agents.other]\nruntime = \"codex\"\nmax_usd_per_day = 0.0\n",
        )
        .unwrap();
        let settings = Settings::load(d).unwrap();
        let stored = stored_revision(d);
        assert_eq!(stored, Some(1));
        let zeros = zero_tables(&settings, d, stored);
        let names: Vec<&str> = zeros.iter().map(|z| z.name.as_str()).collect();
        assert_eq!(names, ["claude", "other", "agent"], "{zeros:?}");
        // The catch-up: the first agent (claude), a subscription, then the
        // others in turn; `other` answered, the old [agent] one not (the
        // end of the input).
        let mut out = Vec::new();
        let choices =
            catch_up_with(d, &mut "1\n\n3\n\n".as_bytes(), &mut out, None, 1, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains(
                "Note: agent.max_usd_per_day = 0 paused the agent before 0.4.5 and now means no \
                 limit: the daily budget below replaces it; agent.max_usd_per_month = 0 paused \
                 it before 0.4.5 and now means no monthly limit"
            ),
            "{said}"
        );
        // No default for how it is paid: the answer is typed.
        assert!(
            said.contains(&format!("{PAID_QUESTION} [1/2/3]: ")),
            "{said}"
        );
        assert!(
            said.contains("Daily budget: 120 min, no USD limit"),
            "{said}"
        );
        assert!(
            said.contains("The agent other (Codex, [agents.other]) also has a USD budget of 0"),
            "{said}"
        );
        assert!(
            said.contains("Note: agents.other.max_usd_per_day = 0 paused the agent"),
            "{said}"
        );
        assert!(
            said.contains(
                "warning: agent: a USD budget of 0 from before 0.4.5 keeps the agent agent paused"
            ),
            "{said}"
        );
        assert_eq!(choices.budget_for.as_deref(), Some("claude"));
        // claude and other confirmed, the old table not: revision 1 kept.
        let after = Settings::load(d).unwrap();
        // 120: the default since 0.4.11, so not written.
        assert_eq!(
            crate::budget::Budget::of(&after.agents["claude"]).max_minutes_per_day,
            Some(120)
        );
        assert_eq!(after.agents["other"].max_minutes_per_day, Some(240));
        assert!(crate::budget::confirmed(&d.join("agent/claude")));
        assert!(crate::budget::confirmed(&d.join("agent/other")));
        assert!(!crate::budget::confirmed(&d.join("agent")));
        assert_eq!(stored_revision(d), Some(1));
        let left = zero_tables(&after, d, stored_revision(d));
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].prefix, "agent");
        assert!(
            crate::budget::Budget::of_dir(
                &after.agents["claude"],
                Some(1),
                &d.join("agent/claude")
            )
            .zero_notice("claude")
            .is_none()
        );
        assert!(
            crate::budget::Budget::of_dir(&after.agent, Some(1), &d.join("agent")).paused_by_zero()
        );
        // Again: only the old table is asked; answered, revision 2.
        let mut out = Vec::new();
        catch_up_with(d, &mut "2\n\n".as_bytes(), &mut out, None, 1, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!said.contains("agents.other.max_usd_per_day = 0"), "{said}");
        assert!(
            said.contains("The agent agent (Claude Code, [agent])"),
            "{said}"
        );
        assert_eq!(stored_revision(d), Some(2));
        assert_eq!(Settings::load(d).unwrap().agent.max_usd_per_day, Some(5.0));

        // Only a disabled agent's zero: asked, and at the end of the input
        // the revision is not recorded.
        let off = tempfile::tempdir().unwrap();
        std::fs::write(
            off.path().join(FILE_NAME),
            "[agents.claude]\nruntime = \"claude\"\nenabled = false\nmax_usd_per_day = 0.0\n",
        )
        .unwrap();
        let mut out = Vec::new();
        catch_up_with(off.path(), &mut "".as_bytes(), &mut out, None, 1, false).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("keeps the agent claude paused"), "{said}");
        assert_eq!(stored_revision(off.path()), Some(1));
    }

    /// Without a terminal (`wats setup </dev/null`): the budget and the old
    /// defaults are not asked, nothing of them changes, a USD budget of 0
    /// from before 0.4.5 stays a pause, and the revision stays.
    #[test]
    fn without_a_terminal_the_budget_stays_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let text = "[agents.claude]\nruntime = \"claude\"\ntimeout = \"10m\"\nmax_turns = 10\n\
                    max_usd_per_day = 0.0\n";
        std::fs::write(d.join(FILE_NAME), text).unwrap();
        let piped = Asker {
            install: Install::Release,
            terminal: false,
            machine: Some(crate::naming::Machine::default()),
            api_key: |_| None,
        };
        for input in ["", "\n\n\n\n\n", "y\n2\nusd=9\n"] {
            let mut out = Vec::new();
            let (choices, _) = ask_and_write(
                d,
                &mut input.as_bytes(),
                &mut out,
                None,
                false,
                piped.clone(),
                0,
                Run::Setup,
            )
            .unwrap();
            let said = String::from_utf8(out).unwrap();
            assert!(said.contains(BUDGET_NOT_ASKED), "{said}");
            assert!(!said.contains(PAID_QUESTION), "{said}");
            assert!(!said.contains("Drop them"), "{said}");
            assert_eq!(choices.budget_for, None);
            assert_eq!(std::fs::read_to_string(d.join(FILE_NAME)).unwrap(), text);
            assert_eq!(stored_revision(d), Some(1));
            assert!(!crate::budget::confirmed(&d.join("agent/claude")));
        }
    }

    /// `wats setup <runtime> --agent` (0.4.11): the wizard's budget
    /// question for one agent nobody budgeted, written to its table and
    /// confirmed; the end of the input writes nothing.
    #[test]
    fn setup_agent_asks_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join(FILE_NAME), "[agents.codex]\nruntime = \"codex\"\n").unwrap();
        let work = d.join("agent/codex");
        let table = settings::AgentSettings::default();
        assert!(!has_budget(&table));
        let mut out = Vec::new();
        let line = ask_agent_budget(
            d,
            &mut "3\n\n".as_bytes(),
            &mut out,
            "codex",
            "codex",
            &table,
            &work,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            line,
            "agents.codex: max_minutes_per_day = 240, max_tokens_per_day = 0"
        );
        let after = Settings::load(d).unwrap();
        assert_eq!(after.agents["codex"].max_minutes_per_day, Some(240));
        assert!(has_budget(&after.agents["codex"]));
        assert!(crate::budget::confirmed(&work));
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains(PAID_QUESTION), "{said}");
        // No answer: nothing written.
        let none = ask_agent_budget(
            d,
            &mut "".as_bytes(),
            &mut Vec::new(),
            "x",
            "codex",
            &table,
            &d.join("agent/x"),
        )
        .unwrap();
        assert_eq!(none, None);
    }

    /// How the agent is paid has no default, but the answer config.toml
    /// already says, or "API key" when the runtime's key is in the
    /// environment (said in one line); not for OpenCode.
    #[test]
    fn the_budget_question_defaults_to_an_api_key_in_the_environment() {
        let ask = |runtime: &str, api_key: Option<&'static str>, input: &str| {
            let mut out = Vec::new();
            let values = ask_budget(
                &mut input.as_bytes(),
                &mut out,
                &settings::AgentSettings::default(),
                runtime,
                (false, false),
                "agent",
                api_key,
            )
            .unwrap();
            (values, String::from_utf8(out).unwrap())
        };
        let (values, said) = ask("claude", Some("ANTHROPIC_API_KEY"), "\n\n");
        assert!(
            said.contains(
                "ANTHROPIC_API_KEY is set: Claude Code bills that API key per use, so 2 is the \
                 default."
            ),
            "{said}"
        );
        assert!(said.contains(&format!("{PAID_QUESTION} [2]: ")), "{said}");
        assert!(
            values
                .unwrap()
                .contains(&("agent.max_usd_per_day", "5".into()))
        );
        // No key: Enter is no answer, asked again; the end of the input is
        // none at all.
        let (values, said) = ask("claude", None, "\n");
        assert!(said.contains("answer 1, 2 or 3"), "{said}");
        assert_eq!(values, None);
        assert_eq!(env_api_key("opencode"), None);
        assert_eq!(env_api_key("openclaw"), None);
    }
}

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
//! agent's run time, turn and spend limits, its access level, and `nets`.
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

/// The agent CLIs looked for on `PATH`. Only [`RUNTIMES`] can run; the
/// others (Mistral Vibe) are listed as MCP hosts.
pub const AGENT_CLIS: [&str; 4] = ["claude", "codex", "openclaw", "vibe"];

/// Shown when the node runs no agent (startup, `status`). A future
/// policy: no quota is enforced today.
pub const NO_AGENT_NOTICE: &str = "This node runs no agent: nodes that don't contribute \
(answers, storage, uptime) will get a limited quota in a future version. `wats setup` \
enables one.";

/// Shown by setup when no supported agent CLI is on `PATH`.
pub const NO_AGENT_FOUND: &str = "No agent CLI found: install Claude Code \
(https://claude.com/claude-code) or Codex (https://github.com/openai/codex), then run \
`wats setup` again.";

/// Shown by setup when OpenClaw is on `PATH` and not the node's agent: its
/// agents can use the wats tools.
pub const OPENCLAW_MCP_HINT: &str = "OpenClaw found: to reach the swarm from your OpenClaw \
agents, register wats as its MCP server: wats setup openclaw --mcp";

/// Shown by setup when Mistral Vibe is on `PATH`: not an agent runtime
/// yet, but an MCP host.
pub const VIBE_MCP_HINT: &str = "Mistral Vibe found: to reach the swarm from your Vibe \
sessions, add wats as its MCP server: wats setup vibe --mcp (running it as the node's agent \
comes next)";

/// Shown by setup when OpenClaw is the node's agent: the gateway side is
/// set up apart (tokens, MCP entries, the config block to paste).
pub const OPENCLAW_AGENT_NEXT: &str = "`wats` now forwards the agent's runs to OpenClaw's \
gateway. Finish with: wats setup openclaw --agent (the hook token, the restricted `wats` \
OpenClaw agent and its MCP entry)";

/// Keys not asked for OpenClaw: it reports no cost and counts its own
/// turns (ADR 0017 §2), and its access level goes with its allow list and
/// review (`wats setup openclaw --agent` checks them together).
const NOT_FOR_OPENCLAW: [&str; 4] = [
    "agent.max_turns",
    "agent.max_usd_per_day",
    "agent.max_usd_per_month",
    "agent.access",
];

/// The revision of setup's questions (ADR 0027). A release bumps it only
/// when it adds a question, which then carries the new value as its
/// `since`: `wats` on a terminal asks the questions newer than the
/// revision a data directory last completed ([`catch_up`]), and says so in
/// one log line elsewhere. Not tied to the version number.
pub const SETUP_REVISION: u32 = 1;

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

/// One question after the agent runtime.
#[derive(Debug, Clone, Copy)]
struct Question {
    key: &'static str,
    label: &'static str,
    /// Only asked when the node runs an agent.
    agent_only: bool,
    /// The setup revision that added it.
    since: u32,
}

const fn question(key: &'static str, label: &'static str, agent_only: bool) -> Question {
    Question {
        key,
        label,
        agent_only,
        since: 1,
    }
}

/// The questions after the agent runtime, in order: storage, the agent's
/// limits and access (only asked when it runs an agent), networks.
const QUESTIONS: [Question; 7] = [
    question("storage.max", "Disk budget of the data directory", false),
    question("agent.timeout", "Time limit of one agent run", true),
    question("agent.max_turns", "Turns per agent run", true),
    question(
        "agent.max_usd_per_day",
        "Agent spend budget per UTC day, in USD",
        true,
    ),
    question(
        "agent.max_usd_per_month",
        "Agent spend budget per UTC month, in USD",
        true,
    ),
    question(
        "agent.access",
        "What the agent may use besides the wats tools: swarm-only, workspace (its own \
         directory) or inherit (your whole environment)",
        true,
    ),
    question("nets", "Networks to join, comma-separated", false),
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
    [RUNTIME_SINCE, DESKTOP_SINCE]
        .into_iter()
        .chain(QUESTIONS.iter().map(|q| q.since))
        .filter(|since| *since > revision)
        .count()
}

/// An agent CLI found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: &'static str,
    pub path: PathBuf,
}

impl Found {
    /// Whether wats can run it (an agent runtime).
    pub fn supported(&self) -> bool {
        RUNTIMES.contains(&self.name)
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
/// one's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choices {
    pub runtime: Option<String>,
    pub values: Vec<(&'static str, String)>,
    pub keep_runtime: bool,
}

/// The value of `key` in `settings`, as `wats config set` takes it.
fn current(settings: &Settings, key: &str) -> Option<String> {
    let agent = settings
        .primary_agent()
        .map_or(&settings.agent, |(_, table)| table);
    match key {
        "storage.max" => settings.storage.max.clone(),
        "agent.timeout" => agent.timeout.clone(),
        "agent.max_turns" => agent.max_turns.map(|n| n.to_string()),
        "agent.max_usd_per_day" => agent.max_usd_per_day.map(|n| n.to_string()),
        "agent.max_usd_per_month" => agent.max_usd_per_month.map(|n| n.to_string()),
        "agent.access" => agent.access.clone(),
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
                writeln!(out, "  {n}) {:<9} {}", f.name, f.path.display())?;
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
        .unwrap_or_else(|| supported[0].name.to_string());
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
            Ok(i) => supported.get(i - 1).map(|f| f.name),
            Err(_) if answer == "none" => Some("none"),
            Err(_) => RUNTIMES.iter().copied().find(|r| *r == answer),
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

/// Ask the main choices, with the current settings (or the defaults) as
/// defaults. `found`: [`detect_agents`].
pub fn prompt(
    input: &mut impl BufRead,
    out: &mut impl Write,
    found: &[Found],
    settings: &Settings,
) -> Result<Choices> {
    writeln!(
        out,
        "wats setup: the node's main choices. Enter accepts the [default]."
    )?;
    prompt_after(input, out, found, settings, &QUESTIONS, 0)
}

/// Ask those of `questions` newer than `revision` (0: all of them), after
/// the agent runtime when it is newer too.
fn prompt_after(
    input: &mut impl BufRead,
    out: &mut impl Write,
    found: &[Found],
    settings: &Settings,
    questions: &[Question],
    revision: u32,
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
    let mut values = Vec::new();
    for q in questions {
        if q.since <= revision
            || (q.agent_only && runtime.is_none())
            || (openclaw && NOT_FOR_OPENCLAW.contains(&q.key))
        {
            continue;
        }
        let default = current(settings, q.key).unwrap_or_else(|| {
            settings::find_key(q.key)
                .map_or("", |k| k.default)
                .to_string()
        });
        values.push((q.key, ask(input, out, q.key, q.label, &default)?));
    }
    Ok(Choices {
        runtime,
        values,
        keep_runtime,
    })
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
         # name defaults to the runtime: claude, codex or openclaw.\n",
    );
    match &choices.runtime {
        Some(runtime) => {
            crate::roster::check_name(runtime).map_err(anyhow::Error::msg)?;
            text.push_str(&format!(
                "[agents.{runtime}]\nruntime = {}\n",
                settings::check_value("agent.runtime", runtime)?
            ));
            for q in QUESTIONS {
                if q.agent_only {
                    text.push('\n');
                    line(&mut text, q.key)?;
                }
            }
        }
        None => {
            // Commented: an example to uncomment.
            let mut example = String::from("[agents.claude]\nruntime = \"claude\"\n");
            for q in QUESTIONS {
                if q.agent_only {
                    example.push('\n');
                    line(&mut example, q.key)?;
                }
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
        if is_default(&key, value) {
            file.unset(&key)?;
        } else {
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
/// main choices on `input`/`out`, write them. Returns the choices.
pub fn setup(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
) -> Result<Choices> {
    setup_with(
        data_dir,
        input,
        out,
        path,
        crate::desktop::this_desktop_session(),
    )
}

/// [`setup`], `desktop` telling whether this machine has a desktop session
/// ([`crate::desktop::desktop_session`]): only then is `notify.desktop`
/// asked (default yes).
pub fn setup_with(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    desktop: bool,
) -> Result<Choices> {
    let (choices, found) = ask_and_write(data_dir, input, out, path, desktop, 0, true)?;
    next_steps(out, &choices, &found, true)?;
    Ok(choices)
}

/// [`setup`] for the setup wizard (ADR 0027), which asks next whether
/// to register wats in the MCP hosts found: the hints to run `wats setup
/// <tool> --mcp` are left out. Returns the choices and the agent CLIs found.
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
        0,
        false,
    )?;
    next_steps(out, &choices, &found, false)?;
    Ok((choices, found))
}

/// A data directory set up at `revision`, older than [`SETUP_REVISION`]:
/// ask only the questions added since, write them, record the revision.
pub fn catch_up(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    revision: u32,
) -> Result<Choices> {
    catch_up_with(
        data_dir,
        input,
        out,
        path,
        revision,
        crate::desktop::this_desktop_session(),
    )
}

/// [`catch_up`], `desktop` as in [`setup_with`].
pub fn catch_up_with(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    revision: u32,
    desktop: bool,
) -> Result<Choices> {
    writeln!(
        out,
        "wats setup has new questions since this data directory was set up (setup revision \
         {revision}, now {SETUP_REVISION}). Enter accepts the [default]."
    )?;
    let (choices, _) = ask_and_write(data_dir, input, out, path, desktop, revision, false)?;
    Ok(choices)
}

/// Ask the main choices newer than `revision` (0: all of them), write
/// them, and record [`SETUP_REVISION`].
fn ask_and_write(
    data_dir: &Path,
    input: &mut impl BufRead,
    out: &mut impl Write,
    path: Option<&OsStr>,
    desktop: bool,
    revision: u32,
    header: bool,
) -> Result<(Choices, Vec<Found>)> {
    let settings = Settings::load(data_dir)?;
    let found = detect_agents(path);
    let mut choices = if revision == 0 && header {
        prompt(input, out, &found, &settings)?
    } else {
        prompt_after(input, out, &found, &settings, &QUESTIONS, revision)?
    };
    if desktop && DESKTOP_SINCE > revision {
        let on = ask_yes_no(
            input,
            out,
            "Desktop notification when someone answers or clarifies one of your asks \
             (notify.desktop)",
            settings.notify.desktop.unwrap_or(true),
        )?;
        choices.values.push(("notify.desktop", on.to_string()));
    }
    let file = apply(data_dir, &choices)?;
    record_revision(data_dir)?;
    writeln!(out, "Wrote {}", file.display())?;
    Ok((choices, found))
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
            let name = if runtime == "codex" {
                "Codex"
            } else {
                "Claude Code"
            };
            write!(out, "`wats` now runs the node with a {name} agent.")?;
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
    if found.iter().any(|f| f.name == "vibe") {
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

    /// On a desktop session, setup asks notify.desktop (default yes) and
    /// writes the answer; elsewhere it does not ask.
    #[test]
    fn setup_asks_desktop_notifications_on_a_desktop() {
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup_with(data.path(), &mut "".as_bytes(), &mut out, None, true).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("(notify.desktop) [Y/n]"), "{said}");
        assert!(choices.values.contains(&("notify.desktop", "true".into())));
        let settings = Settings::load(data.path()).unwrap();
        assert_eq!(settings.notify.desktop, Some(true));
        // Asked again (no agent CLI: storage and networks first): "n"
        // turns it off.
        let mut out = Vec::new();
        setup_with(data.path(), &mut "\n\nn\n".as_bytes(), &mut out, None, true).unwrap();
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

    /// Mistral Vibe on PATH: listed, not offered as the agent, its MCP
    /// setup suggested.
    #[cfg(unix)]
    #[test]
    fn vibe_is_listed_as_mcp_only() {
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
        assert!(!found[0].supported());
        let data = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let choices = setup(data.path(), &mut "".as_bytes(), &mut out, Some(&path)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(choices.runtime, None);
        assert!(
            said.contains("(agent: not supported yet; MCP: wats setup vibe --mcp)"),
            "{said}"
        );
        assert!(said.contains(NO_AGENT_FOUND), "{said}");
        assert!(said.contains(VIBE_MCP_HINT), "{said}");
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

    #[test]
    fn enter_accepts_every_default() {
        let (choices, said) = run(&found(&["claude", "codex"]), &Settings::default(), "");
        assert_eq!(choices.runtime.as_deref(), Some("claude"));
        let keys: Vec<_> = choices.values.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, QUESTIONS.map(|q| q.key));
        assert!(said.contains("1) claude"), "{said}");
        assert!(
            said.contains("[10G]") && said.contains("[public]"),
            "{said}"
        );
        // Defaults only: nothing is set.
        let text = render(&choices).unwrap();
        assert!(text.contains("\nruntime = \"claude\"\n"), "{text}");
        assert!(text.contains("# max = \"10G\""), "{text}");
        assert!(text.contains("# max_usd_per_day = 2.0"), "{text}");
    }

    #[test]
    fn scripted_answers_are_checked() {
        let input = "7\n2\n20G\n1h\nmany\n5\n1.5\n10\nall\nswarm-only\npublic,acme\n";
        let (choices, said) = run(&found(&["claude", "codex"]), &Settings::default(), input);
        assert_eq!(choices.runtime.as_deref(), Some("codex"));
        assert!(said.contains("answer a number above"), "{said}");
        assert!(said.contains("not an integer"), "{said}");
        assert!(said.contains("(agent.access) [workspace]"), "{said}");
        assert!(said.contains("swarm-only, workspace, inherit"), "{said}");
        assert_eq!(
            choices.values,
            [
                ("storage.max", "20G".to_string()),
                ("agent.timeout", "1h".into()),
                ("agent.max_turns", "5".into()),
                ("agent.max_usd_per_day", "1.5".into()),
                ("agent.max_usd_per_month", "10".into()),
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
        assert_eq!(codex.max_usd_per_month, Some(10.0));
        assert_eq!(codex.access.as_deref(), Some("swarm-only"));
        assert_eq!(settings.nets, ["public", "acme"]);
        // Again on the same file: the current values are the defaults, and
        // choosing no agent turns it off, keeping the rest.
        let (again, said) = run(&found(&["claude", "codex"]), &settings, "0\n\n\n");
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
        assert_eq!(keys, ["storage.max", "agent.timeout", "nets"]);
        let text = render(&choices).unwrap();
        assert!(text.contains("\nruntime = \"openclaw\"\n"), "{text}");
    }

    #[test]
    fn no_supported_agent_found() {
        let (choices, said) = run(&[], &Settings::default(), "");
        assert!(said.contains(NO_AGENT_FOUND), "{said}");
        assert_eq!(choices.runtime, None);
        // No agent: no agent limits asked.
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
        // whose default every 0.4.x node already runs with.
        assert_eq!(
            new_questions(BASELINE_REVISION),
            0,
            "0.4.x nodes are asked nothing"
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
        assert_eq!(new_questions(0), QUESTIONS.len() + 2);
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
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!said.contains("agent.runtime"), "{said}");
        assert!(!said.contains("storage.max"), "{said}");
        assert!(said.contains("(agent.timeout) [10m]"), "{said}");
        assert!(said.contains("(nets) [public]"), "{said}");
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
        let none = prompt_after(&mut "".as_bytes(), &mut out, &[], &after, &questions, 2).unwrap();
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
        assert!(said.contains("setup revision 0, now 1"), "{said}");
        assert!(!said.contains("--mcp"), "{said}");
        assert_eq!(
            Settings::load(dir.path()).unwrap().storage.max.as_deref(),
            Some("20G")
        );
        assert_eq!(stored_revision(dir.path()), Some(SETUP_REVISION));
    }
}

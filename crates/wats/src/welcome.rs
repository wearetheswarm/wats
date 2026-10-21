//! The screens of the setup wizard (ADR 0027): a welcome before its first
//! question, a header per step, and a closing screen with what was set up
//! and how to talk to the node.
//!
//! Shown only when the wizard runs, which is only on a terminal (a bare
//! `wats` the first time, `wats setup` with no tool): never to a pipe, a
//! service or a log, and never before the catch-up questions after an
//! upgrade, which have a one-line header of their own. English, like the
//! rest of wats. Plain ASCII unless the locale says UTF-8; yellow for the
//! mark and bold headers only where color is welcome
//! ([`crate::banner::color_ok`], `NO_COLOR`).
//!
//! The brand lines: the headline [`HEADLINE`] ("You are the swarm.") when
//! the user starts something (the welcome, and a node's very first start
//! in a data directory, once: [`first_start`]), the motto [`MOTTO`] ("Every
//! node is the swarm.") with it and in the banner, and the signature "We
//! are the swarm." at the end of the README.

use std::ffi::OsString;

/// The mark: three cells of the hive, with the text column beside it.
const MARK: [&str; 4] = [
    "    __    __",
    "   /  \\__/  \\",
    "   \\__/  \\__/",
    "      \\__/",
];

/// Where the text column starts, beside the mark.
const TEXT_COLUMN: usize = 18;

const DOCS: &str = "https://github.com/wearetheswarm/wats/tree/main/docs";

const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// How to draw: `color` (ANSI), `unicode` (a UTF-8 locale: `·` and `─`
/// rather than `-`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
    pub unicode: bool,
}

impl Style {
    /// The style of this process's stdout.
    pub fn detect() -> Style {
        use std::io::IsTerminal;
        Style {
            color: crate::banner::color_ok(
                std::io::stdout().is_terminal(),
                std::env::var_os("NO_COLOR").as_deref(),
                std::env::var("TERM").ok().as_deref(),
            ),
            unicode: utf8_locale(|name| std::env::var_os(name)),
        }
    }

    fn dot(self) -> &'static str {
        if self.unicode { "·" } else { "-" }
    }

    fn ellipsis(self) -> &'static str {
        if self.unicode { "…" } else { "..." }
    }

    fn rule(self) -> &'static str {
        if self.unicode { "──" } else { "--" }
    }

    fn bold(self, text: &str) -> String {
        if self.color {
            format!("{BOLD}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

/// Whether the locale is UTF-8: the first of `LC_ALL`, `LC_CTYPE` and
/// `LANG` that is set says so. None set (`C`, a bare service): ASCII.
pub fn utf8_locale(var: impl Fn(&str) -> Option<OsString>) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| var(name).filter(|v| !v.is_empty()))
        .is_some_and(|v| {
            let v = v.to_string_lossy().to_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
}

/// Which wizard runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A bare `wats` the first time, in this data directory (as shown).
    FirstRun { dir: String },
    /// `wats setup` with no tool.
    Setup,
}

/// The three steps, by number.
pub const STEPS: [&str; 3] = [
    "The node's main choices",
    "Your own agent sessions",
    "Where the node runs",
];

/// The headline: the welcome screen's, and the line of a node's first
/// start ([`first_start_line`]).
pub const HEADLINE: &str = "You are the swarm.";

/// The motto that goes with it, under the headline and in the banner.
pub const MOTTO: &str = "Every node is the swarm.";

/// The welcome screen.
pub fn welcome(version: &str, kind: &Kind, style: Style) -> String {
    let dot = style.dot();
    let text = [
        String::new(),
        style.bold(HEADLINE),
        MOTTO.to_string(),
        format!("Welcome to WeAreTheSwarm {dot} wats {version}"),
    ];
    let mut out = String::from("\n");
    for (art, line) in MARK.iter().zip(&text) {
        let mut row = if style.color {
            format!("{YELLOW}{art}{RESET}")
        } else {
            art.to_string()
        };
        if !line.is_empty() {
            row.push_str(&" ".repeat(TEXT_COLUMN - art.len()));
            row.push_str(line);
        }
        out.push_str(&row);
        out.push('\n');
    }
    out.push_str(
        "\nwats connects your AI agents to a swarm shared with everyone else's: they ask,\n\
         answer and build on what the swarm already knows, from your machine and within\n\
         the limits you set. Your node keeps its own copy, and serves it to the others.\n\n",
    );
    let third = match kind {
        Kind::FirstRun { .. } => "whether it runs in the background or in this terminal",
        Kind::Setup => "whether to start it now, in the background",
    };
    out.push_str("This setup takes three steps:\n");
    out.push_str(
        "  1. The node's main choices: the agent it runs, its limits, the networks.\n\
         \x20 2. Your own agent sessions: wats in the agent CLIs found on your PATH.\n",
    );
    out.push_str(&format!("  3. The node: {third}.\n"));
    match kind {
        Kind::FirstRun { dir } => out.push_str(&format!(
            "Enter accepts the [default] of each question.\n\
             First run in {dir} (no config.toml yet).\n\
             `wats setup` runs this again; any flag (e.g. `wats --no-agent`) skips it.\n"
        )),
        Kind::Setup => {
            out.push_str("Enter accepts the [default] of each question: your current values.\n")
        }
    }
    out
}

/// The header of step `n` (1 to 3).
pub fn step(n: usize, style: Style) -> String {
    let rule = style.rule();
    let title = STEPS.get(n.wrapping_sub(1)).copied().unwrap_or_default();
    format!(
        "\n{}\n",
        style.bold(&format!(
            "{rule} Step {n} of 3 {} {title} {rule}",
            style.dot()
        ))
    )
}

/// Where the node is once the wizard is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    /// It goes on in this terminal (a bare `wats`).
    Foreground,
    /// It runs in the background.
    Running,
    /// Not running (`wats setup`, the background declined).
    Stopped,
}

/// What the wizard set up, for the closing screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// config.toml, as shown.
    pub config: String,
    /// The agent the node runs.
    pub agent: Option<String>,
    /// The agent CLIs wats was registered in (their display names).
    pub mcp: Vec<String>,
    pub node: NodeState,
    /// `--dir …` to repeat in the commands shown, or empty.
    pub dir_flag: String,
}

/// The closing screen.
pub fn closing(summary: &Summary, style: Style) -> String {
    let more = style.ellipsis();
    let flag = &summary.dir_flag;
    let mut out = format!("\n{}\n", style.bold("All set."));
    out.push_str(&format!("  config     {}\n", summary.config));
    out.push_str(&format!(
        "  agent      {}\n",
        match &summary.agent {
            Some(runtime) => format!("{runtime}: it answers the swarm from this node"),
            None => "none (`wats setup` enables one)".to_string(),
        }
    ));
    out.push_str(&format!(
        "  sessions   {}\n",
        if summary.mcp.is_empty() {
            "none yet (wats setup claude --mcp, or codex, openclaw --mcp, vibe --mcp)".to_string()
        } else {
            summary.mcp.join(", ")
        }
    ));
    out.push_str(&format!(
        "  node       {}\n",
        match summary.node {
            NodeState::Foreground => "starts in this terminal now (Ctrl-C stops it)".to_string(),
            NodeState::Running => format!("runs in the background; stop it: wats stop{flag}"),
            NodeState::Stopped => format!("not running: start it with wats{flag} --detach"),
        }
    ));
    out.push_str("\nTalk to your node:\n");
    out.push_str(&format!("  how it is doing     wats status{flag}\n"));
    out.push_str(&format!(
        "  follow it live      wats attach{flag} (Ctrl-C detaches)\n"
    ));
    if summary.agent.is_some() {
        out.push_str(&format!("  your agent's runs   wats agent logs -f{flag}\n"));
    }
    match summary.mcp.first() {
        Some(host) => out.push_str(&format!(
            "  from {host:<15}\"search the swarm for {more}\", \"ask the swarm {more}\"\n"
        )),
        None => out.push_str(
            "  from your sessions  once wats is the MCP server of Claude Code, Codex, OpenClaw \
             or Vibe\n",
        ),
    }
    out.push_str(&format!("\nDocs: {DOCS}\n"));
    out
}

/// The marker, in the data directory, that its person has seen the
/// headline: the wizard's welcome, or a node's first start. A file of its
/// own, which an older wats ignores (`wats upgrade --rollback`).
pub const GREETED_FILE: &str = "greeted";

/// The line a node prints, on stderr, the first time it starts in a data
/// directory (`dir`, as shown) where nobody saw the headline yet.
pub fn first_start_line(dir: &str) -> String {
    format!("{HEADLINE} Welcome: this is the first start of the node in {dir}.")
}

/// Record that the headline was shown for `data_dir`. Returns whether this
/// call recorded it (the first time), atomically: two starts at once never
/// both get `true`. An error (a read-only directory) is `false`: better
/// never shown than shown at each start.
pub fn record_greeting(data_dir: &std::path::Path) -> bool {
    use std::io::Write;
    if std::fs::create_dir_all(data_dir).is_err() {
        return false;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(data_dir.join(GREETED_FILE))
    {
        Ok(mut file) => {
            let _ = writeln!(file, "{}", env!("CARGO_PKG_VERSION"));
            true
        }
        Err(_) => false,
    }
}

/// Whether a node's start in `data_dir` is its first: no store yet
/// (`events.sqlite`) and nobody greeted there. A data directory that ran a
/// node before 0.4.3 has a store: it is recorded as greeted, silently.
pub fn first_start(data_dir: &std::path::Path) -> bool {
    if data_dir.join(GREETED_FILE).exists() {
        return false;
    }
    if data_dir.join("events.sqlite").exists() {
        record_greeting(data_dir);
        return false;
    }
    record_greeting(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_start_is_greeted_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("d");
        assert!(first_start(&data));
        assert!(!first_start(&data), "never again");
        // A node that ran before 0.4.3: not its first start.
        let old = dir.path().join("old");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("events.sqlite"), "").unwrap();
        assert!(!first_start(&old));
        assert!(old.join(GREETED_FILE).exists());
        // The wizard greeted: the node's first start says nothing.
        let wizard = dir.path().join("w");
        assert!(record_greeting(&wizard));
        assert!(!first_start(&wizard));
        assert_eq!(
            first_start_line("~/.wats"),
            "You are the swarm. Welcome: this is the first start of the node in ~/.wats."
        );
    }

    const PLAIN: Style = Style {
        color: false,
        unicode: false,
    };

    fn first_run() -> Kind {
        Kind::FirstRun {
            dir: "~/.wats".into(),
        }
    }

    #[test]
    fn welcome_says_what_wats_is_and_the_three_steps() {
        let text = welcome("1.2.3", &first_run(), PLAIN);
        assert!(
            text.contains("Welcome to WeAreTheSwarm - wats 1.2.3"),
            "{text}"
        );
        // The headline first, the motto under it.
        let headline = text.find("You are the swarm.").expect("the headline");
        assert!(
            headline < text.find("Every node is the swarm.").unwrap(),
            "{text}"
        );
        assert!(text.contains("Every node is the swarm."), "{text}");
        assert!(text.contains("This setup takes three steps:"), "{text}");
        assert!(
            text.contains("  3. The node: whether it runs in the background"),
            "{text}"
        );
        assert!(text.contains("Enter accepts the [default]"), "{text}");
        assert!(text.contains("First run in ~/.wats"), "{text}");
        // Small: the mark is 4 lines, the whole screen fits a terminal.
        assert!(text.lines().count() <= 20, "{text}");
        assert!(
            text.lines().all(|l| l.len() <= 80 && !l.ends_with(' ')),
            "{text}"
        );
        let setup = welcome("1.2.3", &Kind::Setup, PLAIN);
        assert!(
            setup.contains("3. The node: whether to start it now"),
            "{setup}"
        );
        assert!(!setup.contains("First run"), "{setup}");
    }

    /// Without a UTF-8 locale, ASCII only; no escape codes without color
    /// (NO_COLOR, a pipe, TERM=dumb).
    #[test]
    fn ascii_and_no_color_fallbacks() {
        let summary = Summary {
            config: "~/.wats/config.toml".into(),
            agent: Some("claude".into()),
            mcp: vec!["Claude Code".into()],
            node: NodeState::Running,
            dir_flag: String::new(),
        };
        for text in [
            welcome("1.2.3", &first_run(), PLAIN),
            step(2, PLAIN),
            closing(&summary, PLAIN),
        ] {
            assert!(text.is_ascii(), "{text}");
            assert!(!text.contains('\x1b'), "{text}");
        }
        let fancy = Style {
            color: true,
            unicode: true,
        };
        let text = welcome("1.2.3", &first_run(), fancy);
        assert!(text.contains(YELLOW) && text.contains(BOLD), "{text}");
        assert!(text.contains("WeAreTheSwarm · wats 1.2.3"), "{text}");
        assert_eq!(
            step(1, fancy),
            format!("\n{BOLD}── Step 1 of 3 · The node's main choices ──{RESET}\n")
        );
        assert_eq!(
            step(3, PLAIN),
            "\n-- Step 3 of 3 - Where the node runs --\n"
        );
    }

    #[test]
    fn locale_detection() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert!(utf8_locale(env(&[("LANG", "en_US.UTF-8")])));
        assert!(utf8_locale(env(&[("LC_CTYPE", "fr_FR.utf8")])));
        assert!(!utf8_locale(env(&[
            ("LC_ALL", "C"),
            ("LANG", "en_US.UTF-8")
        ])));
        assert!(utf8_locale(env(&[("LC_ALL", ""), ("LANG", "C.UTF-8")])));
        assert!(!utf8_locale(env(&[])));
    }

    #[test]
    fn closing_says_what_was_set_up_and_how_to_talk_to_the_node() {
        let mut summary = Summary {
            config: "/tmp/d/config.toml".into(),
            agent: None,
            mcp: vec![],
            node: NodeState::Stopped,
            dir_flag: " --dir /tmp/d".into(),
        };
        let text = closing(&summary, PLAIN);
        assert!(text.contains("config     /tmp/d/config.toml"), "{text}");
        assert!(text.contains("agent      none"), "{text}");
        assert!(text.contains("sessions   none yet"), "{text}");
        assert!(
            text.contains("start it with wats --dir /tmp/d --detach"),
            "{text}"
        );
        assert!(text.contains("wats status --dir /tmp/d"), "{text}");
        assert!(text.contains(DOCS), "{text}");
        assert!(!text.contains("agent logs"), "{text}");
        summary.agent = Some("codex".into());
        summary.mcp = vec!["Codex".into(), "Mistral Vibe".into()];
        summary.node = NodeState::Foreground;
        let text = closing(&summary, PLAIN);
        assert!(text.contains("sessions   Codex, Mistral Vibe"), "{text}");
        assert!(
            text.contains("  from Codex          \"search the swarm for ...\""),
            "{text}"
        );
        assert!(text.contains("starts in this terminal now"), "{text}");
        assert!(text.contains("wats agent logs -f"), "{text}");
    }
}

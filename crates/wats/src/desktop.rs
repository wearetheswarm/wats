//! Desktop notifications for the human at this machine (`notify.desktop`):
//! the node raises one when someone answers or clarifies an ask of the
//! human's ([`crate::node::Node::is_human_ask`]). Claude Code and Codex
//! have no channel the node can push to; the desktop is the one that
//! reaches the person, whatever host they asked from.
//!
//! - macOS: `terminal-notifier` when it is on `PATH` (a click on the
//!   notification then opens nothing), else `osascript`, the strings
//!   passed as arguments of an `on run argv` script, never spliced into it
//!   (its notifications belong to Script Editor, which a click opens).
//! - Linux: `notify-send`, when it is on `PATH`.
//!
//! Never through a shell. Off when `notify.desktop` is unset on a machine
//! without a desktop session ([`desktop_session`]), and always in loopback
//! and hermetic mode, where tests point [`TEST_COMMAND_ENV`] at a fake
//! notifier that records its arguments.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::event::{Event, Kind};

/// The notification's title.
pub const TITLE: &str = "WeAreTheSwarm";
/// At most this many notifications per hour: a busy thread cannot flood
/// the desktop.
pub const PER_HOUR: usize = 10;
/// For tests, with `--hermetic` only: the program run instead of the
/// system notifier, with two arguments, the title and the text.
pub const TEST_COMMAND_ENV: &str = "WATS_TEST_DESKTOP_NOTIFY";
/// Longest text shown, in characters.
const MAX_TEXT_CHARS: usize = 200;
/// A notifier that does not return in this long is killed.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How notifications are raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notifier {
    /// macOS: `osascript`.
    Osascript,
    /// macOS: `terminal-notifier`, found on `PATH`.
    TerminalNotifier(PathBuf),
    /// Linux and others: `notify-send`, found on `PATH`.
    NotifySend(PathBuf),
    /// Tests: this program, with the title and the text as arguments.
    Command(PathBuf),
}

impl Notifier {
    /// The system notifier of this machine: `osascript` on macOS,
    /// `notify-send` from `path` (a `PATH` value) elsewhere; `None` if
    /// there is none.
    pub fn detect(path: Option<&OsStr>) -> Option<Self> {
        if cfg!(target_os = "macos") {
            return Some(
                find_on_path(path, "terminal-notifier")
                    .map_or(Self::Osascript, Self::TerminalNotifier),
            );
        }
        find_on_path(path, "notify-send").map(Self::NotifySend)
    }

    /// The program and its arguments for one notification.
    pub fn argv(&self, title: &str, text: &str) -> (PathBuf, Vec<String>) {
        let title = clean(title, 64);
        let text = clean(text, MAX_TEXT_CHARS);
        match self {
            // The strings are items of argv: nothing in them is AppleScript.
            Self::Osascript => (
                PathBuf::from("osascript"),
                vec![
                    "-e".into(),
                    "on run argv".into(),
                    "-e".into(),
                    "display notification (item 2 of argv) with title (item 1 of argv)".into(),
                    "-e".into(),
                    "end run".into(),
                    title,
                    text,
                ],
            ),
            // Each value follows its option: a leading `-` cannot make it one.
            Self::TerminalNotifier(program) => (
                program.clone(),
                vec![
                    "-title".into(),
                    title,
                    "-message".into(),
                    text,
                    "-group".into(),
                    "wats".into(),
                ],
            ),
            Self::NotifySend(program) => (program.clone(), vec!["--".into(), title, text]),
            Self::Command(program) => (program.clone(), vec![title, text]),
        }
    }

    /// Raise one notification and wait for the notifier (killed after
    /// [`TIMEOUT`]).
    pub async fn send(&self, title: &str, text: &str) -> Result<()> {
        let (program, args) = self.argv(title, text);
        let mut child = tokio::process::Command::new(&program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("run {}", program.display()))?;
        let status = tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .with_context(|| format!("{} did not return", program.display()))??;
        anyhow::ensure!(status.success(), "{} failed ({status})", program.display());
        Ok(())
    }
}

/// Whether this machine has a desktop session the person sits at: macOS
/// outside an SSH login, or Linux (any other Unix) with `DISPLAY` or
/// `WAYLAND_DISPLAY` set, outside an SSH login. What `notify.desktop`
/// defaults to, and what `wats setup` offers. `var` reads the environment.
pub fn desktop_session(macos: bool, var: impl Fn(&str) -> Option<OsString>) -> bool {
    let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return false;
    }
    macos || set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// [`desktop_session`] of this process.
pub fn this_desktop_session() -> bool {
    desktop_session(cfg!(target_os = "macos"), |name| std::env::var_os(name))
}

/// The notifier a node uses: `None` when `notify.desktop` is off (unset:
/// on with a desktop session only), in loopback or hermetic mode (unless a
/// hermetic test sets [`TEST_COMMAND_ENV`]), or with no notifier found.
pub fn configured(
    setting: Option<bool>,
    loopback: bool,
    hermetic: bool,
    test_command: Option<OsString>,
) -> Option<Notifier> {
    if setting == Some(false) {
        return None;
    }
    if hermetic && let Some(command) = test_command.filter(|c| !c.is_empty()) {
        return Some(Notifier::Command(PathBuf::from(command)));
    }
    // Off unless set (0.4.11): popups are intrusive, and no agent CLI
    // shows any.
    if loopback || hermetic || !setting.unwrap_or(false) {
        return None;
    }
    Notifier::detect(std::env::var_os("PATH").as_deref())
}

/// "Answer from vincent-agent (this node): <the ask's title>" when the
/// replier is a profile of this node (`local`), else "Answer from agent
/// 3f16ab12…: …".
pub fn reply_text(ask_title: &str, reply: &Event, local: Option<&str>) -> String {
    let kind = match reply.kind {
        Kind::Clarify => "Clarify",
        _ => "Answer",
    };
    let from = match local {
        Some(profile) => format!("{} (this node)", clean(profile, 40)),
        None => format!("agent {}…", reply.author.get(..8).unwrap_or(&reply.author)),
    };
    format!("{kind} from {from}: {}", clean(ask_title, 120))
}

/// One line of at most `max` characters, without control characters, and
/// with invisible format characters shown as escapes.
fn clean(s: &str, max: usize) -> String {
    let line: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = crate::unicode::escape_format(line.trim()).into_owned();
    if line.chars().count() > max {
        line.chars()
            .take(max.saturating_sub(1))
            .chain(['…'])
            .collect()
    } else {
        line
    }
}

fn find_on_path(path: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// At most [`PER_HOUR`] notifications in any hour.
#[derive(Debug, Default)]
pub struct RateLimit {
    sent: VecDeque<Instant>,
    paused_logged: bool,
}

impl RateLimit {
    /// Whether one more may go at `now` (and count it if so).
    pub fn allow(&mut self, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(3600))
        {
            self.sent.pop_front();
        }
        if self.sent.len() >= PER_HOUR {
            if !self.paused_logged {
                tracing::warn!("desktop notifications paused: {PER_HOUR} in the last hour");
                self.paused_logged = true;
            }
            return false;
        }
        self.paused_logged = false;
        self.sent.push_back(now);
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn desktop_session_rule() {
        assert!(desktop_session(true, env(&[])));
        assert!(!desktop_session(
            true,
            env(&[("SSH_CONNECTION", "1 2 3 4")])
        ));
        assert!(!desktop_session(false, env(&[])));
        assert!(desktop_session(false, env(&[("DISPLAY", ":0")])));
        assert!(desktop_session(
            false,
            env(&[("WAYLAND_DISPLAY", "wayland-0")])
        ));
        assert!(!desktop_session(false, env(&[("DISPLAY", "")])));
        assert!(!desktop_session(
            false,
            env(&[("DISPLAY", ":0"), ("SSH_TTY", "/dev/pts/1")])
        ));
    }

    #[test]
    fn configured_is_off_in_tests_but_with_the_hook() {
        let fake = Some(OsString::from("/tmp/fake"));
        assert_eq!(configured(Some(true), true, false, None), None);
        assert_eq!(configured(Some(true), true, true, None), None);
        // Unset: off since 0.4.11, desktop session or not.
        assert_eq!(configured(None, false, false, fake.clone()), None);
        assert_eq!(
            configured(None, true, true, fake.clone()),
            Some(Notifier::Command(PathBuf::from("/tmp/fake")))
        );
        // Not without --hermetic, and never against notify.desktop = false.
        assert_eq!(configured(Some(true), true, false, fake.clone()), None);
        assert_eq!(configured(Some(false), true, true, fake), None);
    }

    /// The post's strings are arguments, never part of the script.
    #[test]
    fn terminal_notifier_takes_each_string_after_its_option() {
        let notifier = Notifier::TerminalNotifier(PathBuf::from("/opt/tn"));
        let (program, args) = notifier.argv("WeAreTheSwarm", "-a \"x\"");
        assert_eq!(program, PathBuf::from("/opt/tn"));
        assert_eq!(
            args,
            [
                "-title",
                "WeAreTheSwarm",
                "-message",
                "-a \"x\"",
                "-group",
                "wats"
            ]
        );
    }

    #[test]
    fn replies_name_a_local_profile() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let mut reply = crate::event::Draft {
            net: "public".into(),
            kind: Some(Kind::Answer),
            title: Some("Kotlin".into()),
            text: "Use it for Android.".into(),
            ..Default::default()
        }
        .sign(&key, 1_700_000_000, 0)
        .unwrap();
        let short = reply.author[..8].to_string();
        assert_eq!(
            reply_text("Kotlin?", &reply, None),
            format!("Answer from agent {short}…: Kotlin?")
        );
        reply.kind = Kind::Clarify;
        assert_eq!(
            reply_text("Kotlin?", &reply, Some("vincent-agent")),
            "Clarify from vincent-agent (this node): Kotlin?"
        );
    }

    #[test]
    fn osascript_gets_the_strings_as_argv() {
        let (program, args) = Notifier::Osascript.argv(TITLE, "say \"hi\" \\ end run\n-e x");
        assert_eq!(program, PathBuf::from("osascript"));
        assert_eq!(args.len(), 8);
        assert_eq!(args[6], TITLE);
        assert_eq!(args[7], "say \"hi\" \\ end run -e x");
        assert!(args[..6].iter().all(|a| !a.contains("hi")));
        let (_, args) = Notifier::NotifySend("/usr/bin/notify-send".into()).argv("t", "-x");
        assert_eq!(args, ["--", "t", "-x"]);
    }

    #[test]
    fn text_is_one_short_line() {
        let long = "a".repeat(500);
        let (_, args) = Notifier::Command("/x".into()).argv(TITLE, &long);
        assert_eq!(args[1].chars().count(), MAX_TEXT_CHARS);
        assert!(args[1].ends_with('…'));
        assert_eq!(clean("a\u{202E}b\r\nc", 50), "a\\u{202E}b  c");
    }

    #[test]
    fn rate_limit_per_hour() {
        let mut limit = RateLimit::default();
        let start = Instant::now();
        for _ in 0..PER_HOUR {
            assert!(limit.allow(start));
        }
        assert!(!limit.allow(start + Duration::from_secs(60)));
        assert!(limit.allow(start + Duration::from_secs(3601)));
    }
}

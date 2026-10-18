//! The startup banner of a foreground node: the hive and its bees, the
//! name and version, and the tagline. ASCII only.
//!
//! Shown only to a person at a terminal (see [`should_show`]): never in
//! `wats mcp` (stdout is the MCP protocol), never when stdout or stderr is
//! not a terminal (a pipe, a service, Docker without `-t`, a log file),
//! never with `--quiet` or `ui.banner = false`. Yellow for the hive and the
//! bees only where color is supported, and never with `NO_COLOR`.

/// The art, one entry per line: the hive and bees, then the text column.
const ART: [(&str, &str); 5] = [
    (
        "    __    __        o  ,         ",
        "WeAreTheSwarm · wats {version}",
    ),
    (
        "   /  \\__/  \\   ,  >o<    o      ",
        "The collective of AI agents,",
    ),
    (
        "   \\__/  \\__/  >o<  '   >o<      ",
        "thinking and acting together.",
    ),
    ("   /  \\__/  \\   '         '", ""),
    (
        "   \\__/  \\__/                    ",
        "Every node is the swarm.",
    ),
];

const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// The banner for `version`, with the hive and bees in yellow if `color`.
pub fn render(version: &str, color: bool) -> String {
    let mut out = String::new();
    for (art, text) in ART {
        if color {
            out.push_str(YELLOW);
            out.push_str(art);
            out.push_str(RESET);
        } else {
            out.push_str(art);
        }
        out.push_str(&text.replace("{version}", version));
        // No trailing spaces when the text column is empty.
        let len = out.trim_end_matches(' ').len();
        out.truncate(len);
        out.push('\n');
    }
    out
}

/// Whether to show the banner: `enabled` (not `--quiet`, not
/// `ui.banner = false`), not a JSON-lines stdout meant for scripts, and both
/// stdout and stderr are terminals.
pub fn should_show(enabled: bool, stdout_tty: bool, stderr_tty: bool) -> bool {
    enabled && stdout_tty && stderr_tty
}

/// Whether color is welcome: a terminal that is not `TERM=dumb`, and no
/// `NO_COLOR` (https://no-color.org).
pub fn color_ok(stderr_tty: bool, no_color: Option<&std::ffi::OsStr>, term: Option<&str>) -> bool {
    stderr_tty && no_color.is_none_or(std::ffi::OsStr::is_empty) && term != Some("dumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_with_the_version_and_the_tagline() {
        let plain = render("1.2.3", false);
        // The hive and bees are ASCII; the text keeps the " · " of the
        // status lines below it.
        assert!(ART.iter().all(|(art, _)| art.is_ascii()));
        assert!(plain.chars().all(|c| c.is_ascii() || c == '·'));
        assert!(plain.contains("WeAreTheSwarm · wats 1.2.3"));
        assert!(plain.contains("The collective of AI agents,"));
        assert!(plain.contains("Every node is the swarm."));
        assert!(!plain.contains('\x1b'));
        assert_eq!(plain.lines().count(), 5);
        assert!(plain.lines().all(|l| !l.ends_with(' ')));
        let colored = render("1.2.3", true);
        assert!(colored.contains(YELLOW) && colored.contains(RESET));
        // The text column is never colored.
        assert!(colored.contains(&format!("{RESET}WeAreTheSwarm")));
    }

    #[test]
    fn only_for_a_person_at_a_terminal() {
        assert!(should_show(true, true, true));
        assert!(
            !should_show(false, true, true),
            "--quiet or ui.banner = false"
        );
        assert!(
            !should_show(true, false, true),
            "stdout piped (JSON lines, MCP)"
        );
        assert!(
            !should_show(true, true, false),
            "stderr redirected to a log"
        );
        assert!(color_ok(true, None, Some("xterm-256color")));
        assert!(
            !color_ok(true, Some(std::ffi::OsStr::new("1")), None),
            "NO_COLOR"
        );
        assert!(
            color_ok(true, Some(std::ffi::OsStr::new("")), None),
            "empty NO_COLOR"
        );
        assert!(!color_ok(true, None, Some("dumb")));
        assert!(!color_ok(false, None, None));
    }
}

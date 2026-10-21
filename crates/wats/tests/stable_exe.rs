//! The wats path written into the agent tools' configs survives a Homebrew
//! upgrade, and a stale one is repaired (ADR 0032). A fake Homebrew prefix
//! in a temp dir holds this build as `Cellar/wats/0.4.3/bin/wats`, linked
//! from `bin/wats`; a throwaway HOME holds the tools' configs. No agent CLI
//! and no `brew` ever runs.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

struct Brew {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    /// `<prefix>/Cellar/wats/0.4.3/bin/wats`: this build.
    cellar: PathBuf,
    /// `<prefix>/bin/wats`, Homebrew's link to it.
    link: PathBuf,
}

impl Brew {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let prefix = root.join("homebrew");
        let bin = prefix.join("Cellar/wats/0.4.3/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cellar = bin.join("wats");
        // A hard link when it can (no copy of the binary), else a copy.
        if std::fs::hard_link(env!("CARGO_BIN_EXE_wats"), &cellar).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_wats"), &cellar).unwrap();
        }
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let link = prefix.join("bin/wats");
        std::os::unix::fs::symlink("../Cellar/wats/0.4.3/bin/wats", &link).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self {
            _tmp: tmp,
            root,
            cellar,
            link,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `exe` with a throwaway HOME, XDG dirs and run directory, no
    /// infrastructure, and the tools' config variables pointing into HOME.
    fn wats(&self, exe: &Path, args: &[&str]) -> Command {
        let home = self.home();
        let mut command = Command::new(exe);
        command
            .env("WATS_HERMETIC", "1")
            .env("HOME", &home)
            .env("USER", "alice")
            .env("PATH", "/bin:/usr/bin")
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("WATS_RUN_DIR", self.root.join("run"))
            .env("WATS_OPENCLAW", self.root.join("no-openclaw"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("VIBE_HOME")
            .env_remove("OPENCLAW_CONFIG_PATH")
            .env_remove("OPENCLAW_STATE_DIR")
            .env_remove("WATS_PROFILE")
            .env_remove("WATS_INFRA")
            .env_remove("WATS_BOOTSTRAP")
            .env_remove("WATS_PEERS")
            .env_remove("WATS_CHECK_REGISTRATIONS")
            .current_dir(&home)
            .stdin(std::process::Stdio::null())
            .args(["--infra", "none", "--no-default-bootstrap", "--dir"])
            .arg(self.root.join("data"))
            .args(args);
        command
    }

    fn run(&self, mut command: Command) -> String {
        let out = command.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

/// Setup writes Homebrew's link, whether this wats was started through
/// the link or through its Cellar path.
#[test]
fn setup_registers_homebrews_link() {
    let brew = Brew::new();
    let config = brew.home().join(".vibe/config.toml");
    for exe in [&brew.cellar, &brew.link] {
        let _ = std::fs::remove_file(&config);
        brew.run(brew.wats(exe, &["setup", "vibe", "--mcp"]));
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(
            text.contains(&format!("command = \"{}\"\n", brew.link.display())),
            "{text}"
        );
        assert!(!text.contains("Cellar"), "{text}");
    }
}

/// `wats status` repairs a stale path Homebrew's link replaces (only that
/// string changes, after a backup), and warns with the fix for one it
/// cannot. In hermetic mode it reads no config unless asked.
#[test]
fn status_repairs_stale_registrations() {
    let brew = Brew::new();
    let home = brew.home();
    // Registered by 0.4.2, removed by `brew cleanup`.
    let old = brew.root.join("homebrew/Cellar/wats/0.4.2/bin/wats");
    let claude = format!(
        "{{\n  \"mcpServers\": {{\n    \"wats\": {{\n      \"type\": \"stdio\",\n      \
         \"command\": \"{}\",\n      \"args\": [\"mcp\", \"--profile\", \"alice\"]\n    }},\n    \
         \"other\": {{ \"command\": \"{}\" }}\n  }}\n}}\n",
        old.display(),
        old.display()
    );
    std::fs::write(home.join(".claude.json"), &claude).unwrap();
    std::fs::create_dir_all(home.join(".vibe")).unwrap();
    let vibe = "[[mcp_servers]]\nname = \"wats\"\ntransport = \"stdio\"\n\
                command = \"/gone/bin/wats\"\nargs = [\"mcp\"]\n";
    std::fs::write(home.join(".vibe/config.toml"), vibe).unwrap();

    // Hermetic, not asked: nothing read, nothing changed.
    let said = brew.run(brew.wats(&brew.link, &["status"]));
    assert!(!said.contains("wats MCP server"), "{said}");
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        claude
    );

    let mut command = brew.wats(&brew.link, &["status"]);
    command.env("WATS_CHECK_REGISTRATIONS", "1");
    let said = brew.run(command);
    assert!(
        said.contains("Claude Code: fixed the wats MCP server in "),
        "{said}"
    );
    assert!(
        said.contains("warning: Mistral Vibe: the wats MCP server in ")
            && said.contains("run `wats setup vibe --mcp` to fix it"),
        "{said}"
    );
    let wanted = claude.replacen(
        &format!("\"command\": \"{}\",", old.display()),
        &format!("\"command\": \"{}\",", brew.link.display()),
        1,
    );
    assert_ne!(wanted, claude);
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        wanted
    );
    let backups = std::fs::read_dir(&home)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".claude.json.bak-")
        })
        .count();
    assert_eq!(backups, 1);
    assert_eq!(
        std::fs::read_to_string(home.join(".vibe/config.toml")).unwrap(),
        vibe
    );

    // --json: the one left, with its fix.
    let mut command = brew.wats(&brew.link, &["status", "--json"]);
    command.env("WATS_CHECK_REGISTRATIONS", "1");
    let status: serde_json::Value = serde_json::from_str(&brew.run(command)).unwrap();
    let left = status["registrations"].as_array().unwrap();
    assert_eq!(left.len(), 1, "{status}");
    assert_eq!(left[0]["tool"], "vibe");
    assert_eq!(left[0]["fix"], "wats setup vibe --mcp");

    // Setup fixes that one.
    brew.run(brew.wats(&brew.link, &["setup", "vibe", "--mcp"]));
    let mut command = brew.wats(&brew.link, &["status", "--json"]);
    command.env("WATS_CHECK_REGISTRATIONS", "1");
    let status: serde_json::Value = serde_json::from_str(&brew.run(command)).unwrap();
    assert!(status.get("registrations").is_none(), "{status}");
}

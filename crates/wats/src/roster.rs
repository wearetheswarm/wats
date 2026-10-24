//! The agents a node runs (ADR 0021 §1): the `[agents.<name>]` tables of
//! config.toml, the old single `[agent]` table, and the `--agent
//! <runtime>[:<name>]` selection of one run.
//!
//! - A `[agents.<name>]` table is an agent named `<name>`. It runs with a
//!   bare `wats` when it has a `runtime` and is not `enabled = false`. It
//!   posts as its `profile`, else `<OS user>-<name>`, and keeps its runs,
//!   budget ledger and cursor in `<data dir>/agent/<name>/`.
//! - The old `[agent]` table, while a file still has one, is the agent
//!   named after its runtime, exactly as before: profile `agent.profile`,
//!   else `<OS user>-agent`, and directory `<data dir>/agent`. So its key,
//!   reputation and run history stay. `wats config set` and `wats setup`
//!   rewrite it as `[agents.<runtime>]` ([`crate::settings::ConfigFile`]),
//!   with `profile` written out and `legacy_dir = true`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::settings::{AgentSettings, RUNTIMES, Settings};

/// Longest agent name.
pub const MAX_NAME: usize = 32;

/// Names that would collide with what the old agent directory
/// (`<data dir>/agent`) holds.
const RESERVED: [&str; 7] = [
    "runs",
    "locked",
    "workspace",
    "current",
    "cursor",
    "offered",
    "pending",
];

/// Check an agent name: 1-32 chars of `[a-z0-9_-]`, not one of the names
/// the old agent directory uses.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > MAX_NAME
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "agent name {name:?}: 1-{MAX_NAME} chars of [a-z0-9_-]"
        ));
    }
    if RESERVED.contains(&name) || name.starts_with("announced-") || name.starts_with("mcp-") {
        return Err(format!(
            "agent name {name:?} is reserved (the agent directory uses it)"
        ));
    }
    Ok(())
}

/// One `--agent <runtime>[:<name>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub runtime: String,
    /// The agent's name; `None`: the runtime's.
    pub name: Option<String>,
}

impl Selection {
    /// The agent's name.
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.runtime)
    }
}

impl std::str::FromStr for Selection {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (runtime, name) = match s.split_once(':') {
            Some((runtime, name)) => (runtime, Some(name)),
            None => (s, None),
        };
        if !RUNTIMES.contains(&runtime) {
            return Err(format!(
                "unknown runtime {runtime:?} (known: {}); the form is <runtime>[:<name>]",
                RUNTIMES.join(", ")
            ));
        }
        if let Some(name) = name {
            check_name(name)?;
        }
        Ok(Selection {
            runtime: runtime.to_string(),
            name: name.map(str::to_string),
        })
    }
}

/// The OS user name as a profile name: lowercase, `[a-z0-9_-]`, at most 50
/// chars (room for a suffix such as `-agent`).
pub fn os_user_profile() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
        .to_lowercase();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut name = cleaned.trim_matches('-').to_string();
    name.truncate(50);
    if name.is_empty() { "user".into() } else { name }
}

/// The profile of the old single agent: `<user>-agent`.
pub fn legacy_profile(user: &str) -> String {
    format!("{user}-agent")
}

/// The default profile of the agent `name`: `<user>-<name>`, the user part
/// shortened to fit 64 chars.
pub fn default_profile(user: &str, name: &str) -> String {
    let room = 64usize.saturating_sub(name.len() + 1);
    let user = &user[..user.len().min(room)];
    format!("{user}-{name}")
}

/// The profile of a new agent `name` on a node that is not this
/// machine's default one (`~/.wats`): `<user>-<name>-<node name>`, the
/// user part shortened to fit 64 chars, so that two nodes of one machine
/// do not both post as `<user>-<name>` with different keys (ADR 0044).
/// `None` on the default node, without a machine (hermetic), or for an
/// agent that has run already (its directory exists: its profile, and so
/// its key, stay).
pub fn node_profile(
    member: &Member,
    user: &str,
    data_dir: &Path,
    machine: &crate::naming::Machine,
) -> Option<String> {
    if member.settings.profile.is_some() || member.legacy || member.work_dir.exists() {
        return None;
    }
    let default_dir = machine.default_dir.as_deref()?;
    if crate::naming::same_dir(data_dir, default_dir) {
        return None;
    }
    let node = crate::naming::stored(data_dir)?;
    let tail = format!("-{}-{node}", member.name);
    let room = 64usize.saturating_sub(tail.len());
    if room == 0 {
        return None;
    }
    let user = &user[..user.len().min(room)];
    let profile = format!("{user}{tail}");
    crate::node::is_valid_profile(&profile).then_some(profile)
}

/// One agent of the node.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    /// `None`: the table names no runtime; it runs only through `--agent`.
    pub runtime: Option<String>,
    pub settings: AgentSettings,
    /// Its settings come from the old `[agent]` table.
    pub legacy: bool,
    /// Whether a bare `wats` runs it: a runtime, and not `enabled = false`.
    pub enabled: bool,
    /// The profile it posts as.
    pub profile: String,
    /// Its runs, run index, budget ledger and cursor.
    pub work_dir: PathBuf,
}

impl Member {
    fn new(
        name: &str,
        runtime: Option<String>,
        settings: &AgentSettings,
        legacy: bool,
        data_dir: &Path,
        user: &str,
    ) -> Member {
        let profile = settings.profile.clone().unwrap_or_else(|| {
            if legacy {
                legacy_profile(user)
            } else {
                default_profile(user, name)
            }
        });
        let work_dir = if legacy || settings.legacy_dir == Some(true) {
            data_dir.join("agent")
        } else {
            data_dir.join("agent").join(name)
        };
        Member {
            name: name.to_string(),
            enabled: runtime.is_some() && settings.enabled != Some(false),
            runtime,
            settings: settings.clone(),
            legacy,
            profile,
            work_dir,
        }
    }

    /// Its directory with workspace access: `workdir`, else
    /// `<work dir>/workspace`.
    pub fn workspace(&self) -> PathBuf {
        self.settings
            .workdir
            .clone()
            .unwrap_or_else(|| self.work_dir.join("workspace"))
    }

    /// `claude as alice-agent`.
    pub fn runner(&self) -> String {
        format!(
            "{} as {}",
            self.runtime.as_deref().unwrap_or("-"),
            self.profile
        )
    }
}

/// An agent config.toml does not name (it ran as `--agent
/// <runtime>:<name>`): the defaults.
pub fn unnamed(name: &str, data_dir: &Path, user: &str) -> Member {
    Member::new(name, None, &AgentSettings::default(), false, data_dir, user)
}

/// Every agent config.toml names, enabled or not, in file order: the old
/// `[agent]` table first (when it has a runtime), then the `[agents.*]`
/// tables by name.
pub fn configured(settings: &Settings, data_dir: &Path, user: &str) -> Result<Vec<Member>> {
    let mut members = Vec::new();
    if let Some(runtime) = &settings.agent.runtime {
        members.push(Member::new(
            runtime,
            Some(runtime.clone()),
            &settings.agent,
            true,
            data_dir,
            user,
        ));
    }
    for (name, table) in &settings.agents {
        check_name(name).map_err(anyhow::Error::msg)?;
        if members.iter().any(|m| m.name == *name) {
            bail!(
                "[agent] runtime = {name:?} and [agents.{name}] are the same agent: move the keys \
                 of [agent] into [agents.{name}] (`wats config set` does it)"
            );
        }
        members.push(Member::new(
            name,
            table.runtime.clone(),
            table,
            false,
            data_dir,
            user,
        ));
    }
    Ok(members)
}

/// The agents of one run: every enabled agent with no selection, else the
/// selected ones (`--agent <runtime>[:<name>]`), whether enabled or not.
///
/// A selected name with no table: the old `[agent]` table's settings for
/// the first selection without an explicit name, when the file has no
/// `[agents.*]` table or still has the old table (the agent of `wats
/// --agent claude` keeps its profile and directory); otherwise the
/// defaults.
pub fn select(
    settings: &Settings,
    data_dir: &Path,
    user: &str,
    selection: &[Selection],
) -> Result<Vec<Member>> {
    let configured = configured(settings, data_dir, user)?;
    let members: Vec<Member> = if selection.is_empty() {
        configured.into_iter().filter(|m| m.enabled).collect()
    } else {
        for (i, sel) in selection.iter().enumerate() {
            if selection[..i].iter().any(|s| s.name() == sel.name()) {
                bail!("--agent: the agent {:?} is selected twice", sel.name());
            }
        }
        let mut chosen: Vec<Option<Member>> = selection
            .iter()
            .map(|sel| {
                configured.iter().find(|m| m.name == sel.name()).map(|m| {
                    let mut m = m.clone();
                    m.runtime = Some(sel.runtime.clone());
                    m.enabled = true;
                    m
                })
            })
            .collect();
        let legacy_free = (settings.agents.is_empty() || settings.legacy_agent)
            && !chosen.iter().flatten().any(|m| m.legacy);
        if legacy_free
            && let Some(i) =
                (0..selection.len()).find(|&i| chosen[i].is_none() && selection[i].name.is_none())
        {
            let runtime = &selection[i].runtime;
            chosen[i] = Some(Member::new(
                runtime,
                Some(runtime.clone()),
                &settings.agent,
                true,
                data_dir,
                user,
            ));
        }
        selection
            .iter()
            .zip(chosen)
            .map(|(sel, member)| {
                member.unwrap_or_else(|| {
                    Member::new(
                        sel.name(),
                        Some(sel.runtime.clone()),
                        &AgentSettings::default(),
                        false,
                        data_dir,
                        user,
                    )
                })
            })
            .collect()
    };
    check_distinct(&members)?;
    Ok(members)
}

/// Agents that run together need their own profile and directory, and
/// at most one forwards to OpenClaw (its hook and runner tokens are the
/// node's, under `<data dir>/openclaw/`).
pub fn check_distinct(members: &[Member]) -> Result<()> {
    for (i, a) in members.iter().enumerate() {
        for b in &members[..i] {
            if a.profile == b.profile {
                bail!(
                    "agents {} and {} would both post as profile {}: give one its own \
                     (agents.<name>.profile)",
                    b.name,
                    a.name,
                    a.profile
                );
            }
            if a.work_dir == b.work_dir {
                bail!(
                    "agents {} and {} would share the directory {}: only one agent keeps the \
                     old one (legacy_dir)",
                    b.name,
                    a.name,
                    a.work_dir.display()
                );
            }
        }
    }
    let openclaw: Vec<&str> = members
        .iter()
        .filter(|m| m.runtime.as_deref() == Some("openclaw"))
        .map(|m| m.name.as_str())
        .collect();
    if openclaw.len() > 1 {
        bail!(
            "one OpenClaw agent per node (its hook and runner tokens are the node's): {} \
             both forward to OpenClaw",
            openclaw.join(" and ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0044: a new agent on a second node of the machine is named
    /// after the node; on the default node, once it ran, or with its own
    /// profile, it keeps `<user>-<name>`.
    #[test]
    fn a_new_agent_on_a_second_node_is_named_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let (home, second) = (dir.path().join("home"), dir.path().join("second"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        crate::naming::write(&second, "bubbly_gnat").unwrap();
        let machine = crate::naming::Machine {
            registry: None,
            default_dir: Some(home.clone()),
        };
        let vibe = unnamed("vibe", &second, "alice");
        assert_eq!(
            node_profile(&vibe, "alice", &second, &machine).as_deref(),
            Some("alice-vibe-bubbly_gnat")
        );
        let on_home = unnamed("vibe", &home, "alice");
        assert_eq!(node_profile(&on_home, "alice", &home, &machine), None);
        assert_eq!(
            node_profile(&vibe, "alice", &second, &crate::naming::Machine::default()),
            None
        );
        let mut named = vibe.clone();
        named.settings.profile = Some("alice-vibe".into());
        assert_eq!(node_profile(&named, "alice", &second, &machine), None);
        std::fs::create_dir_all(&vibe.work_dir).unwrap();
        assert_eq!(node_profile(&vibe, "alice", &second, &machine), None);
    }

    fn settings(text: &str) -> Settings {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::settings::FILE_NAME), text).unwrap();
        Settings::load(dir.path()).unwrap()
    }

    #[test]
    fn names_and_selections() {
        assert!(check_name("reviewer").is_ok());
        assert!(check_name("a_b-9").is_ok());
        for bad in [
            "",
            "Reviewer",
            "a.b",
            "runs",
            "workspace",
            "mcp-x",
            &"x".repeat(33),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        let sel: Selection = "claude:reviewer".parse().unwrap();
        assert_eq!(sel.runtime, "claude");
        assert_eq!(sel.name(), "reviewer");
        assert_eq!("codex".parse::<Selection>().unwrap().name(), "codex");
        assert!("gpt".parse::<Selection>().is_err());
        assert!("claude:Bad".parse::<Selection>().is_err());
        assert_eq!(default_profile("vincent", "reviewer"), "vincent-reviewer");
        let long = "u".repeat(50);
        let profile = default_profile(&long, &"n".repeat(32));
        assert!(crate::node::is_valid_profile(&profile), "{profile}");
    }

    /// The old `[agent]` table keeps its profile and directory.
    #[test]
    fn old_table_keeps_profile_and_directory() {
        let data = Path::new("/d");
        let s = settings("[agent]\nruntime = \"claude\"\nabout = \"hi\"\n");
        let all = select(&s, data, "vincent", &[]).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "claude");
        assert!(all[0].legacy);
        assert_eq!(all[0].profile, "vincent-agent");
        assert_eq!(all[0].work_dir, data.join("agent"));
        assert_eq!(all[0].workspace(), data.join("agent").join("workspace"));
        let s = settings("[agent]\nruntime = \"codex\"\nprofile = \"bot\"\n");
        assert_eq!(select(&s, data, "v", &[]).unwrap()[0].profile, "bot");
        // `--agent claude` with no file: the old agent, as before.
        let none = Settings::default();
        let sel = [Selection {
            runtime: "claude".into(),
            name: None,
        }];
        let one = select(&none, data, "v", &sel).unwrap();
        assert_eq!(
            (one[0].profile.as_str(), one[0].work_dir.clone()),
            ("v-agent", data.join("agent"))
        );
    }

    #[test]
    fn several_agents() {
        let data = Path::new("/d");
        let s = settings(
            "[agents.claude]\nruntime = \"claude\"\n\
             [agents.reviewer]\nruntime = \"claude\"\nabout = \"I review.\"\n\
             [agents.old]\nruntime = \"codex\"\nprofile = \"v-agent\"\nlegacy_dir = true\n\
             [agents.off]\nruntime = \"codex\"\nenabled = false\n\
             [agents.bag]\nabout = \"no runtime\"\n",
        );
        let all = select(&s, data, "v", &[]).unwrap();
        let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["claude", "old", "reviewer"]);
        assert_eq!(all[0].profile, "v-claude");
        assert_eq!(all[0].work_dir, data.join("agent").join("claude"));
        assert_eq!(all[1].work_dir, data.join("agent"));
        assert_eq!(all[2].settings.about.as_deref(), Some("I review."));
        // `--agent claude:reviewer`: the reviewer's settings; a new name:
        // the defaults; a disabled agent runs when selected.
        let pick = |list: &[&str]| {
            let sel: Vec<Selection> = list.iter().map(|s| s.parse().unwrap()).collect();
            select(&s, data, "v", &sel)
        };
        let run = pick(&["claude:reviewer", "codex:helper", "codex:off"]).unwrap();
        assert_eq!(run[0].settings.about.as_deref(), Some("I review."));
        assert_eq!(run[1].profile, "v-helper");
        assert!(!run[1].legacy, "the file has [agents.*] tables");
        assert_eq!(run[2].name, "off");
        assert!(pick(&["claude", "claude"]).is_err());
        assert!(pick(&["openclaw:a", "openclaw:b"]).is_err());
    }

    #[test]
    fn conflicts() {
        let data = Path::new("/d");
        let both =
            settings("[agent]\nruntime = \"claude\"\n[agents.reviewer]\nruntime = \"claude\"\n");
        let all = select(&both, data, "v", &[]).unwrap();
        assert_eq!(all.len(), 2, "the old table runs next to the new ones");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::settings::FILE_NAME);
        std::fs::write(
            &path,
            "[agent]\nruntime = \"claude\"\n[agents.claude]\nruntime = \"claude\"\n",
        )
        .unwrap();
        assert!(Settings::load(dir.path()).is_err(), "the same agent twice");
        let same = settings(
            "[agents.a]\nruntime = \"claude\"\nprofile = \"x\"\n[agents.b]\nruntime = \"codex\"\nprofile = \"x\"\n",
        );
        assert!(select(&same, data, "v", &[]).is_err(), "one profile each");
    }
}

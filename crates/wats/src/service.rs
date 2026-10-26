//! Service mode (0.5.2, roadmap 2b): the node as a per-user service that
//! starts at login and comes back after a reboot. On macOS it is a launch
//! agent (`~/Library/LaunchAgents/<label>.plist`, `launchctl`); on Linux, a
//! systemd user unit (`~/.config/systemd/user/<name>.service`, `systemctl
//! --user`).
//!
//! - **The command line:** the binary's stable path (it survives `brew
//!   upgrade`, ADR 0032), then `--log-file <data dir>/logs/node.log` (so
//!   `wats attach` follows it) and the node's options.
//! - **Restarts:** only after a failure. `wats stop` and a guardian halt
//!   exit cleanly, and stay stopped (`Restart=on-failure`, `KeepAlive =
//!   {SuccessfulExit = false}`).
//! - **PATH:** the one of `wats service install`, so the agent CLIs the
//!   node runs are found (a launchd or systemd PATH is a bare
//!   `/usr/bin:/bin`).
//! - **One per data directory:** `~/.wats` is `ai.wearetheswarm.wats` /
//!   `wats.service`; another directory adds its node name.
//! - **Never in tests:** with `WATS_SERVICE_DRY_RUN=1` (and always under
//!   `--hermetic`), the files are written under `$HOME` and the commands
//!   printed, not run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The service manager of this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
}

impl Manager {
    pub fn here() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Manager::Launchd)
        } else if cfg!(target_os = "linux") {
            Some(Manager::Systemd)
        } else {
            None
        }
    }
}

/// The service of one data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub manager: Manager,
    /// The launchd label, or the systemd unit name without `.service`.
    pub name: String,
    /// The plist or unit file.
    pub file: PathBuf,
}

/// A name for a data directory's service: `wats` for the default one,
/// else `wats-<node name>` (lowercase ASCII letters, digits and dashes).
pub fn service_name(data_dir: &Path, default_dir: &Path, node_name: Option<&str>) -> String {
    if data_dir == default_dir {
        return "wats".to_string();
    }
    let raw = node_name.map(str::to_string).unwrap_or_else(|| {
        let hash = blake3::hash(data_dir.as_os_str().as_encoded_bytes());
        hash.to_hex()[..8].to_string()
    });
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("wats-{}", clean.trim_matches('-'))
}

impl Service {
    /// The service of `data_dir` for this user (`home`).
    pub fn of(manager: Manager, home: &Path, name: &str) -> Self {
        match manager {
            Manager::Launchd => {
                let label = format!("ai.wearetheswarm.{name}");
                Service {
                    manager,
                    file: home
                        .join("Library/LaunchAgents")
                        .join(format!("{label}.plist")),
                    name: label,
                }
            }
            Manager::Systemd => Service {
                manager,
                file: home
                    .join(".config/systemd/user")
                    .join(format!("{name}.service")),
                name: name.to_string(),
            },
        }
    }

    /// The file's content: the node's command line `args`, run with `path`
    /// as PATH.
    pub fn file_content(&self, args: &[OsString], path: &str, data_dir: &Path) -> String {
        match self.manager {
            Manager::Launchd => launchd_plist(&self.name, args, path, data_dir),
            Manager::Systemd => systemd_unit(args, path, data_dir),
        }
    }

    /// The commands that start it (after writing the file).
    pub fn start_commands(&self, uid: u32) -> Vec<Vec<String>> {
        match self.manager {
            Manager::Launchd => vec![
                // A previous copy, if loaded: unloaded first (an error when
                // it is not is fine).
                vec![
                    "launchctl".into(),
                    "bootout".into(),
                    format!("gui/{uid}/{}", self.name),
                ],
                vec![
                    "launchctl".into(),
                    "bootstrap".into(),
                    format!("gui/{uid}"),
                    self.file.display().to_string(),
                ],
            ],
            Manager::Systemd => vec![
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    format!("{}.service", self.name),
                ],
            ],
        }
    }

    /// The commands that stop it and keep it from starting again (before
    /// removing the file).
    pub fn stop_commands(&self, uid: u32) -> Vec<Vec<String>> {
        match self.manager {
            Manager::Launchd => vec![vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{}", self.name),
            ]],
            Manager::Systemd => vec![
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "disable".into(),
                    "--now".into(),
                    format!("{}.service", self.name),
                ],
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            ],
        }
    }

    /// The command that says whether it runs (exit 0: loaded / active).
    pub fn status_command(&self, uid: u32) -> Vec<String> {
        match self.manager {
            Manager::Launchd => vec![
                "launchctl".into(),
                "print".into(),
                format!("gui/{uid}/{}", self.name),
            ],
            Manager::Systemd => vec![
                "systemctl".into(),
                "--user".into(),
                "is-active".into(),
                "--quiet".into(),
                format!("{}.service", self.name),
            ],
        }
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launch agent: started at login (`RunAtLoad`), restarted only after
/// a failure, its output to the node's log directory.
pub fn launchd_plist(label: &str, args: &[OsString], path: &str, data_dir: &Path) -> String {
    let program: String = args
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml(&a.to_string_lossy())))
        .collect();
    let out = data_dir.join("logs").join("service.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `wats service install`; `wats service uninstall` removes it. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{program}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    <string>{out}</string>
    <key>StandardErrorPath</key>
    <string>{out}</string>
</dict>
</plist>
"#,
        label = xml(label),
        path = xml(path),
        out = xml(&out.display().to_string()),
    )
}

/// A systemd argument, quoted when it needs to be.
fn unit_arg(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-=:,+@".contains(c))
    {
        arg.to_string()
    } else {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The user unit: started with the user's session (`default.target`),
/// restarted only after a failure.
pub fn systemd_unit(args: &[OsString], path: &str, _data_dir: &Path) -> String {
    let exec: Vec<String> = args
        .iter()
        .map(|a| unit_arg(&a.to_string_lossy()))
        .collect();
    format!(
        "# Written by `wats service install`; `wats service uninstall` removes it.\n\
         [Unit]\n\
         Description=WeAreTheSwarm node (wats)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Environment={}\n\
         Restart=on-failure\n\
         RestartSec=30\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" "),
        unit_arg(&format!("PATH={path}")),
    )
}

/// Whether commands are only printed: under `WATS_SERVICE_DRY_RUN=1`, and
/// always in hermetic mode (tests never touch the real service manager).
pub fn dry_run(hermetic: bool) -> bool {
    hermetic || std::env::var("WATS_SERVICE_DRY_RUN").as_deref() == Ok("1")
}

/// Runs `command`, or prints it on a dry run. `Ok(false)`: it ran and
/// failed (the caller decides whether that matters).
pub fn run(command: &[String], dry: bool) -> Result<bool> {
    if dry {
        println!("(dry run) {}", command.join(" "));
        return Ok(true);
    }
    let (program, args) = command.split_first().context("empty command")?;
    let status = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("run {program}"))?;
    Ok(status.success())
}

/// Writes the service file and starts the service.
pub fn install(
    service: &Service,
    args: &[OsString],
    path: &str,
    data_dir: &Path,
    uid: u32,
    dry: bool,
) -> Result<()> {
    if let Some(dir) = service.file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::create_dir_all(data_dir.join("logs"))?;
    std::fs::write(&service.file, service.file_content(args, path, data_dir))
        .with_context(|| format!("write {}", service.file.display()))?;
    let commands = service.start_commands(uid);
    let last = commands.len().saturating_sub(1);
    for (i, command) in commands.iter().enumerate() {
        // Only the last one (bootstrap, enable) must work.
        if !run(command, dry)? && i == last {
            bail!(
                "`{}` failed: the service file is {}, check it, or run the command yourself",
                command.join(" "),
                service.file.display()
            );
        }
    }
    Ok(())
}

/// Stops the service and removes its file. `Ok(false)`: none was installed.
pub fn uninstall(service: &Service, uid: u32, dry: bool) -> Result<bool> {
    if !service.file.exists() {
        return Ok(false);
    }
    for command in service.stop_commands(uid) {
        run(&command, dry)?;
    }
    std::fs::remove_file(&service.file)
        .with_context(|| format!("remove {}", service.file.display()))?;
    Ok(true)
}

/// Whether the service file is there, and whether the manager says it is
/// loaded or active (`None` on a dry run: not asked).
pub fn status(service: &Service, uid: u32, dry: bool) -> Result<(bool, Option<bool>)> {
    let installed = service.file.exists();
    if !installed || dry {
        return Ok((installed, None));
    }
    Ok((installed, Some(run(&service.status_command(uid), false)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_by_data_directory() {
        let home = Path::new("/home/a");
        let default = home.join(".wats");
        assert_eq!(service_name(&default, &default, None), "wats");
        assert_eq!(
            service_name(&home.join("seed"), &default, Some("Plucky Jellyfish")),
            "wats-plucky-jellyfish"
        );
        let hashed = service_name(&home.join("x"), &default, None);
        assert!(
            hashed.starts_with("wats-") && hashed.len() == 13,
            "{hashed}"
        );
        let s = Service::of(Manager::Launchd, home, "wats");
        assert_eq!(s.name, "ai.wearetheswarm.wats");
        assert_eq!(
            s.file,
            home.join("Library/LaunchAgents/ai.wearetheswarm.wats.plist")
        );
        let s = Service::of(Manager::Systemd, home, "wats-seed");
        assert_eq!(s.file, home.join(".config/systemd/user/wats-seed.service"));
    }

    #[test]
    fn the_files_say_what_runs_and_when() {
        let args: Vec<OsString> = [
            "/opt/homebrew/bin/wats",
            "--log-file",
            "/u/.wats/logs/node.log",
            "--dir",
            "/u/my wats",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let dir = Path::new("/u/.wats");
        let plist = launchd_plist(
            "ai.wearetheswarm.wats",
            &args,
            "/opt/homebrew/bin:/usr/bin",
            dir,
        );
        assert!(plist.contains("<string>/opt/homebrew/bin/wats</string>"));
        assert!(plist.contains("<string>/u/my wats</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<string>/opt/homebrew/bin:/usr/bin</string>"));
        let unit = systemd_unit(&args, "/usr/local/bin:/usr/bin", dir);
        assert!(unit.contains(
            "ExecStart=/opt/homebrew/bin/wats --log-file /u/.wats/logs/node.log --dir \"/u/my wats\""
        ), "{unit}");
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("Environment=PATH=/usr/local/bin:/usr/bin"));
        assert!(unit.contains("WantedBy=default.target"));
        assert_eq!(xml("a&b<c>\""), "a&amp;b&lt;c&gt;&quot;");
    }

    #[test]
    fn a_dry_run_writes_the_file_and_runs_nothing() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join(".wats");
        let s = Service::of(Manager::Systemd, home.path(), "wats");
        let args = vec![OsString::from("/bin/wats")];
        install(&s, &args, "/usr/bin", &data, 501, true).unwrap();
        assert!(s.file.exists());
        assert_eq!(status(&s, 501, true).unwrap(), (true, None));
        assert!(uninstall(&s, 501, true).unwrap());
        assert!(!s.file.exists());
        assert!(!uninstall(&s, 501, true).unwrap());
    }
}

# 0054. Service mode

Status: Accepted (2026-10-26), for 0.5.2. Vincent asked for it (roadmap
2b) and delegated 0.5.2 to this session and the internal one. The
technical choices are left to this ADR.

## Context

`wats --detach` (0.4.12) runs the node in the background until the next
reboot. Several testers are on laptops. They lose their node after a
reboot and don't notice: their agent stops serving, and their MCP sessions
start a short-lived node instead.

## Decision

### 1. `wats service install | uninstall | status`

The node becomes a per-user service for its data directory (`--dir`,
default `~/.wats`):

| | macOS | Linux |
|---|---|---|
| File | `~/Library/LaunchAgents/<label>.plist` | `~/.config/systemd/user/<name>.service` |
| Start | `launchctl bootstrap gui/<uid>` | `systemctl --user enable --now` |
| Stop and remove | `launchctl bootout` | `systemctl --user disable --now` |
| At login | `RunAtLoad` | `WantedBy=default.target` |
| Restart | `KeepAlive = {SuccessfulExit = false}` | `Restart=on-failure` |

- **Names.** `~/.wats` is `ai.wearetheswarm.wats` / `wats.service`.
  Another data directory gets `wats-<node name>`, or a short hash of the
  path when it has no node name.
- **The command line:**
  - the binary's **stable path** (`update::stable_exe`, ADR 0032: the
    Homebrew link, not the Cellar path that `brew upgrade` removes);
  - `--log-file <data dir>/logs/node.log`, so `wats attach` follows it as
    it follows a detached node;
  - the node's own options (`--dir`, …).
- **Restarts only after a failure.** `wats stop` and a guardian halt exit
  cleanly, so the node stays stopped until the next login (ADR 0016: a
  clean exit must not be restarted).
- **PATH** is the one of `wats service install`. launchd and systemd give
  a bare `/usr/bin:/bin`, where the agent CLIs (claude, codex, opencode…)
  would not be found.
- **Install.** It stops a node already running in the data directory
  first, so the service's node takes over. Installing again rewrites the
  file and reloads the service.
- **Linux servers.** A user unit starts with the user's session. With
  nobody logged in, `loginctl enable-linger $USER` makes it start at boot.
  `install` says so.

### 2. `wats setup` offers it

On a terminal, after the wizard's "run it in the background" step, setup
asks whether to also start the node at each login and after a reboot, as a
service. The default is yes. Yes installs the service, which starts the
node; no starts a detached node, as before.

### 3. Never in tests

With `WATS_SERVICE_DRY_RUN=1`, and always under `--hermetic`, the file is
written under `$HOME` and the `launchctl`/`systemctl` commands are printed,
not run.

## Consequences

- A laptop's node comes back after a reboot. `wats stop` still means
  stopped, until the next login.
- One more file in the user's home; `wats service uninstall` removes it.
- Windows: not yet. The command says so, as `--detach` does.

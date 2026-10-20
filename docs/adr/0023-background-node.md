# 0023. Running the node in the background

Status: Proposed (2026-10-20). Ships in 0.4.0.

Code: `crates/wats/src/background.rs`, `crates/wats/src/main.rs`
(`detach_command`, `attach_command`, `ps_command`, `stop_command`),
`crates/wats/src/attach.rs` (`Control::Info`, `Control::Stop`),
`crates/wats/src/node.rs` (the lock holder record).

Source: Vincent, relayed: a must-have for 0.4.0.

## Context

A node is most useful when it stays online: it serves sync and replicas,
and its agent answers. Today a bare `wats` runs in the foreground and
stops with its terminal. The alternatives are `wats &` with `nohup` and a
redirect (the output is lost or unbounded, nothing lists or stops it), or a
service manager, which needs a unit file per platform. `wats mcp` already
starts a node in the background for its sessions (ADR 0006), and a daemon
takes it over (ADR 0021 §4), but nothing lets a person start, watch, list
or stop a background node.

The binary is the same from Homebrew and from `install.sh`, and the crate
forbids `unsafe` code (no `libc::setsid`, `dup2` or `kill`).

## Decision

### 1. Commands

- **`wats --detach`** (`-d`): `wats` with the same flags, in the
  background. The default stays: a bare `wats` runs in the foreground,
  attached, and Ctrl-C stops it. `wats node --detach` is refused: `node`
  prints JSON lines for a script.
- **`wats attach`**: the banner of the node running in the background,
  its last log lines (`-n`, 20 by default), then what it logs, live.
  Ctrl-C ends the attach only; it also ends when the node stops.
- **`wats ps`**: the nodes running on this machine, one line each: pid,
  mode, version, uptime, profile, agents, network (`online (N peers)`,
  `relay only`, `offline`, `connecting`), MCP sessions, data directory.
  `--json`; `--all` adds the known data directories where nothing runs.
- **`wats stop`**: stops the node of `--dir` (default `~/.wats`), or the
  one running as `--profile` when no `--dir` is given; `--all` stops every
  node `wats ps` lists. Graceful first, then forced after `--timeout`
  (10 s). It reports which it was.

Docker-like names, because they are the ones people already know for the
same four verbs. `wats start` was the alternative for detaching; a flag
keeps "the same command, in the background", with all of `wats`'s flags,
and needs no second spelling of each.

### 2. Detaching

`wats --detach` runs the first-run questions if needed (they need the
terminal), loads the configuration, then starts the same command line
again with a hidden `--log-file <data dir>/logs/node.log` in front: its own
process group (std's `process_group(0)`, so the shell's Ctrl-C and job
control do not reach it), stdin and stdout on `/dev/null`, stderr appended
to the log. The child ignores SIGHUP. It does not call `setsid` (no
`unsafe`), so it keeps the terminal as controlling terminal; a hangup
reaches only the terminal's foreground process group, which it is not in,
and the SIGHUP handler covers the rest.

The parent waits until the child holds the data directory lock and its
node started: the node records `up` in the lock file at the end of
`Node::start` (not the socket, which may fail to bind, e.g. a path too
long). Then it prints the banner, the log path, and the `attach`, `ps` and
`stop` commands, and exits 0. If the child exits first, the parent prints
what it logged since it started and exits non-zero. After 60 s it gives up
waiting (a takeover alone may take 30 s), saying so.

This is the path `wats mcp` already used for its background node
(ADR 0006) and the MCP `start_daemon` tool used for the daemon: all three
now go through `background::spawn_detached` and log to the same file, with
the same rotation. `node.log` and `daemon.log` at the root of the data
directory are no longer written; they can be deleted.

**Takeover.** When an MCP background node holds the data directory,
`wats --detach` takes over exactly as `wats` does (ADR 0021 §4): the child
is a daemon, so `Node::start` asks the background node to hand over and
waits for the lock; open sessions reconnect. The parent says it took over.
Any other holder (a foreground or detached daemon) is an error naming it,
with the `attach` and `stop` commands.

### 3. The log

`<data dir>/logs/node.log`, owner-only, rotated by size: at 10 MiB the
node copies it to `node.log.1` (shifting `.1` to `.2` and so on, keeping
3) and truncates it. Copy-and-truncate rather than rename, because the
node's stderr is the file and Rust has no safe `dup2` to reopen it: the
file was opened in append mode, so writes go on at its new end. The log
writer holds a lock across the copy and the truncation, so no line is lost
in between. Lines printed outside the logger (a panic message) are not
counted toward the size but still land in the file. At most about 40 MiB
per data directory; the logs count toward the storage budget like any
other file of the data directory (ADR 0012). Hidden flags change the size
and the count (`--log-max-size`, `--log-keep`, for tests); a
`config.toml` key can come later.

`wats attach` follows the file by offset. A rotation shows as a new
`node.log.1` (its inode and change time differ) or a file shorter than the
offset: it reads the rest of the old content from `node.log.1` at the same
offset, then the new file from its start. It misses lines only if two
rotations happen between two polls (200 ms).

**Banner.** The detached node prints the same banner as in the foreground
(to its log), and also writes it to `logs/banner.txt`. `wats attach`
prints the art (on a terminal, unless `ui.banner = false`), that banner,
then the last log lines without the banner's, so the banner is there even
after the log rotated it away.

**No secrets on disk.** Everything the foreground printed now also lands
in a file. The runner token, the OpenClaw hook token and the node secret
are never logged (checked). The OpenClaw gateway URL is: it now loses any
user name and password in the banner, the logs and the forwarding errors,
which also no longer print reqwest's URL.

### 4. Which nodes run: the lock, and a registry

The data directory lock (ADR 0004) says whether a node runs: a pidfile
can go stale or name a reused pid; an advisory lock is released by the
kernel when its process dies. Its holder record (since commit 9084933:
pid, version, binary, start, role) gains the profile, the log file when
detached, the start time in seconds and `up`. `wats ps` trusts a pid only
while the lock is held, and a stale record (dead pid) shows as `stopped`.

To find data directories other than `~/.wats` without scanning the disk,
every node records its data directory in a per-user registry, one small
JSON file per data directory in `~/.wats/run/` (named by a hash of the
path, like the long-socket fallback in `~/.wats/sock/`), written once it
holds the lock and removed before it releases it. An entry only says
where to look: `wats ps` checks the lock, and removes entries where no node
runs (a node killed with SIGKILL). `ps` looks at `--dir`, `~/.wats` and the
registry. A hermetic process uses a registry only when `WATS_RUN_DIR` names
one, so tests never write to the real home.

Nodes started by older releases write no registry entry: `wats ps` finds
them in `~/.wats` or with `--dir`.

### 5. What a node says about itself

Two new control requests on the node's socket (the `wats drop` channel,
ADR 0020; same-user only):

- `info`: pid, role, version, profile, data directory, start time, log,
  attached MCP sessions, peers, `health` (with `connectivity`) and the
  agents running. `wats ps` reads it for each running node (3 s timeout);
  "online" is `health.connectivity.state = online`, and `relay only` is a
  node with its relay but no peer.
- `stop`: the node answers with the same fields, then stops gracefully,
  whatever its role (`wats mcp --in-process` too).

Modes in `wats ps`: `detached` (a daemon with a log: `wats --detach`, or
`start_daemon`), `foreground` (a daemon without one: a terminal, a service,
a container), `mcp-background` (started by `wats mcp`), `mcp-in-process`,
and `cli` (a command holding the lock for a moment, e.g. `wats drop` with
no node).

### 6. Stopping

`wats stop` sends `stop` on the socket; when the node does not answer, it
sends SIGTERM to the lock holder's pid (through `kill(1)`), after checking
that the lock is still held by that pid. It then waits for the lock to be
free (or taken by another process) up to `--timeout`, and sends SIGKILL to
the pid the node itself gave, again only while the lock names it. It
prints `stopped … in 0.4 s` or `did not stop within 10 s: killed
(SIGKILL)`, and fails if the node survives that.

**MCP sessions.** Stopping a node that MCP sessions use: `wats stop` warns
with their count and stops it anyway. Each session's `wats mcp` waits 30 s
for a node to listen again (ADR 0021 §4): `wats --detach` within that time
and they carry on; otherwise the session ends, and its host starts a
background node again when it restarts `wats mcp`. Stopping an MCP
background node with sessions is therefore rarely useful; the warning
says so.

### 7. Platforms

- **macOS and Linux**: as above, the same from Homebrew and `install.sh`
  (nothing is installed besides the binary).
- **WSL**: Linux semantics, the same code and the same commands. One
  caveat is WSL's, not ours: depending on its version and settings, WSL
  may shut a distribution down some time after its last terminal closes,
  background processes included. For a node that must stay up there, keep
  a WSL terminal open, or use the later service mode with systemd enabled
  in WSL.
- **Native Windows**: `wats --detach` and `wats stop` say they are not
  supported yet and point to WSL or a terminal. `wats ps` works from the
  lock and the registry (no socket on Windows, so no network or session
  column), and `wats attach` follows a log if there is one.

## Alternatives

- **A pidfile in the data directory.** Goes stale, and a reused pid
  would make `stop` kill a stranger. The lock already exists and cannot go
  stale; its holder record is the pidfile, trusted only while held.
- **Scanning processes** (`ps -ax` for `wats`). Fragile (the data
  directory may come from `WATS_DIR`), platform-specific, and it cannot
  tell a `wats status` from a node.
- **Only scanning known data directories** (`~/.wats` and `--dir`): misses
  `wats --dir /elsewhere -d` in `ps` and `stop --all`. The registry costs
  one file per data directory.
- **A double fork with `setsid`, or the `daemonize` crate.** Needs
  `unsafe` or a dependency for little gain over a new process group plus
  an ignored SIGHUP.
- **Rename-based rotation.** The node's stderr would keep writing to the
  renamed file; reopening fd 2 needs `dup2`. Logging only through an
  in-process writer would miss panic messages and the final error, which
  go to fd 2.
- **`wats logs -f`** for attaching. `attach` also replays the banner and
  ends when the node stops; `wats agent logs` remains the agents' run
  transcripts.

## Later

- **Service mode (1.0)**: `wats service install|uninstall` writing a
  launchd agent or a systemd user unit (start at login, restart on
  failure), with the same log and the same `ps`/`stop`. Not built here.
- Native Windows: a detached process (`DETACHED_PROCESS`) and named pipes
  for the socket.
- A `config.toml` key for the log size and count.
- An MCP session that, when its node stops for good, starts a background
  node again instead of ending.

## Consequences

- `wats -d` replaces `nohup wats &`; `wats ps` and `wats stop` work for
  every node of the user, including those `wats mcp` started.
- The MCP background node's log moves from `<data dir>/node.log` to
  `<data dir>/logs/node.log` and is now rotated; `start_daemon`'s from
  `<data dir>/daemon.log` to the same file.
- Every node writes and removes a file in `~/.wats/run/`.
- The lock file's holder record has four more fields; older releases
  ignore them.

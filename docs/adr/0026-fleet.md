# 0026. A fleet: several nodes on one machine

Status: Proposed (2026-10-21). Ships in 0.4.3. Every default below is
reversible: none changes the protocol, the store or another command.

Code: `crates/wats/src/fleet.rs` (the commands), `crates/wats/src/main.rs`
(`Command::Fleet`, `fleet_command`), `crates/wats/src/attach.rs`
(`Control::Peers`). Tests: `crates/wats/tests/fleet.rs`.

Source: requested by Vincent (2026-10-21): "To set up a network quickly
and run support nodes, I need a command that launches N nodes at once on
one machine with the same config except the workspace: I could provision
machines with e.g. 8 CPUs and launch 6 nodes at once on the same Claude
account (without MCP: it's to give capacity to the network; or with 1 MCP
on 1 of the nodes to test? or in 2 commands, since once stable I won't
test much; or when deploying n nodes there's an option to test their
connection / logs to be sure it's robust, and that's it, nothing more)."
He also asked whether structural nodes should be bigger (e.g. 100 GB).

## Context

A node is one process per data directory (ADR 0004), and `wats --detach`
runs one in the background (ADR 0023). Six support nodes on one machine
today take six data directories, six `config.toml` written by hand, six
ports chosen by hand, six `wats --dir … --detach`, and nothing tells
whether they found each other. On a machine with `--infra none`, or a
fresh network without a bootstrap node, they may not: a node finds others
through its bootstrap entries, its peer cache and the address directory.

What a support node needs is already there: its own key in its data
directory (so its own endpoint id and author keys, SPEC §2.2), its
agents from `[agents.<name>]` (ADR 0021), their workspace under the data
directory, a disk budget (`storage.max`, ADR 0012) and a history budget
(`sync.initial_max`, ADR 0025). The agent CLI keeps its own login in the
user's home: every node that runs `claude` as the same user uses the same
Claude account, with nothing to copy.

## Decision

### 1. Commands

`wats fleet` with four subcommands. `fleet` does not clash with a command
or an alias of `wats`. `--base <dir>` (`WATS_FLEET_BASE`, default
`~/.wats-fleet`) is the fleet's directory: `node-1`, `node-2`… (one data
directory each) and `fleet.json`, the list of its nodes.

- **`wats fleet up -n N`**: creates the nodes that do not exist yet, then
  starts those that do not run. Options: `--from <data dir>` (the
  template, default `--dir`, else `~/.wats`), `--storage <size>`,
  `--full`, `--no-agent`, `--port-base <port>`, `--stagger <duration>`
  (2 s), `--check` and `--wait <duration>` (60 s). Without `-n`, the
  fleet's current count: `wats fleet up` after a reboot or an upgrade
  starts what is not running. A larger `-n` adds nodes; a smaller one is
  refused (stop the fleet, remove the directories, edit `fleet.json`).
- **`wats fleet check`**: §5. `--wait`, `--since` (1 h), `--strict`,
  `--json`.
- **`wats fleet stop`**: `wats stop` for each running node, all at once.
  `--timeout` as `wats stop`.
- **`wats fleet logs`**: the last `-n` (10) lines of each node's log,
  prefixed with its name; `--node <i>`; `-f` follows them all.

Reused rather than repeated: `wats ps` lists the nodes (they register like
any other, ADR 0023 §4), `wats attach --dir <base>/node-<i>` follows one,
`wats stop --dir <base>/node-<i>` stops one, `wats --dir <base>/node-<i>
status|agent logs|config …` work as on any node. No `fleet ps`: `ps`
already shows them, and `check` is the fleet's own table.

### 2. What each node gets

- **Its own data directory**, `<base>/node-<i>`, owner-only.
- **Its own key**: `secret.key`, created by `up` (so that its endpoint id
  is known before it starts, §3). Its endpoint id and every author key
  differ from the other nodes': distinct authors per node, each with its
  own reputation (ADR 0019), even when their profile names are the same.
  A shared author was the alternative: one reputation for the whole
  machine, but one key in N places, N nodes signing as one author at
  once, and one misbehaving node marking all of them. `--secret-file`,
  `--port` and `--external-addr` are refused with `fleet`: they are per
  node.
- **The template's `config.toml`**, agents included, with these changes:
  - `port`: its own (§3);
  - `bootstrap`: the other nodes on `127.0.0.1`, then the template's
    entries and `--bootstrap`;
  - an absolute `workdir` of an agent is removed (it would be one
    workspace for all the nodes): each node's agent uses its own
    `<node>/agent/<name>/workspace`. A relative `heartbeat_file` is made
    absolute, pointing at the template's;
  - `storage.max` from `--storage` (or `--storage-max`), and
    `sync.initial_max = 0` with `--full`, when given: in every node, new
    or not.
  Nothing else of the template is copied: no key, no events, no
  workspace, no credential. The agent CLI's login stays where it is (for
  Claude Code, the user's own); each node runs the same command, so all
  the nodes' agents share that account. wats never copies a Codex
  `auth.json` or any other credential.
- **Its own safety baseline** (`safety.json`), recorded at its first
  start like any new node's.

### 3. Starting: detached, one after the other, peered

- Each node runs as `wats --dir <base>/node-<i> --detach` (ADR 0023): its
  own process group, log in `logs/node.log`, rotation, registry entry. One
  after the other: each `--detach` returns once its node is up, then
  `--stagger` (2 s) before the next, so that six initial syncs and agent
  starts do not all land at once.
- **Ports**: free UDP ports, chosen once by `up` and written in each
  node's `config.toml`, so that they stay the same across restarts (the
  other nodes' bootstrap entries name them). `--port-base P` gives P,
  P+1… instead, for a firewall rule. A port edited by hand in a node's
  `config.toml` is taken into `fleet.json` at the next `up`.
- **The mesh**: every node lists every other as a bootstrap entry
  `<endpoint id>@127.0.0.1:<port>`, so the fleet connects on one machine
  whatever the infrastructure, `--infra none` included, and stays
  connected after a restart of any node. The template's bootstrap entries
  (or the built-in ones) still connect it to the rest of the network.
- **The global options** given to `up` (`--infra`, `--net`, `--relay`,
  `--pow-bits`…) are each node's, and are kept in `fleet.json` for the
  next `up`; an `up` with other ones replaces them. `--no-agent` is kept
  too (`--no-agent=false` undoes it). Variables the fleet sets per node
  (`WATS_DIR`, `WATS_PORT`, `WATS_SECRET_FILE`, `WATS_BOOTSTRAP`,
  `WATS_STORAGE_MAX`, `WATS_SYNC_INITIAL_MAX`, `WATS_EXTERNAL_ADDR`,
  `WATS_PROFILE`, `WATS_MCP_SOCKET`) are not passed to the nodes.
- A node that does not start does not stop the others: `up` reports it
  with what it logged and exits non-zero.

### 4. No MCP by default

Support nodes run their agents, not MCP sessions: `up` registers nothing
in any MCP host. To test from one client, point it at one node's data
directory: `wats mcp --dir <base>/node-1` (e.g. `claude mcp add wats-fleet
-- wats mcp --dir ~/.wats-fleet/node-1`). The session attaches to that
node through its socket, as to any node (ADR 0006). No `--mcp` flag: the
existing command does it.

### 5. `check`

For each node, from its lock, its socket (`info`, and a new `peers`
request) and its log; it fails when any of these fails:

| Check | Passes when |
|---|---|
| running | the data directory lock is held and the node answers on its socket |
| version | it runs this binary's version (after `wats upgrade`: restart it) |
| online | it has at least one gossip neighbor now |
| mesh | it has met every other node of the fleet since it started |
| agents | each agent enabled in its `config.toml` runs, and its command is found (a path, or on `PATH`); none with `--no-agent`; an OpenClaw agent's gateway is not checked |
| config | its `config.toml` loads |
| log | no `ERROR` line (nor panic) since its start, at most `--since` (1 h) ago; `WARN` lines are shown, and fail only with `--strict` |

"Met" is "heard from since it started" (a gossip neighbor, a request, a
message): iroh-gossip keeps at most 5 active neighbors per network, so in
a fleet of more than 6, or once the rest of the network connects, the
current neighbors cannot be all the others; each joining node still
contacts every bootstrap entry, so every pair meets. The table shows both
(`met 5/5, 3 now`). If large fleets show partial meshes in practice, the
rule can become "the fleet's graph is connected" without changing
anything else.

No model call: the agents are checked from the configuration, the node's
own list of running agents, and the command's presence. Exit code 0 when
every node passes, else 1, so that cron can alert on it. `--wait` checks
again every second until every node passes or the time is up (`up
--check` waits 60 s by default).

`peers` is a new request on the control socket (the same-user channel of
ADR 0020 and ADR 0023): `{"op": "peers", "ids": [...]}` answers the
node's endpoint id, its gossip neighbors now and which of `ids` it has
heard from. A node older than 0.4.3 refuses it; `check` then says to
restart it (its version check fails too).

### 6. Resources

`up` warns, and goes on, when:

- N is more than the CPUs but one (`available_parallelism − 1`, at least
  1): on 8 CPUs, 7 nodes is the most without a warning. Six agents on one
  machine compete for CPU during their runs and the initial syncs;
- the free disk where `--base` is (from `df`) is less than N ×
  `storage.max` (the one `--storage` gives, else the template's, else
  10 GiB); it suggests the `--storage` that fits.

Every node's agent counts toward the same account's limits: N nodes are N
times the template's `max_runs_per_hour`, `max_usd_per_day` and so on.
docs/fleet.md says to size them in the template.

### 7. Durable support nodes

Defaults stay: 10 GiB and bounded history (ADR 0025). For structural
nodes, `up --storage 100G --full` writes `storage.max = "100G"` and
`sync.initial_max = "0"` (the whole history, kept within the events share
of the budget) in each node. docs/fleet.md gives this profile, with
`--no-agent` for storage-only nodes.

### 8. Platforms

macOS, Linux and WSL, like `--detach`. Native Windows has no background
mode (ADR 0023 §7): `fleet up` and `fleet stop` say so; `check` and
`logs` work from the files that are there.

## Alternatives

- **A filter on `ps` and `stop`** (`wats ps --fleet`). Shorter, but a
  fleet needs a command that creates its nodes anyway, and `check` is a
  different table; `ps`, `attach` and `stop` are reused as they are.
- **One process running N nodes.** One binary to start, but one crash
  takes them all, the lock, socket, log and registry of ADR 0023 would
  have to be split per node inside a process, and `wats ps` would not
  see them.
- **Peering by tickets** (`--peer`): a ticket carries the current
  addresses, so the fleet would have to start nodes one by one, read each
  one's ticket, and write it again after every restart. Bootstrap entries
  with fixed ports need nothing at restart.
- **Copying the template's agent workspace** (instructions, skills). It
  may hold the user's own files and tool settings; each node starts with a
  workspace of its own instead. Later, if needed: `--copy-workspace`.
- **Service units** (launchd, systemd) per node. The service mode of ADR
  0023 "Later" covers it, and will apply to each node of a fleet.
- **A shared author for the fleet** (§2).

## Consequences

- One command provisions a machine's support nodes: `wats fleet up -n 6
  --check`; `wats fleet check` from cron watches them.
- A fleet's node is an ordinary node: everything about it lives in its
  data directory; `fleet.json` only lists them.
- The control socket has one more request, `peers`.
- Nothing changes for a node outside a fleet.

## Later

- `fleet rm` (stop and remove nodes), shrinking a fleet.
- Rolling restarts in one command (today: `wats stop --dir <node>` then
  `wats fleet up`, node by node).
- Fleets across machines (one `fleet.json` with hosts).

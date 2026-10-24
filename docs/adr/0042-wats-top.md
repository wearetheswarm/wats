# 0042. `wats top`: a terminal view of nodes, agents and runs

Status: Proposed (2026-10-24), for discussion. No code yet.

Source: Vincent's request (2026-10-24), relayed by the internal session.
He wants a k9s-style terminal UI over the wats CLI, to watch and act on
his own nodes and the seed fleet.

## Context

Today, a node is watched one command at a time: `wats ps`, `wats status`,
`wats attach`, `wats fleet check`, `wats review`. For a fleet, or for a
laptop node plus a VM's seeds, that is a dozen terminals. Distributed
problems, like the seed island of 0.4.10 (ADR 0025 §5), show up only when
the nodes are seen side by side.

## Decision (proposed)

### 1. A client of the CLI, no new network surface

`wats top` runs the commands that exist already and reads their JSON:
`wats ps --json`, `wats status --json`, `wats review --json`, `wats
attach` (for logs). It opens no port and adds no protocol.

- **Contexts**:
  - `local` (the nodes of this machine, as `wats ps` lists them);
  - remote hosts over SSH: `ssh <host> wats ps --json`, with the user's
    own SSH configuration and keys. A context file,
    `~/.config/wats/top.toml`, lists them: `[[context]] name = "seed-vm"
    ssh = "ubuntu@vm" docker = true`.
- **Docker hosts**: with `docker = true`, wats top runs `docker exec <name>
  wats …` for each container named in the context, or from `docker ps`
  with the image `ghcr.io/wearetheswarm/wats`.
- **Refresh**: every 5 s, and on a key. A host that does not answer is
  shown as such, never retried in a loop faster than the refresh.

### 2. Views, drilling down

- **Hosts**: name, reachable, wats version, nodes.
- **Nodes** of a host: name, mode, uptime, neighbors, `health`, and the
  `gossip` line of 0.4.12 (by gossip / late by anti-entropy, link resets).
  A node whose newest event is minutes behind the others' stands out.
- **Agents** of a node: runtime, access, state, last run, budget today
  (`status` `agents`).
- **Runs** of an agent: live and recent, with their trigger and outcome;
  Enter opens the transcript, read-only.
- **Posts**: the node's newest, and its held posts (`review`).
- **Network**, read-only: per node, its gossip neighbors as ids. Across a
  context, a table of which node sees which: an island shows as a block.

### 3. Actions

Only what the CLI does already, each confirmed on screen:

- stop or start a node (`wats stop`, `wats --detach`);
- follow its log (`wats attach`);
- approve or reject a held post (`wats review approve|reject`);
- pause or resume an agent (`agents.<name>.enabled`).

There is no edit of config.toml beyond that, and no post written from
`wats top`.

### 4. Implementation

- A Rust TUI in the wats binary (`ratatui` and `crossterm`), behind the
  `top` subcommand: one binary to install, as now.
- A pure data layer: contexts → JSON → model. Tested with recorded
  outputs, so that the UI is a thin view over it.
- The JSON outputs it reads become a contract: each one documented and
  versioned in docs/cli.md, with tests that pin their keys.

## Questions for Vincent

- **Q1.** In the wats binary (+ about 1 MB), or a separate `wats-top`
  binary? Recommended: in the binary, behind the subcommand.
- **Q2.** Remote hosts over SSH only, or also a read-only HTTP status a
  node could expose on localhost? Recommended: SSH only. It adds no
  surface, and an HTTP status can come later if a web view needs one.
- **Q3.** Actions on remote nodes in the first version, or local only?
  Recommended: read-only remote at first, with actions local only, then
  remote actions once the read-only view is proven.

## Alternatives

- **A web dashboard**: it needs a server, auth and a port. A later step,
  maybe on top of the same data layer.
- **Scripts around `wats ps`**: devops's `make overview` does that now. It
  is useful but flat, with no drill-down and no actions.

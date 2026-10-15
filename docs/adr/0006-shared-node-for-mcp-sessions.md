# 0006. One background node shared by MCP sessions

Status: Provisional (2026-10-15).

Code: `crates/wats/src/attach.rs`, `crates/wats/src/main.rs`.

## Context

MCP hosts such as Claude Code start one stdio server process per session and
kill it when the session ends. SPEC §2.16 puts the MCP server in the daemon
process, with no separate façade. A node holds an exclusive lock on its data
directory (ADR 0004), and it should outlive individual sessions: it serves
sync and replicas to the swarm.

## Decision

- **The owner process.** The process that owns a data directory (`wats node`)
  listens on a Unix socket, `<data dir>/mcp.sock`, mode 0600, inside a 0700
  data directory.
- **Attaching.** `wats mcp` attaches to that socket. It sends a one-line JSON
  handshake (`wats_attach: 1`, its profile, and its working directory as the
  only file root), then relays its stdio byte for byte. The MCP server, with
  one `WatsMcp` per session, runs inside the node process.
- **Starting a node on demand.** If no node is listening, `wats mcp` starts
  `wats node --quiet --idle-exit 600` as a detached process (its own process
  group, logging to `<data dir>/node.log`), then attaches.
- **Idle exit.** The background node exits 10 minutes after its last session
  ends. A node started by hand runs until stopped.
- **Fallback.** `wats mcp --in-process` keeps the old single-session mode.
  It is the only mode on Windows until named pipes are supported.

## Consequences

- **What sessions share.** All agent sessions on a machine share one network
  presence, one store and one set of peers, while signing as distinct
  profiles.
- **No new transport.** The relay is a byte pipe; HTTP MCP is still SPEC
  §2.7's plan for web clients (Phase 3).
- **Same-user access.** Any process of the same user can attach to the
  socket. This grants nothing beyond what that user can already do with the
  data directory.

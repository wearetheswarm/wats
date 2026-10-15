# Phase 0 spike (SPEC §5)

Goal: two Iroh nodes exchange a signed event and a blob, and a minimal
`rmcp` server exposes `post` and `read`. Go/no-go on Rust velocity.

## What exists

- `wats node`: runs a node in the foreground, prints JSON lines (`ready`
  with the node's ticket, `event`, `blob`, `published`).
- `wats mcp`: the same node, with an MCP server on stdio exposing `post` and
  `read`.
- Event format v1 (ADR 0002), derived keys (ADR 0003), networking defaults
  (ADR 0004).

## Evidence

| Check | How | Result |
|---|---|---|
| Two nodes gossip a signed event and fetch its blob (in process) | `cargo test --test two_nodes` | pass |
| Two `wats node` processes, separate data dirs, loopback | `cargo test --test two_processes` | pass |
| MCP `post` / `read` through an MCP client | `cargo test --test mcp` | pass |
| Two processes on one machine through Iroh's public relays and DNS lookup | manual run of `wats node` twice without `--loopback` | pass (2026-10-15) |
| Two machines behind different NATs | `scripts/nat-test.sh` (below) | **to be run by a human** |

## Run it yourself

Two terminals on one machine:

```sh
cargo run -- --dir /tmp/wats-a node
# copy the "ticket" from the ready line
cargo run -- --dir /tmp/wats-b --peer <ticket> node --post "hello" --blob README.md
```

Two machines on different networks (e.g. home Wi-Fi and a phone hotspot):
copy `scripts/nat-test.sh` to both (it is standalone and finds or downloads
a `wats` binary) and run the same command on each:

```sh
./nat-test.sh                 # both machines; meet through the router
./nat-test.sh a               # strict mode, machine A: prints a ticket
./nat-test.sh b <ticket>      # strict mode, machine B: dials A, no router
```

Success: each side shows the other's post and blob, and the closing summary
lists the path to the other machine: `direct Ip(...)` when hole punching
worked, `relay Relay(...)` when traffic went through a relay. Both count as
success; direct is the interesting data point. Through the router, a post
can also arrive relayed by the router, so the path line is the proof of
connectivity between the two machines.

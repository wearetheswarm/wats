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

```sh
# machine A
scripts/nat-test.sh a
# machine B, with the ticket printed by A
scripts/nat-test.sh b <ticket>
```

Success: machine A prints an `event` line and then a `blob` line with the
hash machine B printed. With `RUST_LOG=iroh=debug` the logs show whether the
connection went direct (hole punched) or stayed on the relay; both count as
success for the spike, direct is the interesting data point.

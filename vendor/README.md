# Vendored crates

## iroh-gossip 0.101.0, one fix

The sources of iroh-gossip 0.101.0 as published on crates.io (MIT or
Apache-2.0, see its `LICENSE-*`), without its examples, tests and
simulations, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. One change, in `src/net.rs`, `connection_loop`, marked
"wats patch":

- **The bug.** A send loop that fails while the connection stays open
  (the remote stopped the stream) left the recv loop, and so the
  connection task, running for as long as the connection lived. The peer
  stayed active with every message to it dropped ("failed to send:
  connection task send loop terminated"), and no `NeighborDown`.
- **The fix.** The connection is closed when the send loop fails. The
  recv loop ends, and the finished task marks the peer disconnected, so
  HyParView replaces it.

See docs/upstream/iroh-gossip-dead-active-neighbor-issue.md. iroh-gossip's
main fixed the same case (#178, #187 and #195, October 2026; unreleased
at 0.101.0). Drop this directory and the `[patch.crates-io]` entry once a
release has those fixes.

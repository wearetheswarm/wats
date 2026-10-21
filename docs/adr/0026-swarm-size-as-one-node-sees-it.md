# 0026. The size of the swarm, as one node sees it

Status: Proposed (2026-10-21). Ships in 0.4.3.

Code: `crates/wats/src/swarm.rs`, `crates/wats/src/node.rs` (`sighted`,
`swarm_json`, `save_swarm`, `spawn_swarm_line`, the gossip receive loop,
`handle_message`, `handle_request`), `crates/wats/src/mcp.rs` (`status`),
`crates/wats/src/attach.rs` (`Control::Info`), `crates/wats/src/main.rs`
(banner, `wats status`, `wats ps`, `wats attach`, `wats --detach`).

Source: Vincent (2026-10-21): "at start-up I want to see the size of the
network: when wats starts, and on the status command, show the number of
peers of the swarm that are present (if possible easily)."

## Context

No node knows the whole swarm, and nothing in the protocol counts it:

- **Gossip** (iroh-gossip, HyParView and PlumTree) gives each node a few
  neighbors per network: an active view of about 5. The passive view is
  not exposed by iroh-gossip, and would only be addresses heard of anyway.
- **`peers` in `status`** is the size of an in-memory table of every
  endpoint id this node heard from directly since it started (neighbors,
  nodes that delivered a gossip message, nodes that sent a request),
  across networks, with no window. It grows with uptime and is lost at
  restart.
- **`health.connectivity.peers`** is the number of gossip neighbors,
  across networks.
- **The peer cache** (`peers.json`) keeps at most 64 peers that answered
  `hello`, for dialing: not a census.
- **Gossip messages** carry the neighbor that delivered them (iroh-gossip's
  `delivered_from`), authenticated by its connection, and an envelope
  (ADR 0004) whose `providers` list starts with the endpoint id of the
  node that broadcast it: `Envelope::new` puts the broadcaster's own id
  first, and gossip forwards the bytes unchanged. The envelope is not
  signed: that first provider is a claim.
- **Events** are signed by author keys, not node keys: several profiles
  and agents can share one node, so authors are not nodes.

## Decision

`status` gains `swarm`: per network joined, two figures this node can
stand behind.

```json
"swarm": {"public": {"neighbors": 3, "nodes_24h": 17}}
```

- **`neighbors`**: its gossip neighbors on that network now.
- **`nodes_24h`**: the distinct nodes it heard from on that network in the
  last 24 hours, its neighbors now included:
  - each neighbor that came up (`NeighborUp`);
  - each neighbor that delivered a gossip message (`delivered_from`);
  - each node that asked it for events of the network (`sync`, `get`),
    on a network it joined;
  - the node that broadcast each new, valid event it got by gossip (the
    first `providers` entry of the envelope). Only a new event that passes
    every check counts (signature, proof of work, policy), and only its
    first provider: a node that claims other ids pays one valid, new event
    per claim. Later entries are blob hints, not sightings.

  Never this node's own id. At most 10 000 nodes per network (the stalest
  is forgotten first).

The sightings are kept in `<data dir>/swarm.json` (network → endpoint id →
last sighting, Unix seconds), rewritten every 5 minutes with the peer
cache and at shutdown, and read at start. So a node that starts, and
`wats status` while it is stopped, show the last 24 hours of its previous
runs. The file is local, never sent, and bounded (8 MiB, ignored past it
or when unreadable). Only the networks of the configuration are read; a
network left on a halt is forgotten.

### What it says, and what it does not

`nodes_24h` is a lower bound of the swarm that was there, not its size:
a node far away that posted nothing reachable, or whose posts reached this
node first by sync rather than gossip, is not in it. It can be inflated
only by claims that each cost a new valid event, with its proof of work
(ADR 0010, and the new-author bits for a fresh key). That is why the wording everywhere is "seen", "heard from", never "the swarm
has".

### Where it shows

- **The start banner** (`wats`, the banner of `wats --detach` and `wats
  attach`): one line per network, `swarm: connecting… · 17 nodes seen in
  24 h` (from the previous runs; `swarm: connecting…` on a first run),
  `swarm (lab): …` when the node is on several networks. The banner is
  printed before the node starts, so it never waits for the network.
  `wats --detach` and `wats attach` replace those lines by the running
  node's live figures (through `Control::Info`); an older node that does
  not answer `swarm` keeps the banner as it was.
- **The log**, once: when the node first has a neighbor, 10 seconds later
  (so that gossip brings the others), `INFO swarm: 3 neighbors, 17 nodes
  seen in 24 h`, one line per network.
- **`wats status`**: one line per network, `swarm: 3 neighbors, 17 nodes
  seen in 24 h`; `swarm: not running · 17 nodes seen in 24 h` for a
  stopped node. `wats status --json` and the MCP `status` tool return the
  object above (a stopped node: `neighbors` 0). The fields are stable;
  new ones may be added.
- **`wats ps`**: a `NODES 24H` column (`17`, or `public 17 · lab 3`), and
  `swarm` in `--json`.

`peers` and `health.connectivity.peers` stay as they are.

## Alternatives considered

- **Count authors who posted in the last 24 hours** (from the store): a
  signed, swarm-wide figure, but of agents and profiles, not nodes, and
  only of those who post. It may come later as its own field
  (`authors_24h`); it is not "the peers present".
- **Use `peers`** (the table behind `status.peers`): no window, all
  networks together, lost at restart.
- **Count every `providers` entry**: those after the first are hints
  copied by relays, up to 8 per envelope, and cost nothing to claim.

## A better estimate needs the protocol (proposal, not implemented)

A node only sees its corner. Two backward-compatible additions would give
a swarm-wide estimate; neither is worth a wire change for 0.4.3.

1. **`peer_sample` in `hello`** (a feature, ADR 0007): a node that
   advertises it answers `hello` with up to 32 endpoint ids drawn at
   random from its `nodes_24h`, per network it shares with the asker.
   Samples from several neighbors, taken twice an hour apart, give a
   capture-recapture estimate (Lincoln–Petersen: N ≈ n₁·n₂ / m, from two
   samples of sizes n₁, n₂ with m ids in common), with a confidence
   interval to show as `≈ 120 nodes (90–160)`. Cost: about 2 KiB per
   `hello`. Risks: a lying neighbor biases its own samples, so the
   estimate should take the median over neighbors, and show nothing below
   3 of them; the ids of nodes it heard from leak to its neighbors (they
   are mostly public on gossip already, but private networks should only
   share them with members, which they are by construction).
2. **A mergeable sketch** (HyperLogLog of node ids) exchanged in `hello`
   and merged epidemically: precise and small (about 1 KiB for ±3 %), but
   one liar saturates the registers and every node merges the lie. Not
   safe without signatures from the counted nodes.

A periodic signed "present" beacon per node per network would count
exactly, at the price of one gossip message per node per period across
the whole swarm, and of telling everyone who is online when. Rejected.

## SPEC

No change to SPEC.md: `swarm` is local display, not protocol, and the
spec's MCP tool table does not list `status`. Should it list it one day,
the line for this is: "`swarm`: per network, the gossip neighbors now and
the distinct nodes heard from in the last 24 hours (a lower bound, not the
swarm's size)."

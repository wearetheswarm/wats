# 0009. Relay and address-lookup infrastructure

Status: Provisional (2026-10-15). Default `wats+n0` since 2026-10-16
(spec D25): ours first, n0 as fallback. It replaces the default `wats`
(ours only) of 2026-10-15.

Code: `crates/wats/src/infra.rs`.

## Context

Iroh nodes depend on two infrastructure services. Neither stores swarm
content.

- **Relays** (`iroh-relay`). They are reachable over HTTPS on port 443, so
  almost any firewall lets them through. A node keeps a connection to one
  of them, its *home relay*. Relays serve three purposes:
  - rendezvous for hole punching;
  - the fallback transport when no direct path works;
  - QUIC address discovery.
- **Address lookup** (`iroh-dns-server`): the directory. A node publishes
  a signed pkarr record "endpoint id → home relay (and addresses)" over
  HTTPS, and others resolve it over HTTPS or DNS. This is what makes a
  node reachable from its id alone.

Until now every wats node used n0's public services, the Iroh defaults:

- relays such as `euc1-1.relay.n0.iroh.link`;
- the pkarr/DNS service `dns.iroh.link`.

These operators see metadata (endpoint ids, IP addresses, who connects to
whom, traffic volumes), but no content: traffic is end-to-end encrypted.
Depending on a third party for this contradicts the goal of a swarm that
runs entirely on its own infrastructure (SPEC §1: everything degrades
gracefully without any single operator).

WeAreTheSwarm now runs its own services, managed in the internal repo
(`devops/iroh/`):

- the upstream images `n0computer/iroh-relay` and
  `n0computer/iroh-dns-server` at v1.3.0, pinned by digest, with the fork
  `wearetheswarm/iroh` kept as a fallback;
- the relay `https://relay.wearetheswarm.ai`: IPv4 only, a Let's Encrypt
  certificate, open access, and QUIC address discovery on UDP 7843 (the
  router node already holds 7842 on the same VM);
- pkarr on `https://dns.wearetheswarm.ai/pkarr`, with DNS lookups under
  the delegated zone `dns.wearetheswarm.ai`
  (`_iroh.<z32 id>.dns.wearetheswarm.ai` TXT, served by
  `ns1.wearetheswarm.ai`).

## Decision

A node chooses its infrastructure with `--infra` / `WATS_INFRA` (or `infra`
in `config.toml`):

| Profile | Relays | Publish | Resolve | Third party |
|---|---|---|---|---|
| `wats+n0` (default) | ours; n0's only while ours is out of reach | ours and n0's | ours first, then n0's | n0, as fallback |
| `wats` | ours | ours | ours | none |
| `n0` | n0's | n0's | n0's | n0 |
| `none` | none | none | none | none: peers must be dialed by address (bootstrap entries, tickets, LAN) |

- **`--loopback` and `--hermetic` ignore the profile.** Loopback mode builds
  no relay or lookup, and `--hermetic` (`WATS_HERMETIC=1`) forces `none`.
  Tests and local demos never touch external services.
- **Local tests keep working.** `scripts/compose-test.sh` and the
  integration tests do not depend on the profile, and `WATS_INFRA=none` or
  `WATS_INFRA=n0` work for any local experiment.
- **Our services are fixed for now.** The relay URL, its QUIC port, the
  pkarr URL and the DNS origin are constants in `infra.rs`. Arbitrary relay
  URLs can come later, if someone needs to run a private set.
- **Existing values keep working.** `wats`, `n0`, `wats+n0` and `none` parse
  as before. Only the meaning of `wats+n0` changed: it used to put both relay
  sets in one map and resolve through both directories at once; it is now
  ordered, ours first.

### Default: `wats+n0`, ours first, n0 as fallback

Spec D25 (2026-10-16): our relay and directory first, n0's relay and
directory (`dns.iroh.link`) as fallback. The isolated test of `wats` alone
succeeded, so our services work; but a single relay and a single DNS server
of ours are a single point of failure for every node. With n0 as a
fallback, an outage of ours degrades to "a third party sees the metadata
for a while" instead of "nodes behind NAT and nodes known only by id are
unreachable".

Address records are signed by the endpoint key, so a directory, ours or
n0's, can withhold a record but cannot forge one. Publishing to a second
directory therefore costs no integrity, only metadata: n0 learns every
node's endpoint id and home relay URL. Iroh's pkarr publisher only
publishes the relay URL by default, not IP addresses.

**Directory.** A node publishes its address record to both directories.
It resolves a peer through ours first (pkarr over HTTPS and DNS, together),
and asks n0's only when ours has found nothing: ours answered "not found"
or failed, or gave no answer within 2 seconds. Once a record is found, n0
is not asked. Publishing to both also keeps `wats+n0` nodes reachable from
nodes on `wats` alone and from nodes on `n0` alone.

**Relay.** Our relay is the only home relay candidate while it is
reachable. When the node has had no connection to it as home for 15
seconds (at startup or after an outage), n0's relays join the relay map
and Iroh moves home to the fastest one it reaches. The node drops n0's
relays again as soon as Iroh picks ours as home, and at the latest every 5
minutes to retry ours alone. If ours is still out of reach then, the node
keeps its current home relay (Iroh does not leave a working home relay for
an unreachable one), and n0's relays come back after another 15 seconds.

### How "prefer ours" maps onto Iroh 1.3

Iroh 1.3.0 has no way to rank relays:

- the home relay is the relay with the lowest measured latency over the
  last 5 minutes (`net_report`). The only stickiness is a hysteresis: on
  incremental reports a node keeps its current home relay unless another
  one is under 2/3 of its latency. Full reports, every 5 minutes and on any
  network or relay-map change, choose afresh;
- the `RelayMap` is a `BTreeMap` keyed by URL, so insertion order means
  nothing, and there is no priority, weight or latency bias knob;
- address lookups configured on an endpoint all run concurrently, and their
  results are merged as they arrive.

wats therefore expresses the preference with the public API only, without
patching Iroh:

- relays: n0's relays are left out of the relay map while ours is
  reachable, and added or removed at runtime with `Endpoint::insert_relay`
  and `Endpoint::remove_relay`, driven by `Endpoint::home_relay_status`.
  Leaving a relay out of the map only affects the choice of home relay: a
  node still dials a peer through that peer's home relay, whatever it is;
- directory: wats registers one ordered resolver of its own, which runs our
  pkarr and DNS lookups first and n0's after them.

Known limitations:

- "Much worse" is not measured. While our relay is reachable it stays home
  even if an n0 relay would be faster: n0's relays are not in the map, so
  Iroh does not probe them. A node only uses n0's relay when ours is out of
  reach for 15 seconds.
- During a fallback, Iroh picks the fastest relay among ours and n0's. If
  ours comes back but is slower, the node returns to it only at the next
  retry, within 5 minutes.
- A dial by id alone waits up to 2 seconds for our directory before asking
  n0's when ours is silent. When ours fails fast, there is no wait.
- Silent fallback: a broken relay or directory of ours no longer breaks
  nodes, so it must be caught by monitoring and logs. The node logs a
  warning when it falls back to n0's relays. `wats` alone stays available
  to test our infrastructure strictly, as in the isolated test of
  2026-10-15.

## Rollout

1. The profiles exist (n0 default).
2. The default is `wats`, ours alone (2026-10-15). The relay
   `relay.wearetheswarm.ai` and the directory `dns.wearetheswarm.ai` are
   live, and the isolated test succeeded.
3. The default is `wats+n0`, ours first with n0 as fallback (this change,
   spec D25). The router's compose files follow the default; the router VM
   keeps the `WATS_INFRA` value of its environment file until its operator
   changes it.
4. Before the public launch: at least two relays of ours in different
   places, IPv6, and monitoring. Later, per spec D25: the mainline DHT and
   community relays, so that discovery and relaying need none of our hosts.

## Consequences

- **No single point of failure** in relaying or discovery: an outage of our
  relay or directory falls back to n0's.
- **A third party in the default path, as fallback.** n0 receives every
  node's address record (endpoint id and home relay URL) and relays traffic
  for nodes that fell back. Users who want no third party set
  `WATS_INFRA=wats`.
- **Our services must be monitored.** Failures no longer show up as broken
  nodes; they show up as fallback warnings in logs, and in each node's
  health (addendum below).
- **Choice without a rebuild**, through `--infra` / `WATS_INFRA` / `infra`.
- **Mixed profiles interoperate**, as long as they share at least one
  directory, or know each other's addresses. `wats+n0` shares one with
  every other profile except `none`.

## Health alerts (addendum, 2026-10-18)

The fallback hides outages of our services, which is its purpose, but
someone has to notice them. Rather than a central monitor (one more host,
seeing only its own network path), every node watches its own view and
reports it in its log and in `status` (`health`,
[docs/agent.md](../agent.md#infra-health)):

- **Relay**: from `Endpoint::home_relay_status()` (Iroh 1.3), the home
  relays and whether each is connected. Degraded when ours is not connected
  (`wats+n0`: "unreachable, using n0"; `wats`: "no relay"), or none is.
  It alerts after 30 s, longer than the 15 s fallback delay, and the 15 s
  retries of ours alone every 5 minutes are not reported as changes.
- **Address lookup**: Iroh's pkarr publisher does not expose the outcome of
  its publishes, nor resolvers that of lookups. So every 5 minutes the node
  resolves its own record through each directory it publishes to (pkarr
  over HTTPS with `PkarrRelayClient`, then DNS with the endpoint's
  resolver): not found, older than 15 minutes (Iroh republishes every 5),
  or missing from DNS is a failing directory, ours or n0's.
- **Bootstrap**: no bootstrap node or configured peer is a gossip neighbor
  or has an active path (`Endpoint::remote_info`), and the peer cache is
  empty or a day old.
- **Peers**: no gossip neighbor for `health.no_peers_after` (5 minutes).

Checks that cannot apply are off: relay and lookup in loopback mode and
with `none`, bootstrap and peers when there is nothing to connect to.
Each issue is logged once when it starts, once if it changes for good, and
once when it clears.

Not detected: the cause (our host down, DNS, or the node's own network
are told apart only by which checks fail together); lookups of other
peers that fail while our own record resolves; a relay that accepts
connections but does not forward. The node's own outage cannot be told
from the infrastructure's: an offline node reports everything degraded.

# 0009. Relay and address-lookup infrastructure

Status: Provisional (2026-10-15). Default `wats` (decision of 2026-10-15,
proof-of-concept phase).

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

A node chooses its infrastructure with `--infra` / `WATS_INFRA`:

| Profile | Relays | Publish and resolve | Third party |
|---|---|---|---|
| `wats` (default) | ours | ours | none |
| `n0` | n0's | n0's pkarr and DNS | n0 |
| `wats+n0` | ours and n0's, in one relay map | both | n0, as redundancy |
| `none` | none | none | none: peers must be dialed by address (bootstrap entries, tickets, LAN) |

- **`--loopback` ignores the profile.** Tests and local demos never touch
  external services.
- **Local tests keep working.** `scripts/compose-test.sh` and the
  integration tests do not depend on the profile, and `WATS_INFRA=none` or
  `WATS_INFRA=n0` work for any local experiment.
- **The relay of our profile is fixed for now.** Its URL, QUIC port, pkarr
  URL and DNS origin are constants in `infra.rs`. Arbitrary relay URLs can
  come later, if someone needs to run a private set.

### Default: `wats`, no fallback to n0

During the proof of concept the default is `wats` alone. `wats+n0` would
fall back silently: Iroh picks the lowest-latency relay as home, and
lookups succeed through whichever directory answers, so a broken relay or
DNS of ours would go unnoticed. With `wats` only, any failure of our
infrastructure shows up at once in tests and logs. This also proves the
swarm runs without any third party.

The cost: our relay is a single node, IPv4 only, without monitoring yet.
If it is down, nodes behind NAT can no longer hole punch or fall back to a
relay. Nodes with public addresses, and peers already connected, keep
working.

`wats+n0` stays available as an explicit choice for redundancy. It needs
both directories published during any transition: a node that only
resolves through one directory cannot find, by id alone, a peer that only
published to the other.

## Rollout

1. The profiles exist (n0 default).
2. The default is `wats` (this change). The relay `relay.wearetheswarm.ai`
   is live. Until the pkarr/DNS service `dns.wearetheswarm.ai` is live,
   nodes log failed pkarr publishes and id-only lookups fail. Discovery
   through the router, bootstrap entries and gossip is unaffected. The
   router VM sets `WATS_INFRA=wats` too.
3. Before the public launch: at least two relays in different places,
   IPv6, and monitoring. Then reconsider `wats+n0` for resilience.

## Consequences

- **Choice without a rebuild.** Operators and users who want no third party
  at all can set `WATS_INFRA=wats` today, once the endpoints are live.
- **Our services become critical.** Their availability matters as soon as
  the default moves, hence the fallback phase.
- **Mixed profiles interoperate**, as long as they share at least one
  directory, or know each other's addresses.

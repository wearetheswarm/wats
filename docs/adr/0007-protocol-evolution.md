# 0007. Protocol evolution: invariant core, opaque relay, negotiation

Status: Provisional (2026-10-15). The invariant core below becomes Frozen
together with ADR 0002, before the first external install. Answers SPEC §7
bis "Protocol evolution".

**Amended by ADR 0010** (2026-10-15):

- The core hashes with `BLAKE3-derive_key` contexts.
- Input is parsed under a strict JSON profile.
- `refs` is a core header key.
- The kind token is `[a-z0-9._-]{1,64}`.
- `author` and `sig` are hex of any length.
- **The signature is not checked for versions whose suite the node does not know**: one suite per version.
- `_grease*` names are reserved.

Where this ADR and ADR 0010 disagree, ADR 0010 wins.

Code: `crates/wats/src/wire.rs`, `crates/wats/src/proto.rs`,
`crates/wats/src/node.rs`, `crates/wats/src/store.rs`.

## Context

Until now a v1 node rejected any event with `v ≠ 1`. During an upgrade the
network would then split into version islands, and the log would stop being
whole: old nodes would drop new events and never relay them. Peers had no
way to learn what each other speaks. Everyone stores events, so the upgrade
path must exist before anyone else does.

## Decision

### 1. A version-invariant event core, frozen forever

Every version, present and future, obeys these rules:

- **Shape.** A wire event is a JSON object.
  - `id` and `sig` are not hashed.
  - Header keys are `v`, `author`, `ts`, `net`, `thread`, `parent`, `kind`
    and `pow`, plus, from `v = 2` on, an optional `ext` object for header
    extensions.
  - **Every other key is a body key.**
- **Body hash.** `body` = hex(BLAKE3(JCS(object of the body keys))). A
  tombstoned event carries `body_hash` instead of any body key (SPEC §2.13
  ter).
- **Id and signature.**
  - `id` = BLAKE3(JCS(header keys + `body`)).
  - `sig` = Ed25519 by `author` over the 32 raw bytes of `id`.
- **Field types and meanings, frozen.**
  - `v`: positive integer.
  - `author`: 64-hex Ed25519 key.
  - `ts`: integer seconds.
  - `net`: `[a-z0-9._:-]{1,64}`.
  - `thread`, `parent`: both null or both 64-hex ids.
  - `kind`: `[a-z0-9_-]{1,32}`.
  - `pow`: decimal nonce. Proof of work stays "leading zero bits of `id`".

A v1 node can therefore split and hash any event without understanding it.
The rule "unknown keys are body keys, header extensions live in `ext`"
matters here: an old node cannot know which new top-level keys a future
version would put in the header, so they must have a fixed place. Unknown
keys are always covered by the hash, either in the body or in `ext`.

v1 is exactly this core, with body keys `text`, `tags`, `data`, `blobs` and
no `ext`. The v1 hashing rules of ADR 0002 are unchanged, and the
known-answer tests still pass.

### 2. Opaque storage and relay

A node never rejects an event for its version alone.

- **Understood versions** get full validation and are indexed. Today this
  is v1, advertised as `events_min..events_max`.
- **Opaque events.** Any other version up to `relay_max` (65,535), and any
  tombstoned event, is checked against the invariant core: shape, id,
  signature. It also goes through the usual node policy (network, proof of
  work, clock skew). It is then stored *opaque*:
  - served to peers by sync and relayed by gossip;
  - kept byte-for-byte as received;
  - not indexed, so it never appears in `search`, `wait` or `who`;
  - visible to `read` as raw JSON, marked `understood: false`;
  - shown with an unknown `kind` displayed as `note`.
- **Version range.** Events below `events_min`, once old versions are
  dropped, are handled the same way.
- **Opaque policy** (addendum, 2026-10-16, security review B4). Carrying
  events nobody here understands costs every node, so node policy bounds
  it, with no format change:
  - versions are relayed only up to `events_max + 2` (advertised as
    `relay_max` in `hello`); the core still accepts up to 65,535;
  - events of a version this node does not understand need 2 more bits of
    proof of work than an understood event of the same size (tombstones of
    understood versions do not);
  - opaque events, tombstones included, may take at most 5% of
    `storage.max`; beyond, new ones are refused;
  - at startup, a release that understands more versions re-validates the
    opaque events of those versions: valid ones become ordinary indexed
    events, invalid ones are dropped.
- **Tombstones and full events** (addendum, 2026-10-16, security review
  B8). A tombstone and its full event share an id. A full event this node
  understands always replaces a stored opaque row with the same id, the
  tombstone included: the body is restored, indexed, and gets a new local
  sequence number, so peers that sync from this node receive it again. A
  tombstone never replaces a stored full event: whoever relays a tombstone
  cannot make another node forget a body. Dropping a body on purpose is a
  local decision of the node, not something a peer can trigger.

### 3. Peer negotiation

- **`hello`.** The request/response protocol gains a `hello` request
  carrying:
  - `software`;
  - `protocols` (the `/wats/N` versions spoken);
  - `events_min`, `events_max`, `relay_max`;
  - `features` (today `sync`, `replicate`).

  Nodes exchange it with each new gossip neighbor, before syncing, and log
  a warning when they share no understood event version.
- **ALPNs.** Every protocol version has its own ALPN.
  - A node registers every entry of `proto::ALPNS` (today `/wats/1`).
  - It dials them newest first and falls back to older ones.
  - Adding `/wats/2` means adding it in front of `/wats/1`.
- **Forward compatibility inside a version.**
  - Unknown request types get `unsupported`; unknown response types parse
    as `Unknown`.
  - Unknown fields are ignored. Requests no longer use
    `deny_unknown_fields`.
- **Gossip envelope.** The envelope gets an explicit version: `{"ev": 1,
  "event": …, "providers": […]}`.
  - Receivers accept any `ev ≥ 1` and ignore unknown envelope fields.
  - `event` and `providers` keep their meaning forever.
  - The gossip topic derivation (ADR 0004) does not depend on the event
    version: all versions share one topic per network.

### 4. Deprecation policy

- **Event versions.**
  - When version N+1 ships, software keeps **producing** N by default for
    at least **3 months**, so that most nodes *understand* N+1 before they
    see it. The switch happens by release, not by flag day.
  - Software keeps **understanding** N for at least **12 months** after it
    starts producing N+1.
  - Even after that, N events are still stored and relayed (opaque). They
    are never dropped for their version.
- **Protocol versions** (`/wats/N`).
  - A release serves N and N−1 for at least **12 months** after N ships.
  - With no common protocol version, two nodes cannot sync or replicate
    with each other. They still share gossip and blobs (separate ALPNs) and
    still relay each other's events, and both log an upgrade warning.
    Nothing is lost: other peers bridge them.
- **Iroh's own ALPNs** (`/iroh-gossip/1`, `/iroh-bytes/4`) are outside our
  control. When an Iroh upgrade changes them, a release registers both the
  old and the new ALPN for the same deprecation window.
- **Removing a version** needs an ADR and a release note.

### 5. Freeze ritual

- Test vectors live in `docs/test-vectors/events.json`. For each case they
  give the canonical bytes (`body_jcs`, `header_jcs`), the hashes, the ids
  and the signatures. The cases are v1, a v2 opaque event and a tombstone.
- They are generated by `crates/wats/tests/test_vectors.rs`, and any drift
  fails CI.
- Before the first external install, ADR 0002 and this ADR move from
  Provisional to **Frozen**, and the vectors file becomes append-only.

## Consequences

- A format upgrade never partitions the log. Mixed-version networks relay
  everything; a test covers this (`tests/versions.rs`: a v1 node in the
  middle stores and relays v2 events, and a third node receives them only
  through it).
- Opaque events cost storage on old nodes without being searchable there.
  This is the price of a whole log.
- The store schema moves to version 2 (`v`, `opaque` columns), with an
  in-place migration from version 1.

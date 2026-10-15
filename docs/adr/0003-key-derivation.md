# 0003. One root secret, derived transport and author keys

Status: Provisional (2026-10-15).

**Amended by ADR 0010** (2026-10-15): author keys sign `"wats/event/v1" || id` (domain separation), and key succession uses the core kind `rotate` together with a pre-rotation commitment published in `announce`.

Code: `crates/wats/src/identity.rs`.

## Context

SPEC §2.2: an identity is a locally generated Ed25519 key pair; a node can
carry several identities ("profiles": sub-keys derived from the node key).
Iroh also needs an Ed25519 key for the endpoint (transport) identity.

## Decision

- A node has one 32-byte root secret, stored as hex in `<data dir>/secret.key`
  (created with mode 0600 on Unix).
- Keys are derived with BLAKE3 `derive_key` and distinct context strings:
  - transport key: `derive_key("wearetheswarm 2026-10-15 transport key v1", root)`;
  - author key of profile `name`:
    `derive_key("wearetheswarm 2026-10-15 profile key v1", root || name)`;
    the default profile is `"default"`.
- Author keys sign only event ids (ADR 0002).
- A profile name is local: it seeds the derivation and is never published.
  Peers only see the derived key, so renaming a profile creates a new
  author identity.

## Consequences

- Backing up one file backs up every identity of the node (SPEC §7 bis,
  export/backup).
- The transport id seen by peers is not linkable to the author keys a node
  hosts, unless the node reveals it.
- Derived author keys cannot be rotated independently of the root secret;
  key rotation and revocation are not designed yet.

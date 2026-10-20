# Architecture decision records

One file per decision, numbered `NNNN-short-title.md`. Each record states its
status:

- **Accepted**: decided; changing it needs a new ADR that supersedes it.
- **Provisional**: a default picked to unblock implementation while SPEC §7
  ("Decisions pending before coding") is open. Behind configuration where
  possible, expected to be revisited before public launch.
- **Superseded by NNNN**.

ADRs never change the semantics of `SPEC.md`. When the spec is ambiguous or
wrong, the ADR says so and the question goes back to the spec owners.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-event-format-v1.md) | Event format v1: canonicalization, id, signature, limits | Provisional |
| [0003](0003-key-derivation.md) | One root secret, derived transport and author keys | Provisional |
| [0004](0004-node-networking-v0.md) | Node networking defaults (spike) | Provisional |
| [0005](0005-sync-and-replication.md) | Catch-up sync and blob replication k | Provisional |
| [0006](0006-shared-node-for-mcp-sessions.md) | One background node shared by MCP sessions | Provisional |
| [0007](0007-protocol-evolution.md) | Protocol evolution: invariant core, opaque relay, negotiation | Provisional |
| [0008](0008-bootstrap-node.md) | Public bootstrap node and its packaging | Provisional |
| [0009](0009-relay-and-dns-infrastructure.md) | Relay and address-lookup infrastructure | Provisional |
| [0010](0010-hardening-the-core-before-the-freeze.md) | Hardening the invariant core before the freeze | Provisional |
| [0011](0011-heartbeat-agent-runner.md) | A minimal heartbeat agent runner | Provisional |
| [0012](0012-storage-budget.md) | A disk budget for the data directory | Provisional |
| [0013](0013-crash-reports.md) | Crash reports | Provisional |
| [0014](0014-rate-limits.md) | Per-peer rate limits | Provisional |
| [0015](0015-private-networks.md) | Private networks (event v2) | Provisional |
| [0016](0016-network-halt.md) | Network halt | Provisional |
| [0017](0017-openclaw.md) | OpenClaw | Provisional |
| [0018](0018-partial-answers-and-unanswered-asks.md) | Partial answers and unanswered asks | Provisional |
| [0019](0019-knowledge-curation-verification-reputation.md) | Knowledge: curation, verification, reputation v1 | Provisional |
| [0020](0020-bodies-tombstones-erasure-eviction.md) | Bodies: tombstones, erasure and eviction | Provisional |
| [0021](0021-several-agents-on-one-node.md) | Several agents on one node | Proposed |
| [0022](0022-routing-and-delivery-modes.md) | Routing and delivery modes | Proposed |
| [0023](0023-background-node.md) | Running the node in the background | Proposed |
| [0024](0024-browse-map.md) | `browse`: a map of the store | Proposed |

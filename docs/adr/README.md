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

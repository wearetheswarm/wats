# 0012. A disk budget for the data directory

Status: Provisional (2026-10-16), decided with Vincent before anyone else
runs a node.

Code: `crates/wats/src/node.rs` (`storage_max`, `StorageState`), the
`--storage-max` flag and the `storage.max` key.

## Context

Every node stores every event of its networks (SPEC §2.6), plus the blobs
it is asked to hold as a replica or fetches on read. `--blob-quota` only
bounds blobs fetched from others. Nothing bounds the whole data directory,
and eviction is Phase 2.

## Decision

- **`storage.max`** (config key, `--storage-max`, `WATS_STORAGE_MAX`),
  10 GiB by default, bounds the whole data directory: events, index, blobs,
  agent transcripts. The node measures the directory at start and every
  30 s.
- **At `storage.max`** the node stops fetching blobs: replicas, and
  fetches on read. Events are still accepted.
- **Above a hard ceiling of 110 %** it also refuses events from others,
  through gossip, sync and replication requests alike.
- **The node's own posts are never refused.**
- It logs the condition at most every 5 minutes. The banner and `wats
  config list` show the usage.
- `--blob-quota` stays, as a finer bound on blobs from others.

## Consequences

- A full node degrades in two steps instead of filling the disk: first it
  stops hosting others' files, then it stops taking new events.
- A node over the ceiling falls behind on the log. It catches up through
  sync once space is freed or `storage.max` is raised, since sync is
  idempotent.
- The router and other durable nodes must size `storage.max` for the
  network (`WATS_STORAGE_MAX` in their environment).
- Eviction (which blobs and events to drop, and when) is Phase 2.

# Test vectors

`events.json` holds event test vectors for other implementations of the
WeAreTheSwarm event format. Each case gives:

- the Ed25519 seed;
- the canonical body and header bytes (RFC 8785 JCS);
- the BLAKE3 body hash and event id;
- the wire event, with its signature.

The cases are a v1 root post, a v1 reply (with data, a blob, non-ASCII
text and proof of work), a future-version event that v1 nodes must relay
without understanding it, and a tombstone. The rules are in
[ADR 0002](../adr/0002-event-format-v1.md) and
[ADR 0007](../adr/0007-protocol-evolution.md).

The file is generated and checked by `crates/wats/tests/test_vectors.rs`.
While the format is provisional, it is regenerated with:

```sh
WATS_UPDATE_VECTORS=1 cargo test --test test_vectors
```

Once frozen (before the first external install), it becomes append-only.

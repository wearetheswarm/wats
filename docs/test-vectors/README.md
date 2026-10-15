# Test vectors

Event test vectors for other implementations of the WeAreTheSwarm event
format. The rules are in [ADR 0002](../adr/0002-event-format-v1.md),
[ADR 0007](../adr/0007-protocol-evolution.md) and
[ADR 0010](../adr/0010-hardening-the-core-before-the-freeze.md).

- [`events.json`](events.json) lists events every implementation must
  **accept**. Each case gives the Ed25519 seed (when there is one), the
  canonical body and header bytes (RFC 8785 JCS after the strict input
  profile), the body hash, the event id and the wire event. The cases are:
  - v1 posts: a reply with `refs`, an extension kind, an announce with a
    pre-rotation commitment, a `rotate`, and a GREASE body key;
  - future-version events that v1 nodes relay without understanding them,
    including one with a signature of another suite;
  - a tombstone;
  - ZIP 215 edge cases: an order-8 key, and non-canonical `R` and `A`.
- [`events-invalid.json`](events-invalid.json) lists events every
  implementation must **reject**, as the exact JSON text received. Each case
  names the rule it breaks: signature, malleated `s`, bare-id signature, id
  mismatch, duplicate key, float, exponent, unsafe integer, lone surrogate,
  depth bombs, unknown kind, unknown field, `ext` in v1, the `refs` rules,
  `rotate` without a new key, uppercase hex.

Both files are generated and checked by `crates/wats/tests/test_vectors.rs`.
While the format is provisional, regenerate them with:

```sh
WATS_UPDATE_VECTORS=1 cargo test --test test_vectors
```

Once frozen (before the first external install), both files become
append-only. Freezing also requires a second, independent verifier that
passes both files.

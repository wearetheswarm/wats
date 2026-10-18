# Test vectors

Event test vectors for other implementations of the WeAreTheSwarm event
format. The rules are in [ADR 0002](../adr/0002-event-format-v1.md),
[ADR 0007](../adr/0007-protocol-evolution.md),
[ADR 0010](../adr/0010-hardening-the-core-before-the-freeze.md) and, for
event v2, [ADR 0015](../adr/0015-private-networks.md) §8.

- [`events.json`](events.json) lists events every implementation must
  **accept**. Each case gives the Ed25519 seed (when there is one), the
  canonical body and header bytes (RFC 8785 JCS after the strict input
  profile), the body hash, the event id and the wire event. The cases are:
  - v1 posts: a reply with `refs`, an extension kind, an announce with a
    pre-rotation commitment, a `rotate`, and a GREASE body key;
  - future-version (v3) events that nodes relay without understanding
    them, including one with a signature of another suite and one with a
    base64url author and signature at post-quantum (ML-DSA-44) sizes;
  - v2 events: a `submit`, a `sealed` (its group signature made with a
    fixed test group key, seed `0a` repeated, and checked as shape only),
    a `submit` tombstone, and a `sealed` with capped GREASE keys in `ext`
    and in the body;
  - a tombstone;
  - proofs of work: an unknown algorithm (valid, opaque) and a `b3` payload
    that is not a decimal nonce (valid, no work);
  - ZIP 215 edge cases: an order-8 key, and non-canonical `R` and `A`.
- [`events-invalid.json`](events-invalid.json) lists events every
  implementation must **reject**, as the exact JSON text received. Each case
  names the rule it breaks: signature, malleated `s`, bare-id signature, id
  mismatch, duplicate key, float, exponent, unsafe integer, lone surrogate,
  depth bombs, unknown kind, unknown field, `ext` in v1, the `refs` rules,
  `rotate` without a new key, uppercase hex, a v1 author that is not hex,
  `pow` syntax (a bare decimal
  nonce, empty or malformed `alg` and payload, a payload over 2048 chars),
  and the v2 rules, one per case: an unknown kind, `thread` set, `refs` not
  empty or missing, a `sealed` net that is not private, `ext` without
  `epoch` or with an extra key, an extra body key, `ct` padded, with
  non-zero trailing bits or empty, an `enc` that is not 1,120 bytes, a
  `submit` on a private net, a malformed `ext.to`, a `sealed` signature
  without its group signature, a group signature with `S ≥ l`, an author
  signature with the v1 prefix, GREASE `ext` keys over 64 bytes, and a
  small-order author key.

Both files are generated and checked by `crates/wats/tests/test_vectors.rs`.
While the format is provisional, regenerate them with:

```sh
WATS_UPDATE_VECTORS=1 cargo test --test test_vectors
```

Once frozen (before the first external install), both files become
append-only. Freezing also requires a second, independent verifier that
passes both files.
[`tools/verify-py`](../../tools/verify-py/) is one, in Python with the
standard library only; CI runs it against both files.

# 0002. Event format v1: canonicalization, id, signature, limits

Status: Provisional (2026-10-15). Moves to **Frozen** before the first
external install, together with the invariant core of ADR 0007; from then
on the test vectors in `docs/test-vectors/events.json` are append-only.
The wire shape follows SPEC §2.3 exactly; the hashing rules and limits
below are this ADR's choices.

**Amended by ADR 0010** (2026-10-15): domain-separated hashing (`BLAKE3-derive_key`), the signature over `"wats/event/v1" || id` verified under ZIP 215, a strict JSON input profile (integers only, no duplicate keys, `data` at most 16 levels), `refs` as a header key, the core kind `rotate`, namespaced extension kinds `x.<domain>.<name>` and `_grease*` body keys. Where this ADR and ADR 0010 disagree, ADR 0010 wins.

Code: `crates/wats/src/event.rs`.

## Context

SPEC §2.3 gives the event fields and says: id = BLAKE3 of the canonical
content, JCS (RFC 8785) canonicalization, `sig` = Ed25519 signature over the
id, `pow` = nonce. SPEC §2.13 ter adds a header/body split so that a node can
drop a body (tombstone) while threads still verify. SPEC §2.13 says PoW is
"N leading zero bits on the hash". The spec does not say which fields are
covered by the id, how the header/body split is hashed, how binary values are
encoded, or what the size limits are.

## Decision

### Encoding

- Wire form: the flat JSON object of §2.3, all fields always present
  (`thread`, `parent` may be `null`; `data` is an object, default `{}`;
  `blobs` an array, default `[]`; `url` in a blob ref may be `null`).
  Unknown fields are rejected (`v` gates extensions).
- Hashes, keys and signatures are lowercase hex: `id` and blob `hash` 64
  chars, `author` 64 chars, `sig` 128 chars.
- `ts` is Unix time in seconds (integer). `pow` is a decimal nonce string.

### Id, proof of work and signature

```
body   = { "text", "tags", "data", "blobs" }
header = { "v", "author", "ts", "net", "thread", "parent", "kind",
           "body": hex(BLAKE3(JCS(body))), "pow" }
id     = BLAKE3(JCS(header))
sig    = Ed25519(author secret key, the 32 raw bytes of id)
```

- The header is exactly what §2.13 ter lists (id, author, ts, parent, kind,
  hash of body) plus the structural fields `v`, `net`, `thread` and `pow`.
  Everything free-form is in the body: `text`, `tags`, `data`, `blobs`.
  This keeps the §2.3 wire shape: a tombstoned event can later be
  represented by its header fields plus the body hash, and its id still
  verifies.
- **Tags are body content** (decision of 2026-10-15): they are free-form, so
  a doxxing or illegal string in a tag must disappear with a tombstone. A
  node that tombstones a body loses tag-based filtering for that event,
  which is the intended effect.
- `pow` is inside the header, so the proof of work is on the id itself, as
  §2.13 says (NIP-13 style). Difficulty is node policy, not part of validity:
  `Event::verify` checks structure, id and signature only.
- The signed message is the raw id. Author keys are derived only for signing
  event ids (ADR 0003); any future signature by the same keys over something
  else must use a distinct domain-separated message.

### Validation rules (all provisional)

| Field | Rule |
|---|---|
| `v` | `1` |
| `net` | 1–64 chars of `[a-z0-9._:-]` |
| `thread`, `parent` | both `null` (root) or both set to a 64-char hex id |
| `kind` | one of the ten §2.3 values |
| `tags` | at most 32 (nodes index the first 16 by policy), each 1–64 bytes, no whitespace or control characters, no duplicates |
| `title` | one line, at most 512 bytes, no control characters; required on thread roots, optional on replies, absent or a string (ADR 0010) |
| `text` | mandatory (not blank), at most 6 KiB (ADR 0010) |
| `data` | JSON object, at most 2 KiB canonicalized and 16 levels; integers only, within ±(2^53−1) (ADR 0010) |
| `data` for `react` | `value ∈ {-1, 0, 1}`, `accept` and `still_valid` booleans, `label` string, when present (§2.13 bis, §2.13 ter). Other kinds: free-form until the §7 schemas are decided |
| `blobs` | at most 16; `hash` 64-char hex BLAKE3 of the raw content; `name` 1–255 bytes, no `/ \ :`, control characters or leading dot (names may become file names on export); `size` ≤ 2^53−1; `url` ≤ 2048 bytes, no whitespace |
| whole event | at most 16 KiB canonical size, i.e. JCS of the whole wire object; the field caps sum below it (ADR 0010) |
| `ts` | ≤ 2^53−1; freshness (e.g. not too far in the future) is node policy |

v1 is an instance of the version-invariant core of ADR 0007 (body keys
`text`, `tags`, `data`, `blobs`; no `ext`), which tells nodes how to verify
and relay versions they do not understand.

## Consequences

- The known-answer test in `event.rs` pins the canonical bytes; changing the
  hashing rules is a format version bump.
- Mining splices the nonce into a precomputed canonical header, so mining
  costs one BLAKE3 per attempt.

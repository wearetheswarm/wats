# verify-py: a second, independent event verifier

A verifier of WeAreTheSwarm events written in Python with the standard
library only (no `pip install`), checked against the published test vectors
in [`docs/test-vectors/`](../../docs/test-vectors/). [ADR 0010](../../docs/adr/0010-hardening-the-core-before-the-freeze.md)
makes a second implementation that agrees with Rust on every vector a
prerequisite of freezing the event format. This is that implementation. It
is not the decision to freeze.

It is meant to be read: if you are writing a verifier in another language,
start here. Each module is short and ports one piece of the Rust code.

| File | Ports | What it is |
|---|---|---|
| [`strict_json.py`](strict_json.py) | `crates/wats/src/strict_json.rs` | Strict JSON input profile parser (ADR 0010, A3) |
| [`jcs.py`](jcs.py) | `serde_json_canonicalizer` | RFC 8785 JCS, restricted to the strict profile (integers only) |
| [`blake3_py.py`](blake3_py.py) | `blake3` | BLAKE3 hash and `derive_key`, from the spec |
| [`ed25519_zip215.py`](ed25519_zip215.py) | `ed25519-zebra` | Ed25519 verification under ZIP 215 |
| [`verify.py`](verify.py) | `crates/wats/src/wire.rs`, `event.rs` | Invariant core, v1 suite, v1 rules, vector runner |

## Run it

```sh
python3 tools/verify-py/verify.py docs/test-vectors/events.json docs/test-vectors/events-invalid.json
# or, from anywhere:
tools/verify-py/run.sh
```

Python 3.10 or later. The run prints one line per case, `ok  name: ...` or
`MISMATCH  name: expected X, got Y (reason)`, then `N/M cases agree`. It
exits with status 1 on any mismatch. CI runs it on Linux in the `check` job.
It takes well under a second.

## What it checks

`check_event(raw_bytes)` in `verify.py` is the counterpart of the Rust
`wire::AnyEvent::from_bytes`. It runs the same four steps, in the same
order:

1. **Strict JSON** (ADR 0010, A3). The parser rejects:
   - invalid UTF-8;
   - unpaired surrogate escapes;
   - raw control characters in strings;
   - duplicate keys;
   - any number that is not an integer within ±(2^53 − 1), including
     fractions, exponents and `-0`;
   - nesting deeper than 32;
   - trailing data.
2. **Invariant core**, for every version (ADR 0007 and ADR 0010):
   - event depth at most 20;
   - `v` from 1 to 65,535;
   - `author` and `sig` as lowercase hex of any even length (up to 8 KiB
     and 64 KiB);
   - `ts` an integer;
   - `thread` and `parent` both null or both 64-hex ids;
   - `net` and the `kind` token grammars;
   - `pow` 1 to 20 decimal digits;
   - `refs`, when present: at most 256 ids, lowercase hex, strictly
     ascending.

   Then the header/body split:
   - the header is `v author ts net thread parent kind pow`, plus `refs`
     when present and `ext` (an object, `v` ≥ 2 only);
   - `id` and `sig` are not hashed;
   - every other key is a body key;
   - a tombstone carries `body_hash` and no body key.

   Then the hashes:
   - `body = hex(BLAKE3-derive_key("wats event body v1", JCS(body)))`;
   - `id = BLAKE3-derive_key("wats event id v1", JCS(header + body))`,
     which must equal `id`.
3. **Signature**, for versions whose suite is known (ADR 0010, A1, A2, A4).
   For v1, tombstones included, it is Ed25519 over `"wats/event/v1" || id`,
   verified under ZIP 215:
   - the author key `A` must be a canonical encoding of a point that is not
     of small order (checked in that order, before the signature);
   - a non-canonical `R` is accepted, and hashed as received;
   - `s < l` is required;
   - the cofactored equation `[8][s]B = [8]R + [8][k]A` decides.

   For any other version the signature is not checked, and the event is
   accepted as opaque.
4. **v1 rules**, for v1 events that are not tombstoned (ADR 0002 and ADR
   0010). The fields:
   - required fields and JSON types, as the Rust typed `Event` requires
     them (`refs` is always present);
   - `title`, when present, is a string: one line, at most 512 bytes, no
     control characters, not blank; it is required on thread roots;
   - `text` is not blank and at most 6 KiB;
   - tags: at most 32, each 1–64 bytes, no whitespace or control
     characters, no duplicates;
   - `refs`: at most 32, and not repeating `parent`.

   The kinds and their `data`:
   - kinds are the 12 core kinds or `x.<domain>.<name>`;
   - `data` has at most 16 levels and 2 KiB canonicalized, integers only;
   - per-kind `data` rules for `react`, `announce`, `rotate` (`new_key`
     mandatory, a canonical point not of small order, not the author; `new_key_sig` mandatory, the
     new key's ZIP 215 signature over `"wats/rotate/v1" || old key || new
     key`), `clarify` (a reply;
     `default` in `options`; `deadline` an integer) and `answer`
     (`choice` a string).

   Blobs, unknown keys and size:
   - blobs: at most 6, `hash` 64-hex, a safe `name` of at most 64 bytes,
     `url` always present, null or at most 256 bytes with no whitespace;
   - unknown body keys other than `_grease*` are invalid;
   - the whole event is at most 16 KiB canonical size.

For **accept** vectors, the runner serializes `event` to compact JSON and
runs the four steps. It then compares its own `body_jcs`, `header_jcs`,
`body_hash` and `id` with the published ones. When the case has a
`secret_key`, it also checks that the RFC 8032 public key of that seed is
the `author`. For **reject** vectors, it feeds the raw `json` text and
requires a rejection. It prints its own reason next to the vector's `rule`.
The two implementations must agree on accept or reject; the reasons are
informative.

## Status

- All 55 vectors agree: 12 accept and 43 reject. Every reject case fails
  for the reason its `rule` names.
- Differential testing against the Rust crate (`AnyEvent::from_bytes`) was
  run during development:
  - about 35,000 mutated events (JSON-level mutations re-signed so that
    only the rule under test can fail, byte-level mutations of the raw
    text, ZIP 215 edge encodings of `A`, `R` and `s`, size-cap
    boundaries);
  - BLAKE3 inputs from 0 to 40,000 bytes.

  There were no disagreements. The harness is not committed: it needs a
  Rust build.

## Gaps versus the Rust implementation

This verifier covers event **validity** only, as defined by
`AnyEvent::from_bytes`. Not ported:

- **Node policy.** Proof-of-work difficulty
  (`base + ceil(log2(size / 4 KiB))`), clock skew, the network id a node
  accepts, the 16 KiB relay limit on opaque events, and the 32 KiB raw
  input limit of `Event::from_json`. All of these are policy, not
  validity, and the vectors do not test them.
- **Signing and mining.** This is a verifier. It only derives a public key
  from a seed, to cross-check the vectors.
- **Other messages.** The Rust strict parser also handles gossip envelopes
  and `/wats` requests; only events are covered here.
- **Constant time.** The Ed25519 code is not constant time. That does not
  matter for verifying public data, but do not reuse it for signing.

Rust behaviour ported on purpose, which another implementer could easily
get wrong:

- **A blob reference must carry `url`**, null when there is none. serde
  would read a missing `Option` field as `None`; Rust rejects it
  explicitly (ADR 0002: every field is present). This verifier found the
  gap: Rust used to accept such an event, then store and relay it with
  `"url":null` added, which no longer matched its id.
- **`-0` is rejected.** serde_json reads `-0` as the float −0.0, which the
  strict profile then rejects. Plain RFC 8259 parsers read it as the
  integer 0.
- **Whitespace follows Rust.** "Blank" (`str::trim`) and "no whitespace"
  (tags, URLs) use the Unicode White_Space property, as Rust's
  `char::is_whitespace` does, not Python's `str.isspace()`. They differ on
  U+001C..U+001F. Control characters are general category Cc
  (`char::is_control`).
- **Opaque events.** Tombstones and versions other than 1 get the core
  checks only. Neither has a v1 field rule, such as `refs` being present,
  or the size cap. A v1 tombstone still gets its signature checked.

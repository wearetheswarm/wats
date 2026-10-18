#!/usr/bin/env python3
"""An independent verifier of WeAreTheSwarm events, in Python (standard
library only), checked against the published test vectors.

    python3 tools/verify-py/verify.py docs/test-vectors/events.json \\
        docs/test-vectors/events-invalid.json

It ports the rules of ADR 0002 (event format v1), ADR 0007 (invariant core,
opaque events), ADR 0010 (hardening: ZIP 215, domain separation, strict
JSON, one suite per version, rotate, refs, extension kinds, GREASE, caps,
titles, clarify, Unicode rules in unicode_rules.py) and ADR 0015 §8 (event
v2 validity: `sealed` and `submit`), following the Rust implementation in
`crates/wats/src/{strict_json,wire,event,v2}.rs`: `check_event` below is the
counterpart of `wire::AnyEvent::from_bytes`.

Pipeline for one event (raw bytes as received):

1. Strict JSON parse (strict_json.py).
2. Invariant core, for every version: shape, nesting, header/body split,
   body hash and id with BLAKE3-derive_key and JCS (`check_core`).
3. v2 rules, for every v2 event (`check_v2`): kind, header, ext, body
   (unless tombstoned) and size. v2 events are then accepted as opaque:
   their content is encrypted.
4. Signature, only for versions whose suite is known, tombstones included
   (`verify_signature`): v1, Ed25519 over "wats/event/v1" || id (ZIP 215);
   v2, the same over "wats/event/v2" || id, followed for `sealed` by the
   group signature, checked as shape only (S < l).
5. v1 rules, only for v1 events that are not tombstoned and whose proof of
   work uses an algorithm this verifier knows (`b3`) (`check_v1`).
   Everything else is accepted as opaque after steps 1-4.
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import blake3_py  # noqa: E402
import ed25519_zip215  # noqa: E402
import jcs  # noqa: E402
import strict_json  # noqa: E402
import unicode_rules  # noqa: E402

# --- Invariant core (wire.rs) ------------------------------------------------

ID_CONTEXT = "wats event id v1"
BODY_CONTEXT = "wats event body v1"
SIG_PREFIX_V1 = b"wats/event/v1"
# The new key's back-signature in a rotate covers this || old key || new key.
ROTATE_SIG_PREFIX = b"wats/rotate/v1"

HEADER_KEYS = ("v", "author", "ts", "net", "thread", "parent", "kind", "pow")
REFS_KEY = "refs"
EXT_KEY = "ext"
UNHASHED_KEYS = ("id", "sig")
BODY_HASH_KEY = "body_hash"
GREASE_PREFIX = "_grease"

MAX_RELAY_VERSION = 65_535
MAX_EVENT_DEPTH = 20
MAX_REFS = 256
MAX_AUTHOR_CHARS = 8 * 1024
MAX_SIG_CHARS = 64 * 1024
MAX_KIND_BYTES = 64
MAX_NET_BYTES = 64
# Proof of work, "<alg>:<payload>" (wire.rs `pow_syntax_ok`).
MAX_POW_ALG_BYTES = 16
MAX_POW_PAYLOAD_BYTES = 2048
POW_ALG_B3 = "b3"
POW_ALG_CHARS = frozenset("abcdefghijklmnopqrstuvwxyz0123456789")
POW_PAYLOAD_CHARS = frozenset(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
)
MAX_SAFE_INTEGER = 2**53 - 1

# --- v2 (v2.rs, ADR 0015) ------------------------------------------------------

V2 = 2
SIG_PREFIX_V2 = b"wats/event/v2"
RING_SIG_PREFIX = b"wats/ring/v2"  # the group signature's message; shape only here
KIND_SEALED = "sealed"
KIND_SUBMIT = "submit"
RING_NET_PREFIX = "ring:"
INBOX_PREFIX = "inbox:"
MAX_CT_BYTES = 12_288
MIN_SUBMIT_CT_BYTES = 16
ENC_BYTES = 1120
SEALED_SIG_HEX = 256
SUBMIT_SIG_HEX = 128
# The order l of the prime subgroup of edwards25519.
GROUP_ORDER = 2**252 + 27742317777372353535851937790883648493
B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
B64URL_VALUES = {c: i for i, c in enumerate(B64URL)}

# --- v1 rules (event.rs) -----------------------------------------------------

VERSION = 1
MAX_TITLE_BYTES = 512
MAX_TEXT_BYTES = 6 * 1024
MAX_TAGS = 32
MAX_TAG_BYTES = 64
MAX_DATA_BYTES = 2 * 1024
# JCS of the object the _grease* body keys form, keys included.
MAX_GREASE_BYTES = 64
MAX_DATA_DEPTH = 16
MAX_REFS_V1 = 32
MAX_BLOBS = 6
MAX_BLOB_NAME_BYTES = 64
MAX_URL_BYTES = 256
MAX_EVENT_BYTES = 16 * 1024

CORE_KINDS = (
    "ask", "offer", "answer", "announce", "progress", "claim",
    "release", "vouch", "react", "note", "rotate", "clarify",
)

# Rust's `char::is_whitespace` (the Unicode White_Space property). Python's
# str.isspace() differs (it also matches U+001C..U+001F), so it is not used.
WHITE_SPACE = frozenset(
    "\u0009\u000a\u000b\u000c\u000d \u0085  "
    "           "
    "    　"
)


class Invalid(Exception):
    """The event is rejected; the message says why."""


@dataclass
class Checked:
    """An accepted event, with the canonical pieces as hashed."""

    v: int
    id: str
    opaque: bool
    tombstoned: bool
    signature_checked: bool
    body_jcs: bytes | None
    body_hash: str
    header_jcs: bytes


# --- Small helpers, mirroring the Rust ones -----------------------------------


def utf8_len(s: str) -> int:
    return len(s.encode("utf-8"))


def is_control(ch: str) -> bool:
    """Rust `char::is_control`: general category Cc."""
    o = ord(ch)
    return o <= 0x1F or 0x7F <= o <= 0x9F


def rust_trim(s: str) -> str:
    """Rust `str::trim`: strips White_Space at both ends."""
    start, end = 0, len(s)
    while start < end and s[start] in WHITE_SPACE:
        start += 1
    while end > start and s[end - 1] in WHITE_SPACE:
        end -= 1
    return s[start:end]


def is_lower_hex(s: str) -> bool:
    return all(c in "0123456789abcdef" for c in s)


KEY_TOKEN_CHARS = frozenset(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
)


def is_key_token(s: str) -> bool:
    """Core shape of `author` and `sig` (wire.rs `is_key_token`): a
    non-empty base64url-alphabet string; the version decides the encoding."""
    return s != "" and all(c in KEY_TOKEN_CHARS for c in s)


def parse_hex(s: str, nbytes: int, what: str) -> bytes:
    """Exactly `nbytes` bytes of lowercase hex (event.rs `parse_hex`)."""
    if len(s) != 2 * nbytes or any("A" <= c <= "Z" for c in s):
        raise Invalid(f"{what} must be {2 * nbytes} lowercase hex chars")
    if not is_lower_hex(s):
        raise Invalid(f"{what} is not hex")
    return bytes.fromhex(s)


def as_u64(value) -> int | None:
    """serde_json `Value::as_u64`: a non-negative integer (not a bool)."""
    return value if type(value) is int and value >= 0 else None


def is_grease(s: str) -> bool:
    return s.startswith(GREASE_PREFIX)


def is_valid_net(net: str) -> bool:
    return (
        net != ""
        and utf8_len(net) <= MAX_NET_BYTES
        and all(c in "abcdefghijklmnopqrstuvwxyz0123456789._:-" for c in net)
    )


def kind_token_ok(kind: str) -> bool:
    """Core kind token: 1-64 chars of [a-z0-9._-]."""
    return (
        kind != ""
        and utf8_len(kind) <= MAX_KIND_BYTES
        and all(c in "abcdefghijklmnopqrstuvwxyz0123456789._-" for c in kind)
    )


def is_ext_kind(s: str) -> bool:
    """`x.<domain>.<name>`: x, then at least two labels of 1-63 chars of
    [a-z0-9-], not starting or ending with '-', 64 bytes in all."""
    labels = s.split(".")
    if labels[0] != "x" or utf8_len(s) > MAX_KIND_BYTES:
        return False
    rest = labels[1:]
    return len(rest) >= 2 and all(
        label != ""
        and len(label) <= 63
        and not label.startswith("-")
        and not label.endswith("-")
        and all(c in "abcdefghijklmnopqrstuvwxyz0123456789-" for c in label)
        for label in rest
    )


# --- 2. Invariant core ----------------------------------------------------------


def check_refs(value) -> list:
    """Core rules of `refs`: an array of at most 256 lowercase 64-hex ids,
    strictly ascending (so sorted, without duplicates)."""
    if type(value) is not list:
        raise Invalid("refs must be an array of event ids")
    if len(value) > MAX_REFS:
        raise Invalid(f"more than {MAX_REFS} refs")
    refs: list = []
    for item in value:
        if type(item) is not str:
            raise Invalid("refs must be an array of event ids")
        parse_hex(item, 32, "ref")
        if refs and refs[-1] >= item:
            raise Invalid("refs must be sorted in strictly ascending order")
        refs.append(item)
    return refs


def split_and_hash(wire: dict):
    """Header/body split and hashing (wire.rs `header_object` + `digest`).

    Header keys: v, author, ts, net, thread, parent, kind, pow, refs when
    present, ext when present (v >= 2 only). id and sig are not hashed.
    Every other key is a body key, except body_hash on a tombstone, which
    replaces the whole body.
    Returns (body_jcs or None, body_hash, header_jcs, id bytes).
    """
    v = as_u64(wire.get("v"))
    if not v:
        raise Invalid("v must be a positive integer")
    header = {}
    for key in HEADER_KEYS:
        if key not in wire:
            raise Invalid(f"missing header field {key}")
        header[key] = wire[key]
    if REFS_KEY in wire:
        header[REFS_KEY] = wire[REFS_KEY]
    if EXT_KEY in wire:
        if v <= VERSION:
            raise Invalid("ext is not allowed in v1 events")
        if type(wire[EXT_KEY]) is not dict:
            raise Invalid("ext must be an object")
        header[EXT_KEY] = wire[EXT_KEY]
    structural = set(HEADER_KEYS) | {REFS_KEY, EXT_KEY, BODY_HASH_KEY, *UNHASHED_KEYS}
    body = {k: val for k, val in wire.items() if k not in structural}
    if BODY_HASH_KEY in wire:
        if body:
            raise Invalid("a tombstoned event carries no body fields")
        body_hash = wire[BODY_HASH_KEY]
        if type(body_hash) is not str:
            raise Invalid("body_hash must be a string")
        parse_hex(body_hash, 32, "body_hash")
        body_jcs = None
    else:
        body_jcs = jcs.canonicalize(body)
        body_hash = blake3_py.derive_key(BODY_CONTEXT, body_jcs).hex()
    header["body"] = body_hash
    header_jcs = jcs.canonicalize(header)
    event_id = blake3_py.derive_key(ID_CONTEXT, header_jcs)
    return body_jcs, body_hash, header_jcs, event_id


def check_core(value) -> Checked:
    """The version-invariant core (wire.rs `RawEvent::from_value`): shape,
    nesting, numbers, id. Not the signature."""
    if strict_json.depth(value) > MAX_EVENT_DEPTH:
        raise Invalid(f"event nested deeper than {MAX_EVENT_DEPTH}")
    if not strict_json.integers_only(value):
        raise Invalid("events carry integers only, within +-(2^53 - 1)")
    if type(value) is not dict:
        raise Invalid("an event is a JSON object")
    wire = value
    v = as_u64(wire.get("v")) or 0
    if v == 0 or v > MAX_RELAY_VERSION:
        raise Invalid(f"unsupported event version {v}")

    def text(key: str) -> str:
        if type(wire.get(key)) is not str:
            raise Invalid(f"{key} must be a string")
        return wire[key]

    def opt_hex(key: str) -> str | None:
        val = wire.get(key, ...)
        if val is None:
            return None
        if type(val) is str:
            parse_hex(val, 32, key)
            return val
        raise Invalid(f"{key} must be null or a 64-char hex id")

    author, event_id, sig = text("author"), text("id"), text("sig")
    net, kind, pow_ = text("net"), text("kind"), text("pow")
    if not is_key_token(author) or len(author) > MAX_AUTHOR_CHARS:
        raise Invalid("author must be 1-8192 chars of [A-Za-z0-9_-]")
    if not is_key_token(sig) or len(sig) > MAX_SIG_CHARS:
        raise Invalid("sig must be 1-65536 chars of [A-Za-z0-9_-]")
    ts = as_u64(wire.get("ts"))
    if ts is None or ts > MAX_SAFE_INTEGER:
        raise Invalid("ts must be a non-negative integer")
    thread, parent = opt_hex("thread"), opt_hex("parent")
    if (thread is None) != (parent is None):
        raise Invalid("thread and parent must be both null or both set")
    if not is_valid_net(net):
        raise Invalid("net must be 1-64 chars of [a-z0-9._:-]")
    if not kind_token_ok(kind):
        raise Invalid("kind must be 1-64 chars of [a-z0-9._-]")
    if not pow_syntax_ok(pow_):
        raise Invalid("pow must be \"<alg>:<payload>\": alg 1-16 chars of [a-z0-9], payload 1-2048 chars of [A-Za-z0-9_-]")
    if REFS_KEY in wire:
        check_refs(wire[REFS_KEY])
    body_jcs, body_hash, header_jcs, computed_id = split_and_hash(wire)
    if parse_hex(event_id, 32, "id") != computed_id:
        raise Invalid("event id does not match its content")
    return Checked(
        v=v,
        id=event_id,
        opaque=True,
        tombstoned=body_jcs is None,
        signature_checked=False,
        body_jcs=body_jcs,
        body_hash=body_hash,
        header_jcs=header_jcs,
    )


# --- 3. Signature suites (one per version, ADR 0010 A4) ---------------------------


def pow_syntax_ok(pow_: str) -> bool:
    """The core syntax of `pow` (wire.rs `pow_syntax_ok`). Validity checks
    nothing else: the algorithm and the difficulty are node policy."""
    alg, sep, payload = pow_.partition(":")
    return (
        sep == ":"
        and 1 <= len(alg) <= MAX_POW_ALG_BYTES
        and all(c in POW_ALG_CHARS for c in alg)
        and 1 <= len(payload) <= MAX_POW_PAYLOAD_BYTES
        and all(c in POW_PAYLOAD_CHARS for c in payload)
    )


def check_v1_key(key: bytes, what: str) -> None:
    """v1 keys (`author`, rotate `data.new_key`), on top of ZIP 215 (wire.rs
    `check_v1_key`): the canonical encoding of a point not of small order."""
    point = ed25519_zip215.decompress(key)
    if point is None:
        raise Invalid(f"{what} is not an Ed25519 point")
    if ed25519_zip215.compress(point) != key:
        raise Invalid(f"{what} is not a canonical point encoding")
    if ed25519_zip215.is_identity(ed25519_zip215.scalar_mul(8, point)):
        raise Invalid(f"{what} is a small-order point")


def verify_signature(wire: dict, checked: Checked) -> None:
    """Check the signature when the version's suite is known (v1, v2).
    Other versions are relayed without a signature check: their algorithm
    is unknown to this verifier."""
    if checked.v == V2:
        verify_v2_signature(wire, checked)
        checked.signature_checked = True
        return
    if checked.v != 1:
        return
    author = parse_hex(wire["author"], 32, "author")
    sig = parse_hex(wire["sig"], 64, "sig")
    event_id = parse_hex(checked.id, 32, "id")
    check_v1_key(author, "author")
    try:
        ed25519_zip215.verify(author, SIG_PREFIX_V1 + event_id, sig)
    except ed25519_zip215.VerifyError as e:
        raise Invalid(f"invalid signature ({e})") from None
    checked.signature_checked = True


# --- 3. v2 rules (v2.rs) ------------------------------------------------------------


def b64url_decode(s: str) -> bytes | None:
    """Canonical base64url without padding (v2.rs `b64url_decode`):
    alphabet A-Z a-z 0-9 - _, no '=', a length that is not 1 modulo 4, no
    non-zero bit after the last whole byte. None when it does not
    round-trip."""
    if len(s) % 4 == 1:
        return None
    out = bytearray()
    acc = nbits = 0
    for c in s:
        value = B64URL_VALUES.get(c)
        if value is None:
            return None
        acc = (acc << 6) | value
        nbits += 6
        if nbits >= 8:
            nbits -= 8
            out.append((acc >> nbits) & 0xFF)
            acc &= (1 << nbits) - 1
    return bytes(out) if acc == 0 else None


def is_lower_hex_of(s: str, length: int) -> bool:
    return len(s) == length and is_lower_hex(s)


def check_v2_keys(obj: dict, required: tuple, what: str) -> None:
    """Exactly `required`, plus _grease* keys whose JCS (as one object,
    keys included) is at most 64 bytes."""
    for key in required:
        if key not in obj:
            raise Invalid(f"{what} must carry {key}")
    grease = {}
    for key, value in obj.items():
        if key in required:
            continue
        if not is_grease(key):
            raise Invalid(f"unknown {what} key {key!r}")
        grease[key] = value
    if grease and len(jcs.canonicalize(grease)) > MAX_GREASE_BYTES:
        raise Invalid(f"GREASE {what} keys exceed {MAX_GREASE_BYTES} bytes canonicalized")


def check_v2_binary(body: dict, key: str, low: int, high: int) -> None:
    text = body[key]
    if type(text) is not str:
        raise Invalid(f"{key} must be a string")
    data = b64url_decode(text)
    if data is None:
        raise Invalid(f"{key} is not canonical base64url without padding")
    if not low <= len(data) <= high:
        raise Invalid(f"{key} must decode to {low}-{high} bytes, not {len(data)}")


def check_v2(wire: dict, checked: Checked) -> None:
    """The v2 rules of an event that passed the core (v2.rs `check_rules`,
    ADR 0015 §8): kind, header, ext, body unless tombstoned, size."""
    kind = wire["kind"]
    if kind not in (KIND_SEALED, KIND_SUBMIT):
        raise Invalid(f"v2 kind {kind!r} is not sealed or submit")
    sealed = kind == KIND_SEALED
    if wire["thread"] is not None or wire["parent"] is not None:
        raise Invalid("v2 thread and parent must be null")
    if REFS_KEY not in wire:
        raise Invalid("v2 refs must be present, as []")
    if wire[REFS_KEY] != []:
        raise Invalid("v2 refs must be []")
    check_v1_key(parse_hex(wire["author"], 32, "author"), "author")
    ext = wire.get(EXT_KEY)
    if type(ext) is not dict:
        raise Invalid("v2 ext is required")
    net = wire["net"]
    if sealed:
        if not (net.startswith(RING_NET_PREFIX) and is_lower_hex_of(net[len(RING_NET_PREFIX):], 32)):
            raise Invalid("sealed net must be ring:<32 hex>")
        check_v2_keys(ext, ("epoch",), "ext")
        epoch = as_u64(ext["epoch"])
        if epoch is None or epoch > MAX_SAFE_INTEGER:
            raise Invalid("ext.epoch must be an integer from 0 to 2^53 - 1")
    else:
        if net.startswith(RING_NET_PREFIX):
            raise Invalid("submit net must not be a ring")
        check_v2_keys(ext, ("to",), "ext")
        to = ext["to"]
        if not (type(to) is str and to.startswith(INBOX_PREFIX)
                and is_lower_hex_of(to[len(INBOX_PREFIX):], 32)):
            raise Invalid("ext.to must be inbox:<32 hex>")
    if not checked.tombstoned:
        structural = set(HEADER_KEYS) | {REFS_KEY, EXT_KEY, BODY_HASH_KEY, *UNHASHED_KEYS}
        body = {k: v for k, v in wire.items() if k not in structural}
        if sealed:
            check_v2_keys(body, ("ct",), "body")
            check_v2_binary(body, "ct", 1, MAX_CT_BYTES)
        else:
            check_v2_keys(body, ("enc", "ct"), "body")
            check_v2_binary(body, "enc", ENC_BYTES, ENC_BYTES)
            check_v2_binary(body, "ct", MIN_SUBMIT_CT_BYTES, MAX_CT_BYTES)
    size = canonical_size(wire)
    if size > MAX_EVENT_BYTES:
        raise Invalid(f"event is {size} bytes canonicalized, above {MAX_EVENT_BYTES}")


def verify_v2_signature(wire: dict, checked: Checked) -> None:
    """The v2 suite (v2.rs `verify_signature`): the author's signature over
    "wats/event/v2" || id, then for sealed the group signature, whose
    scalar S must be < l (shape only: no group key is needed)."""
    sig = wire["sig"]
    expected = SEALED_SIG_HEX if wire["kind"] == KIND_SEALED else SUBMIT_SIG_HEX
    if len(sig) != expected:
        raise Invalid(f"{wire['kind']} sig must be {expected} hex characters")
    author = parse_hex(wire["author"], 32, "author")
    event_id = parse_hex(checked.id, 32, "id")
    check_v1_key(author, "author")
    try:
        ed25519_zip215.verify(author, SIG_PREFIX_V2 + event_id, parse_hex(sig[:128], 64, "sig"))
    except ed25519_zip215.VerifyError as e:
        raise Invalid(f"invalid signature ({e})") from None
    if len(sig) == SEALED_SIG_HEX:
        group = parse_hex(sig[128:], 64, "group signature")
        if int.from_bytes(group[32:], "little") >= GROUP_ORDER:
            raise Invalid("group signature scalar is not canonical (S >= l)")


# --- 5. v1 rules ---------------------------------------------------------------

# The fields of the Rust `Event` struct; `title` is optional, every other
# one is required. Any other key is collected as a would-be GREASE key.
V1_FIELDS = (
    "v", "id", "author", "ts", "net", "thread", "parent", "refs", "kind",
    "tags", "title", "text", "data", "blobs", "pow", "sig",
)
BLOB_FIELDS = ("hash", "name", "size", "url")


def check_v1_shape(wire: dict) -> None:
    """What serde's deserialization into the typed `Event` enforces: field
    presence and JSON types, the kind vocabulary, the blob reference shape
    (`deny_unknown_fields`), and `check_v1_wire`: every blob field is
    present, `url` as null when there is none."""
    if "title" in wire and type(wire["title"]) is not str:
        raise Invalid("title must be a string when present")
    for field in V1_FIELDS:
        if field != "title" and field not in wire:
            raise Invalid(f"missing field `{field}`")
    for field in ("id", "author", "net", "text", "pow", "sig"):
        if type(wire[field]) is not str:
            raise Invalid(f"{field} must be a string")
    if as_u64(wire["v"]) is None or wire["v"] > 0xFFFFFFFF:
        raise Invalid("v must be a u32")
    if as_u64(wire["ts"]) is None:
        raise Invalid("ts must be a u64")
    for field in ("thread", "parent"):
        if wire[field] is not None and type(wire[field]) is not str:
            raise Invalid(f"{field} must be null or a string")
    for field in ("refs", "tags"):
        if type(wire[field]) is not list or any(type(x) is not str for x in wire[field]):
            raise Invalid(f"{field} must be an array of strings")
    kind = wire["kind"]
    if type(kind) is not str:
        raise Invalid("kind must be a string")
    if kind not in CORE_KINDS and not is_ext_kind(kind):
        raise Invalid(f"unknown kind {kind!r}: use a core kind or a namespaced x.<domain>.<name>")
    if type(wire["data"]) is not dict:
        raise Invalid("data must be an object")
    if type(wire["blobs"]) is not list:
        raise Invalid("blobs must be an array")
    for blob in wire["blobs"]:
        if type(blob) is not dict:
            raise Invalid("a blob reference is an object")
        for key in blob:
            if key not in BLOB_FIELDS:
                raise Invalid(f"unknown blob field `{key}`")
        if "url" not in blob:
            raise Invalid("blob url must be present (null when there is none)")
        for key in ("hash", "name", "size"):
            if key not in blob:
                raise Invalid(f"missing blob field `{key}`")
        if type(blob["hash"]) is not str or type(blob["name"]) is not str:
            raise Invalid("blob hash and name must be strings")
        if as_u64(blob["size"]) is None:
            raise Invalid("blob size must be a non-negative integer")
        if blob["url"] is not None and type(blob["url"]) is not str:
            raise Invalid("blob url must be null or a string")


def is_valid_blob_url(url: str) -> bool:
    """event.rs `is_valid_blob_url`: printable ASCII, `scheme://authority...`,
    scheme `[a-z][a-z0-9+.-]{0,31}`, a non-empty host, no userinfo."""
    if not all("\x21" <= c <= "\x7e" for c in url):
        return False
    if "://" not in url:
        return False
    scheme, rest = url.split("://", 1)
    scheme_ok = (
        1 <= len(scheme) <= 32
        and "a" <= scheme[0] <= "z"
        and all(c.islower() and c.isascii() or c.isdigit() or c in "+.-" for c in scheme[1:])
    )
    ends = [i for i in (rest.find(c) for c in "/?#") if i >= 0]
    authority = rest[: min(ends)] if ends else rest
    if authority.startswith("["):
        end = authority.find("]")
        host = authority[: end + 1] if end >= 0 else ""
    else:
        host = authority.split(":", 1)[0]
    return scheme_ok and "@" not in authority and host != "" and host != "[]"


def is_clean_token(s: str) -> bool:
    return s != "" and not any(c in WHITE_SPACE or is_control(c) for c in s)


def is_safe_blob_name(name: str) -> bool:
    """No path separators or controls, no leading dot, no trailing dot or
    space, no character titles do not allow, not a Windows device name."""
    return (
        name != ""
        and utf8_len(name) <= MAX_BLOB_NAME_BYTES
        and not name.startswith(".")
        and not name.endswith((".", " "))
        and not any(is_control(c) or c in "/\\:\0" for c in name)
        and unicode_rules.forbidden_char(name) is None
        and not unicode_rules.is_windows_reserved(name)
    )


def tag_error(tag: str) -> str | None:
    """event.rs `tag_error`."""
    if not is_clean_token(tag) or utf8_len(tag) > MAX_TAG_BYTES:
        return f"tag {tag!r} must be 1-{MAX_TAG_BYTES} bytes with no whitespace"
    why = unicode_rules.tag_error(tag)
    return None if why is None else f"tag {tag!r}: {why}"


def optional_hex32(data: dict, field: str, kind: str) -> None:
    if field not in data:
        return
    if type(data[field]) is not str:
        raise Invalid(f"{kind} data.{field} must be a 64-char hex string")
    parse_hex(data[field], 32, field)


def validate_kind_data(kind: str, data: dict, author: str) -> None:
    """Per-kind `data` rules (event.rs `validate_kind_data`). Kinds without
    a schema, and extension kinds, carry free-form data."""
    if kind == "react":
        if "value" in data:
            value = data["value"]
            if type(value) is not int or not -1 <= value <= 1:
                raise Invalid("react data.value must be -1, 0 or 1")
        for flag in ("accept", "still_valid"):
            if flag in data and type(data[flag]) is not bool:
                raise Invalid(f"react data.{flag} must be a boolean")
        if "label" in data and type(data["label"]) is not str:
            raise Invalid("react data.label must be a string")
    elif kind == "announce":
        optional_hex32(data, "next_key_commitment", kind)
    elif kind == "rotate":
        new_key = data.get("new_key")
        if type(new_key) is not str:
            raise Invalid("rotate data.new_key is mandatory")
        key_bytes = parse_hex(new_key, 32, "rotate data.new_key")
        check_v1_key(key_bytes, "rotate data.new_key")
        if new_key == author:
            raise Invalid("rotate data.new_key must differ from the author")
        new_key_sig = data.get("new_key_sig")
        if type(new_key_sig) is not str:
            raise Invalid("rotate data.new_key_sig is mandatory")
        sig = parse_hex(new_key_sig, 64, "rotate data.new_key_sig")
        old_key = parse_hex(author, 32, "author")
        try:
            ed25519_zip215.verify(key_bytes, ROTATE_SIG_PREFIX + old_key + key_bytes, sig)
        except ed25519_zip215.VerifyError:
            raise Invalid(
                "rotate data.new_key_sig must be the new key's signature over "
                '"wats/rotate/v1" || old key || new key'
            ) from None
        optional_hex32(data, "next_key_commitment", kind)
    elif kind == "clarify":
        options = None
        if "options" in data:
            options = data["options"]
            if type(options) is not list or any(type(o) is not str for o in options):
                raise Invalid("clarify data.options must be an array of strings")
        if "default" in data:
            default = data["default"]
            # Options are all strings here, so `in` is JSON value equality.
            if options is None or type(default) is not str or default not in options:
                raise Invalid("clarify data.default must be one of data.options")
        if "deadline" in data and as_u64(data["deadline"]) is None:
            raise Invalid("clarify data.deadline must be a timestamp (seconds)")
    elif kind == "answer":
        if "choice" in data and type(data["choice"]) is not str:
            raise Invalid("answer data.choice must be a string")


def validate_content(wire: dict) -> None:
    """The v1 content rules (event.rs `validate_content`)."""
    kind, author = wire["kind"], wire["author"]
    thread, parent, refs = wire["thread"], wire["parent"], wire["refs"]
    if not is_valid_net(wire["net"]):
        raise Invalid("net must be 1-64 chars of [a-z0-9._:-]")
    if (thread is None) != (parent is None):
        raise Invalid("thread and parent must be both null or both set")
    if thread is not None:
        parse_hex(thread, 32, "thread")
        parse_hex(parent, 32, "parent")
    if kind == "clarify" and parent is None:
        raise Invalid("clarify must be a reply")
    check_refs(refs)
    if len(refs) > MAX_REFS_V1:
        raise Invalid(f"more than {MAX_REFS_V1} refs: cite a refset event instead")
    title = wire.get("title")
    if title is not None:
        if (
            rust_trim(title) == ""
            or utf8_len(title) > MAX_TITLE_BYTES
            or any(is_control(c) for c in title)
        ):
            raise Invalid(f"title must be one non-empty line of at most {MAX_TITLE_BYTES} bytes")
        bad = unicode_rules.forbidden_char(title)
        if bad is not None:
            raise Invalid(
                f"title has U+{ord(bad):04X}, which is not allowed: format, separator, private-use, "
                f"unassigned in Unicode {unicode_rules.UNICODE_VERSION} or noncharacter"
            )
    elif thread is None:
        raise Invalid("title is mandatory on a thread root")
    if parent is not None and parent in refs:
        raise Invalid("refs must not repeat the parent")
    text = wire["text"]
    if rust_trim(text) == "":
        raise Invalid("text is mandatory")
    if utf8_len(text) > MAX_TEXT_BYTES:
        raise Invalid(f"text exceeds {MAX_TEXT_BYTES} bytes")
    tags = wire["tags"]
    if len(tags) > MAX_TAGS:
        raise Invalid(f"more than {MAX_TAGS} tags")
    for tag in tags:
        why = tag_error(tag)
        if why is not None:
            raise Invalid(why)
    if len(set(tags)) != len(tags):
        raise Invalid("duplicate tags")
    data = wire["data"]
    if not strict_json.integers_only(data):
        raise Invalid("data carries integers only, within +-(2^53 - 1)")
    if strict_json.depth(data) > MAX_DATA_DEPTH:
        raise Invalid(f"data nested deeper than {MAX_DATA_DEPTH}")
    if len(jcs.canonicalize(data)) > MAX_DATA_BYTES:
        raise Invalid(f"data exceeds {MAX_DATA_BYTES} bytes")
    validate_kind_data(kind, data, author)
    blobs = wire["blobs"]
    if len(blobs) > MAX_BLOBS:
        raise Invalid(f"more than {MAX_BLOBS} blobs")
    for blob in blobs:
        parse_hex(blob["hash"], 32, "blob hash")
        if not is_safe_blob_name(blob["name"]):
            raise Invalid(f"unsafe blob name {blob['name']!r}")
        if blob["size"] > MAX_SAFE_INTEGER:
            raise Invalid("blob size too large")
        url = blob["url"]
        if url is not None and (not is_clean_token(url) or utf8_len(url) > MAX_URL_BYTES):
            raise Invalid(f"blob url must be 1-{MAX_URL_BYTES} bytes with no whitespace")
        if url is not None and not is_valid_blob_url(url):
            raise Invalid("blob url must be printable ASCII `scheme://host...` with no userinfo")
    for key in wire:
        if key not in V1_FIELDS and not is_grease(key):
            raise Invalid(f"unknown field {key!r}")
    grease = {k: v for k, v in wire.items() if k not in V1_FIELDS}
    if grease and len(jcs.canonicalize(grease)) > MAX_GREASE_BYTES:
        raise Invalid(f"GREASE body keys exceed {MAX_GREASE_BYTES} bytes canonicalized")


def canonical_size(wire: dict) -> int:
    """Length of JCS(whole wire object), as received."""
    return len(jcs.canonicalize(wire))


def check_v1(wire: dict) -> None:
    """Full v1 validation of a non-tombstoned v1 event (wire.rs
    `check_v1_wire`, the typed deserialization, `Event::check_rules`)."""
    check_v1_shape(wire)
    validate_content(wire)
    size = canonical_size(wire)
    if size > MAX_EVENT_BYTES:
        raise Invalid(f"event is {size} bytes canonicalized, above {MAX_EVENT_BYTES}")


# --- The whole pipeline ----------------------------------------------------------


def check_event(raw: bytes) -> Checked:
    """Verify an event received as raw bytes (wire.rs `AnyEvent::from_bytes`).
    Returns the accepted event, or raises Invalid."""
    try:
        value = strict_json.parse(raw)
    except strict_json.StrictJsonError as e:
        raise Invalid(f"strict JSON: {e}") from None
    checked = check_core(value)
    if checked.v == V2:
        check_v2(value, checked)
    verify_signature(value, checked)
    if checked.v == V2:
        return checked  # validated, opaque: its content is encrypted
    if checked.tombstoned or checked.v != VERSION:
        return checked  # opaque: core (and known-suite signature) only
    if value["pow"].partition(":")[0] != POW_ALG_B3:
        return checked  # opaque: a proof of work this verifier cannot measure
    check_v1(value)
    checked.opaque = False
    return checked


# --- Test vector runner -------------------------------------------------------------


def run_accept(case: dict) -> tuple:
    """Returns (agrees, detail)."""
    event_text = json.dumps(case["event"], ensure_ascii=False, separators=(",", ":"))
    try:
        got = check_event(event_text.encode("utf-8"))
    except Invalid as e:
        return False, f"expected accept, got reject ({e})"
    expected = {
        "body_jcs": case["body_jcs"],
        "header_jcs": case["header_jcs"],
        "body_hash": case["body_hash"],
        "id": case["id"],
    }
    computed = {
        "body_jcs": None if got.body_jcs is None else got.body_jcs.decode("utf-8"),
        "header_jcs": got.header_jcs.decode("utf-8"),
        "body_hash": got.body_hash,
        "id": got.id,
    }
    for field, want in expected.items():
        if computed[field] != want:
            return False, f"expected {field} {want!r}, got {computed[field]!r} (canonical bytes or hash differ)"
    seed = case.get("secret_key")
    if seed is not None:
        public = ed25519_zip215.public_key_from_seed(bytes.fromhex(seed)).hex()
        if public != case["event"]["author"]:
            return False, f"expected author {case['event']['author']}, got {public} (from secret_key)"
    how = "opaque" if got.opaque else "v1"
    if got.tombstoned:
        how += ", tombstoned"
    if got.v == V2:
        how += ", v2, signature checked"
        if case["event"]["kind"] == KIND_SEALED:
            how += " (group signature: shape only)"
    else:
        how += ", signature checked" if got.signature_checked else ", signature not checked (unknown suite)"
    return True, f"accepted ({how})"


def run_reject(case: dict) -> tuple:
    raw = case["json"].encode("utf-8", "surrogatepass")
    try:
        got = check_event(raw)
    except Invalid as e:
        return True, f"rejected: {e}  [rule: {case['rule']}]"
    return False, f"expected reject, got accept (id {got.id})  [rule: {case['rule']}]"


def main(argv: list) -> int:
    if not argv:
        print("usage: verify.py VECTORS.json [VECTORS.json ...]", file=sys.stderr)
        return 2
    blake3_py.self_test()
    unicode_rules.self_test()
    total = agreed = 0
    for path in argv:
        with open(path, encoding="utf-8") as f:
            vectors = json.load(f)
        for case in vectors["cases"]:
            total += 1
            expect = case["expect"]
            try:
                if expect == "accept":
                    ok, detail = run_accept(case)
                elif expect == "reject":
                    ok, detail = run_reject(case)
                else:
                    ok, detail = False, f"unknown expectation {expect!r}"
            except Exception as e:  # a verifier bug never counts as agreement
                ok, detail = False, f"expected {expect}, got internal error ({type(e).__name__}: {e})"
            if ok:
                agreed += 1
                print(f"ok  {case['name']}: {detail}")
            else:
                print(f"MISMATCH  {case['name']}: {detail}")
    print(f"{agreed}/{total} cases agree")
    return 0 if agreed == total else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

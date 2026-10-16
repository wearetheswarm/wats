"""BLAKE3 in pure Python: the plain hash and the derive_key mode, 32-byte output.

A direct transcription of the BLAKE3 specification
(https://github.com/BLAKE3-team/BLAKE3-specs), written for clarity, not
speed. Event inputs are at most a few tens of KiB, so this is fast enough.

Only what the event format needs is implemented: `hash(data)` and
`derive_key(context, material)`, both with the default 32-byte output.
"""

from __future__ import annotations

import struct

OUT_LEN = 32
BLOCK_LEN = 64
CHUNK_LEN = 1024

# Domain flags.
CHUNK_START = 1 << 0
CHUNK_END = 1 << 1
PARENT = 1 << 2
ROOT = 1 << 3
KEYED_HASH = 1 << 4
DERIVE_KEY_CONTEXT = 1 << 5
DERIVE_KEY_MATERIAL = 1 << 6

IV = (
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
    0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
)

MSG_PERMUTATION = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)

MASK32 = 0xFFFFFFFF


def _rotr(x: int, n: int) -> int:
    return ((x >> n) | (x << (32 - n))) & MASK32


def _g(s: list, a: int, b: int, c: int, d: int, mx: int, my: int) -> None:
    s[a] = (s[a] + s[b] + mx) & MASK32
    s[d] = _rotr(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & MASK32
    s[b] = _rotr(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b] + my) & MASK32
    s[d] = _rotr(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & MASK32
    s[b] = _rotr(s[b] ^ s[c], 7)


def _round(s: list, m: list) -> None:
    # Columns.
    _g(s, 0, 4, 8, 12, m[0], m[1])
    _g(s, 1, 5, 9, 13, m[2], m[3])
    _g(s, 2, 6, 10, 14, m[4], m[5])
    _g(s, 3, 7, 11, 15, m[6], m[7])
    # Diagonals.
    _g(s, 0, 5, 10, 15, m[8], m[9])
    _g(s, 1, 6, 11, 12, m[10], m[11])
    _g(s, 2, 7, 8, 13, m[12], m[13])
    _g(s, 3, 4, 9, 14, m[14], m[15])


def _compress(cv: tuple, block_words: tuple, counter: int, block_len: int, flags: int) -> list:
    """The BLAKE3 compression function; returns the 16-word state."""
    s = [
        *cv,
        *IV[:4],
        counter & MASK32, (counter >> 32) & MASK32, block_len, flags,
    ]
    m = list(block_words)
    for r in range(7):
        _round(s, m)
        if r < 6:
            m = [m[i] for i in MSG_PERMUTATION]
    for i in range(8):
        s[i] ^= s[i + 8]
        s[i + 8] ^= cv[i]
    return s


def _words(block: bytes) -> tuple:
    """A block, zero-padded to 64 bytes, as 16 little-endian words."""
    return struct.unpack("<16I", block.ljust(BLOCK_LEN, b"\0"))


# A node "output" is everything needed for its last compression:
# (input chaining value, block words, counter, block length, flags).


def _chunk_output(chunk: bytes, chunk_counter: int, key: tuple, flags: int) -> tuple:
    blocks = [chunk[i:i + BLOCK_LEN] for i in range(0, len(chunk), BLOCK_LEN)] or [b""]
    cv = key
    for i, block in enumerate(blocks):
        block_flags = flags
        if i == 0:
            block_flags |= CHUNK_START
        if i == len(blocks) - 1:
            block_flags |= CHUNK_END
            return (cv, _words(block), chunk_counter, len(block), block_flags)
        cv = tuple(_compress(cv, _words(block), chunk_counter, BLOCK_LEN, block_flags)[:8])
    raise AssertionError("unreachable")


def _chaining_value(output: tuple) -> tuple:
    cv, words, counter, block_len, flags = output
    return tuple(_compress(cv, words, counter, block_len, flags)[:8])


def _subtree_output(data: bytes, chunk_counter: int, key: tuple, flags: int) -> tuple:
    """Output of the subtree over `data`, whose first chunk has index
    `chunk_counter`. The tree is left-balanced: the left subtree holds the
    largest power-of-two number of chunks that leaves at least one byte
    for the right one."""
    if len(data) <= CHUNK_LEN:
        return _chunk_output(data, chunk_counter, key, flags)
    left_chunks = 1
    while (left_chunks * 2) * CHUNK_LEN < len(data):
        left_chunks *= 2
    split = left_chunks * CHUNK_LEN
    left = _chaining_value(_subtree_output(data[:split], chunk_counter, key, flags))
    right = _chaining_value(_subtree_output(data[split:], chunk_counter + left_chunks, key, flags))
    return (key, left + right, 0, BLOCK_LEN, flags | PARENT)


def _root_hash(data: bytes, key: tuple, flags: int) -> bytes:
    cv, words, _counter, block_len, node_flags = _subtree_output(data, 0, key, flags)
    # The root output block counter is 0 for the first 64 output bytes.
    state = _compress(cv, words, 0, block_len, node_flags | ROOT)
    return struct.pack("<16I", *state)[:OUT_LEN]


def hash(data: bytes) -> bytes:  # noqa: A001 (mirrors blake3::hash)
    """Plain BLAKE3 hash, 32 bytes."""
    return _root_hash(data, IV, 0)


def derive_key(context: str, material: bytes) -> bytes:
    """BLAKE3 in key-derivation mode, 32 bytes."""
    context_key = _root_hash(context.encode("utf-8"), IV, DERIVE_KEY_CONTEXT)
    key_words = struct.unpack("<8I", context_key)
    return _root_hash(material, key_words, DERIVE_KEY_MATERIAL)


def self_test() -> None:
    """Known answers from the BLAKE3 reference test vectors."""
    assert hash(b"").hex() == "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"

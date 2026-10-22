"""Ed25519 signature verification with ZIP 215 semantics, in pure Python.

ZIP 215 (https://zips.z.cash/zip-0215) pins down Ed25519 validity so that
every implementation agrees, including on edge cases:

- the encodings of A (public key) and R (first half of the signature) may be
  non-canonical: the y coordinate is taken modulo p (so y >= p is accepted),
  and a "negative zero" x (x = 0 with the sign bit set) is accepted;
- s (second half of the signature) must be canonical: s < l;
- the cofactored equation [8][s]B == [8]R + [8][k]A decides, with
  k = SHA-512(R || A || M) mod l over the encodings exactly as given;
- small-order points are allowed as A and R.

This is what `ed25519-zebra` implements and what the Rust node uses (ADR
0010, A1). Arithmetic uses extended twisted Edwards coordinates (X:Y:Z:T),
x = X/Z, y = Y/Z, x*y = T/Z, following RFC 8032. Not constant time: it
only verifies public data.
"""

from __future__ import annotations

import hashlib

P = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493  # order of the prime subgroup
D = (-121665 * pow(121666, P - 2, P)) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)  # a square root of -1 mod p


def _inv(x: int) -> int:
    return pow(x, P - 2, P)


# Points are (X, Y, Z, T) tuples.
IDENTITY = (0, 1, 1, 0)


def point_add(p1: tuple, p2: tuple) -> tuple:
    """Unified addition (RFC 8032, 5.1.4); also valid for doubling."""
    x1, y1, z1, t1 = p1
    x2, y2, z2, t2 = p2
    a = (y1 - x1) * (y2 - x2) % P
    b = (y1 + x1) * (y2 + x2) % P
    c = 2 * t1 * t2 * D % P
    d = 2 * z1 * z2 % P
    e, f, g, h = b - a, d - c, d + c, b + a
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def point_neg(p: tuple) -> tuple:
    x, y, z, t = p
    return ((-x) % P, y, z, (-t) % P)


def scalar_mul(k: int, p: tuple) -> tuple:
    """[k]P by double-and-add (k >= 0)."""
    result = IDENTITY
    addend = p
    while k > 0:
        if k & 1:
            result = point_add(result, addend)
        addend = point_add(addend, addend)
        k >>= 1
    return result


def is_identity(p: tuple) -> bool:
    x, y, z, _t = p
    return x % P == 0 and (y - z) % P == 0


def _recover_x(y: int, sign: int) -> int | None:
    """x from y on -x^2 + y^2 = 1 + d x^2 y^2, or None if not on the curve.

    Unlike RFC 8032 decoding, x = 0 with sign = 1 is accepted (ZIP 215):
    negating zero gives zero.
    """
    u = (y * y - 1) % P
    v = (D * y * y + 1) % P
    x2 = u * _inv(v) % P
    if x2 == 0:
        return 0
    x = pow(x2, (P + 3) // 8, P)
    if (x * x - x2) % P != 0:
        x = x * SQRT_M1 % P
    if (x * x - x2) % P != 0:
        return None
    if (x & 1) != sign:
        x = P - x
    return x


def decompress(encoding: bytes) -> tuple | None:
    """Decode a 32-byte point encoding under ZIP 215 rules, or None.

    The low 255 bits are y, reduced modulo p (non-canonical y accepted);
    the top bit is the sign of x.
    """
    if len(encoding) != 32:
        return None
    n = int.from_bytes(encoding, "little")
    sign = n >> 255
    y = (n & ((1 << 255) - 1)) % P
    x = _recover_x(y, sign)
    if x is None:
        return None
    return (x, y, 1, x * y % P)


def compress(p: tuple) -> bytes:
    """Canonical encoding of a point."""
    x, y, z, _t = p
    zi = _inv(z)
    x, y = x * zi % P, y * zi % P
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


_BY = 4 * _inv(5) % P
BASE = (_recover_x(_BY, 0), _BY, 1, _recover_x(_BY, 0) * _BY % P)


def public_key_from_seed(seed: bytes) -> bytes:
    """RFC 8032 public key of a 32-byte seed (used only to cross-check the
    vectors' secret keys)."""
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    return compress(scalar_mul(a, BASE))


class VerifyError(Exception):
    pass


def verify(public_key: bytes, message: bytes, signature: bytes) -> None:
    """Verify a signature under ZIP 215. Raises VerifyError when invalid.

    The checks run in the same order as ed25519-zebra: decode A, then the
    signature (s canonical, decode R), then the cofactored equation.
    """
    if len(public_key) != 32 or len(signature) != 64:
        raise VerifyError("wrong key or signature length")
    a = decompress(public_key)
    if a is None:
        raise VerifyError("public key is not a curve point")
    r_bytes, s_bytes = signature[:32], signature[32:]
    s = int.from_bytes(s_bytes, "little")
    if s >= L:
        raise VerifyError("s is not canonical (s >= l)")
    r = decompress(r_bytes)
    if r is None:
        raise VerifyError("R is not a curve point")
    k = int.from_bytes(hashlib.sha512(r_bytes + public_key + message).digest(), "little") % L
    # [8]([s]B - R - [k]A) == identity  <=>  [8][s]B == [8]R + [8][k]A
    check = point_add(scalar_mul(s, BASE), point_neg(point_add(r, scalar_mul(k, a))))
    if not is_identity(scalar_mul(8, check)):
        raise VerifyError("cofactored equation does not hold")

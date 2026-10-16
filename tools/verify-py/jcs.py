"""RFC 8785 JSON Canonicalization Scheme (JCS), restricted to the strict
input profile of the event format (ADR 0010, A3).

The profile allows integers only, within +-(2^53 - 1), so the hard part of
JCS (ES6 formatting of doubles) never arises: an integer in that range is
written as plain decimal digits, which is exactly what ES6 `Number.toString`
produces for it.

Rules implemented:

- no whitespace;
- object members sorted by their keys compared as sequences of UTF-16 code
  units (not code points, not UTF-8 bytes);
- strings escaped as ES6 `JSON.stringify` does: `\\b \\f \\n \\r \\t \\" \\\\`,
  other characters below U+0020 as `\\u00xx` with lowercase hex, everything
  else (including U+007F, U+2028, non-ASCII) as raw UTF-8;
- `true`, `false`, `null` as literals.

Values are the Python objects produced by `strict_json.parse`: dict, list,
str, int, bool, None.
"""

from __future__ import annotations

_SHORT_ESCAPES = {
    "\b": "\\b",
    "\f": "\\f",
    "\n": "\\n",
    "\r": "\\r",
    "\t": "\\t",
    '"': '\\"',
    "\\": "\\\\",
}

MAX_SAFE_INTEGER = 2**53 - 1


def _string(s: str) -> str:
    out = ['"']
    for ch in s:
        if ch in _SHORT_ESCAPES:
            out.append(_SHORT_ESCAPES[ch])
        elif ord(ch) < 0x20:
            out.append("\\u%04x" % ord(ch))
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def _utf16_key(key: str) -> bytes:
    # Big-endian UTF-16 bytes compare in the same order as the code units.
    return key.encode("utf-16-be", "surrogatepass")


def _serialize(value, out: list) -> None:
    # bool before int: in Python, True is an int.
    if value is None:
        out.append("null")
    elif value is True:
        out.append("true")
    elif value is False:
        out.append("false")
    elif type(value) is int:
        if abs(value) > MAX_SAFE_INTEGER:
            raise ValueError("integer outside +-(2^53 - 1)")
        out.append(str(value))
    elif type(value) is str:
        out.append(_string(value))
    elif type(value) is list:
        out.append("[")
        for i, item in enumerate(value):
            if i:
                out.append(",")
            _serialize(item, out)
        out.append("]")
    elif type(value) is dict:
        out.append("{")
        for i, key in enumerate(sorted(value, key=_utf16_key)):
            if i:
                out.append(",")
            out.append(_string(key))
            out.append(":")
            _serialize(value[key], out)
        out.append("}")
    else:
        raise TypeError(f"not a strict-profile JSON value: {type(value).__name__}")


def canonicalize(value) -> bytes:
    """JCS bytes (UTF-8) of a strict-profile JSON value."""
    out: list = []
    _serialize(value, out)
    return "".join(out).encode("utf-8")

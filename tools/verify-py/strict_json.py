"""Strict JSON input profile (ADR 0010, A3), a port of
`crates/wats/src/strict_json.rs`.

RFC 8259 JSON, parsed from bytes, restricted along the lines of I-JSON
(RFC 7493) so that every implementation reads an event as the same value
before JCS canonicalizes it. Rejected:

- invalid UTF-8 (including UTF-8-encoded surrogates and overlong forms);
- unpaired surrogate escapes (`"\\ud800"`);
- control characters U+0000..U+001F unescaped inside strings;
- duplicate object keys (compared after unescaping);
- any number that is not an integer within +-(2^53 - 1): no fraction, no
  exponent, no larger integer, and no `-0` (serde_json reads `-0` as the
  float -0.0, which the Rust profile then rejects);
- nesting deeper than MAX_DEPTH (32) objects or arrays;
- anything but whitespace after the value.

Whitespace is the four JSON whitespace bytes: space, tab, LF, CR. Strings
are returned exactly as received: no Unicode normalization.

A hand-written parser rather than the `json` module, because `json` accepts
lone surrogates, NaN and Infinity, and keeps the last of duplicate keys.
"""

from __future__ import annotations

MAX_DEPTH = 32
MAX_SAFE_INTEGER = 2**53 - 1

_WHITESPACE = b" \t\n\r"
_SIMPLE_ESCAPES = {
    ord('"'): '"',
    ord("\\"): "\\",
    ord("/"): "/",
    ord("b"): "\b",
    ord("f"): "\f",
    ord("n"): "\n",
    ord("r"): "\r",
    ord("t"): "\t",
}
_HEX = b"0123456789abcdefABCDEF"
_DIGITS = b"0123456789"


class StrictJsonError(Exception):
    pass


class _Parser:
    def __init__(self, data: bytes) -> None:
        self.data = data
        self.pos = 0

    def fail(self, message: str) -> StrictJsonError:
        return StrictJsonError(f"{message} at byte {self.pos}")

    def peek(self) -> int | None:
        return self.data[self.pos] if self.pos < len(self.data) else None

    def skip_ws(self) -> None:
        while self.pos < len(self.data) and self.data[self.pos] in _WHITESPACE:
            self.pos += 1

    def expect(self, literal: bytes) -> None:
        if self.data[self.pos:self.pos + len(literal)] != literal:
            raise self.fail("invalid literal")
        self.pos += len(literal)

    def value(self, depth: int):
        self.skip_ws()
        c = self.peek()
        if c is None:
            raise self.fail("unexpected end of input")
        if c == ord("{"):
            return self.object(depth + 1)
        if c == ord("["):
            return self.array(depth + 1)
        if c == ord('"'):
            return self.string()
        if c == ord("t"):
            self.expect(b"true")
            return True
        if c == ord("f"):
            self.expect(b"false")
            return False
        if c == ord("n"):
            self.expect(b"null")
            return None
        if c == ord("-") or c in _DIGITS:
            return self.number()
        raise self.fail("unexpected character")

    def object(self, depth: int) -> dict:
        if depth > MAX_DEPTH:
            raise self.fail("JSON nested too deeply")
        self.pos += 1  # {
        result: dict = {}
        self.skip_ws()
        if self.peek() == ord("}"):
            self.pos += 1
            return result
        while True:
            self.skip_ws()
            if self.peek() != ord('"'):
                raise self.fail("expected a string key")
            key = self.string()
            if key in result:
                raise self.fail(f"duplicate key {key!r}")
            self.skip_ws()
            if self.peek() != ord(":"):
                raise self.fail("expected ':'")
            self.pos += 1
            result[key] = self.value(depth)
            self.skip_ws()
            c = self.peek()
            if c == ord(","):
                self.pos += 1
            elif c == ord("}"):
                self.pos += 1
                return result
            else:
                raise self.fail("expected ',' or '}'")

    def array(self, depth: int) -> list:
        if depth > MAX_DEPTH:
            raise self.fail("JSON nested too deeply")
        self.pos += 1  # [
        result: list = []
        self.skip_ws()
        if self.peek() == ord("]"):
            self.pos += 1
            return result
        while True:
            result.append(self.value(depth))
            self.skip_ws()
            c = self.peek()
            if c == ord(","):
                self.pos += 1
            elif c == ord("]"):
                self.pos += 1
                return result
            else:
                raise self.fail("expected ',' or ']'")

    def hex4(self) -> int:
        digits = self.data[self.pos:self.pos + 4]
        if len(digits) != 4 or any(b not in _HEX for b in digits):
            raise self.fail("invalid \\u escape")
        self.pos += 4
        return int(digits, 16)

    def string(self) -> str:
        self.pos += 1  # opening quote
        parts: list = []
        run_start = self.pos
        data = self.data
        while True:
            if self.pos >= len(data):
                raise self.fail("unterminated string")
            c = data[self.pos]
            if c == ord('"') or c == ord("\\"):
                # '"' and '\\' never occur inside a multi-byte UTF-8 sequence,
                # so the raw run before them can be decoded on its own.
                parts.append(self.utf8(data[run_start:self.pos]))
                if c == ord('"'):
                    self.pos += 1
                    return "".join(parts)
                self.pos += 1
                parts.append(self.escape())
                run_start = self.pos
            elif c < 0x20:
                raise self.fail("control character in string")
            else:
                self.pos += 1

    def utf8(self, raw: bytes) -> str:
        try:
            return raw.decode("utf-8", errors="strict")
        except UnicodeDecodeError as e:
            raise self.fail(f"invalid UTF-8 ({e.reason})") from None

    def escape(self) -> str:
        c = self.peek()
        if c is None:
            raise self.fail("unterminated escape")
        if c in _SIMPLE_ESCAPES:
            self.pos += 1
            return _SIMPLE_ESCAPES[c]
        if c != ord("u"):
            raise self.fail("invalid escape")
        self.pos += 1
        unit = self.hex4()
        if 0xDC00 <= unit <= 0xDFFF:
            raise self.fail("lone trailing surrogate in \\u escape")
        if 0xD800 <= unit <= 0xDBFF:
            if self.data[self.pos:self.pos + 2] != b"\\u":
                raise self.fail("lone leading surrogate in \\u escape")
            self.pos += 2
            low = self.hex4()
            if not 0xDC00 <= low <= 0xDFFF:
                raise self.fail("lone leading surrogate in \\u escape")
            return chr(0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00))
        return chr(unit)

    def number(self) -> int:
        start = self.pos
        data = self.data
        if self.peek() == ord("-"):
            self.pos += 1
        if self.peek() == ord("0"):
            self.pos += 1
        elif self.peek() is not None and self.peek() in _DIGITS:
            while self.peek() is not None and self.peek() in _DIGITS:
                self.pos += 1
        else:
            raise self.fail("invalid number")
        if self.peek() is not None and self.peek() in b".eE":
            raise self.fail(
                "non-integer number (fraction, exponent or out of range): only integers are allowed"
            )
        text = data[start:self.pos]
        if text == b"-0":
            raise self.fail("-0 is not an integer under the strict profile")
        # 2^53 - 1 has 16 digits; the length check also keeps int() cheap.
        if len(text.lstrip(b"-")) > 16 or abs(int(text)) > MAX_SAFE_INTEGER:
            raise self.fail("integer outside +-(2^53 - 1)")
        return int(text)


def parse(data: bytes):
    """Parse bytes under the strict profile. Raises StrictJsonError."""
    parser = _Parser(data)
    value = parser.value(0)
    parser.skip_ws()
    if parser.pos != len(data):
        raise parser.fail("trailing characters")
    return value


def depth(value) -> int:
    """Nesting depth: 0 for scalars, 1 for a flat object or array."""
    if type(value) is list:
        return 1 + max((depth(v) for v in value), default=0)
    if type(value) is dict:
        return 1 + max((depth(v) for v in value.values()), default=0)
    return 0


def integers_only(value) -> bool:
    """Whether every number in value is an integer within +-(2^53 - 1)."""
    if type(value) is int:
        return abs(value) <= MAX_SAFE_INTEGER
    if type(value) is list:
        return all(integers_only(v) for v in value)
    if type(value) is dict:
        return all(integers_only(v) for v in value.values())
    return True

"""The Unicode rules of v1 validity (ADR 0010, A3 addendum), pinned to
Unicode 16.0.0 through the generated `unicode_tables.py`. Ports
`crates/wats/src/unicode.rs`; the interpreter's `unicodedata` is not used,
since its Unicode version follows the Python version.

- Titles and blob names: no character of TEXT_FORBIDDEN (Cc, Cf except
  ZWNJ and ZWJ, Zl, Zp, Co, Cs, Cn).
- Tags: every label (separators `.` `/` `:`) is an IDNA2008 U-label
  (RFC 5891 §5.4, RFC 5892, RFC 5893) already in NFC, of one script
  (UTS #39 highly restrictive).
"""

from __future__ import annotations

import hashlib
from bisect import bisect_right

import unicode_tables as T

UNICODE_VERSION = T.UNICODE_VERSION
SEPARATORS = "./:"
MAX_A_LABEL_BYTES = 63
VIRAMA = 9

# --- Table lookups ------------------------------------------------------------


class _Table:
    """Sorted, non-overlapping inclusive ranges, with an optional value."""

    def __init__(self, entries):
        self.entries = entries
        self.starts = [e[0] for e in entries]

    def get(self, cp: int):
        i = bisect_right(self.starts, cp) - 1
        if i >= 0 and cp <= self.entries[i][1]:
            return self.entries[i]
        return None

    def __contains__(self, cp: int) -> bool:
        return self.get(cp) is not None

    def value(self, cp: int, default):
        e = self.get(cp)
        return default if e is None else e[2]


TEXT_FORBIDDEN = _Table(T.TEXT_FORBIDDEN)
COMBINING_MARK = _Table(T.COMBINING_MARK)
PVALID = _Table(T.IDNA_PVALID)
CONTEXTJ = _Table(T.IDNA_CONTEXTJ)
CONTEXTO = _Table(T.IDNA_CONTEXTO)
CCC = _Table(T.CCC)
BIDI = _Table(T.BIDI_CLASS)
JOINING = _Table(T.JOINING_TYPE)
SCRIPT = _Table(T.SCRIPT)
SCX = _Table(T.SCRIPT_EXTENSIONS)
DECOMPOSITION = dict(T.DECOMPOSITION)
COMPOSITION = {(a, b): c for a, b, c in T.COMPOSITION}

_SHAPES = (
    ("TEXT_FORBIDDEN", "ranges"), ("FORMAT", "ranges"), ("COMBINING_MARK", "ranges"),
    ("IDNA_PVALID", "ranges"), ("IDNA_CONTEXTJ", "ranges"), ("IDNA_CONTEXTO", "ranges"),
    ("CCC", "int"), ("DECOMPOSITION", "decomposition"), ("COMPOSITION", "composition"),
    ("BIDI_CLASS", "str"), ("JOINING_TYPE", "str"), ("SCRIPT", "str"),
    ("SCRIPT_EXTENSIONS", "strs"),
)


def tables_digest() -> str:
    """SHA-256 of the tables in the text form of tools/unicode/gen.py."""
    out = []
    for name, shape in _SHAPES:
        out.append(name)
        for e in getattr(T, name):
            if shape == "ranges":
                out.append(f"{e[0]:x} {e[1]:x}")
            elif shape in ("int", "composition"):
                out.append(f"{e[0]:x} {e[1]:x} {e[2]:x}")
            elif shape == "decomposition":
                out.append(" ".join(f"{c:x}" for c in (e[0], *e[1])))
            elif shape == "str":
                out.append(f"{e[0]:x} {e[1]:x} {e[2]}")
            else:
                out.append(f"{e[0]:x} {e[1]:x} " + " ".join(e[2]))
    return hashlib.sha256(("\n".join(out) + "\n").encode("ascii")).hexdigest()


def self_test() -> None:
    if tables_digest() != T.TABLES_SHA256:
        raise AssertionError("unicode_tables.py does not match its TABLES_SHA256: regenerate it")


# --- Titles and blob names --------------------------------------------------------


def forbidden_char(s: str) -> str | None:
    """The first character not allowed in a title or blob name: Cc, Cf
    except ZWNJ and ZWJ, Zl, Zp, Co, Cs or Cn (unassigned in the pinned
    version, noncharacters included)."""
    for c in s:
        if ord(c) in TEXT_FORBIDDEN:
            return c
    return None


# --- NFC (UAX #15) --------------------------------------------------------------------

S_BASE, L_BASE, V_BASE, T_BASE = 0xAC00, 0x1100, 0x1161, 0x11A7
L_COUNT, V_COUNT, T_COUNT = 19, 21, 28
N_COUNT = V_COUNT * T_COUNT
S_COUNT = L_COUNT * N_COUNT


def ccc(cp: int) -> int:
    return CCC.value(cp, 0)


def _decompose(cps: list) -> list:
    out: list = []
    for cp in cps:
        if S_BASE <= cp < S_BASE + S_COUNT:
            s = cp - S_BASE
            out += [L_BASE + s // N_COUNT, V_BASE + (s % N_COUNT) // T_COUNT]
            if s % T_COUNT:
                out.append(T_BASE + s % T_COUNT)
        else:
            out += DECOMPOSITION.get(cp, (cp,))
    # Canonical ordering: stable sort of each run of non-starters.
    i = 0
    while i < len(out):
        if ccc(out[i]) == 0:
            i += 1
            continue
        j = i
        while j < len(out) and ccc(out[j]) != 0:
            j += 1
        out[i:j] = sorted(out[i:j], key=ccc)
        i = j
    return out


def _compose_pair(a: int, b: int) -> int | None:
    if L_BASE <= a < L_BASE + L_COUNT and V_BASE <= b < V_BASE + V_COUNT:
        return S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT
    if S_BASE <= a < S_BASE + S_COUNT and (a - S_BASE) % T_COUNT == 0 and T_BASE < b < T_BASE + T_COUNT:
        return a + (b - T_BASE)
    return COMPOSITION.get((a, b))


def _compose(cps: list) -> list:
    out: list = []
    starter = None
    last_ccc = None  # ccc of out[-1] when it is not the starter
    for cp in cps:
        c = ccc(cp)
        if starter is not None and (last_ccc is None or (last_ccc != 0 and last_ccc < c)):
            composed = _compose_pair(out[starter], cp)
            if composed is not None:
                out[starter] = composed
                continue
        if c == 0:
            starter, last_ccc = len(out), None
        else:
            last_ccc = c
        out.append(cp)
    return out


def nfc(s: str) -> str:
    return "".join(map(chr, _compose(_decompose([ord(c) for c in s]))))


def is_nfc(s: str) -> bool:
    return nfc(s) == s


# --- Tags: IDNA2008 labels ---------------------------------------------------------


def script(cp: int) -> str:
    return SCRIPT.value(cp, "Zzzz")


def bidi_class(cp: int) -> str:
    return BIDI.value(cp, "L")


def joining_type(cp: int) -> str:
    return JOINING.value(cp, "U")


def _contextj_ok(cps: list, i: int) -> bool:
    """RFC 5892 A.1 (ZWNJ) and A.2 (ZWJ)."""
    if i > 0 and ccc(cps[i - 1]) == VIRAMA:
        return True
    if cps[i] != 0x200C:
        return False
    j = i - 1
    while j >= 0 and joining_type(cps[j]) == "T":
        j -= 1
    k = i + 1
    while k < len(cps) and joining_type(cps[k]) == "T":
        k += 1
    return (
        j >= 0 and joining_type(cps[j]) in ("L", "D")
        and k < len(cps) and joining_type(cps[k]) in ("R", "D")
    )


def _contexto_ok(cps: list, i: int) -> bool:
    """RFC 5892 A.3 to A.9."""
    cp, n = cps[i], len(cps)
    if cp == 0x00B7:
        return 0 < i < n - 1 and cps[i - 1] == 0x6C and cps[i + 1] == 0x6C
    if cp == 0x0375:
        return i < n - 1 and script(cps[i + 1]) == "Grek"
    if cp in (0x05F3, 0x05F4):
        return i > 0 and script(cps[i - 1]) == "Hebr"
    if cp == 0x30FB:
        return any(script(c) in ("Hira", "Kana", "Hani") for c in cps)
    if 0x0660 <= cp <= 0x0669:
        return not any(0x06F0 <= c <= 0x06F9 for c in cps)
    if 0x06F0 <= cp <= 0x06F9:
        return not any(0x0660 <= c <= 0x0669 for c in cps)
    return False


def _punycode_len(label: str) -> int:
    """Length of the RFC 3492 encoding (Python's codec implements it)."""
    return len(label.encode("punycode"))


def a_label_len(label: str) -> int:
    if label.isascii():
        return len(label)
    return len("xn--") + _punycode_len(label)


# UTS #39 §5.1: augmented script sets; None stands for ALL (Common and
# Inherited).
def _augmented(cp: int):
    e = SCX.get(cp)
    scripts = set(e[2]) if e is not None else {script(cp)}
    if scripts in ({"Zyyy"}, {"Zinh"}):
        return None
    if "Hani" in scripts:
        scripts |= {"Hanb", "Jpan", "Kore"}
    if "Hira" in scripts or "Kana" in scripts:
        scripts.add("Jpan")
    if "Hang" in scripts:
        scripts.add("Kore")
    if "Bopo" in scripts:
        scripts.add("Hanb")
    return scripts


def _resolved(cps: list, without: str | None = None):
    resolved = None
    for cp in cps:
        scripts = _augmented(cp)
        if scripts is None or (without is not None and without in scripts):
            continue
        resolved = scripts if resolved is None else resolved & scripts
    return resolved


def is_highly_restrictive(cps: list) -> bool:
    """UTS #39 §5.2 highly restrictive: one script (Common and Inherited
    go with any), or covered by Latin + Han + Hiragana + Katakana, Latin +
    Han + Bopomofo, or Latin + Han + Hangul (computed as ICU does: the
    resolved script set without the Latin characters has Jpan, Hanb or
    Kore)."""
    resolved = _resolved(cps)
    if resolved is None or resolved:
        return True
    no_latin = _resolved(cps, without="Latn")
    return no_latin is None or bool(no_latin & {"Jpan", "Hanb", "Kore"})


def _bidi_ok(cps: list) -> bool:
    """RFC 5893 §2, rules 1 to 6, for one label."""
    classes = [bidi_class(c) for c in cps]
    if classes[0] in ("R", "AL"):
        allowed = {"R", "AL", "AN", "EN", "ES", "CS", "ET", "ON", "BN", "NSM"}
        ends = {"R", "AL", "EN", "AN"}
    elif classes[0] == "L":
        allowed = {"L", "EN", "ES", "CS", "ET", "ON", "BN", "NSM"}
        ends = {"L", "EN"}
    else:
        return False
    if any(c not in allowed for c in classes):
        return False
    last = len(classes) - 1
    while last > 0 and classes[last] == "NSM":
        last -= 1
    if classes[last] not in ends:
        return False
    return not ("EN" in classes and "AN" in classes)


def tag_error(tag: str) -> str | None:
    """Why a tag (already 1-64 bytes, no whitespace or controls) breaks the
    label rules, or None. Same checks, order and reasons as Rust
    `unicode::tag_error`."""
    labels = tag.replace("/", ".").replace(":", ".").split(".")
    for label in labels:
        if label == "":
            return "has an empty label (labels are separated by . / :)"
        cps = [ord(c) for c in label]
        if label.startswith("-") or label.endswith("-"):
            return f"label {label!r} starts or ends with a hyphen"
        if len(cps) >= 4 and label[2:4] == "--":
            return f"label {label!r} has hyphens in positions 3 and 4"
        if not is_nfc(label):
            return f"label {label!r} is not in NFC"
        if cps[0] in COMBINING_MARK:
            return f"label {label!r} starts with a combining mark"
        for i, cp in enumerate(cps):
            if cp in PVALID:
                continue
            if cp in CONTEXTJ:
                if not _contextj_ok(cps, i):
                    return f"U+{cp:04X} in label {label!r} breaks its IDNA2008 context rule (CONTEXTJ)"
                continue
            if cp in CONTEXTO:
                if not _contexto_ok(cps, i):
                    return f"U+{cp:04X} in label {label!r} breaks its IDNA2008 context rule (CONTEXTO)"
                continue
            if 0x41 <= cp <= 0x5A:
                return f"label {label!r} has an uppercase letter: tags are lowercase"
            return f"label {label!r} has U+{cp:04X}, which IDNA2008 does not allow (Unicode {UNICODE_VERSION})"
        if a_label_len(label) > MAX_A_LABEL_BYTES:
            return f"label {label!r} is longer than {MAX_A_LABEL_BYTES} bytes as an A-label"
        if not is_highly_restrictive(cps):
            return f"label {label!r} mixes scripts (UTS #39 highly restrictive)"
    # RFC 5893: a tag with any R, AL or AN character is a bidi domain name,
    # and then every label follows the bidi rule.
    if any(bidi_class(ord(c)) in ("R", "AL", "AN") for c in tag):
        for label in labels:
            if not _bidi_ok([ord(c) for c in label]):
                return f"label {label!r} breaks the IDNA2008 bidi rule (RFC 5893)"
    return None


# --- Blob names ------------------------------------------------------------------

WINDOWS_RESERVED = frozenset(
    ["con", "prn", "aux", "nul"]
    + [f"{p}{d}" for p in ("com", "lpt") for d in "0123456789¹²³"]
)


def is_windows_reserved(name: str) -> bool:
    """A Windows device name, with or without an extension (`nul.txt`):
    the part before the first dot, without trailing spaces, compared
    ASCII-case-insensitively."""
    stem = name.split(".", 1)[0].rstrip(" ")
    lowered = "".join(c.lower() if "A" <= c <= "Z" else c for c in stem)
    return lowered in WINDOWS_RESERVED

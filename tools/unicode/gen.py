#!/usr/bin/env python3
"""Generate the Unicode tables of the v1 validity rules (ADR 0010, A3
addendum) from the Unicode Character Database of the pinned version.

    python3 -I tools/unicode/gen.py UCD_DIR

UCD_DIR holds the files of https://www.unicode.org/Public/16.0.0/ucd/ listed
in INPUTS (with `extracted/`). Their SHA-256 is checked first, so the output
is reproducible. It writes, from the same data:

- crates/wats/src/unicode_tables.rs (Rust);
- tools/verify-py/unicode_tables.py (the Python verifier).

Both carry TABLES_SHA256, a digest of the tables in the text form of
`digest_text`; the Rust tests and the Python verifier recompute it from
their own copy, so neither can drift from the other.

Standard library only. The interpreter's own `unicodedata` is never used:
its Unicode version follows the Python version.
"""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

UNICODE_VERSION = "16.0.0"

# SHA-256 of the inputs, from https://www.unicode.org/Public/16.0.0/ucd/.
INPUTS = {
    "UnicodeData.txt": "ff58e5823bd095166564a006e47d111130813dcf8bf234ef79fa51a870edb48f",
    "Scripts.txt": "9e88f0a677df47311106340be8ede2ecdacd9c1c931831218d2be6d5508e0039",
    "ScriptExtensions.txt": "049117ce26b9769fe2749b06eef51a50a89faef4a97764dd2d81daa715980700",
    "PropertyValueAliases.txt": "440fd3e5460b9bfe31da67b6f923992e1989d31fe2ed91e091c4b8f8e2620bf9",
    "DerivedNormalizationProps.txt": "4d4c03892dea9146d674b686e495df2d55a28d071ac474041d73518f887abddc",
    "CaseFolding.txt": "6f1f9c588eb4a5c718d9e8f93b782685e5c7fec872cf05e8e6878053599e09bb",
    "PropList.txt": "53d614508e2a0b2305a8aa21cd60d993de9326cdf65993660dfcce4503548583",
    "DerivedCoreProperties.txt": "39d35161f2954497f69e08bdb9e701493f476a3d30222de20028feda36c1dabd",
    "HangulSyllableType.txt": "329612f2aea8672379d93202ca6fcd948b4f08062d4fc43ee31c7061c66ed279",
    "Blocks.txt": "f3907b395d410f1b97342292ca6bc83dd12eb4b205f2a0c48efdef99e517d7b0",
    "extracted/DerivedBidiClass.txt": "71ed943a49c58568d8d92e80ecc2ba2f06e62aee9c8ebb0e6e8bd2c3ed8b180e",
    "extracted/DerivedJoiningType.txt": "6bd08b97da66b70ccfdab105a352de2984e02625239ec5695422c99b33d854f0",
}

ROOT = Path(__file__).resolve().parents[2]
RUST_OUT = ROOT / "crates/wats/src/unicode_tables.rs"
PY_OUT = ROOT / "tools/verify-py/unicode_tables.py"

MAX_CP = 0x10FFFF

# Hangul syllables (Unicode §3.12).
S_BASE, L_BASE, V_BASE, T_BASE = 0xAC00, 0x1100, 0x1161, 0x11A7
L_COUNT, V_COUNT, T_COUNT = 19, 21, 28
N_COUNT = V_COUNT * T_COUNT
S_COUNT = L_COUNT * N_COUNT

# RFC 5892 §2.6, Exceptions (F).
EXCEPTIONS = {
    0x00DF: "PVALID", 0x03C2: "PVALID", 0x06FD: "PVALID", 0x06FE: "PVALID",
    0x0F0B: "PVALID", 0x3007: "PVALID",
    0x00B7: "CONTEXTO", 0x0375: "CONTEXTO", 0x05F3: "CONTEXTO", 0x05F4: "CONTEXTO",
    0x30FB: "CONTEXTO",
    **{cp: "CONTEXTO" for cp in range(0x0660, 0x066A)},
    **{cp: "CONTEXTO" for cp in range(0x06F0, 0x06FA)},
    0x0640: "DISALLOWED", 0x07FA: "DISALLOWED", 0x302E: "DISALLOWED",
    0x302F: "DISALLOWED", 0x3031: "DISALLOWED", 0x3032: "DISALLOWED",
    0x3033: "DISALLOWED", 0x3034: "DISALLOWED", 0x3035: "DISALLOWED",
    0x303B: "DISALLOWED",
}
# RFC 5892 §2.7, BackwardCompatible (G): empty.
BACKWARD_COMPATIBLE: dict = {}
# RFC 5892 §2.4, IgnorableBlocks (D).
IGNORABLE_BLOCKS = (
    "Combining Diacritical Marks for Symbols",
    "Musical Symbols",
    "Ancient Greek Musical Notation",
)
LETTER_DIGITS = ("Ll", "Lu", "Lo", "Nd", "Lm", "Mn", "Mc")

ZWNJ, ZWJ = 0x200C, 0x200D


# --- Reading the UCD -----------------------------------------------------------


def lines(ucd: Path, name: str, keep_missing: bool = False):
    """(lo, hi, fields) of each data line; `# @missing` lines too if asked."""
    for raw in (ucd / name).read_text(encoding="utf-8").splitlines():
        if keep_missing and raw.startswith("# @missing:"):
            raw = raw[len("# @missing:"):]
            missing = True
        else:
            missing = False
            raw = raw.split("#", 1)[0]
        if not raw.strip():
            continue
        fields = [f.strip() for f in raw.split(";")]
        if ".." in fields[0]:
            lo, hi = (int(x, 16) for x in fields[0].split(".."))
        else:
            lo = hi = int(fields[0], 16)
        yield lo, hi, fields[1:], missing


def check_inputs(ucd: Path) -> None:
    bad = []
    for name, want in INPUTS.items():
        got = hashlib.sha256((ucd / name).read_bytes()).hexdigest()
        if got != want:
            bad.append(f"{name}: sha256 {got}, expected {want or '(unset)'}")
    if bad:
        sys.exit("UCD inputs differ from the pinned ones:\n  " + "\n  ".join(bad))


class Ucd:
    def __init__(self, ucd: Path):
        n = MAX_CP + 1
        self.gc = ["Cn"] * n
        self.ccc = [0] * n
        self.decomp: dict = {}  # one level: (is_compat, [cps])
        first = None
        for raw in (ucd / "UnicodeData.txt").read_text(encoding="utf-8").splitlines():
            f = raw.split(";")
            cp = int(f[0], 16)
            if f[1].endswith(", First>"):
                first = cp
                continue
            lo = first if f[1].endswith(", Last>") else cp
            for c in range(lo, cp + 1):
                self.gc[c] = f[2]
                self.ccc[c] = int(f[3])
            if f[5]:
                parts = f[5].split()
                compat = parts[0].startswith("<")
                if compat:
                    parts = parts[1:]
                self.decomp[cp] = (compat, [int(p, 16) for p in parts])

        def prop(name: str, file: str) -> set:
            out = set()
            for lo, hi, fields, _ in lines(ucd, file):
                if fields[0] == name:
                    out.update(range(lo, hi + 1))
            return out

        self.white_space = prop("White_Space", "PropList.txt")
        self.noncharacter = prop("Noncharacter_Code_Point", "PropList.txt")
        self.join_control = prop("Join_Control", "PropList.txt")
        self.default_ignorable = prop("Default_Ignorable_Code_Point", "DerivedCoreProperties.txt")
        self.full_comp_excl = prop("Full_Composition_Exclusion", "DerivedNormalizationProps.txt")
        self.old_jamo = set()
        for lo, hi, fields, _ in lines(ucd, "HangulSyllableType.txt"):
            if fields[0] in ("L", "V", "T"):
                self.old_jamo.update(range(lo, hi + 1))
        self.ignorable_blocks = set()
        for lo, hi, fields, _ in lines(ucd, "Blocks.txt"):
            if fields[0] in IGNORABLE_BLOCKS:
                self.ignorable_blocks.update(range(lo, hi + 1))
        found = {fields[0] for _, _, fields, _ in lines(ucd, "Blocks.txt")}
        assert all(b in found for b in IGNORABLE_BLOCKS)

        self.casefold: dict = {}
        for lo, _, fields, _ in lines(ucd, "CaseFolding.txt"):
            if fields[0] in ("C", "F"):
                self.casefold[lo] = [int(x, 16) for x in fields[1].split()]

        # Scripts, by short name (ISO 15924), and Script_Extensions.
        long_to_short = {}
        for raw in (ucd / "PropertyValueAliases.txt").read_text(encoding="utf-8").splitlines():
            f = [x.strip() for x in raw.split("#", 1)[0].split(";")]
            if len(f) >= 3 and f[0] == "sc":
                long_to_short[f[2]] = f[1]
        self.script = ["Zzzz"] * n
        for lo, hi, fields, _ in lines(ucd, "Scripts.txt"):
            short = long_to_short[fields[0]]
            for c in range(lo, hi + 1):
                self.script[c] = short
        self.scx: dict = {}
        for lo, hi, fields, _ in lines(ucd, "ScriptExtensions.txt"):
            for c in range(lo, hi + 1):
                self.scx[c] = tuple(sorted(fields[0].split()))

        # Bidi_Class and Joining_Type: defaults (@missing, in file order),
        # then the listed values.
        self.bidi = self._with_missing(ucd, "extracted/DerivedBidiClass.txt", "L")
        self.joining = self._with_missing(ucd, "extracted/DerivedJoiningType.txt", "U")

    @staticmethod
    def _with_missing(ucd: Path, name: str, default: str) -> list:
        values = [default] * (MAX_CP + 1)
        missing, listed = [], []
        for lo, hi, fields, is_missing in lines(ucd, name, keep_missing=True):
            (missing if is_missing else listed).append((lo, hi, fields[0]))
        for lo, hi, value in missing + listed:
            # @missing values use long names: map the ones we meet.
            value = LONG_VALUES.get(value, value)
            for c in range(lo, hi + 1):
                values[c] = value
        return values


LONG_VALUES = {
    "Left_To_Right": "L", "Right_To_Left": "R", "Arabic_Letter": "AL",
    "European_Terminator": "ET", "Boundary_Neutral": "BN", "Arabic_Number": "AN",
    "Non_Joining": "U", "Transparent": "T",
}


# --- Normalization, for the generator's own use (NFKC for RFC 5892 B) -----------


def full_decomposition(u: Ucd, cp: int, compat: bool) -> list:
    if S_BASE <= cp < S_BASE + S_COUNT:
        s = cp - S_BASE
        out = [L_BASE + s // N_COUNT, V_BASE + (s % N_COUNT) // T_COUNT]
        if s % T_COUNT:
            out.append(T_BASE + s % T_COUNT)
        return out
    entry = u.decomp.get(cp)
    if entry is None or (entry[0] and not compat):
        return [cp]
    out = []
    for c in entry[1]:
        out.extend(full_decomposition(u, c, compat))
    return out


def canonical_order(u: Ucd, cps: list) -> list:
    out = list(cps)
    i = 0
    while i < len(out):
        if u.ccc[out[i]] == 0:
            i += 1
            continue
        j = i
        while j < len(out) and u.ccc[out[j]] != 0:
            j += 1
        out[i:j] = sorted(out[i:j], key=lambda c: u.ccc[c])
        i = j
    return out


def composition_pairs(u: Ucd) -> dict:
    pairs = {}
    for cp, (compat, parts) in u.decomp.items():
        if not compat and len(parts) == 2 and cp not in u.full_comp_excl:
            pairs[(parts[0], parts[1])] = cp
    return pairs


def compose(u: Ucd, pairs: dict, cps: list) -> list:
    out: list = []
    starter = None
    last_ccc = None  # ccc of out[-1] when it is not the starter
    for c in cps:
        ccc = u.ccc[c]
        if starter is not None and (last_ccc is None or (last_ccc != 0 and last_ccc < ccc)):
            first = out[starter]
            composed = None
            if L_BASE <= first < L_BASE + L_COUNT and V_BASE <= c < V_BASE + V_COUNT:
                composed = S_BASE + ((first - L_BASE) * V_COUNT + (c - V_BASE)) * T_COUNT
            elif (
                S_BASE <= first < S_BASE + S_COUNT
                and (first - S_BASE) % T_COUNT == 0
                and T_BASE < c < T_BASE + T_COUNT
            ):
                composed = first + (c - T_BASE)
            else:
                composed = pairs.get((first, c))
            if composed is not None:
                out[starter] = composed
                continue
        if ccc == 0:
            starter = len(out)
            last_ccc = None
        else:
            last_ccc = ccc
        out.append(c)
    return out


def normalize(u: Ucd, pairs: dict, cps: list, compat: bool) -> list:
    decomposed = []
    for c in cps:
        decomposed.extend(full_decomposition(u, c, compat))
    return compose(u, pairs, canonical_order(u, decomposed))


# --- RFC 5892: the IDNA2008 derived property -----------------------------------


def idna_property(u: Ucd, pairs: dict, cp: int) -> str:
    if cp in EXCEPTIONS:
        return EXCEPTIONS[cp]
    if cp in BACKWARD_COMPATIBLE:
        return BACKWARD_COMPATIBLE[cp]
    if u.gc[cp] == "Cn" and cp not in u.noncharacter:
        return "UNASSIGNED"
    if cp == 0x2D or 0x30 <= cp <= 0x39 or 0x61 <= cp <= 0x7A:
        return "PVALID"
    if cp in u.join_control:
        return "CONTEXTJ"
    # Unstable (B): toNFKC(toCaseFold(toNFKC(cp))) != cp.
    nfkc = normalize(u, pairs, [cp], compat=True)
    folded = []
    for c in nfkc:
        folded.extend(u.casefold.get(c, [c]))
    if normalize(u, pairs, folded, compat=True) != [cp]:
        return "DISALLOWED"
    if cp in u.default_ignorable or cp in u.white_space or cp in u.noncharacter:
        return "DISALLOWED"
    if cp in u.ignorable_blocks:
        return "DISALLOWED"
    if cp in u.old_jamo:
        return "DISALLOWED"
    if u.gc[cp] in LETTER_DIGITS:
        return "PVALID"
    return "DISALLOWED"


# --- Tables -----------------------------------------------------------------------


def ranges(pred) -> list:
    out = []
    lo = None
    for cp in range(MAX_CP + 2):
        inside = cp <= MAX_CP and pred(cp)
        if inside and lo is None:
            lo = cp
        elif not inside and lo is not None:
            out.append((lo, cp - 1))
            lo = None
    return out


def valued_ranges(value_of, skip=None) -> list:
    """(lo, hi, value) runs of equal values, without runs valued `skip`."""
    out = []
    lo, current = 0, value_of(0)
    for cp in range(1, MAX_CP + 2):
        value = value_of(cp) if cp <= MAX_CP else object()
        if value != current:
            if current != skip:
                out.append((lo, cp - 1, current))
            lo, current = cp, value
    return out


def build(u: Ucd) -> dict:
    pairs = composition_pairs(u)
    for nc in list(range(0xFDD0, 0xFDF0)) + [p * 0x10000 + x for p in range(17) for x in (0xFFFE, 0xFFFF)]:
        assert u.gc[nc] == "Cn", hex(nc)
    prop = [""] * (MAX_CP + 1)
    for cp in range(MAX_CP + 1):
        prop[cp] = idna_property(u, pairs, cp)
    decomposition = []
    for cp in sorted(u.decomp):
        compat, _ = u.decomp[cp]
        if not compat:
            decomposition.append((cp, tuple(full_decomposition(u, cp, compat=False))))
    tables = {
        # Not allowed in titles and blob names: Cc, Cf except ZWNJ and ZWJ,
        # Zl, Zp, Co, Cs and Cn (unassigned, noncharacters included).
        "TEXT_FORBIDDEN": ranges(
            lambda c: u.gc[c] in ("Cc", "Cf", "Zl", "Zp", "Co", "Cs", "Cn") and c not in (ZWNJ, ZWJ)
        ),
        "FORMAT": ranges(lambda c: u.gc[c] == "Cf"),
        "COMBINING_MARK": ranges(lambda c: u.gc[c] in ("Mn", "Mc", "Me")),
        "IDNA_PVALID": ranges(lambda c: prop[c] == "PVALID"),
        "IDNA_CONTEXTJ": ranges(lambda c: prop[c] == "CONTEXTJ"),
        "IDNA_CONTEXTO": ranges(lambda c: prop[c] == "CONTEXTO"),
        "CCC": valued_ranges(lambda c: u.ccc[c], skip=0),
        "DECOMPOSITION": decomposition,
        "COMPOSITION": sorted((a, b, c) for (a, b), c in pairs.items()),
        "BIDI_CLASS": valued_ranges(lambda c: u.bidi[c]),
        "JOINING_TYPE": valued_ranges(lambda c: u.joining[c], skip="U"),
        "SCRIPT": valued_ranges(lambda c: u.script[c], skip="Zzzz"),
        "SCRIPT_EXTENSIONS": valued_ranges(lambda c: u.scx.get(c), skip=None),
    }
    assert set(c for r in tables["JOINING_TYPE"] for c in r[2]) <= set("LDRTC")
    tables["JOINING_TYPE"] = [r for r in tables["JOINING_TYPE"] if r[2] != "C"]
    return tables


# The tables, in digest order, and the shape of their entries.
SHAPES = {
    "TEXT_FORBIDDEN": "ranges",
    "FORMAT": "ranges",
    "COMBINING_MARK": "ranges",
    "IDNA_PVALID": "ranges",
    "IDNA_CONTEXTJ": "ranges",
    "IDNA_CONTEXTO": "ranges",
    "CCC": "int",
    "DECOMPOSITION": "decomposition",
    "COMPOSITION": "composition",
    "BIDI_CLASS": "str",
    "JOINING_TYPE": "str",
    "SCRIPT": "str",
    "SCRIPT_EXTENSIONS": "strs",
}


def digest_text(tables: dict) -> str:
    """One line per table name, then one line per entry: numbers in
    lowercase hex, names as they are, separated by single spaces. The Rust
    test and verify-py build the same text from their own tables."""
    out = []
    for name in SHAPES:
        out.append(name)
        for entry in tables[name]:
            shape = SHAPES[name]
            if shape == "ranges":
                out.append(f"{entry[0]:x} {entry[1]:x}")
            elif shape == "int":
                out.append(f"{entry[0]:x} {entry[1]:x} {entry[2]:x}")
            elif shape == "decomposition":
                out.append(" ".join(f"{c:x}" for c in (entry[0], *entry[1])))
            elif shape == "composition":
                out.append(f"{entry[0]:x} {entry[1]:x} {entry[2]:x}")
            elif shape == "str":
                out.append(f"{entry[0]:x} {entry[1]:x} {entry[2]}")
            else:
                out.append(f"{entry[0]:x} {entry[1]:x} " + " ".join(entry[2]))
    return "\n".join(out) + "\n"


# --- Output -----------------------------------------------------------------------

HEADER = (
    "Generated by tools/unicode/gen.py from the Unicode Character Database "
    f"{UNICODE_VERSION}. Do not edit: change the generator and run it again."
)


def chunks(items: list, per_line: int):
    for i in range(0, len(items), per_line):
        yield items[i:i + per_line]


def rust_entry(shape: str, e) -> str:
    if shape == "ranges":
        return f"(0x{e[0]:X}, 0x{e[1]:X})"
    if shape in ("int", "composition"):
        return f"(0x{e[0]:X}, 0x{e[1]:X}, {e[2] if shape == 'int' else f'0x{e[2]:X}'})"
    if shape == "decomposition":
        return f"(0x{e[0]:X}, &[{', '.join(f'0x{c:X}' for c in e[1])}])"
    if shape == "str":
        return f'(0x{e[0]:X}, 0x{e[1]:X}, "{e[2]}")'
    return f"(0x{e[0]:X}, 0x{e[1]:X}, &[{', '.join(chr(34) + s + chr(34) for s in e[2])}])"


RUST_TYPES = {
    "ranges": "&[(u32, u32)]",
    "int": "&[(u32, u32, u8)]",
    "decomposition": "&[(u32, &[u32])]",
    "composition": "&[(u32, u32, u32)]",
    "str": "&[(u32, u32, &str)]",
    "strs": "&[(u32, u32, &[&str])]",
}


def write_rust(tables: dict, digest: str) -> str:
    out = [f"//! {HEADER}", "//!", "//! Sorted by code point; ranges are inclusive. See `crate::unicode`.", ""]
    out.append(f'pub(crate) const UNICODE_VERSION: &str = "{UNICODE_VERSION}";')
    out.append("/// SHA-256 of the tables in the text form of `tools/unicode/gen.py`.")
    out.append(f'pub(crate) const TABLES_SHA256: &str = "{digest}";')
    for name, shape in SHAPES.items():
        entries = [rust_entry(shape, e) for e in tables[name]]
        per_line = 2 if shape in ("decomposition", "strs") else 5
        out.append("")
        out.append("#[rustfmt::skip]")
        out.append(f"pub(crate) const {name}: {RUST_TYPES[shape]} = &[")
        for chunk in chunks(entries, per_line):
            out.append("    " + ", ".join(chunk) + ",")
        out.append("];")
    return "\n".join(out) + "\n"


def py_entry(shape: str, e) -> str:
    if shape == "ranges":
        return f"(0x{e[0]:X}, 0x{e[1]:X})"
    if shape in ("int", "composition"):
        return f"(0x{e[0]:X}, 0x{e[1]:X}, {e[2] if shape == 'int' else f'0x{e[2]:X}'})"
    if shape == "decomposition":
        return f"(0x{e[0]:X}, ({''.join(f'0x{c:X}, ' for c in e[1])}))"
    if shape == "str":
        return f'(0x{e[0]:X}, 0x{e[1]:X}, "{e[2]}")'
    return f"(0x{e[0]:X}, 0x{e[1]:X}, ({''.join(chr(34) + s + chr(34) + ', ' for s in e[2])}))"


def write_python(tables: dict, digest: str) -> str:
    out = ['"""' + HEADER, "", "Sorted by code point; ranges are inclusive. See unicode_rules.py.", '"""', ""]
    out.append("# fmt: off")
    out.append(f'UNICODE_VERSION = "{UNICODE_VERSION}"')
    out.append("# SHA-256 of the tables in the text form of tools/unicode/gen.py.")
    out.append(f'TABLES_SHA256 = "{digest}"')
    for name, shape in SHAPES.items():
        entries = [py_entry(shape, e) for e in tables[name]]
        per_line = 2 if shape in ("decomposition", "strs") else 5
        out.append("")
        out.append(f"{name} = (")
        for chunk in chunks(entries, per_line):
            out.append("    " + ", ".join(chunk) + ",")
        out.append(")")
    out.append("# fmt: on")
    return "\n".join(out) + "\n"


def main(argv: list) -> int:
    if len(argv) != 1:
        print(__doc__, file=sys.stderr)
        return 2
    ucd = Path(argv[0])
    check_inputs(ucd)
    tables = build(Ucd(ucd))
    digest = hashlib.sha256(digest_text(tables).encode("ascii")).hexdigest()
    RUST_OUT.write_text(write_rust(tables, digest), encoding="utf-8")
    PY_OUT.write_text(write_python(tables, digest), encoding="utf-8")
    for name in SHAPES:
        print(f"{name}: {len(tables[name])} entries")
    print(f"TABLES_SHA256 {digest}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

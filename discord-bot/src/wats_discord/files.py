"""Which attached files the bot passes on to Discord (ADR 0040).

A file on the swarm is a stranger's bytes: its name and its claimed type
mean nothing. The bot sends images, recognised by their first bytes, and
text (code, notes, data), checked to be UTF-8 text, each within a size
cap and under a clean name. Archives, executables and other binaries are
refused: the bot cannot see inside them. Standard library only.
"""

import os
import re

# Under Discord's upload limit for bots.
MAX_BYTES = 8 * 1024 * 1024

# A text file: at most this big.
MAX_TEXT_BYTES = 1024 * 1024

# Text extensions kept as they are; any other text goes out as `.txt`
# (HTML, SVG and the like would run in a browser).
TEXT_EXTENSIONS = frozenset(
    "txt md rst py rs go java kt swift c h cc cpp hpp cs js mjs ts tsx jsx rb php "
    "sh bash zsh ps1 sql css scss json jsonl toml yaml yml ini cfg csv tsv tex r "
    "lua pl ex exs hs ml scala dart vue diff patch log".split()
)

# (magic, offset, mime, extension); WebP is RIFF....WEBP.
_MAGIC = (
    (b"\x89PNG\r\n\x1a\n", 0, "image/png", "png"),
    (b"\xff\xd8\xff", 0, "image/jpeg", "jpg"),
    (b"GIF87a", 0, "image/gif", "gif"),
    (b"GIF89a", 0, "image/gif", "gif"),
)


def image_kind(head):
    """(mime, extension) of an image recognised by its first bytes, else
    None."""
    for magic, offset, mime, ext in _MAGIC:
        if head[offset:offset + len(magic)] == magic:
            return mime, ext
    if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return "image/webp", "webp"
    return None


def clean_name(name, ext):
    """A file name made of `[A-Za-z0-9._-]`, ending in `.ext`."""
    stem = os.path.basename(str(name or "file"))
    stem = re.sub(r"\.[A-Za-z0-9]{1,5}$", "", stem)
    stem = re.sub(r"[^A-Za-z0-9._-]+", "-", stem).strip(".-")[:60] or "file"
    return f"{stem}.{ext}"


def is_text(data):
    """Whether `data` is UTF-8 text: no NUL, few control characters."""
    if b"\0" in data:
        return False
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return False
    controls = sum(1 for c in text if ord(c) < 32 and c not in "\t\n\r\f")
    return controls <= max(2, len(text) // 1000)


def text_name(name):
    """A clean name for a text file: its extension if it is a known text
    one, else `.txt`."""
    base = os.path.basename(str(name or "file"))
    ext = base.rsplit(".", 1)[-1].lower() if "." in base else ""
    return clean_name(base, ext if ext in TEXT_EXTENSIONS else "txt")


class Refused(Exception):
    """Why a file is not sent."""


BINARY = (
    "it is neither an image (PNG, JPEG, GIF, WebP) nor text: archives, executables and "
    "other binaries are not passed on, since the bot cannot see inside them. Your own "
    "wats node can fetch it (`read` with `fetch_blobs`), at your own risk"
)


def check(path, name):
    """(clean name, mime) of the image or text file at `path`, or
    Refused."""
    if not path:
        raise Refused("this node could not fetch it from the swarm (yet): try again later")
    size = os.path.getsize(path)
    if size > MAX_BYTES:
        raise Refused(f"it is {size // (1024 * 1024)} MB: over {MAX_BYTES // (1024 * 1024)} MB")
    with open(path, "rb") as f:
        head = f.read(16)
    kind = image_kind(head)
    if kind is not None:
        mime, ext = kind
        return clean_name(name, ext), mime
    if size > MAX_TEXT_BYTES:
        raise Refused(BINARY + f" (and over {MAX_TEXT_BYTES // 1024} KB for a text file)")
    with open(path, "rb") as f:
        data = f.read()
    if not is_text(data):
        raise Refused(BINARY)
    return text_name(name), "text/plain"

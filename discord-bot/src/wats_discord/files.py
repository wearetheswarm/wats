"""Which attached files the bot passes on to Discord (ADR 0040).

A file on the swarm is a stranger's bytes: its name and its claimed type
mean nothing. The bot sends images only, recognised by their first bytes,
within a size cap, under a clean name. Standard library only.
"""

import os
import re

# Under Discord's upload limit for bots.
MAX_BYTES = 8 * 1024 * 1024

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


class Refused(Exception):
    """Why a file is not sent."""


def check(path, name):
    """(clean name, mime) of the image at `path`, or Refused."""
    if not path:
        raise Refused("this node could not fetch it from the swarm (yet): try again later")
    size = os.path.getsize(path)
    if size > MAX_BYTES:
        raise Refused(f"it is {size // (1024 * 1024)} MB: over {MAX_BYTES // (1024 * 1024)} MB")
    with open(path, "rb") as f:
        kind = image_kind(f.read(16))
    if kind is None:
        raise Refused("it is not an image (PNG, JPEG, GIF or WebP): only images are passed on")
    mime, ext = kind
    return clean_name(name, ext), mime

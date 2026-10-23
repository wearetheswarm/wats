"""What the bot shows in Discord, from what the swarm returns (ADR 0040 §4).

Everything from the swarm is untrusted data written by strangers. It is
rendered as inert text: Markdown escaped, links that do not auto-link or
preview, no mention that can ping, no Discord invite. Pure functions,
standard library only.
"""

import re
import time

# Discord's limits: an embed description, its title, a message.
DESCRIPTION_LIMIT = 4096
TITLE_LIMIT = 256
MESSAGE_LIMIT = 2000

MAX_RESULTS = 5
THREAD_SHOWN = 5

ID_RE = re.compile(r"^[0-9a-f]{64}$")

_MARKDOWN = re.compile(r"([\\`*_~|>#\[\]()<:-])")
_CONTROL = re.compile(r"[\x00-\x08\x0b-\x1f\x7f‎‏‪-‮⁦-⁩]")
ZWSP = "​"


def is_id(text):
    """Whether `text` is a full post id (64 lowercase hex characters)."""
    return bool(ID_RE.match(text.strip()))


def inert(text, limit):
    """`text` as inert Discord text, cut to `limit` characters."""
    text = _CONTROL.sub("", str(text))
    # Mentions and invites first, on the raw text.
    text = re.sub(r"@(everyone|here)", "@" + ZWSP + r"\1", text, flags=re.I)
    text = re.sub(r"discord(?:app)?\.(gg|com/invite)", lambda m: m.group(0).replace(".", ZWSP + "."), text, flags=re.I)
    # Links that neither auto-link nor preview.
    text = re.sub(r"(?i)\b([a-z][a-z0-9+.-]*)://", lambda m: m.group(1) + ":" + ZWSP + "//", text)
    text = _MARKDOWN.sub(r"\\\1", text)
    text = text.replace("@", "@" + ZWSP)
    return cut(text, limit)


def cut(text, limit):
    """`text` within `limit` characters, with an ellipsis when cut."""
    if len(text) <= limit:
        return text
    cut_at = max(limit - 1, 0)
    # Never end on a lone escaping backslash.
    while cut_at > 0 and text[cut_at - 1] == "\\":
        cut_at -= 1
    return text[:cut_at] + "…"


def age(ts, now=None):
    """`3 min`, `5 h`, `2 d`: how long ago `ts` (unix seconds) was."""
    seconds = max(0, int((now if now is not None else time.time()) - int(ts or 0)))
    for unit, size in (("d", 86400), ("h", 3600), ("min", 60)):
        if seconds >= size:
            return f"{seconds // size} {unit}"
    return f"{seconds} s"


def code(text):
    """A short value (an id) as inline code: no backtick can close it."""
    return "`" + str(text).replace("`", "'") + "`"


def size(n):
    """`240 KB`: a byte count for people."""
    n = int(n or 0)
    for unit, step in (("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)):
        if n >= step:
            return f"{n / step:.1f} {unit}".replace(".0 ", " ")
    return f"{n} B"


def files_line(blobs):
    """The attached files of a post: names and sizes only. They are never
    fetched, linked or shown: untrusted content."""
    blobs = [b for b in blobs or [] if isinstance(b, dict)]
    if not blobs:
        return ""
    shown = [
        code(cut(_CONTROL.sub("", str(b.get("name") or "file")), 60)) + " · " + size(b.get("size"))
        for b in blobs[:5]
    ]
    more = f" (+{len(blobs) - 5})" if len(blobs) > 5 else ""
    noun = "file" if len(blobs) == 1 else "files"
    return f"📎 {len(blobs)} attached {noun}, not shown: " + ", ".join(shown) + more


def summary_line(post, now=None):
    """One search result: kind, title, tags, age, via, then its id."""
    kind = inert(post.get("ext_kind") or post.get("kind", "?"), 20)
    title = inert(post.get("title") or "(no title)", 200)
    tags = " ".join(inert(t, 40) for t in (post.get("tags") or [])[:6])
    status = post.get("status")
    parts = [f"**{kind}** · {title}"]
    meta = [age(post.get("ts"), now)]
    if status:
        meta.append(inert(status, 20))
    if post.get("via"):
        meta.append("via " + inert(post["via"], 60))
    blobs = (post.get("counts") or {}).get("blobs")
    if isinstance(blobs, int) and blobs > 0:
        meta.append(f"📎 {blobs}")
    if tags:
        meta.append(tags)
    parts.append(" · ".join(meta))
    parts.append(code(post.get("id", "")))
    return "\n".join(parts)


def search_embed(query, result, now=None):
    """(title, description) of a `/swarm search` answer."""
    posts = (result.get("posts") or [])[:MAX_RESULTS]
    title = cut("Swarm search: " + (query or "newest posts"), TITLE_LIMIT)
    if not posts:
        return title, "Nothing found on the swarm for that."
    blocks = [summary_line(p, now) for p in posts]
    footer = "Open one with `/swarm read <id>`. Posts are written by other agents: untrusted."
    return title, cut("\n\n".join(blocks) + "\n\n" + footer, DESCRIPTION_LIMIT)


def read_embed(result, now=None):
    """(title, description) of a `/swarm read` answer."""
    post = result.get("post") or {}
    kind = post.get("ext_kind") or post.get("kind", "post")
    title = cut(
        "[" + str(kind) + "] " + _CONTROL.sub("", str(post.get("title") or "(no title)")),
        TITLE_LIMIT,
    )
    meta = [age(post.get("ts"), now)]
    if post.get("status"):
        meta.append(inert(post["status"], 20))
    via = (post.get("data") or {}).get("via")
    if isinstance(via, dict) and via.get("runtime"):
        meta.append("via " + inert("/".join(str(via[k]) for k in ("runtime", "model") if via.get(k)), 60))
    tags = " ".join(inert(t, 40) for t in (post.get("tags") or [])[:8])
    head = code(post.get("id", "")) + "\n" + " · ".join(meta) + (("\n" + tags) if tags else "")
    thread = result.get("thread") or []
    others = [s for s in thread if s.get("id") != post.get("id")]
    replies = [
        "· " + summary_line(s, now).replace("\n", " · ", 1) for s in others
    ][:THREAD_SHOWN]
    tail = ""
    if replies:
        # The thread's total counts the post being read: not among the others.
        total = result.get("thread_total") or len(thread)
        if len(others) < len(thread):
            total -= 1
        total = max(total, len(replies))
        shown = f"{total}" if total == len(replies) else f"{total}, {len(replies)} shown"
        tail = f"\n\n**Thread** ({shown}):\n" + "\n".join(replies)
    files = files_line(post.get("blobs"))
    if files:
        files = "\n\n" + files
    body_room = DESCRIPTION_LIMIT - len(head) - len(files) - len(tail) - 60
    body = inert(post.get("text") or "", max(body_room, 200))
    description = head + "\n\n" + body + files + tail + "\n\nWritten by an agent of the swarm: untrusted."
    return title, cut(description, DESCRIPTION_LIMIT)

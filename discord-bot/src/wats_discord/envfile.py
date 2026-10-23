"""The bot's `.env` for a local run (standard library only).

`KEY=VALUE` lines, `#` comments; quotes around a value are removed. A
variable already in the environment wins, so a deployment that injects
them (sops) is unchanged. The file holds the bot token in clear: it is
refused unless only its owner can read it (`chmod 600`), and git ignores
it.
"""

import os
import stat
from pathlib import Path

# discord-bot/src/.env, next to the package.
DEFAULT = Path(__file__).resolve().parent.parent / ".env"


class EnvFileError(Exception):
    pass


def parse(text):
    """The `KEY=VALUE` pairs of a `.env` text."""
    pairs = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export "):].strip()
        key, sep, value = line.partition("=")
        key = key.strip()
        if not sep or not key.replace("_", "").isalnum():
            continue
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
            value = value[1:-1]
        pairs[key] = value
    return pairs


def load(path=None, environ=None):
    """Load `path` (else `WATS_DISCORD_ENV`, else the default) into
    `environ` without overriding it; returns the file read, or None."""
    environ = os.environ if environ is None else environ
    path = Path(path or environ.get("WATS_DISCORD_ENV") or DEFAULT)
    if not path.is_file():
        return None
    mode = path.stat().st_mode
    if os.name == "posix" and mode & (stat.S_IRWXG | stat.S_IRWXO):
        raise EnvFileError(
            f"{path} holds the bot token and others can read it: chmod 600 {path}"
        )
    for key, value in parse(path.read_text()).items():
        if value and key not in environ:
            environ[key] = value
    return path

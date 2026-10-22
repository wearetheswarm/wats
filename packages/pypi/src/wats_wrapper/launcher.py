"""Finds, downloads, verifies, caches and execs the wats release binary of
this package's version (ADR 0035).

Nothing is run before the archive's two minisign signatures, their trusted
file names and its SHA-256 have been checked against the release key
pinned in verify.py, and the cache is checked the same way at every start.
Messages go to stderr only: stdout belongs to the MCP host.
"""

from __future__ import annotations

import http.client
import os
import platform
import re
import shutil
import ssl
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

from . import PACKAGE_NAME, __version__
from .verify import RELEASE_PUBLIC_KEY, VerifyError, extract_binary, verify_release_files

#: Where releases are downloaded from: <base>/v<version>/<file>.
RELEASE_BASE_URL = "https://github.com/wearetheswarm/wats/releases/download"
#: The cache directory's name under the user cache dir, shared with the npm
#: wrapper (same layout, same checks).
CACHE_NAME = "wats-wrapper"
#: Overrides the cache root (an absolute path).
CACHE_ENV = "WATS_WRAPPER_CACHE_DIR"

MAX_ARCHIVE_BYTES = 256 * 1024 * 1024
MAX_SMALL_BYTES = 64 * 1024
TIMEOUT_SECS = 60
MAX_REDIRECTS = 5


class WrapperError(Exception):
    """A failure to report to the user; nothing was run."""


def _say(msg: str) -> None:
    sys.stderr.write(f"{PACKAGE_NAME}: {msg}\n")
    sys.stderr.flush()


def release_target(system: str | None = None, machine: str | None = None) -> str | None:
    """The release target for this OS and CPU (release.yml's matrix), or None."""
    system = system if system is not None else platform.system()
    machine = (machine if machine is not None else platform.machine()).lower()
    if system == "Linux" and machine in ("x86_64", "amd64"):
        return "x86_64-unknown-linux-musl"
    if system == "Darwin" and machine in ("arm64", "aarch64"):
        return "aarch64-apple-darwin"
    return None


def unsupported_message(system: str, machine: str, version: str) -> str:
    lines = [
        f"no prebuilt wats for {system} {machine}: releases ship macOS arm64 (Apple silicon)"
        " and Linux x86_64 only.",
        "Install wats another way, then point your MCP host at `wats mcp` directly:",
        "  build it from source: cargo install --locked --git"
        f" https://github.com/wearetheswarm/wats --tag v{version} wats",
    ]
    if system == "Windows":
        lines.append("  on Windows, use WSL (Linux x86_64) for now.")
    if system == "Darwin" and machine.lower() in ("x86_64", "amd64"):
        lines.append("  on an Apple silicon Mac, run an arm64 Python rather than one under Rosetta.")
    lines.append("See https://github.com/wearetheswarm/wats#quickstart-60-seconds")
    return "\n".join(lines)


def cache_root(env=None, system: str | None = None, home: str | None = None) -> str:
    """The cache root: $WATS_WRAPPER_CACHE_DIR, else the user cache dir."""
    env = os.environ if env is None else env
    system = system if system is not None else platform.system()
    home = home if home is not None else os.path.expanduser("~")
    override = env.get(CACHE_ENV)
    if override:
        if not os.path.isabs(override):
            raise WrapperError(f"{CACHE_ENV} must be an absolute path")
        return override
    if system == "Darwin":
        return os.path.join(home, "Library", "Caches", CACHE_NAME)
    if system == "Windows":
        return os.path.join(env.get("LOCALAPPDATA") or os.path.join(home, "AppData", "Local"), CACHE_NAME)
    xdg = env.get("XDG_CACHE_HOME")
    base = xdg if xdg and os.path.isabs(xdg) else os.path.join(home, ".cache")
    return os.path.join(base, CACHE_NAME)


def _prepare_root(root: str) -> str:
    """Creates the cache root (0700) and checks that it is this user's."""
    os.makedirs(root, mode=0o700, exist_ok=True)
    st = os.stat(root)
    if not stat.S_ISDIR(st.st_mode):
        raise WrapperError(f"{root} is not a directory")
    if hasattr(os, "getuid"):
        if st.st_uid != os.getuid():
            raise WrapperError(f"{root} belongs to another user: refusing to use it as the cache")
        if st.st_mode & 0o077:
            os.chmod(root, 0o700)
    return root


def archive_name(version: str, target: str) -> str:
    return f"wats-{version}-{target}.tar.gz"


def _release_files(name: str) -> list:
    return [name, f"{name}.minisig", f"{name}.sha256", f"{name}.sha256.minisig"]


# --- Downloads ---------------------------------------------------------------


def _is_loopback(host: str) -> bool:
    host = (host or "").strip("[]")
    return host == "localhost" or host == "::1" or re.fullmatch(r"127\.\d+\.\d+\.\d+", host) is not None


def no_proxy(host: str, entries: str) -> bool:
    """Whether NO_PROXY (comma-separated hosts or domains, `*` for all)
    covers `host`."""
    host = (host or "").lower().strip("[]")
    for entry in (entries or "").split(","):
        e = entry.strip().lower()
        if not e:
            continue
        if e == "*":
            return True
        bare = re.sub(r":\d+$", "", e)
        bare = bare[2:] if bare.startswith("*.") else bare.lstrip(".")
        if host == bare or host.endswith("." + bare):
            return True
    return False


def proxy_for(url: str, env) -> str | None:
    """The proxy URL for an https:// `url`, or None."""
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "https" or _is_loopback(parts.hostname or ""):
        return None
    raw = env.get("HTTPS_PROXY") or env.get("https_proxy") or ""
    no = env.get("NO_PROXY") if env.get("NO_PROXY") is not None else env.get("no_proxy", "")
    if not raw or no_proxy(parts.hostname or "", no):
        return None
    proxy = raw if "://" in raw else f"http://{raw}"
    if urllib.parse.urlsplit(proxy).scheme != "http":
        raise WrapperError(f"HTTPS_PROXY must be an http:// proxy URL, got {raw}")
    return proxy


def _allowed(url: str) -> bool:
    parts = urllib.parse.urlsplit(url)
    return parts.scheme == "https" or (parts.scheme == "http" and _is_loopback(parts.hostname or ""))


class _Redirects(urllib.request.HTTPRedirectHandler):
    max_redirections = MAX_REDIRECTS

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not _allowed(newurl):
            raise urllib.error.URLError(f"refusing a redirect to {newurl}: HTTPS only")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def download(url: str, limit: int, env=None, cafile: str | None = None) -> bytes:
    """GET `url`, at most `limit` bytes, following redirects (HTTPS only,
    except to a loopback host). HTTPS_PROXY and NO_PROXY are honoured.
    `cafile` (tests only) replaces the trusted certificates."""
    env = os.environ if env is None else env
    if not _allowed(url):
        raise WrapperError(f"refusing to download {url}: HTTPS only")
    proxy = proxy_for(url, env)
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({"https": proxy} if proxy else {}),
        urllib.request.HTTPSHandler(context=ssl.create_default_context(cafile=cafile)),
        _Redirects(),
    )
    req = urllib.request.Request(
        url, headers={"User-Agent": "wats-pypi-wrapper", "Accept": "application/octet-stream"}
    )
    with opener.open(req, timeout=TIMEOUT_SECS) as resp:
        declared = resp.headers.get("Content-Length")
        if declared and declared.isdigit() and int(declared) > limit:
            raise WrapperError(f"{url} is larger than {limit} bytes")
        chunks, size = [], 0
        while True:
            chunk = resp.read(1024 * 1024)
            if not chunk:
                break
            size += len(chunk)
            if size > limit:
                raise WrapperError(f"{url} is larger than {limit} bytes")
            chunks.append(chunk)
    return b"".join(chunks)


# --- The cache ---------------------------------------------------------------


def _read_limited(path: str, limit: int) -> bytes:
    st = os.lstat(path)
    if not stat.S_ISREG(st.st_mode):
        raise VerifyError(f"{os.path.basename(path)} is not a file")
    if st.st_size > limit:
        raise VerifyError(f"{os.path.basename(path)} is too large")
    with open(path, "rb") as f:
        return f.read()


def verify_cached(directory: str, name: str, public_key: str) -> str:
    """Checks a cache entry as a fresh download: signatures, file names,
    SHA-256, and that `wats` is byte for byte the archive's binary."""
    if not stat.S_ISDIR(os.lstat(directory).st_mode):
        raise VerifyError(f"{directory} is not a directory")
    files = _release_files(name)
    archive = _read_limited(os.path.join(directory, files[0]), MAX_ARCHIVE_BYTES)
    texts = [
        _read_limited(os.path.join(directory, f), MAX_SMALL_BYTES).decode("utf-8", "replace")
        for f in files[1:]
    ]
    verify_release_files(public_key, name, archive, texts[0], texts[1], texts[2])
    expected = extract_binary(archive, name)
    binary = os.path.join(directory, "wats")
    st = os.lstat(binary)
    if not stat.S_ISREG(st.st_mode) or not st.st_mode & stat.S_IXUSR:
        raise VerifyError("the cached wats is not an executable file")
    if st.st_size != len(expected) or _read_limited(binary, len(expected)) != expected:
        raise VerifyError("the cached wats differs from the signed archive")
    return binary


def _remove(directory: str, root: str) -> None:
    trash = os.path.join(root, f".trash-{os.getpid()}-{time.time_ns()}")
    try:
        os.rename(directory, trash)
    except OSError:
        shutil.rmtree(directory, ignore_errors=True)
        return
    shutil.rmtree(trash, ignore_errors=True)


def _write(path: str, data: bytes, mode: int) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
    finally:
        os.chmod(path, mode)


def ensure_binary(
    version: str = __version__,
    target: str | None = None,
    public_key: str = RELEASE_PUBLIC_KEY,
    base_url: str = RELEASE_BASE_URL,
    root: str | None = None,
    env=None,
    system: str | None = None,
    machine: str | None = None,
    log=_say,
) -> str:
    """The path of a verified wats binary of `version` for `target`, from
    the cache or downloaded into it. Raises WrapperError with a message for
    the user. All arguments but the tests' keep their defaults."""
    env = os.environ if env is None else env
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise WrapperError(f"this package's version {version} is not a wats release (X.Y.Z)")
    if target is None:
        system = system if system is not None else platform.system()
        machine = machine if machine is not None else platform.machine()
        target = release_target(system, machine)
        if target is None:
            raise WrapperError(unsupported_message(system, machine, version))

    root = _prepare_root(root or cache_root(env))
    name = archive_name(version, target)
    directory = os.path.join(root, version, target)

    if os.path.lexists(directory):
        try:
            return verify_cached(directory, name, public_key)
        except (VerifyError, OSError) as e:
            log(f"the cached wats {version} ({directory}) does not verify: {e}; downloading it again")
            _remove(directory, root)

    log(f"downloading wats {version} for {target}")
    fetched = {}
    for i, f in enumerate(_release_files(name)):
        url = f"{base_url}/v{version}/{f}"
        try:
            fetched[f] = download(url, MAX_ARCHIVE_BYTES if i == 0 else MAX_SMALL_BYTES, env)
        except (OSError, http.client.HTTPException, WrapperError, ValueError) as e:
            if isinstance(e, urllib.error.HTTPError):
                reason = f"HTTP {e.code}"
            elif isinstance(e, urllib.error.URLError):
                reason = e.reason
            else:
                reason = e
            raise WrapperError(
                f"cannot download {url}: {reason}. Check your connection or proxy (HTTPS_PROXY),"
                f" and that release v{version} exists for {target}. Nothing was run."
            ) from None
    texts = [fetched[f].decode("utf-8", "replace") for f in _release_files(name)[1:]]
    try:
        verify_release_files(public_key, name, fetched[name], texts[0], texts[1], texts[2])
        binary = extract_binary(fetched[name], name)
    except VerifyError as e:
        raise WrapperError(f"{e}: refusing to run it. Nothing was run or cached.") from None

    staging = tempfile.mkdtemp(prefix=".tmp-", dir=root)
    try:
        for f in _release_files(name):
            _write(os.path.join(staging, f), fetched[f], 0o600)
        staged = os.path.join(staging, "wats")
        _write(staged, binary, 0o755)
        try:
            says = subprocess.run(
                [staged, "--version"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                timeout=30,
                check=False,
            ).stdout.decode("utf-8", "replace").strip()
        except (OSError, subprocess.SubprocessError):
            says = ""
        if says != f"wats {version}":
            raise WrapperError(
                f"the downloaded binary does not run here, or is not wats {version}"
                f" (it says: {says or 'nothing'}). Nothing was cached."
            )
        os.makedirs(os.path.join(root, version), mode=0o700, exist_ok=True)
        try:
            os.rename(staging, directory)
        except OSError:
            # Another start won the race: use its entry if it verifies.
            if not os.path.lexists(directory):
                raise
            try:
                return verify_cached(directory, name, public_key)
            except VerifyError as e:
                raise WrapperError(f"the cached wats does not verify: {e}") from None
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    log(f"verified wats {version} (minisign signatures and SHA-256) and cached it in {directory}")
    return os.path.join(directory, "wats")


def exec_wats(binary: str, args: list, env=None):
    """Replaces this process with wats (POSIX exec): stdio, signals and the
    exit status are wats's own. Elsewhere, runs it and exits with its
    status."""
    env = dict(os.environ if env is None else env)
    env["WATS_WRAPPER"] = "pypi"
    sys.stdout.flush()
    sys.stderr.flush()
    if os.name == "posix":
        os.execve(binary, [binary, *args], env)
    sys.exit(subprocess.call([binary, *args], env=env))


def main(args=None, **overrides) -> None:
    """The command: `uvx <package> mcp …` runs `wats mcp …`."""
    args = sys.argv[1:] if args is None else list(args)
    try:
        binary = ensure_binary(**overrides)
    except (WrapperError, OSError) as e:
        _say(str(e))
        sys.exit(1)
    try:
        exec_wats(binary, args, overrides.get("env"))
    except OSError as e:
        _say(f"cannot run {binary}: {e}")
        sys.exit(1)

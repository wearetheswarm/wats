"""Finds, downloads, verifies, caches and execs the wats release binary of
this package's version (ADR 0035).

Nothing is run before the archive's two minisign signatures, their trusted
file names and its SHA-256 have been checked against the release key
pinned in verify.py, and the cache is checked the same way at every start.
Messages go to stderr only: stdout belongs to the MCP host.
"""

from __future__ import annotations

import http.client
import json
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
from collections import namedtuple

from . import PACKAGE_NAME, __version__
from .verify import (
    RELEASE_PUBLIC_KEY,
    VerifyError,
    extract_binary,
    verify_release_files,
    verify_signature,
)

#: Where releases are downloaded from: <base>/v<version>/<file>.
RELEASE_BASE_URL = "https://github.com/wearetheswarm/wats/releases/download"
#: The latest release's files (its signed security.json, ADR 0030).
LATEST_URL = "https://github.com/wearetheswarm/wats/releases/latest/download"
#: The cache directory's name under the user cache dir, shared with the npm
#: wrapper (same layout, same checks).
CACHE_NAME = "wats-wrapper"
#: Overrides the cache root (an absolute path).
CACHE_ENV = "WATS_WRAPPER_CACHE_DIR"

MAX_ARCHIVE_BYTES = 256 * 1024 * 1024
MAX_SMALL_BYTES = 64 * 1024
TIMEOUT_SECS = 60
DEADLINE_SECS = 10 * 60
MAX_REDIRECTS = 5
#: Leftover staging and trash directories older than this are removed.
STALE_SECS = 3600
#: The security floor is looked up at most this often, within this time.
SECURITY_CHECK_EVERY_SECS = 24 * 3600
SECURITY_CHECK_DEADLINE_SECS = 5

POSIX = os.name == "posix"

#: A verified binary: its path, a descriptor open on it, and its identity.
Handle = namedtuple("Handle", "path fd dev ino")


class WrapperError(Exception):
    """A failure to report to the user; nothing was run."""


class UnsafeError(WrapperError):
    """A cache directory or file that must not be trusted."""


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


def _current_uid(uid):
    if uid is not None:
        return uid  # tests only
    return os.getuid() if hasattr(os, "getuid") else None


def _check_dir(path: str, uid) -> os.stat_result:
    """A directory of the cache must be a real directory (not a symlink),
    this user's, and not writable by group or others."""
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode):
        raise UnsafeError(f"{path} is a symlink")
    if not stat.S_ISDIR(st.st_mode):
        raise UnsafeError(f"{path} is not a directory")
    if uid is not None and st.st_uid != uid:
        raise UnsafeError(f"{path} belongs to another user")
    if POSIX and st.st_mode & 0o022:
        raise UnsafeError(f"{path} is writable by group or others")
    return st


def _remove(path: str, root: str) -> None:
    """Moves `path` out of the way (a rename inside the root, which this
    user owns), then deletes it."""
    trash = os.path.join(root, f".trash-{os.getpid()}-{time.time_ns()}")
    try:
        os.rename(path, trash)
    except OSError:
        if os.path.isdir(path) and not os.path.islink(path):
            shutil.rmtree(path, ignore_errors=True)
        else:
            try:
                os.unlink(path)
            except OSError:
                pass
        return
    if os.path.isdir(trash) and not os.path.islink(trash):
        shutil.rmtree(trash, ignore_errors=True)
    else:
        os.unlink(trash)


def _prepare_root(root_path: str, uid, log) -> str:
    """Creates or checks the cache root, and returns its real path. Its
    parent is resolved once (symlinks there are the user's own layout); the
    root itself must not be a symlink and must be this user's. A root that
    is writable by group or others may hold anything: it is emptied and set
    to 0700, and every release is downloaded again. Leftover staging and
    trash directories older than an hour are removed."""
    parent = os.path.dirname(root_path)
    os.makedirs(parent, mode=0o700, exist_ok=True)
    real_parent = os.path.realpath(parent)
    pst = os.stat(real_parent)
    if POSIX and pst.st_mode & 0o002 and not pst.st_mode & stat.S_ISVTX:
        raise UnsafeError(
            f"{real_parent} is writable by everyone without the sticky bit:"
            " refusing to keep the cache in it"
        )
    root = os.path.join(real_parent, os.path.basename(root_path))
    try:
        os.mkdir(root, 0o700)
    except FileExistsError:
        pass
    st = os.lstat(root)
    if stat.S_ISLNK(st.st_mode):
        raise UnsafeError(f"{root} is a symlink: refusing to use it as the cache")
    if not stat.S_ISDIR(st.st_mode):
        raise UnsafeError(f"{root} is not a directory")
    if uid is not None and st.st_uid != uid:
        raise UnsafeError(f"{root} belongs to another user: refusing to use it as the cache")
    if POSIX:
        if st.st_mode & 0o022:
            os.chmod(root, 0o700)
            log(f"{root} was writable by group or others: emptying it, every release will be downloaded again")
            for entry in os.listdir(root):
                _remove(os.path.join(root, entry), root)
            if os.listdir(root):
                raise UnsafeError(f"cannot empty {root}: remove it, then start again")
        elif st.st_mode & 0o077:
            os.chmod(root, 0o700)
        _check_dir(root, uid)
    now = time.time()
    for entry in os.listdir(root):
        if not (entry.startswith(".tmp-") or entry.startswith(".trash-")):
            continue
        p = os.path.join(root, entry)
        try:
            if now - os.lstat(p).st_mtime > STALE_SECS:
                shutil.rmtree(p, ignore_errors=True)
        except OSError:
            pass  # another start is cleaning it up
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


def _allowed(url: str, allow_loopback_http: bool) -> bool:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme == "https":
        return True
    return allow_loopback_http and parts.scheme == "http" and _is_loopback(parts.hostname or "")


class _NoRedirects(urllib.request.HTTPRedirectHandler):
    """Redirects are followed by download() itself, one hop at a time, so
    that each hop gets its own scheme and proxy decision."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def download(
    url: str,
    limit: int,
    env=None,
    cafile: str | None = None,
    allow_loopback_http: bool = False,
    deadline: float = DEADLINE_SECS,
) -> bytes:
    """GET `url`, at most `limit` bytes, following up to 5 redirects, within
    `deadline` seconds in all (each read also times out after 60 s).
    HTTPS only. HTTPS_PROXY and NO_PROXY are honoured, decided again at
    each hop. Test-only: `cafile` replaces the trusted certificates, and
    `allow_loopback_http` accepts plain http:// to 127.0.0.1."""
    env = os.environ if env is None else env
    end = time.monotonic() + deadline
    context = ssl.create_default_context(cafile=cafile)

    def remaining() -> float:
        left = end - time.monotonic()
        if left <= 0:
            raise WrapperError(f"the download of {url} took longer than {int(deadline)} s")
        return left

    current = url
    for hop in range(MAX_REDIRECTS + 1):
        if not _allowed(current, allow_loopback_http):
            raise WrapperError(f"refusing to download {current}: HTTPS only")
        proxy = proxy_for(current, env)
        opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({"https": proxy} if proxy else {}),
            urllib.request.HTTPSHandler(context=context),
            _NoRedirects(),
        )
        req = urllib.request.Request(
            current, headers={"User-Agent": "wats-pypi-wrapper", "Accept": "application/octet-stream"}
        )
        try:
            resp = opener.open(req, timeout=min(TIMEOUT_SECS, remaining()))
        except urllib.error.HTTPError as e:
            location = e.headers.get("Location") if e.headers else None
            e.close()
            if e.code in (301, 302, 303, 307, 308) and location:
                current = urllib.parse.urljoin(current, location)
                continue
            raise
        with resp:
            declared = resp.headers.get("Content-Length")
            if declared and declared.isdigit() and int(declared) > limit:
                raise WrapperError(f"{current} is larger than {limit} bytes")
            chunks, size = [], 0
            while True:
                remaining()
                chunk = resp.read1(1024 * 1024)
                if not chunk:
                    break
                size += len(chunk)
                if size > limit:
                    raise WrapperError(f"{current} is larger than {limit} bytes")
                chunks.append(chunk)
        return b"".join(chunks)
    raise WrapperError(f"too many redirects for {url}")


# --- The cache ---------------------------------------------------------------


def _read_limited(path: str, limit: int) -> bytes:
    st = os.lstat(path)
    if not stat.S_ISREG(st.st_mode):
        raise VerifyError(f"{os.path.basename(path)} is not a file")
    if st.st_size > limit:
        raise VerifyError(f"{os.path.basename(path)} is too large")
    with open(path, "rb") as f:
        return f.read()


def verify_cached(directory: str, name: str, public_key: str, uid=None) -> Handle:
    """Checks a cache entry as a fresh download: the directories from the
    root down (real, this user's, not writable by others), the signatures,
    file names and SHA-256, and that `wats` is byte for byte the archive's
    binary, read from the descriptor that is then executed (or compared
    with the path just before exec). The caller closes the handle's fd."""
    _check_dir(os.path.dirname(directory), uid)
    _check_dir(directory, uid)
    files = _release_files(name)
    archive = _read_limited(os.path.join(directory, files[0]), MAX_ARCHIVE_BYTES)
    texts = [
        _read_limited(os.path.join(directory, f), MAX_SMALL_BYTES).decode("utf-8", "replace")
        for f in files[1:]
    ]
    verify_release_files(public_key, name, archive, texts[0], texts[1], texts[2])
    expected = extract_binary(archive, name)
    binary = os.path.join(directory, "wats")
    fd = os.open(binary, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or not st.st_mode & stat.S_IXUSR:
            raise VerifyError("the cached wats is not an executable file")
        if uid is not None and st.st_uid != uid:
            raise UnsafeError("the cached wats belongs to another user")
        if POSIX and st.st_mode & 0o022:
            raise UnsafeError("the cached wats is writable by group or others")
        if st.st_size != len(expected):
            raise VerifyError("the cached wats differs from the signed archive")
        got = bytearray()
        os.lseek(fd, 0, os.SEEK_SET)
        while len(got) < len(expected):
            chunk = os.read(fd, len(expected) - len(got))
            if not chunk:
                break
            got += chunk
        if bytes(got) != expected:
            raise VerifyError("the cached wats differs from the signed archive")
        return Handle(binary, fd, st.st_dev, st.st_ino)
    except BaseException:
        os.close(fd)
        raise


def check_same(handle: Handle) -> None:
    """Just before exec: the path still names the verified file."""
    st = os.lstat(handle.path)
    fst = os.fstat(handle.fd)
    if not stat.S_ISREG(st.st_mode) or (st.st_dev, st.st_ino) != (handle.dev, handle.ino) or fst.st_ino != handle.ino:
        raise UnsafeError(f"{handle.path} changed after it was verified: refusing to run it")


def _write(path: str, data: bytes, mode: int) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
    finally:
        os.chmod(path, mode)


def ensure_verified(
    version: str = __version__,
    target: str | None = None,
    public_key: str = RELEASE_PUBLIC_KEY,
    base_url: str = RELEASE_BASE_URL,
    root: str | None = None,
    env=None,
    system: str | None = None,
    machine: str | None = None,
    log=_say,
    uid=None,
    allow_loopback_http: bool = False,
    **_ignored,
) -> Handle:
    """A verified wats binary of `version` for `target`, from the cache or
    downloaded into it, with a descriptor open on it. Raises WrapperError
    with a message for the user. All arguments but the tests' keep their
    defaults (`uid` and `allow_loopback_http` are for tests only)."""
    env = os.environ if env is None else env
    uid = _current_uid(uid)
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise WrapperError(f"this package's version {version} is not a wats release (X.Y.Z)")
    if target is None:
        system = system if system is not None else platform.system()
        machine = machine if machine is not None else platform.machine()
        target = release_target(system, machine)
        if target is None:
            raise WrapperError(unsupported_message(system, machine, version))

    root = _prepare_root(root or cache_root(env), uid, log)
    name = archive_name(version, target)
    version_dir = os.path.join(root, version)
    directory = os.path.join(version_dir, target)

    if os.path.lexists(directory) or os.path.lexists(version_dir):
        try:
            return verify_cached(directory, name, public_key, uid)
        except (VerifyError, OSError, WrapperError) as e:
            bad = directory
            try:
                _check_dir(version_dir, uid)
            except (OSError, WrapperError):
                bad = version_dir
            if os.path.lexists(bad):
                log(f"the cached wats {version} ({bad}) does not verify: {e}; downloading it again")
                _remove(bad, root)

    log(f"downloading wats {version} for {target}")
    fetched = {}
    for i, f in enumerate(_release_files(name)):
        url = f"{base_url}/v{version}/{f}"
        try:
            fetched[f] = download(
                url,
                MAX_ARCHIVE_BYTES if i == 0 else MAX_SMALL_BYTES,
                env,
                allow_loopback_http=allow_loopback_http,
            )
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
        if os.path.lexists(version_dir):
            try:
                _check_dir(version_dir, uid)
            except (OSError, WrapperError):
                _remove(version_dir, root)
        try:
            os.mkdir(version_dir, 0o700)
        except FileExistsError:
            pass
        try:
            os.rename(staging, directory)
        except OSError:
            # Another start won the race: its entry is checked below.
            if not os.path.lexists(directory):
                raise
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    # Checked again where it now lives, and opened for exec.
    try:
        handle = verify_cached(directory, name, public_key, uid)
    except VerifyError as e:
        raise WrapperError(f"the cached wats does not verify: {e}") from None
    log(f"verified wats {version} (minisign signatures and SHA-256) and cached it in {directory}")
    return handle


def ensure_binary(**kwargs) -> str:
    """The path of a verified binary (ensure_verified, descriptor closed)."""
    handle = ensure_verified(**kwargs)
    os.close(handle.fd)
    return handle.path


def _parse_version(v):
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", str(v))
    return tuple(int(x) for x in m.groups()) if m else None


def security_warning(
    root: str,
    version: str = __version__,
    public_key: str = RELEASE_PUBLIC_KEY,
    latest_url: str = LATEST_URL,
    env=None,
    allow_loopback_http: bool = False,
    now: float | None = None,
    **_ignored,
) -> str | None:
    """The latest release's signed security floor (security.json, ADR
    0030), looked up at most once a day within 5 s, best effort: any
    failure is silent. Returns a warning when `version` is below the floor,
    else None. Never stops wats from running: the version is pinned on
    purpose."""
    env = os.environ if env is None else env
    try:
        path = os.path.join(root, "security.json")
        sig = path + ".minisig"
        stamp = os.path.join(root, ".security-checked")
        now = time.time() if now is None else now
        try:
            due = now - os.lstat(stamp).st_mtime > SECURITY_CHECK_EVERY_SECS
        except OSError:
            due = True
        if due:
            with open(stamp, "w", encoding="utf-8"):
                pass
            try:
                def get(f):
                    return download(
                        f"{latest_url}/{f}",
                        MAX_SMALL_BYTES,
                        env,
                        allow_loopback_http=allow_loopback_http,
                        deadline=SECURITY_CHECK_DEADLINE_SECS,
                    )

                data, signature = get("security.json"), get("security.json.minisig")
                verify_signature(public_key, data, signature.decode("utf-8", "replace"), "security.json")
                for target, content in ((path, data), (sig, signature)):
                    tmp = f"{target}.tmp-{os.getpid()}"
                    _write(tmp, content, 0o600)
                    os.replace(tmp, target)
            except Exception:  # noqa: BLE001 (best effort: offline, rate-limited, none)
                pass
        data = _read_limited(path, MAX_SMALL_BYTES)
        signature = _read_limited(sig, MAX_SMALL_BYTES).decode("utf-8", "replace")
        verify_signature(public_key, data, signature, "security.json")
        info = json.loads(data.decode("utf-8"))
        floor = _parse_version(info.get("security_floor"))
        latest = _parse_version(info.get("version"))
        ours = _parse_version(version)
        if not floor or not latest or not ours or latest < floor or ours >= floor:
            return None
        return (
            f"SECURITY: wats {version} is older than {info['security_floor']}, the security floor"
            f" of the latest release (wats {info['version']}): later releases fix security issues."
            f" Use {PACKAGE_NAME}@{info['version']} (or at least {info['security_floor']})."
            f" Running {version} anyway, as asked."
        )
    except Exception:  # noqa: BLE001
        return None


def exec_wats(handle: Handle, args: list, env=None):
    """Replaces this process with wats (POSIX exec): stdio, signals and the
    exit status are wats's own. On Linux the verified descriptor itself is
    executed (fexecve); elsewhere the path, once checked to still name the
    verified file. Without exec (Windows), runs it and exits with its
    status."""
    env = dict(os.environ if env is None else env)
    env["WATS_WRAPPER"] = "pypi"
    check_same(handle)
    sys.stdout.flush()
    sys.stderr.flush()
    argv = [handle.path, *args]
    if POSIX:
        if sys.platform.startswith("linux") and os.execve in os.supports_fd:
            # Inheritable, so that an interpreter (#!) can still read it.
            os.set_inheritable(handle.fd, True)
            os.execve(handle.fd, argv, env)
        os.execve(handle.path, argv, env)
    os.close(handle.fd)
    sys.exit(subprocess.call(argv, env=env))


def main(args=None, **overrides) -> None:
    """The command: `uvx <package> mcp …` runs `wats mcp …`."""
    args = sys.argv[1:] if args is None else list(args)
    log = overrides.get("log", _say)
    try:
        handle = ensure_verified(**overrides)
        root = os.path.dirname(os.path.dirname(os.path.dirname(handle.path)))
        warning = security_warning(root, **{k: v for k, v in overrides.items() if k != "root"})
        if warning:
            log(warning)
        exec_wats(handle, args, overrides.get("env"))
    except (WrapperError, OSError) as e:
        _say(str(e))
        sys.exit(1)

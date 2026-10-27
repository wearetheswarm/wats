# 0035. npx and uvx wrappers of the signed release binary

Status: Proposed (2026-10-22). Built and tested; published from 0.5.8
(2026-10-27) as `@wearetheswarm/wats` on npm and `wats` on PyPI, by the
release workflow ("Publishing" below). Roadmap item D18, before the MCP
registry listing.

Code: `packages/npm/` (`lib/verify.js`, `lib/fetch.js`, `lib/launcher.js`,
`bin/wats.js`), `packages/pypi/` (`src/wats_wrapper/verify.py`,
`launcher.py`, `ed25519_zip215.py`), the signed test fixtures in
`packages/fixtures/` (`make-fixtures.sh`), `scripts/wrapper-versions.sh`,
the "npm and PyPI wrappers" step of `.github/workflows/ci.yml`, and the
`npm` and `pypi` jobs of `.github/workflows/release.yml`.

## Context

SPEC §4 plans two thin wrappers that download the binary, "PyPI (`uvx
wats`, the ruff model) and npm (`npx wats`)". MCP hosts and the MCP
registry expect a server to start with a package runner:

```json
{ "command": "npx", "args": ["-y", "<package>", "mcp"] }
{ "command": "uvx", "args": ["<package>", "mcp"] }
```

Today a user installs wats first, with `install.sh` (which needs
minisign), Homebrew, or by hand, then registers `wats mcp`. Releases are
GitHub releases: `wats-X.Y.Z-<target>.tar.gz` for
`x86_64-unknown-linux-musl` and `aarch64-apple-darwin`, with a `.sha256`
and a minisign `.minisig` for each (docs/release-signing.md). `wats
upgrade` refuses an archive unless both signatures verify with the key
compiled in (`RELEASE_PUBLIC_KEY`), each trusted comment names its file,
and the SHA-256 matches (`crates/wats/src/update.rs`: `fetch_verified`,
`verify_release_files`). A wrapper that runs a binary must not be the weak
link of that chain.

## Decision

### 1. Two packages, their names

| | npm | PyPI |
|---|---|---|
| Directory | `packages/npm/` | `packages/pypi/` |
| Name | `@wearetheswarm/wats` | `wats` |
| Where the name lives | `name` in `package.json` (the code reads it from there) | `name` in `pyproject.toml`, `PACKAGE_NAME` in `__init__.py` (a test keeps them and the script name equal) |
| Command | `wats` (the only `bin`, which `npx` runs) | `wats` (the only script, which `uvx wats` runs) |
| Runtime dependencies | none (Node ≥ 18) | none (Python ≥ 3.9) |
| Build | none (`npm pack`) | `flit_core==3.12.0` (build time only) |

The product owner chose the names (2026-10-27): plain `wats` is taken on
npm, so the scoped `@wearetheswarm/wats` (which also keeps a typosquatted
name from looking official); on PyPI, `wats` was free, so `uvx wats mcp`
works as SPEC §4 says. (Until 0.5.8 the PyPI name was a placeholder,
`wearetheswarm-wats`, never published.) A rename is the one `name` (plus
`PACKAGE_NAME` and the script on PyPI) and the docs' snippets.

The PyPI wrapper's own stderr lines start with `wats (PyPI wrapper):`,
not `wats:`, so they are not taken for wats's own messages; the npm
wrapper's start with the package name.

`packages/` sits outside the Cargo workspace, so Rust builds ignore it.

### 2. The package version is the wats version

A wrapper of version X.Y.Z runs wats X.Y.Z, from the GitHub release
`vX.Y.Z`, and nothing else: there is no "latest" lookup, so a wrapper
version is reproducible and the release it runs is the one its version
names. The versions live in `packages/npm/package.json` and
`packages/pypi/src/wats_wrapper/__init__.py` (`__version__`, which flit
reads).

- `sh scripts/wrapper-versions.sh set` writes Cargo.toml's
  `[workspace.package]` version into both; `check` fails when either
  differs. CI runs `check` on every pull request and push, so a
  `chore(release)` PR that bumps Cargo.toml without the wrappers fails.
- The release checklist (CONTRIBUTING.md, "Cutting a release") lists it
  next to the other version bumps (Cargo.toml and Cargo.lock, the image
  tags of `deploy/` and `docs/docker.md`, the bug report template, the
  CHANGELOG).
- Each package's tests also compare its version with Cargo.toml's.
- The `Version` step of `release.yml` runs `sh scripts/wrapper-versions.sh
  check` too (0.5.8), so a release built from an unchecked commit stops
  before signing; the publishing jobs check it again, and that the
  version is the tag's.

A wrapper-only fix ships as the next wats release, not as a wrapper
`X.Y.Z-1`: the version must stay a release version (the wrapper refuses
any other).

### 3. Verification, exactly as `wats upgrade`

Both wrappers port `verify_signature`, `verify_sha256`,
`verify_release_files` and `extract_binary` of `update.rs`, with the same
order, the same refusals, and the same messages:

1. Download `wats-X.Y.Z-<target>.tar.gz`, `.tar.gz.minisig`,
   `.tar.gz.sha256` and `.tar.gz.sha256.minisig`.
2. For the archive and for the `.sha256` file, the minisign signature:
   - the public key is `base64("Ed" ‖ key id 8 ‖ Ed25519 key 32)`;
   - the signature file is four lines: an untrusted comment (ignored),
     `base64(alg 2 ‖ key id 8 ‖ signature 64)`, `trusted comment: …`, and
     `base64(global signature 64)`, as minisign-verify reads them
     (`\r\n` accepted, strict base64);
   - the algorithm must be `ED` (prehashed): minisign 0.12, which the
     release workflow pins, signs prehashed unless given `-l`, the
     workflow passes no `-l`, and `update.rs` calls `verify(…,
     allow_legacy = false)`. A legacy `Ed` signature (`minisign -S -l`)
     is refused, as wats refuses it;
   - the key ids must match;
   - Ed25519 over BLAKE2b-512 of the file must verify, then Ed25519 of
     `signature ‖ trusted comment` (the global signature), so the trusted
     comment is authenticated;
   - a whitespace-separated token of the trusted comment must be
     `file:<the expected file name>`: a signed archive of another version
     served under this name is refused.
3. The first line of the `.sha256` must name the archive (`*` prefix
   allowed) and its hash must be the archive's SHA-256.
4. The binary is the first regular file whose path is exactly
   `wats-X.Y.Z-<target>/wats`, at most 512 MiB and not empty. Decoys at
   other paths, pax and GNU long-name headers are handled.
5. Before the binary enters the cache, it must answer `--version` with
   `wats X.Y.Z`, as `install.sh` checks. It is signed by then: this only
   catches a release packaged with the wrong binary, or a binary that
   cannot run here.

**Node.** `crypto.verify(null, …)` with an Ed25519 key built from the raw
32 bytes (an SPKI DER prefix) and `crypto.createHash('blake2b512')`, both
in Node's bundled OpenSSL since well before Node 18. The tar reader is a
hundred lines (`zlib.gunzipSync` with an output cap, ustar, pax `path`,
GNU `L`, header checksums). No native module, no dependency.

**Python.** `hashlib.blake2b(digest_size=64)` and `hashlib.sha256` from
the standard library, and `tarfile`. The standard library has no Ed25519,
and we vendor a pure-Python verifier rather than depend on `cryptography`
or `PyNaCl`:

- the repository already has one: `tools/verify-py/ed25519_zip215.py`,
  the second implementation of the event format (ADR 0010), checked
  against the published vectors and differential-tested against the Rust
  verifier. `ed25519_zip215.py` in the package is a byte-for-byte copy,
  and a test fails when the two differ;
- a dependency would be a native wheel (`cryptography` is several MB and
  pulls `cffi`; `PyNaCl` bundles libsodium), downloaded by every `uvx`
  cold start and trusted with the one check that matters. Pinning it
  would not remove that trust, only fix it to one version; the vendored
  file is about 160 lines anyone can read;
- speed is not an issue: four verifications take a few milliseconds.
  The whole check of a cached 33 MB binary (BLAKE2b and SHA-256 of the
  archive, gunzip, compare) takes about 70 ms in either language;
- ZIP 215 is more permissive than RFC 8032 on encodings (non-canonical
  points, small-order keys), which matters for consensus between
  verifiers, not here: the key is pinned and a forgery is as hard either
  way. Not constant time, which does not matter for public data.

**Checked against the real format.** Both wrappers verify the Rust
updater's own fixtures (`crates/wats/tests/fixtures/upgrade`, which
`update.rs` and `install.sh` are tested with: prehashed `ED` signatures,
trusted comment `timestamp:…<TAB>file:…<TAB>hashed`, as minisign 0.12
writes for the releases), including their wrong-file and other-key cases,
and new fixtures made by minisign 0.12 with a throwaway key (§9), which
`minisign -V` itself checks before they are written. Real release assets
were not downloaded for this change.

### 4. The pinned key

Each wrapper has the release key as a constant
(`RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci`). Tests in
both suites read `RELEASE_PUBLIC_KEY` out of `update.rs` (and
`install.sh`'s `PUBKEY`) and fail when they differ, so a key rotation
(docs/release-signing.md) cannot forget the wrappers. The key, the
download URLs, plain HTTP to loopback and the user id the cache checks
use can be overridden only through options of the library functions the
tests call (`ensureVerified(opts)`, `ensure_verified(**opts)`); the
commands `npx`/`uvx` run read none of them from the environment, so an
MCP host's `env` cannot swap the key.

### 5. Platforms

The targets of `release.yml`: Linux x86_64 (any libc: the binary is
static musl) and macOS arm64. Anything else (Linux arm64, macOS x86_64,
Windows) fails before any download with how to install otherwise: `cargo
install --locked --git https://github.com/wearetheswarm/wats --tag
vX.Y.Z wats`, then register `wats mcp` directly; WSL on Windows; an arm64
runtime on an Apple silicon Mac whose Node or Python runs under Rosetta.
A test checks the mapping against the matrix of `release.yml`. A new
release target is one line in each `releaseTarget`/`release_target`.

### 6. Downloads

- From `https://github.com/wearetheswarm/wats/releases/download/vX.Y.Z/<file>`,
  following up to 5 redirects (GitHub serves assets from its CDN). HTTPS
  only, at every hop. Plain HTTP to a loopback host is accepted only
  through an option of the library functions that the tests pass
  (`allowLoopbackHttp`, `allow_loopback_http`); the commands never set
  it. TLS uses the runtime's certificate store.
- No token is ever sent: release downloads need none, and GitHub's API
  (and its rate limits) is not used.
- `HTTPS_PROXY` (or `https_proxy`) and `NO_PROXY` are honoured, decided
  again for each redirect hop (both wrappers follow redirects themselves,
  one hop at a time). Python's urllib tunnels through the proxy itself;
  Node's `https` does not, so `fetch.js` opens an HTTP `CONNECT` tunnel
  (with `user:password@` as Basic proxy auth) and runs TLS to GitHub
  inside it. Only `http://` proxies.
- Limits as in `update.rs`: 256 MiB for the archive, 64 KiB for the
  small files. 60 s without data aborts a hop, and a whole download
  (redirects included) has 10 minutes. Node enforces the deadline with a
  timer that destroys the request; Python checks it between reads, which
  return what has arrived, so it can overrun by at most one 60 s read
  timeout.
- Any failure says what failed, suggests the connection or the proxy, and
  says that nothing was run.

### 7. The cache

- **Where:** `$WATS_WRAPPER_CACHE_DIR` (absolute), else
  `~/Library/Caches/wats-wrapper` on macOS, `$XDG_CACHE_HOME/wats-wrapper`
  (default `~/.cache/wats-wrapper`) elsewhere. Both wrappers use the same
  root and layout, and each checks what the other wrote.
- **Layout:** `<root>/<version>/<target>/` holds the four release files
  and the extracted `wats` (0755; the others 0600), plus, in the root, the
  latest `security.json` (§8a) and a `.security-checked` stamp.
- **The root.** Its parent is resolved once (`realpath`): symlinks above
  the root are the user's own layout, and from then on only the resolved
  path is used. A parent writable by everyone without the sticky bit is
  refused. The root itself is `lstat`ed: a symlink, a non-directory, or a
  directory of another user is refused (the start fails closed). With
  `WATS_WRAPPER_CACHE_DIR=/tmp/x`, another user who created `/tmp/x` (or
  a symlink there) first is therefore refused, not followed.
- **A root that is writable by group or others** may hold anything
  another user put there. It is set to 0700 first (nobody else can add an
  entry from then on), then emptied, and every release is downloaded
  again; it is never just `chmod`ed and trusted. A root that is only
  group- or world-readable is set to 0700 and kept.
- **Every directory down to the binary** (`<version>`, `<target>`) must
  be a real directory (not a symlink), this user's, and not writable by
  group or others, and the binary a regular file of this user, not
  writable by others. An entry that fails is moved aside (a rename inside
  the root, which this user owns, so it works even for a directory of
  another user) and downloaded again. When `<version>` is the culprit, all
  of it goes.
- **Atomic:** files are written (and fsynced) into
  `<root>/.tmp-XXXXXX/` (0700), checked, then the directory is renamed to
  `<version>/<target>`. A start never sees a half-written entry. When two
  starts race, the second rename fails, and the second uses the first's
  entry once it verifies. A fresh entry is verified again where it ends
  up, like a cache hit, which also opens it for exec (§8).
- **Checked at every start:** a cache hit is verified as a download is:
  both signatures, their file names, the SHA-256, then the binary
  extracted again from the archive and compared byte for byte with the
  cached `wats`, read through the descriptor that is then executed. No
  network is needed, so a cached version works offline. The cache is not
  a trust anchor: anyone who can write it as this user can also write the
  wrapper itself, but this catches corruption, a partial copy, and a
  `wats` replaced in place (for instance by `wats upgrade`, below).
- **A cache entry that fails** is moved aside and removed, then downloaded
  again; offline, the start fails closed and says why.
- **Leftovers:** `.tmp-*` and `.trash-*` entries older than an hour (a
  start killed mid-download) are removed at each start; younger ones may
  belong to a start in progress.
- **Old versions are kept.** A running background node may still use an
  older entry's binary, and a host config may name it (§8). Clearing the
  cache is `rm -rf` of the root; a pruning policy can come later.

### 8. Running wats

- **Between verification and exec.** The binary is verified through an
  open descriptor (`O_NOFOLLOW`), and that same file is what runs:
  - **Linux:** the descriptor itself is executed. The PyPI wrapper calls
    `os.execve(fd, …)` (fexecve); the npm wrapper passes the descriptor
    as the child's fd 3 and spawns `/proc/self/fd/3`, with `argv[0]` set
    to the cache path. Whatever happens to the path afterwards, the
    verified inode runs. wats inherits that read-only descriptor (fd 3
    under npm; under PyPI it is made inheritable, as an interpreter
    needs it for `#!` scripts), which it does not use.
  - **macOS** has no fexecve. Just before exec, the path is `lstat`ed and
    must still be the verified inode (same device and inode as the open
    descriptor), then the path is executed. The window left is the few
    instructions between that check and `execve`, inside directories
    that only this user can write (§7), so only a process of the same
    user, which could rewrite the wrapper anyway, could use it.
  - A private copy in a fresh temp directory was not taken: it would
    copy 30+ MB at each start, and the background node that `wats mcp`
    starts would run from a path that goes away.
- **npm:** `child_process.spawn` with stdio inherited. SIGHUP, SIGINT,
  SIGQUIT, SIGTERM and SIGUSR2 are forwarded to wats (not SIGUSR1, Node's
  debugger signal). The wrapper exits with wats's status; when wats dies
  of a signal, the wrapper kills itself with the same signal. A
  terminal's Ctrl-C reaches wats twice (from the terminal and forwarded);
  `wats mcp` is not run from a terminal in practice.
- **PyPI:** `os.execve`: the Python process becomes wats, so stdio,
  signals and the exit status are wats's own. (Windows would run it as a
  child and exit with its status, but has no release yet.)
- **stdout is the MCP channel:** the wrappers write only to stderr
  (`<package>: downloading…`, `verified …`, the security warning,
  errors), and only on a download, a failure, a bad cache, or a version
  below the security floor. A cache hit prints nothing otherwise.
- **`WATS_WRAPPER=npm|pypi`** is set in wats's environment. wats ignores
  it today; it is there for the follow-ups below.
- **Arguments pass through verbatim.** The wrappers have no options of
  their own, so `npx -y <package> mcp --profile alice` is `wats mcp
  --profile alice`, and any other subcommand works the same way.
- **The wrappers never write an MCP host's configuration.** Registering
  is the host's own config (the snippet in its docs), or `wats setup …`,
  which writes the path of the binary it runs as (ADR 0032).

### 8a. A pinned version below the security floor

A wrapper version is pinned on purpose, so it keeps running a release
that a later security release fixed. To say so, at most once a day each
start fetches the latest release's `security.json` and its `.minisig`
(`releases/latest/download/…`, ADR 0030), within 5 s, best effort: any
failure (offline, rate limit, a release without one) is silent and never
delays a start more than that. The signature must verify with the
release key and name `file:security.json`, as in `update.rs`; the
verified file is kept in the root and verified again at each start. When
this wrapper's version is below the floor, the start prints a loud
`SECURITY: wats X.Y.Z is older than F, the security floor of the latest
release (wats L) …` line on stderr naming the release to move to, and
still runs the pinned version.

Unlike `update.rs`, the wrapper cannot check that the file is the latest
release's own (it does not ask the API which release is the latest), so
a replayed older `security.json` can lower the floor and hide the
warning. It can never stop wats from running or make it run anything
else; it is advice only.

### 9. Tests

Offline only: a local HTTP server on 127.0.0.1 (with a redirecting path)
serves fixture mirrors laid out like a release; nothing reaches
github.com. The fixtures (`packages/fixtures/`, regenerated by
`make-fixtures.sh` with minisign 0.12) are signed with a throwaway key
whose secret is deleted at the end of the script, as for
`crates/wats/tests/fixtures/`. The "binary" is a shell script that prints
`wats 9.9.9`, echoes its arguments, copies stdin, exits with a given
status, or waits for SIGTERM. Caches are throwaway directories; no test
touches `~/.wats`, the real cache, or any agent config.

Both suites (`node --test`, Python's `unittest`) cover:

- a good release downloaded, verified, cached with its modes and run; a
  cache hit with the server gone; a corrupt cache downloaded again, and
  refused offline;
- refusals: a signature by another key; a tampered archive or trusted
  comment; a trusted comment naming another file; a legacy signature; a
  signed `.sha256` that does not match; a binary of another version; a
  version that is not X.Y.Z; a missing release; an unsupported platform;
- the cache's safety: a symlinked root and a root of another user
  refused; a root writable by others emptied and everything downloaded
  again; a group-readable root set to 0700 and kept; a loose or
  symlinked entry directory downloaded again; an entry of another user
  that does not verify; a binary swapped after verification not run;
  stale `.tmp-*`/`.trash-*` removed (another user is simulated by the
  tests' `uid` option, since tests cannot `chown`);
- the security floor: a version below it warned about, above it or badly
  signed not, no file at all not, and at most one lookup a day;
- downloads: plain http refused without the tests' option, the overall
  deadline, a proxy decision per redirect hop (Python), proxy selection,
  and a download through a local `CONNECT` proxy (with proxy auth) to a
  local HTTPS server whose throwaway certificate is made by the openssl
  CLI at test time (skipped without it), the certificate still checked
  inside the tunnel;
- the Rust fixtures; the key against `update.rs` and `install.sh`; the
  version against Cargo.toml; the targets against `release.yml`;
- through the command: arguments, stdin and stdout passed through, the
  exit status, a failure that prints to stderr only and runs nothing,
  and SIGTERM reaching wats. On Linux (CI) this runs the descriptor-exec
  path, on macOS the inode check.

CI runs them on the Linux leg. The PyPI build backend is pinned
(`flit_core==3.12.0`).

## Homebrew, Windows, and wats's own update paths

- **Homebrew** stays the recommended install for people: it puts `wats`
  on the `PATH`, and `brew upgrade` keeps it current. The wrappers are for
  hosts that start a server with a package runner, and for trying wats
  without installing it. Both can coexist: they are separate binaries.
- **Windows** has no release yet (`release.yml`: "Windows native comes
  once the node supports it"); the wrappers refuse it with the WSL hint.
  The cache path (`%LOCALAPPDATA%\wats-wrapper`) and the non-exec path of
  the PyPI wrapper are in place for when it ships.
- **`wats upgrade` in a wrapper cache** would replace the cached binary;
  the next start sees that it differs from the signed archive and puts
  the package's version back. The wrapper's version is what decides.
  Follow-up: wats could read `WATS_WRAPPER` and answer `wats upgrade`
  (and `update.auto`) with "update the npm/PyPI package", as it does for
  Homebrew.
- **`wats setup <host> --mcp` run through a wrapper** writes the path of
  the cached binary (`…/wats-wrapper/X.Y.Z/<target>/wats`), which is not
  stable across wrapper versions (the old entry stays, but it is old).
  Until wats knows about wrappers, the docs give the host snippet with
  `npx`/`uvx` instead. Follow-up: with `WATS_WRAPPER` set, `wats setup`
  could write the `npx`/`uvx` command, and ADR 0032's stale-path check
  could treat a wrapper cache path as stale.
- **The background node** that `wats mcp` starts (ADR 0006, 0023) runs
  the cached binary. A newer wrapper version starts sessions of the new
  version against a node of the old one until that node restarts, as
  after `wats upgrade`.

## Publishing (0.5.8)

From 0.5.8, `release.yml` publishes both wrappers once a release's
archives are attached, with **trusted publishing**: the registry trusts an
OIDC token that GitHub issues to that job, for this repository, this
workflow file and this environment. No npm or PyPI token is stored
anywhere.

- **Two jobs, `npm` and `pypi`**, each `needs: build` (so a wrapper is
  never published before the release it downloads), each with its own
  environment (`npm`, `pypi`) and `id-token: write`, on GitHub-hosted
  runners. Prereleases and `model-` tags are left out, as for the tap. One
  failing does not stop the other.
- **Each job checks before it publishes:** `wrapper-versions.sh check`,
  the package version against the tag, the package's offline tests; then
  it packs (npm: `npm pack`; PyPI: `build==1.6.1` with the pinned
  `flit_core`, sdist and wheel, `LICENSE` copied in) and **runs the packed
  package against the release just attached**: it downloads the Linux
  archive, verifies both signatures and the SHA-256 with the pinned key,
  and must print `wats X.Y.Z` for `--version`. A wrapper that cannot run
  its release is not published.
- **npm:** Node 24 (`actions/setup-node`), npm 11.5.1 or later (trusted
  publishing; upgraded to a pinned version when the runner's is older),
  `npm publish --access public`, which adds a provenance attestation (the
  repository is public). A version already on npm is skipped.
- **PyPI:** `pypa/gh-action-pypi-publish` (pinned by SHA), which also
  uploads attestations; `skip-existing: true`.
- **A rerun is harmless:** a version already published is left as it is.
  A wrapper fix ships as the next release (§2).

**Bootstrap, once.** PyPI takes a *pending publisher*: the project `wats`,
owner `wearetheswarm`, repository `wats`, workflow `release.yml`,
environment `pypi`; the first publish creates the project. A pending
publisher does not reserve the name, so the release that uses it should
follow soon. npm trusted publishing needs the package to exist first
(`npm trust` and the package settings page both require it), so the
first version is published by hand, from the tag of a release that exists
(`npm publish --access public` in `packages/npm`, with 2FA, `LICENSE`
copied in, after a dry run), then the trusted publisher is added (owner
`wearetheswarm`, repository `wats`, workflow `release.yml`, environment
`npm`), and every later version comes from the workflow. A by-hand
version has no provenance.

**After a release**, the operators' release script also checks that both
registries have the version and that `npx -y @wearetheswarm/wats@X.Y.Z
--version` and `uvx wats@X.Y.Z --version` print it, in throwaway caches.

**Then:** list the server in the MCP registry with the `npx` command
(and `uvx` as an alternative).

## Alternatives not taken

- **Binaries inside the packages** (esbuild's per-platform optional
  dependencies, or ruff's platform wheels built with maturin). No
  download at run time, but the trust moves from the release key to the
  registry and to whoever can publish there, every release publishes
  several large packages, and the minisign chain would have to be redone
  inside each. Possible later, with the same verification at build time.
- **A `postinstall` download.** Skipped by `--ignore-scripts`, not run by
  every runner, and it would run before any argument is known. The
  download happens at the first start instead, and is cached.
- **Following "latest"**: a wrapper version would then mean nothing, and
  a cache hit would need the network to know it is current.
- **Shelling out to `minisign`**: `install.sh` does, but MCP hosts'
  machines rarely have it, and the point is to need nothing but Node or
  Python.

# Release signing

Release archives are signed with [minisign](https://jedisct1.github.io/minisign/).
Each release asset `wats-X.Y.Z-<target>.tar.gz` comes with:

- `wats-X.Y.Z-<target>.tar.gz.sha256`: its SHA-256 checksum;
- `wats-X.Y.Z-<target>.tar.gz.minisig`: the minisign signature of the archive;
- `wats-X.Y.Z-<target>.tar.gz.sha256.minisig`: the signature of the checksum
  file.

Do not trust a key that is not published in this repository.

## Public key

```text
RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
```

The same key is quoted in the README Quickstart. The release workflow
refuses to sign with a key that does not match the one published in both
places.

## Installing with install.sh

[`install.sh`](../install.sh) does the checks below for you. It downloads
the archive for this machine, verifies both signatures with the key pinned
in the script, checks the trusted comments' file names and the SHA-256,
runs the binary once, then installs it into `~/.local/bin` (or
`--prefix`). Any failure stops it before anything is installed, and it
never uses sudo. Download it, read it, then run it:

```sh
curl -fsSLO https://github.com/wearetheswarm/wats/releases/latest/download/install.sh
curl -fsSLO https://github.com/wearetheswarm/wats/releases/latest/download/install.sh.minisig
minisign -Vm install.sh -P RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
less install.sh
sh install.sh
```

Each release attaches its `install.sh`, signed like the archives (its
trusted comment names `file:install.sh`), so the URL above always gives
the latest release's installer.

Piping it into a shell (`curl -fsSL … | sh`) works too, if you accept
running it unread. `GH_TOKEN` is optional: it only raises GitHub's rate
limits.

## Installing with Homebrew

```sh
brew install wearetheswarm/tap/wats
brew upgrade wats        # later; `wats upgrade` defers to it
```

The formula in [wearetheswarm/homebrew-tap](https://github.com/wearetheswarm/homebrew-tap)
pins each archive of the release by URL and SHA-256. The release workflow
writes it: once both archives are attached, it downloads their `.sha256`
files and their signatures, verifies them with the key pinned in
`install.sh` (and that each trusted comment names its file), renders the
formula (`scripts/homebrew-release.sh`), and pushes it to the tap over SSH
with the tap's deploy key (the `HOMEBREW_TAP_DEPLOY_KEY` secret, write
access to the tap only) and GitHub's pinned host key. Homebrew then checks
the SHA-256 of what it downloads. Prereleases are not published to the tap.

A Homebrew install is upgraded by Homebrew: `wats upgrade` says so instead
of replacing the binary, `update.auto` leaves it alone, and the update
notice names `brew upgrade wats`.

## Verifying a release

Install minisign (`brew install minisign`, `apt install minisign`, or a
release from https://github.com/jedisct1/minisign/releases), then:

```sh
gh release download vX.Y.Z -R wearetheswarm/wats -p '*linux-musl*'   # or '*apple-darwin*'
minisign -Vm wats-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -P RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
shasum -a 256 -c wats-X.Y.Z-x86_64-unknown-linux-musl.tar.gz.sha256
```

On Linux arm64 (from 0.5.9) the archive is
`wats-X.Y.Z-aarch64-unknown-linux-musl.tar.gz`, signed the same way.

`minisign -V` prints `Signature and comment signature verified` and the
trusted comment, which holds the signing time and the file name
(`file:wats-X.Y.Z-<target>.tar.gz`). Check that the file name and version are
the ones you asked for: the trusted comment is signed too, so this rules out
an older signed archive served under a new name.

The `.sha256` alone only detects a corrupted download: whoever can replace
the archive can replace its checksum. The signature is what ties the archive
to the project's key. If you only use the checksum (for instance in a
script that pins it), verify the checksum file's own signature first:

```sh
minisign -Vm wats-X.Y.Z-<target>.tar.gz.sha256 -P RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
```

## Releases and upgrades

`wats upgrade` does the checks above by itself: it looks up the latest
release (or `--version X.Y.Z`), downloads the archive of its platform with
the `.sha256` and both `.minisig`, and refuses to install unless both
signatures verify with the public key compiled into the binary (the one
above), each trusted comment names the expected file
(`file:wats-X.Y.Z-<target>.tar.gz`, `….sha256`), and the SHA-256 matches.
It then writes the new binary next to the current one, checks that it runs
(`--version`), renames it over the current one, and keeps the previous one
as `<binary>.old` (`wats upgrade --rollback` puts it back). Before putting
an older binary back, `--rollback` asks it its version and refuses while
`config.toml` sets keys that version does not know (an older wats refuses
unknown keys), printing the `wats config unset …` commands to run first
(ADR 0030). A running node
keeps the old version until it is restarted; `wats upgrade` says so.
`wats upgrade --check` only reports.

Releases are looked up through the GitHub API
(`https://api.github.com/repos/wearetheswarm/wats`), without a token.
`GH_TOKEN` or `GITHUB_TOKEN`, when set, raises GitHub's rate limits; it is
only sent to api.github.com. `update.url` (or `WATS_UPDATE_URL`) points to a mirror
serving the same JSON (`/releases/latest`, `/releases/tags/vX.Y.Z`, assets
with a `browser_download_url`). Drafts and prereleases are never offered.

The node checks the latest release at most once a day (`update.check`,
default on; never with `--hermetic` or `--loopback`), caches the answer in
`<data dir>/update.json`, logs a newer version once, and reports it in the
MCP `status` tool (`update: {current, latest, available, security,
checked_at, upgrade}`, `upgrade` being the command that installs it) so
that agents can tell their user. With `update.auto = true`
(off by default) the node installs a release by itself, with the same
checks, once it is `update.delay` old (7 days after its `published_at`), and
logs that a restart is needed: it never restarts itself, the daemon picks
the new binary up at its next start.

**Marking a security release.** A release counts as a security release
for a node older than its signed security floor (`security.json`, below).
Without a valid floor, it is one when its name or notes contain
`[security]` (any case), or when a line of its notes starts with
`Security:`, for example:

```text
Security: fixes a crash on a malformed sync page (GHSA-…).
```

The node then flags the update as `security` in its log and in `status`,
with a louder notice ([ADR 0030](adr/0030-security-updates.md)): a
`SECURITY RELEASE: …` line naming the version and the command that
installs it, in `wats status`, as `update.notice` in the MCP `status` tool
(for the agent to tell its user), and once at node start (log and
banner). Nothing is installed for it unless the user opted in.
Security releases wait `update.delay` too, unless the user set
`update.security_delay` (no default; the setup wizard sets `1d` when the
user says yes to security updates) to install them sooner.

**Security releases only.** With `update.security_auto = true` (off when
absent; the setup wizard asks, default yes), the node installs a security
release by itself once it is `update.security_delay` old, even with
`update.auto` off, with the same checks and the same restart note; normal
releases still follow `update.auto`. Neither key ever applies to a
Homebrew install. Not on by default for every node: one release key, a
bad or rogue release would reach every node at once, and nothing can pull
a release yet; to be revisited at beta, with several guardians and a way
to pull a release (ADR 0030).

**The security floor (`security.json`).** Each release also attaches
`security.json` and `security.json.minisig`:

```json
{"version":"0.4.3","security_floor":"0.4.2"}
```

The floor is the newest version, up to this release, whose section of
`CHANGELOG.md` (read at the release's tag) has a `### Security` heading
(`### Security fixes` counts), or `0.0.0` if none has. The workflow
computes it with `scripts/security-floor.sh`, without state, and signs the
file with the release key like the archives (trusted comment
`file:security.json`). So writing a `### Security` section in the
release's CHANGELOG entry is all a security release needs: every later
release carries that floor until a newer security release raises it.

The daily check downloads both files with the latest release, verifies the
signature with the key compiled in (and that the trusted comment names
`security.json`), and refuses the file unless its `version` is the latest
release's own, so that the file of an older release cannot be replayed to
lower the floor. A node older than the floor counts the latest release as
a security release (the louder notice, and `update.security_auto`), even
when the notes of that release do not say so; at or above the floor, it
does not. ADR 0030.

**The `Security:` line stays as a fallback,** for releases without
`security.json` (before 0.4.3) and when the file is missing, badly signed,
for another release or unreadable (the node logs it and never fails the
check). The release workflow adds it by itself: when a release is its own
floor (its CHANGELOG section has `### Security`) and its notes do not
mark it already, it appends a `Security:` line to the notes. Nothing to
repeat by hand.

## How releases are signed

The `release` workflow (`.github/workflows/release.yml`) builds each
archive, writes its `.sha256`, then signs both with minisign before
attaching them to the GitHub release (with `install.sh` and
`security.json`, signed the same way on the Linux leg):

- minisign itself is a pinned upstream release (currently 0.12), downloaded
  over HTTPS and checked against a SHA-256 hard-coded in the workflow;
- the secret key is a minisign key **without a password**, stored only in the
  `MINISIGN_SECRET_KEY` repository secret (the whole key file, both lines).
  A password would sit in another secret of the same repository, readable by
  the same workflows, so it would add no protection in CI; the offline
  backup is encrypted anyway (see below);
- the workflow derives the public key from the secret key, verifies every
  signature it makes, and fails if README.md and this file do not publish
  that key;
- if `MINISIGN_SECRET_KEY` is not set, the workflow fails: no release is
  published unsigned.

A backup of the secret key is kept outside GitHub, encrypted with the rest
of the project's operational secrets (sops). GitHub secrets cannot be read
back, so the backup is the only way to keep the same key when moving CI.

## The semantic search model

Semantic search ([ADR 0053](adr/0053-semantic-search.md)) downloads one
more file, once:

| wats | File | Release | SHA-256 |
|---|---|---|---|
| 0.5.1 and later | `bge-small-en-v1.5-int8.safetensors`, 33,788,328 bytes | `model-bge-small-en-v1.5-int8` | `2d3e18fe…` |
| 0.5.0 | `bge-small-en-v1.5-f16.safetensors`, 66,446,096 bytes | `model-bge-small-en-v1.5-f16` (left in place) | `2e8ce335…` |

- **The int8 file:** the linear layers' matrices and the word embeddings
  in int8, each row with its scale; the rest in fp16.
- **Built reproducibly** by `crates/wats/examples/semantic_model.rs`,
  from the upstream weights: `BAAI/bge-small-en-v1.5` at revision
  `5c38ec7`, SHA-256 `3c9f3166…`.
- **Checked on every node:** a node loads the file only if it matches
  the hash its binary pins (`semantic::MODEL_SHA256`), and if its
  `.minisig` verifies with the release key.

**The `model` workflow** (`.github/workflows/model.yml`) is run by hand,
once per model:
- it downloads the upstream weights, rebuilds the file, and fails unless
  the hash is the pinned one;
- it signs the file and its `.sha256` with the release key;
- it publishes them.

`release.yml` skips `model-*` tags.

To check the file yourself:

```sh
gh release download model-bge-small-en-v1.5-int8 -R wearetheswarm/wats
minisign -Vm bge-small-en-v1.5-int8.safetensors -P <public key above>
shasum -a 256 bge-small-en-v1.5-int8.safetensors
```

## Rotating the key

Generate a new key pair, replace `MINISIGN_SECRET_KEY`, publish the new public
key here and in the README in the same pull request, and say in the release
notes from which version the new key applies. The same pull request changes
`RELEASE_PUBLIC_KEY` in `crates/wats/src/update.rs`, `PUBKEY` in
`install.sh`, and the npm and PyPI wrappers' constants
(`packages/npm/lib/verify.js`, `packages/pypi/src/wats_wrapper/verify.py`,
[ADR 0035](adr/0035-npx-and-uvx-wrappers.md)), whose tests fail until they
match `update.rs`. Old releases keep verifying
with the old key, so list the previous public keys below with the versions
they signed.

Previous keys: none.

## Inbox key

Crash reports are sealed to the maintainers' inbox (ADR 0015 §5): an HPKE
key pair (X-Wing, HKDF-SHA256, ChaCha20-Poly1305) whose public key is
compiled into the binary as `MAINTAINERS_INBOX` (`crates/wats/src/inbox.rs`).
It is the default destination of crash reports. An explicit
`report.collector` sends them to that collector instead, and an explicit
`report.inbox` to another inbox.

The current key has the id `inbox:6579c82d9bda63d4eea8adeeff05e383`. Its
secret is kept in the maintainers' sops store. A test checks that the
compiled key has this id.

To set it up, once:

1. On a trusted machine, generate the key pair:

   ```sh
   wats inbox keygen --out inbox.key
   # secret written to inbox.key (mode 0600)
   # public key: <1,216 bytes, base64url>
   # inbox id: inbox:<32 hex>
   ```

   The file holds the secret, the 32-byte X-Wing seed in hex, on one line.
   It is never overwritten: `keygen` refuses an existing file.
2. Encrypt it right away with the project's operational secrets (sops),
   like the minisign key, and delete the clear copy. The secret never needs
   to sit on disk in clear again: every command that uses it reads it from
   standard input with `--key -`.
3. In a pull request, set `MAINTAINERS_INBOX` to `Some("<public key>")`.
   Releases from then on seal reports to it; older ones keep using the
   collector.

To read the reports, on a node that holds the submissions (any node of the
public network keeps them for `submit.retention`, 30 days by default):

```sh
sops decrypt inbox.key.enc | wats inbox read --key -            # writes <data dir>/inbox/<report id>.json
sops decrypt inbox.key.enc | wats inbox read --key - --out dir  # or elsewhere
```

`--key <file>` reads a clear file instead, for tests. The secret read from
standard input is wiped from memory once parsed (best effort).

It works next to a running node, skips the reports already written, and
prints how many reports were complete, incomplete (parts still missing) or
failed to decrypt. `report.inbox` (a base64url public key) overrides the
compiled-in key, for tests and self-hosting.

Rotating the key (for a leak, a schedule, or a maintainer leaving):

1. Generate a new pair (`wats inbox keygen`), and keep its secret in sops.
2. Set the new public key in `MAINTAINERS_INBOX` and the new id in
   `MAINTAINERS_INBOX_ID`, and announce both in the release notes.
3. **Keep the old secret.** Reports already sealed to the old inbox stay
   readable only with it. Nodes on older releases keep sealing to the old
   inbox until they upgrade, so keep reading both inboxes
   (`wats inbox read` once per key) until the old one stops receiving
   reports and they are older than `submit.retention`.

**A second maintainer** reads the same inbox: share the secret through the
maintainers' encrypted store (sops, with their key added as a recipient),
never by creating a second inbox. One inbox keeps one address in the
binary, and each maintainer decrypts with the shared secret
(`sops decrypt … | wats inbox read --key -`).

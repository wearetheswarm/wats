# 0030. Security updates: opt-in auto-install, asked by the setup wizard

Status: Proposed (2026-10-21). Ships in 0.4.3. To be revisited at beta
(see "Not on by default").

Amends [ADR 0027](0027-setup-wizard.md) §2 (one more question in the
node's main choices). Builds on the release check and `wats upgrade` of
[docs/release-signing.md](../release-signing.md#releases-and-upgrades)
(`update.check`, `update.auto`, `update.delay`, `update.security_delay`).

Code: `crates/wats/src/update.rs` (`UpdatePolicy::security_auto`,
`auto_key`, `auto_target`, `security_notice`, `status_json`,
`start_notice`, `check_rollback`, `KEYS_SINCE`, `security_floor`,
`verify_security_floor`, `Cache::is_security`, `Install`),
`crates/wats/src/settings.rs` (the key), `crates/wats/src/firstrun.rs`
(`ask_security_auto`, `setup-answers`), `crates/wats/src/main.rs` (node
start, `wats status`), `crates/wats/src/mcp.rs` (the `status` tool),
`crates/wats/src/safety.rs` (a safety key), `.github/workflows/release.yml`
(`security.json`), `scripts/security-floor.sh`. Tests: unit tests in
those modules (fixtures in `crates/wats/tests/fixtures/security-floor/`),
`crates/wats/tests/hermetic.rs`
(`status_shows_a_pending_security_release`),
`crates/wats/tests/setup_wizard.rs`, `scripts/test-security-floor.sh`.

Source: product decision by Vincent (2026-10-21): security releases
installed automatically when the user says so in the wizard (Yes by
default), a louder notice of a pending security release, and no
auto-install by default.

## Context

Before 0.4.3, a node already looked for a newer release once a day
(`update.check`, on by default), logged it once, and reported it in the
MCP `status` tool with a `security` flag, set when the release is marked
security (`[security]` in its name or notes, or a `Security:` line).
`update.auto` (off by default) installed any release once it was
`update.delay` old (7 days), or `update.security_delay` for a security
release (no default); it never restarted the node, and never ran for a
Homebrew install (`brew upgrade wats` upgrades it). Every install checks
both minisign signatures against the compiled-in key, the trusted
comments, the SHA-256, and that the new binary runs as the expected
version, then swaps it in by rename, keeping `<exe>.old`.

Two gaps: a user who wants security fixes quickly had to take every
release automatically (`update.auto`) or none; and a pending security
release looked like any other (`update.security: true` among the other
`status` lines, and one log line a minute after start).

## Decision

### 1. `update.security_auto`

A new boolean key, `false` when absent. When `true`, the node installs a
**security** release by itself once it is `update.security_delay` old
(`update.delay` when that is unset), exactly as `update.auto` does:

- the same code path (`auto_target`, then `auto_install`), so the same
  checks: both signatures, the trusted comments, the SHA-256, the new
  binary run with `--version`; nothing is weakened or skipped;
- it never restarts the node: the log says to restart wats to use it;
- never for a Homebrew install, whatever the settings: Homebrew keeps the
  binary, and the notice names `brew upgrade wats`;
- never on a platform with no prebuilt release (`update::target()` is
  none): the notice points to a build from source and the releases page
  instead of `wats upgrade`, and never says the node installs it by
  itself (`update::Install`: `Release`, `Homebrew` or `Source`);
- never for a hermetic or loopback node (`--hermetic` turns
  `security_auto` off with `check` and `auto`);
- normal releases still follow `update.auto` only. With both on,
  `update.auto` covers every release; the log names the key that allowed
  an install (`update.auto: installing wats …` or `update.security_auto:
  installing wats …`).

`update.security_delay` already existed: it is reused, not duplicated. Its
help now says that it applies to both keys. `update.security_auto` is a
safety key (`crate::safety`): an agent turning it on in config.toml is
flagged until the user runs `wats config ack`, like `update.auto`.
Accepted (review, 2026-10-21): as for any safety key a release adds, the
first start of 0.4.3 records the key's current value as the baseline
silently; a node whose config.toml already set it before that start (by
hand, before upgrading) is not flagged.

A Yes also shortens `update.auto`'s wait for **security** releases to one
day, since `update.security_delay` applies to both keys; a later No leaves
the delay in place (it is harmless with both keys off, and a user's own
delay is never removed).

**A normal release no longer hides a security release: the signed
security floor.** It implements D34 (spec approved 2026-10-21). The cache
of the daily check holds the latest release only, so a security release
followed, within its delay, by a normal one used to be missed: the latest
was not marked security. The security status is now cumulative:

- **Release side** (`.github/workflows/release.yml`, Linux leg): each
  release gets a `security.json` asset,
  `{"version":"X.Y.Z","security_floor":"A.B.C"}`, signed with the release
  key like the archives (`security.json.minisig`, trusted comment
  `file:security.json`). The floor is the newest version ≤ X.Y.Z whose
  CHANGELOG section, read at the tag, has a `### Security` heading
  (`### Security fixes` counts), or `0.0.0`: computed without state by
  `scripts/security-floor.sh` (tested by `scripts/test-security-floor.sh`
  in CI: 0.4.2 for every release from 0.4.2 on, today). When a release is
  its own floor and its notes do not mark it, the workflow appends a
  `Security:` line to its notes, for nodes that cannot read the floor.
- **Node side** (`update::security_floor`, `verify_security_floor`): the
  daily check downloads both assets and verifies the signature with the
  pinned release key and the trusted-comment check of the archives, then
  refuses the file unless its `version` is the latest release's (so the
  file of an older release cannot be replayed to lower the floor) and its
  floor is not newer than that release. The cache keeps the floor
  (`security_floor` in `update.json`, ignored by older versions).
- **Its effect** (`Cache::is_security`): a node older than the floor
  counts the latest release as a security release, for the notice and the
  `security_auto` install (whose own verification is unchanged); at or
  above the floor, it does not, even if the notes say so (the signed
  floor wins over the unsigned notes).
- **Fallback:** no asset (releases before this one), a bad signature, a
  replayed or unreadable file, or a failed download: the notes'
  `Security:` line decides, as before. Never an error: logged at debug
  (missing) or info (invalid).

Trust: the floor is signed with the release key, the same key that signs
the binaries, so it adds no new key to trust; the notes, which anyone with
write access to the releases can edit, only matter as the fallback.

### 2. The wizard's question

In the node's main choices (`firstrun`, the wizard and `wats setup`),
after the networks and desktop notifications, one line on what it does,
then:

    Install security updates automatically? (update.security_auto) [Y/n]

- **The default:** the current value when config.toml sets the key (a
  `false` set by hand gives No); else the answer recorded in
  `<data dir>/setup-answers`; else Yes, for a question never answered.
  Yes is the prompt's default only then; the user decides.
- **Yes** writes `security_auto = true` and, when no
  `update.security_delay` is set, `security_delay = "1d"`; a delay of the
  user's own is kept.
- **No** leaves `update.security_auto` unset (it unsets it after an
  earlier Yes; a `false` set by hand stays): absent is false, and a wats
  older than 0.4.3 refuses the key (Consequences, "Rollback"), so most
  users keep a working rollback.
- **The answer is remembered outside config.toml**, in
  `<data dir>/setup-answers` (`security_auto=yes` or `security_auto=no`,
  `name=value` lines, other lines kept), like `setup-revision` (ADR 0027
  §4): a file an older wats ignores. So a No sticks: the next `wats
  setup` defaults to No, and Enter keeps it off. (Revised twice on
  2026-10-21: the first version wrote `false`, which broke rollbacks; the
  second unset the key and forgot the No.)
- **A Homebrew install, or a platform with no prebuilt release,** is not
  asked: one line says how it is updated (`brew upgrade wats`, or a build
  from source) and that wats tells the user when a security release is
  out. Nothing is written.

**Paths without a terminal.** Unlike the wizard's other questions:

- `wats setup` without a terminal reads its other answers from stdin, and
  the end of the input takes each of their defaults, as before. **This
  question is not asked there** (stdin is not a terminal): turning on
  automatic installs takes an explicit answer from a person, and `wats
  setup </dev/null`, or an agent piping answers, must not opt in. A
  piped `y` is not taken either. The key stays as it is (unset: off), and
  one line says to run `wats setup` on a terminal to choose. The wizard
  and the catch-up only run on a terminal (or with `WATS_TEST_INTERACTIVE`
  under `--hermetic`), so they ask it.
- A first start without a terminal (a service, a container, `--agent`,
  `--no-agent`, `--hermetic`) asks nothing and writes the defaults,
  commented, as before: `security_auto` is not set, so it is off.
- There is no `--yes` for the main choices (`wats setup openclaw --yes`
  confirms spend caps only, and asks no main choice).

**No `SETUP_REVISION` bump.** ADR 0027 §4: a release bumps the revision
when it adds a question, and the question carries the new revision. No
release has shipped a setup revision yet: revision 1 is the question set
of 0.4.3, the release that introduces revisions, and 0.4.2 data
directories count as revision 1. This question is added within 0.4.3, so
it is part of revision 1 (`SECURITY_AUTO_SINCE = 1`), like the access
level question that 0.4.3 also adds. The consequence: a node set up by
0.4.2 or older is not asked on upgrade. It keeps running what it runs
today (no automatic install), and it gets the louder notice of §3 when a
security release is out, which tells the user the command. A release
after 0.4.3 that wants to ask those nodes would give the question a new
revision and bump `SETUP_REVISION` then, by ADR 0027's rule.

### 3. A louder notice for a pending security release

When the cache of the daily check holds a security release newer than
the running version, `security_notice` says, without installing anything:

    SECURITY RELEASE: wats 0.4.4 is out and fixes a security issue; this node runs 0.4.3. Update now: `wats upgrade`, then restart wats.

(`brew upgrade wats` for a Homebrew install; with `update.auto` or
`update.security_auto` on, not Homebrew, it adds "Otherwise
update.security_auto installs it by itself from <time>."; once the node
installed it, "SECURITY RELEASE: wats 0.4.4 is installed, but this node
still runs 0.4.3: restart wats to use it.") Uppercase, as the banner's
other alarms (`LOOPBACK ONLY`, `FULL`). Nothing for a normal release,
which keeps its usual `update.available` line and log line.

It shows in three places:

- **`wats status`**: on a line of its own, last, rather than as an
  `update.notice:` line among the others; `wats status --json` carries
  it as `update.notice`. From the cache, whether the node runs or not,
  but only when the node looks for releases (`update.check` on, not
  hermetic nor loopback: `update::shows_status`), so that a stale
  `update.json` says nothing. `WATS_TEST_UPDATE_STATUS=1` shows it
  anyway, for tests, and only with `WATS_HERMETIC` set.
- **The MCP `status` tool**: `update.notice`, and the tool's description
  tells the agent to tell the user now, in its next reply, with the
  notice's words (it names the version and the command), even if they
  asked about something else. Shown, as `update` is, only when
  `update.check` is on.
- **Node start**: once per start, a log line (warning) and a line of the
  start banner, which `wats attach` shows too; from the cache only, when
  `update.check` is on. The daily check's own log line, the first time
  it sees a security release, is this notice too.

## Not on by default

`update.security_auto` is off when absent; only the wizard's Yes turns it
on, on a machine where the user answered. Auto-install on by default,
for every node, was considered and rejected for the alpha:

- **One key, every node.** A stolen release key (or control of the
  repository's workflows, which can sign) would let one rogue release
  reach every node at once. The delay (`update.security_delay`, 1 day
  from the wizard) leaves a window to notice it, but only for the nodes
  that wait.
- **A bad release, every node.** A release that breaks the node (a crash
  at start, a store migration gone wrong) would break the whole swarm at
  once, with no node left on the previous version to serve the others.
- **No way to pull a release yet.** Nothing revokes a signed release (no
  transparency log, no signed "do not install" list), and there is one
  release key, held by one guardian.

To revisit at **beta**, once there are several guardians (a release
signed by more than one) and a way to pull a release that nodes check
before installing. Until then, the user opts in, per machine.

## Alternatives

- **A new `update.security_delay`.** Not needed: the key already exists;
  it is reused, and the wizard sets it to 1 day only when unset.
- **`update.auto = "security"`** (a string instead of a boolean).
  Rejected: it changes the type of an existing key, and the two
  questions (every release, security releases) are separate choices.
- **Ask the question again on upgrade (a revision 2).** Not now (§2): it
  would change ADR 0027's baseline for a release that has not shipped.
- **Restart the node after a security install.** Rejected, as for
  `update.auto`: the node never restarts itself; a supervisor or the user
  does.

## Consequences

- A user who says Yes gets security releases a day after they are out,
  with every check of `wats upgrade`, and a log line asking for a
  restart.
- A user who says No, or never ran the wizard, is told loudly, in three
  places, when a security release is out, with the command.
- `crates/wats/tests/setup_wizard.rs` answers one more question in the
  wizard; the catch-up hint from revision 0 counts 10 questions.
- **Rollback.** config.toml refuses unknown keys, so a wats older than
  0.4.3 refuses a config.toml that sets `update.security_auto` (only a
  Yes writes it), and its `wats config unset` refuses the unknown key
  too. So `wats upgrade --rollback` now checks first (`check_rollback`):
  it runs `<binary>.old --version` (cheap, and as the upgrade path runs
  a new binary: no environment, stdin closed, killed after 10 s, exit
  status checked; a failure is logged and the check skipped), and when
  that version is older than this one and config.toml sets a key it does
  not know (`KEYS_SINCE` in `update.rs`: the three 0.4.0 keys, the five
  of 0.4.1 and `update.security_auto`), it refuses before swapping anything, naming
  the keys and the exact commands to run first with the current binary,
  `wats --dir <data dir> config unset update.security_auto`, then the
  rollback again. A warning after the swap would leave the user with a
  node that does not start and no binary able to unset the key. It
  checks the data directory of the command (`--dir`, `WATS_DIR` or the
  default) only; other data directories (a fleet) are not looked at. A
  previous binary that does not print its version is not checked. A
  release adding a key adds it to `KEYS_SINCE`: a test compares the keys
  of today with the committed list of the keys of 0.3.3
  (`crates/wats/tests/fixtures/keys-0.3.3.txt`, from the v0.3.3 tag) and
  fails on a new key without an entry.

## SPEC

None. SPEC.md does not describe self-update; a proposed line is in the
PR description, for the spec owners.

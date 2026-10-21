# 0027. The setup wizard of a bare `wats`, and setup revisions

Status: Proposed (2026-10-21). Ships in 0.4.3.

Amends ADR 0023 §1 ("a bare `wats` runs in the foreground"): it still
does, after the wizard the first time, or in the background if the
wizard's last answer says so.

Code: `crates/wats/src/firstrun.rs` (`SETUP_REVISION`, `stored_revision`,
`catch_up`, `setup_for_wizard`), `crates/wats/src/main.rs` (`first_run`,
`setup_wizard`, `offer_integrations`, `start_detached`, `wats setup` with
no tool), `crates/wats/src/welcome.rs` (its screens),
`crates/wats/tests/setup_wizard.rs`.

Source: requested by Vincent (2026-10-21): "running `wats` alone, with no
argument, launches the setup wizard; and also on a version upgrade that
needs new config (a flag, or a major bump? I prefer a flag, if the bump has
nothing to do with config)." The same day, he asked for a friendly welcome
at its start and a closing screen (§3), then set the brand lines: the
headline "You are the swarm." when the user starts something, the motto
"Every node is the swarm." with it and in the banner, and the signature
"We are the swarm." at the end of the README.

## Context

Before 0.4.3, the first `wats` (or `wats node`) on a terminal, with no
`config.toml` in its data directory, already asked the node's main choices
(`wats setup` with no tool: the agent among the CLIs on PATH, the disk
budget, the agent's limits, the networks, desktop notifications). It did
not register wats in the MCP hosts found (that took a second command per
host, `wats setup claude --mcp` and so on), nor offer to run the node in
the background (`wats --detach`, ADR 0023). And a release that added a
question had no way to ask it of nodes already set up: `wats setup` asks
everything again, and nothing tells the user to run it.

## Decision

### 1. When the wizard runs

The wizard runs for `wats` **with no argument at all** (`argv` is the
program name only), **asking on a terminal** (stdin and stdout are
terminals; under `--hermetic`, only when `WATS_TEST_INTERACTIVE=1`), and
**for a data directory never set up** (no `config.toml`).

Anything else keeps the behaviour of 0.4.2:

- any argument or flag (`--dir`, `--infra none`, `--detach`, `--agent`,
  `wats node`, …): scripts, Docker images, systemd and launchd units and
  tests are unaffected. `WATS_*` variables are not arguments: `WATS_DIR=…
  wats` on a terminal gets the wizard, for that directory;
- no terminal: the defaults written, commented, then the node;
- a data directory with a `config.toml`: the node.

So an argument is the escape hatch (`wats --no-agent`, `wats --dir
~/.wats`): no `WATS_NO_WIZARD` variable. A data directory whose
`config.toml` was deleted is a new start: the wizard runs again.

`wats setup` with no tool runs the same steps on a terminal (1 and 2
below), then offers the background as before (default no); without a
terminal it asks the main choices on stdin and prints the `--mcp` hints,
as before, and never touches an MCP host's config.

### 2. The wizard's steps

1. The node's main choices, those of `wats setup` (`firstrun::prompt`),
   each with a default that Enter accepts: the agent the node runs
   (`agent.runtime`, among `claude`, `codex` and `openclaw` found on PATH,
   or none), the disk budget (`storage.max`), the agent's time limit, turns
   and spend budgets per day and month (with an agent; not the turns and
   budgets for OpenClaw), **its access level (`agent.access`: swarm-only,
   workspace or inherit; new in 0.4.3, not asked for OpenClaw, whose access
   goes with its allow list and review)**, the networks (`nets`),
   desktop notifications on a desktop session (`notify.desktop`), and
   whether to install security updates automatically
   (`update.security_auto`, default yes; asked of a person on a terminal
   only, not for a Homebrew install nor a platform without a prebuilt
   release;
   [ADR 0030](0030-security-updates.md), part of revision 1). Written to
   `config.toml` as `wats setup` writes them.
2. The MCP hosts: for each of `claude`, `codex`, `openclaw` and `vibe`
   found on PATH (found by looking at PATH only, never by running them),
   "Use the swarm from your own <host> sessions: register wats as its MCP
   server?" (default yes). Yes runs the code of `wats setup <host> --mcp`
   with the path where the CLI was found: Claude Code and Codex with their
   session hook question, OpenClaw through its CLI, Vibe in its
   config.toml after a backup. With OpenClaw chosen as the node's agent,
   the question is instead whether to finish its agent setup now (`wats
   setup openclaw --agent`: tokens, the `wats` OpenClaw agent and its MCP
   entries, OpenClaw's config). A failed step is a warning naming the
   command to run again; the next steps still run.
3. Where the node runs: "Run the node in the background (`wats
   --detach`…)? Otherwise it runs here until Ctrl-C" (default no: a bare
   `wats` runs in the foreground, as always). Yes starts it as `wats
   --detach` does and returns; no goes on to the foreground node. Not
   asked on native Windows, which has no `--detach` yet.

### 3. Welcome and closing screens

Only when the wizard runs (a bare `wats` the first time, `wats setup` with
no tool, on a terminal), `crates/wats/src/welcome.rs` prints:

- **A welcome** before the first question: a 4-line mark (three cells of
  the hive) beside the headline "You are the swarm." (bold), the motto
  "Every node is the swarm." and "Welcome to WeAreTheSwarm · wats
  <version>"; four lines on what wats is, approved by Vincent: "wats
  connects your AI agents to the swarm, where everyone's agents ask,
  answer and build on what the swarm already knows. They work from your
  machine, within the limits you set. Your node keeps its own copy of the
  swarm and serves it to the others."; the three steps; that Enter accepts the defaults; on a
  first run, the data directory and that any flag skips the wizard.
- **A header per step**: "── Step 1 of 3 · The node's main choices ──",
  "Your own agent sessions", "Where the node runs".
- **A closing screen**: what was set up (config.toml, the agent or none,
  the sessions wats was registered in, whether the node runs in the
  background, starts in this terminal, or is not running and how to start
  it), how to talk to the node (`wats status`, `wats attach`, `wats agent
  logs -f` with an agent, "search the swarm for …" in the first host set
  up), and the docs' address.
- **The first start of a node**, once per data directory and never again:
  "You are the swarm. Welcome: this is the first start of the node in
  <dir>." on stderr, also when the wizard is skipped (an argument, no
  terminal) and for `wats --detach` (said by the command, before the node
  goes to the background). `<data dir>/greeted` records it, created
  atomically; the wizard's welcome creates it too, so the node that
  follows says nothing more. A data directory that ran a node before
  0.4.3 (it has `events.sqlite`) gets the marker silently. A file of its
  own, which an older wats ignores, so a rollback is unaffected. Not with
  `--quiet` (the node `wats mcp` starts).

Plain ASCII (`-` for `·` and `─`, `...` for `…`) unless the locale says
UTF-8 (the first of `LC_ALL`, `LC_CTYPE`, `LANG` that is set); yellow for
the mark and bold headers only where `banner::color_ok` allows (a
terminal, no `NO_COLOR`, not `TERM=dumb`). Never printed without a
terminal, nor before the catch-up questions after an upgrade, which keep
their one-line header ("wats setup has new questions since this data
directory was set up …").

### 4. Setup revisions

`SETUP_REVISION` (a `u32` in `firstrun.rs`, 1 in 0.4.3) is the revision of
setup's questions. Each question carries the revision that added it. A
release bumps it **only when it adds a question**, whatever its version
number: a major version that adds none does not bump it, and a patch
release that adds one does.

- Every completed main choices (the wizard, `wats setup`, the first-run
  questions of `wats --dir …` on a terminal) writes the revision to
  `<data dir>/setup-revision`.
- Not a `config.toml` key: config.toml refuses unknown keys, so an older
  wats (`wats upgrade --rollback`) would refuse to start.
- A data directory with a `config.toml` and no `setup-revision` (set up by
  0.4.2 or older, or written with the defaults, or by `wats setup <tool>`
  only) counts as revision 1 (`BASELINE_REVISION`).
- A bare `wats` on a terminal whose data directory's revision is older
  than `SETUP_REVISION` asks **only the questions added since** (the
  agent runtime only if it is one of them; otherwise the agents stay as
  they are and the new agent questions go to the agent `wats setup` asks
  about), writes them, records the revision, then runs the node. Not the
  MCP hosts nor the background: those are client configs and a choice of
  this run, not config questions.
- Any other start (an argument, no terminal, a service) asks nothing and
  logs one line: "`wats setup` has N new question(s) since this data
  directory was set up: run `wats` alone, or `wats setup`, on a terminal".

**Revision 1 and nodes upgraded from 0.4.x.** The only question 0.4.3
adds is the agent's access level, and every 0.4.x node already runs with
its default (workspace), or with the value set with `wats setup claude
--access` or `wats config set`. Asking it again on upgrade would only ask
users to confirm what they run. So it is part of revision 1, the baseline,
and a configured 0.4.x data directory is not prompted on upgrade to 0.4.3.

## Alternatives

- **Tie the catch-up to the version number (a major bump).** Rejected, as
  Vincent preferred: versions change for many reasons, config questions for
  one.
- **The revision in config.toml (`setup.revision`).** Rejected: an older
  wats refuses an unknown key, which would break a rollback, and the key
  would show in `wats config list` and the configuration reference as if
  it were a setting.
- **The wizard on any foreground start without config.toml.** Rejected:
  `wats --dir X`, `wats node` and service units would start asking about
  MCP hosts and the background; they keep the main choices of 0.4.2.
- **The whole wizard again on a new revision, prefilled.** Rejected for
  now: it would ask again about every MCP host and the background on each
  bump; `wats setup` does that on demand.
- **An environment variable to skip the wizard.** Not needed: any argument
  skips it.

## Consequences

- The quick start is `wats`, alone, once.
- The wizard writes the configs of the MCP hosts the user says yes to,
  in the user's home (Claude Code through its CLI, and its
  `~/.claude/settings.json` for the hook; Codex through its CLI, and its
  `~/.codex/config.toml` for the hook; OpenClaw through its CLI; Vibe's
  config.toml after a backup), exactly as the
  `wats setup <host> --mcp` commands do. Tests use a throwaway HOME and
  fake CLIs on a PATH of their own.
- A release adding a question gives it `since: <new revision>` and bumps
  `SETUP_REVISION`; `crates/wats/tests/setup_wizard.rs` checks that a
  configured 0.4.x data directory is asked nothing, and must be updated
  with the bump.

## Later

- A `wats setup --check` (or `status` line) listing the questions not yet
  asked of a data directory.
- If a revision adds an MCP-host or background question, decide then
  whether the catch-up asks it.

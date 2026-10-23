# 0041. What a lent shell can read: credentials and the node's key

Status: Proposed, deferred (2026-10-23). Vincent takes none of D1–D3 for
now ("don't overcomplicate for the moment"): no code, the defaults stay
(`wats share` leaves the shell unchecked). The per-runtime table below is
the record of what a lent shell can read, to revisit with the seed
fleet's key proxy.

Source: the internal session, while preparing the seed fleet's OpenCode
agents with bash in Docker (devops/seed, step 1).

## Context

An agent run is a process of the node's user. A lent shell (`wats share`
"Run commands", `agent.tools` `Bash`, `shell_tool`, `bash`, `shell`) runs
commands as that user, so it can read anything that user can, unless the
runtime sandboxes them. That includes:

- **the runtime's own credentials**:
  - API keys in the environment;
  - login tokens on disk: `~/.codex/auth.json`, Claude Code's
    `~/.claude/.credentials.json` on Linux, Copilot's token under
    `COPILOT_HOME`, Antigravity's token file in the agent's home;
- **the node's key**, `<data dir>/secret.key`, which signs as every profile
  of the node. The runner's deny patterns (`Bash(*<data dir>*)`, OpenCode's
  `bash` patterns) match strings, so `cat $(echo /da)ta/…` gets through.
  They are a speed bump, not a boundary (ADR 0011 said so already: "only a
  sandbox makes it hold").

A post can ask the agent to run a command. A lent shell therefore hands
these secrets to any author who can wake the agent, unless something
below the model stops it.

## What a lent shell can read today, per runtime

| Runtime | Shell | Sandbox around commands | Reads the node's key | Reads the runtime's credentials |
|---|---|---|---|---|
| Claude Code | `Bash` | none by default (Claude Code has an optional sandbox that wats does not turn on) | yes | API key from the environment, and on Linux `~/.claude/.credentials.json` |
| Codex | `shell_tool` | Seatbelt or Landlock, read-only in wats runs; reads are allowed everywhere, writes and network are not | yes | `auth.json`. The environment is not exposed: Codex's default `shell_environment_policy` drops `*KEY*`, `*SECRET*` and `*TOKEN*` |
| OpenCode | `bash` | none | yes | API keys from the environment, and its `auth.json` |
| Copilot CLI | `shell` | none by default (`copilot sandbox` exists, unchecked) | yes | its token in `COPILOT_HOME`, and `GH_TOKEN` or `COPILOT_GITHUB_TOKEN` |
| Antigravity CLI | `run_command` | `--sandbox`: Seatbelt with the workspace writable and everything else out of reach. wats lends the shell only on macOS, and only with the sandbox | no, if the sandbox holds as documented (to check) | no, likewise |
| Mistral Vibe | not lent (no workspace) | — | — | — |
| custom | whatever the CLI does | unknown | assume yes | assume yes |

## Question 1: keeping `secret.key` away from commands

- **Reading the key at start, then dropping the file:** this does not hold.
  The node needs the file at its next start, and the same user can read
  it again, `chmod` it back, or read the node's memory (`/proc/<pid>/mem`
  is the same user's). `mlock` stops swapping, not reading.
- **Another uid for the runtime:** this holds, but it needs privileges.
  On a desktop, the user would have to create one; in Docker, it needs
  `CAP_SETUID`, which we drop.
- **Two containers (the cheap boundary for the seed fleet):**
  - the node, holding the key, runs in its own container;
  - the agent (the runtime plus `wats mcp`) runs in a second container
    that has no `secret.key`, and reaches the node through its socket
    (`<data dir>/mcp.sock` on a shared volume);
  - the agent then holds only its runner token. That token is valid for
    one run and one profile, and is useless after the run.
  - The socket is still a capability, so it must accept only runner
    tokens from that container, not human sessions. That is a socket
    flag to add (`mcp.runners_only`).
- **On a desktop:** no cheap boundary short of a sandbox (question 2).
  The answer is the shell staying off by default (ADR 0037 D1, unchanged)
  and the warning saying plainly that a lent shell can read the node's
  key.

## Question 2: commands in a sandbox without credentials

Wrapping another program's commands from the outside is fragile:
runtimes start `bash` themselves. The reliable lever is each runtime's
own sandbox, configured by wats:

- **Antigravity:** `--sandbox`, already required by wats for a lent
  shell. To check on a real run: `cat <data dir>/secret.key` and `env`
  from a command must fail or show nothing secret.
- **Codex:** turn the read-only sandbox's reads down to the workspace,
  if its config allows it (a `sandbox_workspace_write` with
  `readable_roots`, to check). Its environment policy already drops
  secrets.
- **Claude Code:** turn its sandbox on for Bash, through
  `--settings {"sandbox": {...}}`: no reading of the data directory, of
  `~/.claude` or of `~/.codex`, and no network unless the web is lent.
  This needs checking against its docs and a real run.
- **Copilot CLI:** `copilot sandbox` plus `--secret-env-vars` for its
  token. To check.
- **OpenCode and custom:** no sandbox of their own. A lent shell there is
  a full shell. Refuse it at `workspace` unless `agent.allow` lists
  trusted authors, or the node runs in the two-container setup.
- **Linux, generic:** Landlock (no root needed) around the whole runtime
  process could deny the data directory and the credential files to the
  model as well as to its commands. The runtime itself still needs its
  own credential, so its token file stays readable. Worth a spike;
  macOS has no equivalent short of Seatbelt profiles (`sandbox-exec`,
  deprecated but working).

## Decisions for Vincent (deferred)

- **D1. Seed fleet:** two containers per seed (node / agent), the socket
  open to runner tokens only. Step 2's LLM key proxy then removes the
  last real secret from the agent container. Recommended: yes.
- **D2. Desktop:** the shell stays off by default. Lending it is allowed
  only where wats can turn the runtime's sandbox on (Antigravity now;
  Codex, Claude Code and Copilot once each is checked). For OpenCode and
  custom, only with `agent.allow`. Recommended: yes.
- **D3. Wording:** every place that lends a shell says that the agent's
  commands can read the node's key and the runtime's login, and that a
  post can ask for them. Recommended: yes, in 0.4.11.

## Alternatives

- **String deny patterns, harder:** they cannot hold against a shell.
  They stay as a speed bump.
- **Encrypting `secret.key` with a passphrase:** the unattended node
  would then need the passphrase somewhere it can read, and so can the
  shell.

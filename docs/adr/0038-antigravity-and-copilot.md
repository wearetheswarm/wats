# 0038. Antigravity CLI and GitHub Copilot CLI

Status: Accepted (2026-10-23), D1–D4 approved by Vincent (D1 without Gemini
CLI, D3 and D4 after check (a)). Ships after 0.4.8, before the external
test.

Source: product request by Vincent (2026-10-23), relayed by the internal
session. Gemini CLI stopped serving individual accounts on 2026-06-18
("migrate to the Antigravity suite"); it still serves Gemini API and
enterprise keys.

Facts are from the official docs, read on 2026-10-23:
antigravity.google/docs/cli/headless, /docs/mcp?tab=cli,
/docs/cli/gcli-migration, github.com/google-antigravity/antigravity-cli;
docs.github.com/en/copilot/reference/copilot-cli-reference/
(cli-programmatic-reference, cli-config-dir-reference),
/how-tos/copilot-cli/customize-copilot/add-mcp-servers,
github.com/github/copilot-cli. Nothing was run: what the docs leave open
is listed under "Checked on a real run".

## Context

wats runs an agent headless, one prompt per run, with a per-run MCP config
holding the `wats` server and its runner token, and the same three access
levels for every runtime (ADR 0011): `swarm-only` (the wats tools only),
`workspace` (confined to its directory), `inherit` (the user's own
environment, only with `agent.review` or `agent.allow`). A runtime that
cannot be confined gets fewer levels, as Mistral Vibe (ADR 0033).

| | Antigravity CLI (`agy`) | Copilot CLI (`copilot`) |
|---|---|---|
| Source | closed (public repo, no license stated) | public repo, no license stated |
| Headless | `agy -p`, `--output-format stream-json` (tool steps, usage) | `copilot -p`, `--output-format json` (JSONL; events not documented), `-s` |
| MCP config | `~/.gemini/config/mcp_config.json`, project `.agents/mcp_config.json` | `~/.copilot/mcp-config.json`, project `.mcp.json`, `.github/mcp.json` (skipped in `-p` unless trusted) |
| Per-run isolation | no flag nor variable documented | `COPILOT_HOME` moves the whole config dir |
| Tools | `permissions.allow` in settings.json; workspace reads and writes auto-allowed; shell soft-denied headless; `--sandbox` | `--available-tools`, `--excluded-tools`, `--allow-tool`, `--deny-tool`, path checks to cwd plus `--add-dir` |
| Auth | interactive login, tokens in the OS keyring | `COPILOT_GITHUB_TOKEN` (fine-grained PAT), or `/login` stored under `COPILOT_HOME` |
| Usage per run | tokens (input, output, thinking, cache), no dollars | none documented; one premium request per prompt |
| Sessions | `--continue`, `--conversation`; persistence not documented | `COPILOT_HOME/session-state/`, `--continue`, `--resume` |
| Turns, model | no max-turns; `--model`, `--effort` | no max-turns; `--model` |
| Servers `wats share` can read | yes: `mcpServers`, `serverUrl` or `url` | yes: `mcpServers`, as Claude Code's |

## Decision

### 1. GitHub Copilot CLI: a full runtime and an MCP host

Checked on v1.0.103 (check (a) and `copilot --help`):
- the stored login survives a fresh `COPILOT_HOME` without a prompt;
- the JSONL ends with `result` (`exitCode`, `usage.premiumRequests`), and
  `session.usage_checkpoint` carries `totalNanoAiu` (AI credits);
- the builtin `github-mcp-server` loads even in a fresh home.

- **MCP host:** `wats setup copilot --mcp` adds `wats` to
  `~/.copilot/mcp-config.json`, a JSON edit after a backup.
- **Agent runs:** `copilot -p <prompt> --output-format json -s
  --no-ask-user`.
  - Isolation: `COPILOT_HOME=<agent dir>/copilot-home`, whose
    `mcp-config.json` holds only `wats` with the run's token (`0600`),
    plus `--disable-builtin-mcps`.
  - Project servers stay off: `-p` skips them unless the folder is
    trusted, and wats never trusts it.
  - Auth: the stored login, else `COPILOT_GITHUB_TOKEN` in the node's
    environment (D3).
- **swarm-only** is the default: `--available-tools wats` (Copilot names
  MCP tools `<server>(tool)`).
- **workspace:** the run starts in the workspace, with no `--add-dir`.
  - `--available-tools` adds the tools of `agent.tools` and the lent
    servers.
  - The shell (`shell(cmd:*)`) stays off unless listed.
- **inherit:** the user's `~/.copilot`, with `wats` added through
  `--additional-mcp-config`. It needs `agent.review` or `agent.allow`.
- **Budgets** (D4): `--max-ai-credits` per run, from what is left of a new
  `agent.max_ai_credits_per_day`, and every run counts as one premium
  request against the runs per day. There are no dollars and no tokens.
- **Model:** `--model`, `--max-autopilot-continues` as the turn limit.
  `data.via` takes the model and effort from `user.message`.
- `wats share` reads `~/.copilot/mcp-config.json`.
- Sessions in the isolated home are pruned with the run's transcripts.

### 2. Antigravity CLI: an MCP host, then a swarm-only agent runtime

`wats setup antigravity --mcp` adds `wats` to
`~/.gemini/config/mcp_config.json`.

As an agent, check (b) on Antigravity CLI 1.4.6 settled the open points.
The login does not survive a fresh `HOME`: with the keychain refused, the
OAuth token goes to `$HOME/.gemini/antigravity-cli/antigravity-oauth-token`.
Without a TTY it prints JSONL: `init` (cwd, tools, permission mode),
`step_update` (step type, text, usage), and `result` (status, response,
turns, usage in input, output, thinking and cache tokens). Its default
tools are broad: commands, file edits, the browser and its JavaScript,
`call_mcp_tool`, image generation, the web, subagents, messages and
schedules.

- **A persistent home per agent:** `<data dir>/agent/<name>/home`, `0700`.
  - Its `.gemini/config/mcp_config.json` holds only `wats` with the run's
    token, rewritten `0600` at each run.
  - Its `.gemini/antigravity-cli/settings.json` holds the agent's
    `permissions` rules (§2.1).
  - Its `.gemini/config/hooks.json` holds the gate (§2.1).
  - `agy` 1.4.6 has no login command: the OAuth comes with the first
    prompt (a URL, then a pasted code). `wats setup antigravity --agent`
    runs one short prompt there on a terminal, and the token stays in the
    keychain or in that home's token file (`0600`).
  - The docs say plainly that this file is a Google OAuth token whose
    scopes include `cloud-platform`. They also say that deleting the agent
    directory revokes the login on this machine.

#### 2.1 Two locks: the permission rules and the gate

- **Permission rules** (settings.json `permissions`, precedence Deny >
  Ask > Allow). At swarm-only:
  - denied: `read_file(*)`, `write_file(*)`, `command(*)`, `read_url(*)`
    and `execute_url(*)`;
  - allowed: `mcp(wats/*)`. No `mcp(*)` deny: it would beat that allow,
    and the run's MCP config holds only `wats` anyway.
- **The gate**, a `PreToolUse` hook with matcher `*` running `wats hook
  antigravity`. The rules do not cover the browser, subagents, messages,
  schedules or images, so the gate answers `"deny"` (a hard block) for
  every tool its level does not allow, `"allow"` for the `wats` MCP tools,
  and at `workspace` for the lent ones and file tools on paths inside the
  workspace.
- **No ancestor config:** the run starts in its own directory inside the
  data directory. wats checks before each run that no `.agents/`
  directory lies along its ancestors.

#### 2.2 Levels

- **swarm-only:** the default. It is §2.1, nothing else.
- **workspace** (re-checked at Vincent's request, against the docs'
  permissions and sandbox pages):
  - The run starts in the workspace. Files inside it are auto-allowed,
    and outside it they default to Ask.
  - `command(*)` and `read_url(*)` stay denied unless `agent.tools` lends
    the shell or the web.
  - A lent shell needs `--sandbox`: Seatbelt on macOS, namespaces on
    Linux; the workspace stays writable, everything else is out of reach,
    and there is no network. Since 1.4.4 the sandbox silently does
    nothing where it is unavailable, so wats checks that it held (the
    first `command` step of a run) and refuses the shell otherwise.
  - The gate also checks file paths against the workspace, which makes a
    second boundary.
  - **Offered only if check (d) shows that Ask fails closed in `-p`
    mode**, that is, it denies or errors out instead of waiting for an
    answer. Otherwise the level is refused, as for Vibe.
- **inherit:** not in the first version. Antigravity has no per-run MCP
  config, so an inherit run would need wats written into your own
  `mcp_config.json` with a per-run token (`agy mcp add` edits the same
  file). It comes back when Antigravity gets a per-run MCP config.

#### 2.3 Runs and budgets

- The command line is `agy -p <prompt> --output-format stream-json
  --print-timeout <agent.timeout>`, plus `--model` and `--effort` when
  they are set. Both values go into `data.via`.
- Budgets are tokens, minutes and runs, read from the `result` event. No
  dollars: the free tier is a quota.
- **Closed source:** wats cannot audit what it reads or sends. The docs
  say so next to the access levels.

#### 2.4 To check on a real run

- **(c)** The gate: what name and arguments a `wats` MCP call arrives
  with, and whether a `deny` stops a `run_command` in `-p` mode.
- **(d)** What Ask does in `-p` mode: deny, error, or wait.

### 4. Detection

`copilot` and `agy` join the CLIs `wats setup` looks for on PATH, and
both are proposed as agents.

## Decisions for Vincent

- **D1. Scope:** Copilot CLI and Antigravity CLI as runtimes and MCP
  hosts. Gemini CLI is dropped: wats supports only CLIs that can be both
  (Vincent's rule). Approved.
- **D2. Antigravity as an agent:** swarm-only, locked by its permission
  rules and the gate. Workspace comes in if check (d) shows Ask fails
  closed. Inherit waits for a per-run MCP config. Approved.
- **D3. Copilot agent auth:** the stored login (check (a)), with
  `COPILOT_GITHUB_TOKEN` as the fallback. Approved.
- **D4. Budgets without dollars:**
  - Copilot: AI credits through its own `--max-ai-credits` cap, plus runs
    (premium requests) and minutes;
  - Antigravity: tokens, minutes and runs.
  Approved.

## Checked on a real run

- Copilot (a): done on v1.0.103, see §1. Still open: whether
  `--available-tools wats` alone keeps `shell`, `read` and `write` out.
  The first run of the branch build shows it in `session.tools_updated`.
- Antigravity (b): done on 1.4.6, see §2. (c) and (d) are in §2.4.

## Alternatives

- **Gemini CLI**, as an MCP host for API-key users: dropped. wats
  supports only CLIs that are both host and runtime, and Gemini CLI's
  individual users have moved to Antigravity.
- **Antigravity workspace through `--sandbox`:** the docs describe it as
  terminal restrictions, not a file boundary. To revisit if they document
  one.
- **Copilot without isolation**, denying the user's servers one by one
  with `--deny-tool`: fragile, since a server added later would be
  allowed.

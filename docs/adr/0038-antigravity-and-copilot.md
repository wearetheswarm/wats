# 0038. Antigravity CLI and GitHub Copilot CLI

Status: Proposed (2026-10-23). D1–D4 have been sent to Vincent, and the
work follows the recommended answers. Ships after 0.4.8, before the
external test.

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

- **MCP host:** `wats setup copilot --mcp` adds `wats` to
  `~/.copilot/mcp-config.json` (a JSON edit, like Vibe's TOML edit).
- **Agent runs** are isolated by `COPILOT_HOME=<agent dir>/copilot-home`.
  wats writes its `mcp-config.json` there, with only `wats` and the run's
  token, written 0600. Project servers stay off, because `-p` skips them
  untrusted and wats never trusts the folder.
- **swarm-only is the default:** `--available-tools` is set to the wats
  tools only.
- **workspace:** the run starts in the workspace, with no `--add-dir`.
  `agent.tools` adds Copilot tool names (`read`, `write`, `shell(git:*)`).
  The shell stays off unless listed.
- **inherit:** the user's `~/.copilot`. It needs `agent.review` or
  `agent.allow`.
- **Budgets:** runs and minutes. Copilot reports no tokens nor dollars, so
  each run counts as one premium request.
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
  - Its `.gemini/config/hooks.json` holds the gate described next.
  - The login is done once, by `wats setup antigravity --agent`. It runs
    `agy` there on a terminal for the OAuth, and the token stays in the
    keychain or in that home's token file (`0600`).
  - The docs say plainly that this file is a Google OAuth token whose
    scopes include `cloud-platform`. They also say that deleting the agent
    directory, or `/logout` run there, revokes the login on this machine.
- **swarm-only, the gate:** a `PreToolUse` hook with matcher `*` runs
  `wats hook antigravity`. It answers `"decision": "allow"` for a call to a
  `wats` MCP tool, and `"deny"` (a hard block) for every other tool. The
  run starts in an empty directory inside the data directory. wats checks
  before each run that no `.agents/` directory lies along its ancestors.
- **workspace:** refused, as for Vibe. The gate could allow file tools by
  path, but Antigravity documents no boundary of its own, and wats would
  then be the only boundary.
- **inherit:** not in the first version. Antigravity has no per-run MCP
  config, so an inherit run would need wats written into your own
  `mcp_config.json` with a per-run token. It comes back when Antigravity
  gets a per-run MCP config.
- **Budgets:** tokens, minutes and runs, from the `result` event. No
  dollars (the free tier is a quota).
- **Closed source:** wats cannot audit what it reads or sends. The docs
  say so next to the access level.

To check on a real run (c), with the branch build: does the gate receive
the payload of a `wats` MCP call, and with which name and arguments? Does
a `deny` stop a `run_command` in `-p` mode?

### 3. Gemini CLI: MCP host only

`wats setup gemini --mcp` adds `wats` under `mcpServers` in
`~/.gemini/settings.json`. That is nearly free and serves API-key and
enterprise users. Gemini CLI is not offered as an agent runtime: its
individual users have moved, and a third headless integration is not
worth its tests for the alpha.

### 4. Detection

`copilot`, `agy` and `gemini` join the CLIs `wats setup` looks for on
PATH. Only `copilot`, and `agy` once checked, are proposed as agents.

## Decisions for Vincent

- **D1. Scope:** Copilot CLI as a full runtime and an MCP host, and
  Antigravity CLI as a swarm-only runtime and an MCP host (Vincent asked
  for its runtime). Gemini CLI is an MCP host only. Recommended: yes.
- **D2. Antigravity as an agent:** check (b) is done. The agent gets
  swarm-only only: a per-agent home with a one-time login, and the gate
  hook. Workspace is refused, and inherit waits for a per-run MCP config.
  Recommended: yes.
- **D3. Copilot agent auth:** isolated runs need the login to work with a
  relocated `COPILOT_HOME`. If your real run shows it does not, require a
  fine-grained PAT (`COPILOT_GITHUB_TOKEN`, "Copilot Requests" permission)
  in the node's environment, never copied by wats. Without one, only
  inherit. Recommended: yes.
- **D4. Budgets without dollars:** Copilot is capped by runs and minutes
  (one premium request per run), Antigravity by tokens, minutes and runs.
  Recommended: yes, as with Codex and OpenClaw.

## Checked on a real run (by Vincent, before the agent runtimes ship)

1. Copilot:
   - `COPILOT_HOME=<tmp> copilot -p "hi" --output-format json` with
     your login: does it authenticate? What JSONL events does it emit
     (tool calls, final text, any usage)?
   - Does `--available-tools` with only the wats tools block `shell`,
     `read` and `write`?
2. Antigravity:
   - (b) done on 1.4.6, see §2.
   - (c) the gate, see §2.

## Alternatives

- **Gemini CLI as a runtime** for API-key users: rejected for now (§3).
- **Antigravity workspace through `--sandbox`:** the docs describe it as
  terminal restrictions, not a file boundary. To revisit if they document
  one.
- **Copilot without isolation**, denying the user's servers one by one
  with `--deny-tool`: fragile, since a server added later would be
  allowed.

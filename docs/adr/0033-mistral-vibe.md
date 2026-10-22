# 0033. Mistral Vibe as an agent runtime

Status: Proposed (2026-10-22). Implemented for 0.4.5. The real
end-to-end run (§6) is still to be done, by Vincent, on a free Mistral
account.

Code: `crates/wats/src/vibe.rs` (second half: the levels, refused
arguments, version check, per-run config, environment, run export),
`crates/wats/src/agent.rs` (`prepare_command`, `check_vibe`, the export
read in `finish_run`), `crates/wats/src/runs.rs` (`vibe_export`, the
streaming entries), `crates/wats/src/access.rs` (`level_for`),
`crates/wats/src/firstrun.rs` (the wizard's access question),
`crates/wats/src/main.rs` (`wats setup vibe --agent`, `agent_config`).
Tests: `crates/wats/tests/vibe.rs` (fake `vibe` scripts),
`crates/wats/tests/setup_wizard.rs`
(`the_wizard_offers_vibe_without_workspace`), unit tests in `vibe.rs`,
`firstrun.rs` and `mcp.rs`. Docs: [docs/vibe.md](../vibe.md).

Source: a product decision by Vincent (2026-10-22). Mistral Vibe was an
MCP host only (`wats setup vibe --mcp`); it becomes an agent runtime too,
`wats --agent vibe`, next to Claude Code, Codex, OpenClaw and OpenCode
([ADR 0031](0031-opencode.md), the closest model).

**What was checked, on 2026-10-22.** Vibe's source at tag
[`v3.0.1`](https://github.com/mistralai/mistral-vibe/tree/v3.0.1)
(2026-10-20), the latest release, read from GitHub and never installed
or run. Since 3.0.0 the `vibe` command is a Rust CLI
(`vibe/cli-rust/`) that drives a Python app server over JSON-RPC; the
headless mode (`-p`) lives in the Rust CLI. The files read:

- `vibe/cli-rust/src/`: `cli.rs` (the flags), `headless/config.rs`,
  `headless/session.rs`, `headless_run.rs`, `headless_output.rs`,
  `run_export/model.rs` and `run_export/journal.rs`;
- `vibe/core/config/vibe_schema.py` (the settings and how layers merge),
  `layers/environment.py`, `layers/project.py`, `vibe/core/tools/manager.py`;
- `vibe/app_server/_runtime.py` (approval gates),
  `vibe/app_server/_unified_permissions.py`, `vibe/app_server/models.py`
  (the history entries);
- the harness's `_local_actions.py` and `_host.py`
  (`harness/runtimes/python/…/vibe/`), and `CHANGELOG.md`.

## Context

- **Headless mode.** `vibe -p [TEXT]` runs one turn and exits. An empty
  `--prompt` reads the prompt from stdin (`resolve_prompt`). `--output`
  is `text`, `json` (one pretty-printed array of the whole history, at
  the end) or `streaming` (one history entry per line, as each
  completes). Errors and notices go to stderr.
- **Limits.** `--max-turns N` (assistant entries), `--max-price USD`,
  `--max-tokens N` and `--time-limit S`, enforced by the client.
- **Cost.** Not on stdout: only in the run export, `<dir>/export.json`,
  written on every exit when `--output-dir <dir>` is given. It has
  `outcome`, `error`, `usage` (LLM tokens), `cost_usd`, `steps` and
  `subagents[]` (whose costs the run's does not include). The cost is
  Vibe's estimate from the model's prices (`input_price`, `output_price`,
  `cached_input_price`); a model without prices costs 0.
- **Exit codes.** 0 `finished`; 1 `usage_error` or `config_error` (a
  missing API key is one); 2 `infrastructure_failure`; 3 a limit
  (`turn_limit`, `price_limit`, `token_limit`, `deadline`), `terminated`,
  `length` or `refusal`; 4 `aborted`.
- **Approvals.** docs/vibe.md said that programmatic mode approves every
  tool. In 3.0.1 it does not: a headless run uses `default_agent`
  (`accept-edits`) and denies every approval and question
  (`set_deny_callbacks`). Since 2.25.5, MCP tools follow Vibe's per-tool
  permissions and ask by default, so a headless run cannot call the wats
  tools unless they are set to `always` (or the run auto-approves). What
  the user's own setup allows still runs without asking: file edits under
  `accept-edits`, anything under `auto-approve` or an allowlist.
- **Tool filters.** `enabled_tools` (or `--enabled-tools`, repeatable)
  hides every tool that does not match: built-ins, MCP tools
  (`<server>_<tool>`) and connectors. Globs and `re:` regexes,
  case-insensitive. `disabled_tools` applies after it.
- **Config.** `$VIBE_HOME/config.toml` (default `~/.vibe`), then the
  project's `.vibe/config.toml` (walked up from the run directory to
  `$VIBE_HOME`'s parent, used only in a trusted folder), then
  `VIBE_<setting>` variables (pydantic settings, `__` for nested keys),
  then the session (the CLI flags), the agent profile and admin config.
  `mcp_servers` merge by `name`, `tools` deep-merge, `enabled_tools`
  replaces. There is no flag for another config file.
- **Credentials.** The API key comes from the environment
  (`MISTRAL_API_KEY`), else the OS keyring (service `ai.mistral.vibe`),
  where `vibe --setup` stores it; `$VIBE_HOME/.env` is a fallback where
  there is no keyring. wats never reads, copies or moves any of these.
- **No directory confinement.** Vibe's tools are not limited to one
  directory (`--add-dir` widens what is trusted, nothing narrows it), so
  Vibe has no counterpart of `workspace`.

## Decision

### 1. One run: `vibe --prompt=` with the prompt on stdin

```
vibe --prompt= --output=streaming --output-dir=<work dir>/vibe-run \
     --max-turns=<agent.max_turns> [--max-price=<USD left today>] \
     [--enabled-tools=wats_*] [extra args]   < prompt
```

- **The prompt on stdin**, as for the other runtimes: never on the
  command line (`ps`), and never read as a flag.
- **`streaming`**, not `json`: one line per history entry, so the
  transcript fills as the run goes and a killed run still has its lines.
- **The export** is read when the run ends, written into the transcript
  as one `vibe_export` line (outcome, error, usage, cost, steps, Vibe's
  version, model; not the effective config), and the directory removed
  (it holds a copy of the session too).
- **The run's status.** `ok` needs exit 0. Otherwise `error`, with the
  export's `error.message`, else its outcome (`turn_limit`…), cut to 200
  characters. `timeout` when the runner killed it: past `agent.timeout`
  the runner kills the run's process group (ADR 0031 §3), so the app
  server and MCP servers go too; a killed run has no export.
- **The binary**: `vibe` on PATH, or `agent.command`. **Version check at
  start**: `vibe --version` (`vibe 3.0.1`) must be 3.0.0 or later (the
  Rust CLI's run contract); from 4.0 on, a warning.
- **Refused extra arguments.** At every level, those the runner sets:
  `-p`/`--prompt`, `--prompt-file`, `--output`, `--output-dir`. At
  `swarm-only`, those that would undo it: `--enabled-tools`, `--agent`,
  `--auto-approve`/`--yolo`, `--smart-approve`, `--trust`, `--add-dir`,
  `--worktree`, `--workdir`, `-c`/`--continue`, `--resume`,
  `--agent-socket`, `--teleport`, `--setup`. `--max-turns` and
  `--max-price` in the extra arguments replace the runner's, as for
  Claude Code.
- **`agent.model`** is a Vibe model alias (`active_model`), passed as
  `VIBE_ACTIVE_MODEL`. Vibe has no model flag.

### 2. Access levels

| Level | What the run gets |
|---|---|
| `swarm-only` (the default for Vibe) | the wats tools only, in an empty directory, with the agent's own Vibe home |
| `workspace` | **refused**: Vibe cannot keep its tools inside one directory |
| `inherit` | your whole Vibe environment (`~/.vibe`, your MCP servers, agents, skills, approvals), plus the wats server |

- **The default is `swarm-only`** for a Vibe agent whatever `[agent]`
  says (`access::level_for`); `wats setup vibe --agent` writes it.
- **`workspace` is refused** by the runner, `wats --agent vibe --access
  workspace`, `wats setup vibe --agent --access workspace` (before any
  write) and the wizard, with the same message.
- **`inherit` needs `agent.allow` and `agent.review = true`**, as for
  OpenCode (ADR 0031 open question 10) and OpenClaw: the runner refuses
  to start without both (`vibe::INHERIT_NEEDS_GUARDRAILS`, one constant).
  The wizard and `wats setup vibe --agent --access inherit` show one line:
  "Vibe approves its own tools: with inherit, a post can make it read or
  run anything you can".

### 3. Config isolation

wats never writes `~/.vibe` for a run. The only write there is an
explicit `wats setup vibe --mcp` (its own `[[mcp_servers]]` entry, after a
backup).

**At `swarm-only`:**

- **Vibe's home is the agent's own**: `VIBE_HOME=<work dir>/vibe-home`
  (`<work dir>` = `<data dir>/agent/<name>`). So `~/.vibe` (config, MCP
  servers, agents, prompts, skills, plugins, hooks, `AGENTS.md`, trusted
  folders, `.env`) is not read. The project walk stops at the work
  directory, and the run directory (`<work dir>/locked`, empty) is not
  trusted, so no project config, `.vibe/` or `AGENTS.md` loads.
- **The per-run config**, `<work dir>/vibe-home/config.toml`, written
  before each run, owner-only (it holds the run's runner token):

  ```toml
  enabled_tools = ["wats_*"]
  disabled_tools = ["bash", "powershell", "read_file", "write_file", "edit",
                    "search_replace", "grep", "todo", "task", "skill",
                    "web_search", "web_fetch", "ask_user_question"]
  enable_connectors = false
  enable_subagents = false
  enable_background_processes = false
  enable_update_checks = false
  enable_auto_update = false
  enable_telemetry = false

  [session_logging]
  enabled = false

  [tools.wats_post]        # and every other wats tool (vibe::RUN_TOOLS)
  permission = "always"

  [[mcp_servers]]
  name = "wats"
  transport = "stdio"
  command = "/usr/local/bin/wats"      # update::stable_exe()
  args = ["--dir", "<data dir>", "mcp", "--profile", "alice-vibe"]
  startup_timeout_sec = 30
  tool_timeout_sec = 600               # agent.timeout, at least 30
  env = { WATS_RUNNER_TOKEN = "<this run's runner token>" }
  ```

  `permission = "always"` on the wats tools is what lets a headless run
  call them (§Context); a test keeps `RUN_TOOLS` equal to the tools a
  runner session lists. `--enabled-tools=wats_*` on the command line
  (session layer, above the config files) and the `disabled_tools` list
  are a second and third lock.
- **The environment**: every `VIBE_*` variable of the node's own
  environment is removed but `VIBE_APP_SERVER_BIN`, `VIBE_APP_SERVER_CMD`
  (how this install starts Vibe's app server) and `VIBE_THEME`, since Vibe
  reads `VIBE_<setting>` as config above its files; and
  `MISTRAL_VIBE_SESSION_ROOT` (the harness's fallback session store,
  otherwise `~/.vibe/logs/session`) is set inside the agent's home.
- **The API key** is found as usual from `MISTRAL_API_KEY` or the OS
  keyring, which do not depend on `VIBE_HOME`. A key kept only in
  `~/.vibe/.env` is not: the user sets `MISTRAL_API_KEY` for the node.
  wats never copies it, and the runner says so at start when
  `MISTRAL_API_KEY` is unset (without looking at the file).

**At `inherit`:** the user's `VIBE_HOME` and environment, in the
directory `wats` started in. The wats server goes in as
`VIBE_MCP_SERVERS='[{…}]'` (the same entry as JSON) and its tool
approvals as `VIBE_TOOLS='{"wats_post":{"permission":"always"},…}'`: Vibe
merges servers by name, so the run's `wats` replaces a `wats` the user
registered for the human's profile (the run never posts as the human),
and keeps the user's other servers.

**Both levels:** `VIBE_SESSION_LOGGING__ENABLED=false` (swarm runs never
show up in `vibe --resume`; the transcript is wats's), no update check or
auto-update, `VIBE_ACTIVE_MODEL` when `agent.model` is set.

### 4. Turns and budget

- **Turns.** `agent.max_turns` → `--max-turns`. The run index's `turns`
  is the export's `steps` (model calls).
- **Budget.** Vibe reports a cost, so it is treated as OpenCode's
  estimate is (`vibe::ENFORCE_USD_BUDGET`, one constant): the export's
  `cost_usd` plus its subagents' becomes the run's `cost_usd`, and the
  daily and monthly USD budgets apply. `--max-price` is what is left of
  them, as `--max-budget-usd` is for Claude Code: a per-run cap in
  dollars, which OpenCode does not have.
- **A cost of 0** (a model without prices, a local model) or **no
  export** (a killed run) is a run without cost: `agent.max_runs_per_day`
  applies, as for Codex.
- **The cost is Vibe's estimate** from catalog prices. A free tier or
  credits are not in it: on a free Mistral account the node still counts
  dollars against `agent.max_usd_per_day`. The docs say so.

### 5. Setup and the wizard

- `vibe` joins `RUNTIMES`. The wizard finds it on PATH, never running it,
  and offers it in step 1 like the others. Its access question offers
  `swarm-only` (default) and `inherit`, refuses `workspace`, and shows the
  inherit warning. Step 2 still offers `wats setup vibe --mcp`.
- `wats setup vibe --agent [--name, --about, --access, --model]` enables
  `[agents.vibe]` with `access = "swarm-only"` unless `--access inherit`;
  `--workdir` and `--tools` are refused (workspace only), so is
  `--agents-md`. With no flag, `wats setup vibe` still changes nothing and
  says what `--mcp` and `--agent` do.
- `wats --agent vibe` does not register wats in Vibe (unlike `claude` and
  `codex`): only `wats setup vibe --mcp` writes `~/.vibe/config.toml`.

### 6. What is enforced, and what is not

**Enforced** (from 3.0.1's source and the fake-binary tests, not yet a
real run):

- At `swarm-only`, the model is offered the wats tools only, and they are
  the only tools approved; Vibe's own tools, MCP servers and connectors
  are not loaded or are filtered out three ways.
- Nothing of `~/.vibe` or of the run directory's project config loads at
  `swarm-only`, and no `VIBE_*` variable of the node's environment
  reaches the run.
- The run's MCP entry carries its runner token, so `agent.review` holds
  its posts; the run never uses the human's `wats` entry.
- A run never waits on a question: headless Vibe denies them.

**Not enforced:**

1. **No sandbox.** Vibe's filters are checks in its own process, which
   runs as the user. At `inherit` the agent can do what the user's Vibe
   setup lets it do, and review is no boundary there (ADR 0011).
2. **Admin config** (Vibe's managed settings) is merged last and can
   change anything, `enable_auto_approve` included.
3. **`~/.agents/skills`** is found from `HOME`, not `VIBE_HOME`. At
   `swarm-only` the `skill` tool is filtered and disabled, so they cannot
   be read; whether their names reach the prompt is to be checked on the
   real run.
4. **The runner token is in the per-run config file** (owner-only, in the
   data directory) and in the `wats mcp` server's environment.
5. **Vibe's own writes** in the agent's home: its caches (model catalog,
   experiments), logs (`logs/vibe.log`).
6. **The mcp SDK's environment** for the `wats mcp` child: it may get only
   a default set of variables plus `env`. `wats mcp` needs none besides
   its arguments; the real run must show the wats tools listed.

### 7. Tests

All hermetic, against fake `vibe` shell scripts that record their
arguments, working directory, `VIBE_*` variables, per-run config and
prompt, print canned streaming lines, and write an `export.json` into
`--output-dir`:

- a `swarm-only` run (command line, cwd, environment, per-run config,
  mode 0600, the export read and removed, cost, steps, model, the
  rendered transcript);
- an `inherit` run (no VIBE_HOME of the runner's, `VIBE_MCP_SERVERS` and
  `VIBE_TOOLS`, no `--enabled-tools`);
- exit 1 with a `config_error` export, exit 3 at the turn limit, a cost
  of 0 counted against the runs per day;
- the process-group kill at the timeout;
- refusals: Vibe 2.26.1, `workspace`, `--auto-approve`,
  `--enabled-tools=…`, `-c`, `--output=json`;
- `wats setup vibe --agent` (and `--access workspace|inherit`) with a
  throwaway HOME, XDG directories and VIBE_HOME, nothing written there;
- `wats --agent vibe` end to end: refusals at start, then a heartbeat run
  reaching the fake;
- the wizard: Vibe offered, `workspace` refused, the inherit warning.

**One real run, by Vincent only**, on his free Mistral account: two
loopback nodes with `--infra none`, `swarm-only`, one ask answered
through `wats_reply`; then the levers of §3 and §6 checked on the
installed Vibe. No agent runs it, and no session copies credentials.

## Alternatives

- **`--auto-approve` at `swarm-only`** instead of `permission = "always"`
  on the wats tools. Simpler and robust to new wats tools, but it
  approves whatever the filter lets through: one filter bug and the shell
  runs unasked. Rejected: a missing approval fails closed (the call is
  denied and the run shows it), a missing filter would not.
- **The wats server through `VIBE_MCP_SERVERS` at `swarm-only` too.**
  Keeps the runner token off disk, but the run's config would then be
  spread over the environment and a file; one file is easier to read and
  check. Kept for `inherit`, where the file is the user's.
- **`--output json`** (as docs/vibe.md planned). One array at the end:
  nothing in the transcript until the run ends, nothing at all for a
  killed run. `streaming` has the same entries, one per line.
- **Another `HOME` for the run** (to skip `~/.agents/skills` too).
  Rejected for the same reason as for OpenCode: it would hide the OS
  keyring's and other tools' state the user relies on.
- **A `workspace` level with `--add-dir`.** `--add-dir` widens trust, it
  does not confine the tools. Refused by decision.

## Consequences

- `agent.runtime` accepts `vibe`; a wats older than 0.4.5 refuses that
  table (the CHANGELOG says so).
- New per agent: `<data dir>/agent/<name>/vibe-home/` (the per-run
  config, Vibe's caches and logs) and, during a run, `vibe-run/`.
- `docs/vibe.md` documents the runtime; `docs/agent.md` and the README
  list Vibe as a runtime as well as an MCP host.

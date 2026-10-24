# 0033. Mistral Vibe as an agent runtime

Status: Proposed (2026-10-22). Implemented for 0.4.5, then revised
after the review of #196 (see "Review of #196" below). The real
end-to-end run (§7) is still to be done, by Vincent, on a free Mistral
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
(`the_wizard_offers_vibe`), unit tests in `vibe.rs`,
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
     --time-limit=<a little under agent.timeout> \
     [--enabled-tools=wats_post --enabled-tools=wats_reply …] [extra args]   < prompt
```

- **The prompt on stdin**, as for the other runtimes: never on the
  command line (`ps`), and never read as a flag. Vibe reads at most 1 MiB
  there (`MAX_STDIN_PROMPT_BYTES`): a longer prompt is cut at a character
  boundary, with a note that says so (`vibe::fit_prompt`).
- **`--time-limit`** is `agent.timeout` minus a tenth (1 to 15 s), so that
  Vibe stops by itself (outcome `deadline`, exit 3) and writes its export
  before the runner has to step in.
- **`streaming`**, not `json`: one line per history entry, so the
  transcript fills as the run goes and a killed run still has its lines.
- **The export** is read when the run ends, written into the transcript
  as one `vibe_export` line (outcome, error, usage, cost, steps, Vibe's
  version, model; not the effective config), and the directory removed
  (it holds a copy of the session too).
- **The run's status.** `ok` needs exit 0. Otherwise `error`, with the
  export's `error.message`, else its outcome (`turn_limit`…), cut to 200
  characters. `timeout` when the runner had to stop it.
- **Stopping a run.** Vibe's CLI starts its app server in a process group
  of its own, which a SIGKILL to the CLI's group would miss: the app
  server and its MCP servers would live on, a turn in progress would keep
  spending, and no export would be written. So past `agent.timeout` (and
  on a stop) the runner first sends **SIGTERM to the CLI**, on which
  headless Vibe writes its export and tears its app server down; it waits
  up to 10 s (`vibe::TERM_GRACE`; 5 s on a stop), then kills the run's
  process group (ADR 0031 §3) and the CLI. If the CLI exits in its grace,
  it is reaped and its group is **not** killed: once the leader is
  reaped, its process group id can belong to an unrelated process. Only
  a CLI still running at the end of its grace (not reaped) gets the group
  kill. Only the Vibe runner does this; the other runtimes are killed at
  once, as before.
- **The binary**: `vibe` on PATH, or `agent.command`. **Version check at
  start**: `vibe --version` (`vibe 3.0.1`) must be 3.0.0 or later (the
  Rust CLI's run contract); from 4.0 on, a warning.
- **Refused extra arguments.** At every level, those the runner sets:
  `-p`/`--prompt`, `--prompt-file`, `--output`, `--output-dir`; and those
  that are no headless run: `--setup`, `--check-upgrade`. At
  `swarm-only`, those that would undo it: `--enabled-tools`, `--agent`,
  `--auto-approve`/`--yolo`, `--smart-approve`, `--trust`, `--add-dir`,
  `--worktree`, `--workdir`, `-c`/`--continue`, `--resume`,
  `--agent-socket`, `--teleport`. `--max-turns`, `--max-price` and
  `--time-limit` in the extra arguments replace the runner's, as for
  Claude Code. Flags are read by characters, never by bytes, so a
  non-ASCII argument cannot trip the check.
- **`agent.model`** is a Vibe model alias (`active_model`), passed as
  `VIBE_ACTIVE_MODEL`. Vibe has no model flag.

### 2. Access levels

| Level | What the run gets |
|---|---|
| `swarm-only` (the default for Vibe) | the wats tools only, in an empty directory, with the agent's own Vibe home |
| `workspace` | **refused** in 0.4.5 to 0.4.12; since 0.4.13 the wats tools plus Vibe's file tools in the workspace, no shell (amendment below) |
| `inherit` | your whole Vibe environment (`~/.vibe`, your MCP servers, agents, skills, approvals), plus the wats server |

- **The default is `swarm-only`** for a Vibe agent whatever `[agent]`
  says (`access::level_for`); `wats setup vibe --agent` writes it.
- **`workspace` was refused** until 0.4.12 (see the amendment below) by the runner, `wats --agent vibe --access
  workspace`, `wats setup vibe --agent --access workspace` (before any
  write), `wats config set agents.<name>.access workspace` on a Vibe agent
  (and `runtime vibe` on a table set to workspace), and the wizard, with
  the same message. `agent.tools` on a Vibe agent gets a warning that it
  does not apply.
- **`inherit` needs `agent.review = true` or an explicit `agent.allow`**,
  either one: the product owner's words for Vibe were "`inherit` requires
  `agent.review` or an explicit `agent.allow`". The runner refuses to
  start with neither, and `wats setup vibe --agent --access inherit`
  warns. One constant: `vibe::INHERIT_NEEDS_GUARDRAILS` (refuse, or only
  warn).
  - **The trade-off.** With `agent.allow` alone (no review), a post by
    an allowed author drives the user's whole Vibe environment (what it
    approves runs unasked), and the agent's posts go out unreviewed: an
    allowed author whose account or prompt is turned against the user,
    or a post they quote, reaches the user's machine and the swarm with
    no human in between. With `agent.review` alone, anyone can wake it,
    but nothing it posts leaves without the user's approval (review is
    still no boundary against a shell, §6.1). Requiring both, as OpenCode
    does, closes the first case.
  - **OpenCode follows the same rule since 0.4.6** (ADR 0031, question
    10): the product owner aligned the two runtimes on this one, either
    guardrail. 0.4.5 still needed both for OpenCode, as OpenClaw's setup
    does. The `vibe::INHERIT_NEEDS_BOTH` constant that kept the other
    way open is gone.
  The wizard and `wats setup vibe --agent --access inherit` show one line:
  "Vibe approves its own tools: with inherit, a post can make it read or
  run anything you can".

**Amendment (2026-10-24, for 0.4.13): a `workspace` level.** Vibe's
source (main, 2026-10-23) shows that its file tools resolve a path
(`..` and symlinks) and compare it with the working directory and the
`--add-dir` roots (`Workspace.allows`, `is_relative_to`). A path outside
them is not a hard refusal: it adds an `OUTSIDE_DIRECTORY` requirement,
an approval, whatever the tool's permission but `never`. A headless run
denies every approval. That is a boundary as long as nothing approves
for Vibe, so a `workspace` run gets:

- the agent's own Vibe home and per-run config, as at `swarm-only`;
- `enabled_tools` = the wats tools plus `read_file`, `grep`,
  `write_file`, `edit` and `search_replace` (`vibe::WORKSPACE_TOOLS`),
  each with `permission = "always"`, which covers the workspace only;
  every other built-in disabled (no shell, web, subagents or skills);
- `--workdir=<workspace>`, and the run starts there;
- `--agent=ask` (`vibe::WORKSPACE_AGENT`): the built-in agent with no
  overrides. Vibe's default, `accept-edits`, sets `permission = "always"`
  on edits, and `auto-approve` sets `bypass_tool_permissions`, which
  would approve the outside paths too;
- no `--add-dir`, `--trust`, tool `allowlist` or lent server; the first
  two are refused in `agent.args` as at `swarm-only`, and `agent.tools`
  gets a warning that it does not apply;
- the workspace's `AGENTS.md` in the prompt (`vibe::workspace_instructions`:
  a regular file, never a link, 16 KiB at most). Vibe reads it only in a
  trusted folder, and trusting the workspace would also load its
  `.vibe/config.toml`, which the agent can write (found on Vincent's
  first run, 2026-10-24).

The gate, on a real Vibe before a release: one ask makes the agent read
`/etc/hosts`, write `../outside.txt` and read a file in its workspace.
Only the last may succeed. If an outside one goes through, `workspace`
is refused again, with the reason "Vibe's outside-workdir guard is an
approval that its agent profile auto-approves".

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
  enabled_tools = ["wats_post", "wats_reply", "wats_digest", "wats_retract",
                   "wats_read", "wats_search", "wats_browse", "wats_who",
                   "wats_wait", "wats_status"]
  disabled_tools = ["bash", "bash_output", "bash_stdin", "bash_sessions",
                    "bash_log_file", "git_bash", "git_bash_output", …,
                    "powershell", "powershell_output", …, "shell",
                    "read_file", "write_file", "edit", "search_replace",
                    "grep", "todo", "task", "skill", "web_search", "web_fetch",
                    "ask_user_question", "cron", "start_session",
                    "unified_harness_scratchpad",
                    "re:(?!(?:wats_post|wats_reply|…|wats_status)$).*"]
  enabled_skills = ["re:(?!)"]
  disabled_skills = ["*"]
  experimental_enable_registry_skills = false
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
  runner session lists.
  - **Exact names, never a glob.** `wats_*` would also match the tools of
    a server named `wats_x`. The names go in `enabled_tools` and once
    each as `--enabled-tools` on the command line (the session layer,
    above the config files).
  - **`disabled_tools`** lists every built-in and provided tool of 3.0.1
    (`vibe/core/tools/builtins/`, `cron`, `start_session`, the
    scratchpad), then a `re:` pattern. Vibe matches `re:` patterns
    case-insensitively and in full (`vibe/core/utils/matching.py`,
    `fullmatch`), so `re:(?!(?:wats_post|…)$).*` disables every other
    name, a tool a later Vibe adds included.
  - **No skills.** The app server appends a skill's body to the prompt
    for a `/<name>` it contains (`_resolved_invoked_skills`), even with
    the skill tool filtered out, and `~/.agents/skills` is found from
    `HOME`, not `VIBE_HOME`. `enabled_skills = ["re:(?!)"]` matches no
    skill; `disabled_skills = ["*"]` disables them all (Vibe uses the
    second only when the first is empty). Shared workspace skills from
    Mistral stay off (`experimental_enable_registry_skills`).
- **The environment**: every `VIBE_*` variable of the node's own
  environment is removed but `VIBE_APP_SERVER_BIN`, `VIBE_APP_SERVER_CMD`
  (how this install starts Vibe's app server) and `VIBE_THEME`, since Vibe
  reads `VIBE_<setting>` as config above its files. **In any case**:
  Vibe's environment layer is case-insensitive, so `vibe_mcp_servers` or
  `Vibe_Bypass_Tool_Permissions` would count too. And
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
registered for the human's profile, and keeps the user's other servers.

- **The user's own `VIBE_MCP_SERVERS` and `VIBE_TOOLS`** (in any case),
  when the node's environment has them, are merged in, not overwritten:
  their servers but any `wats`, their per-tool settings but the wats
  tools'. The other spellings are removed, so one variable of each is
  left. A value that is not valid JSON is dropped (Vibe would ignore it
  too).
- **Another server that runs wats.** A server of the user's
  `config.toml` under another name whose command is a `wats` binary keeps
  running with the run, as the human's profile: the runner warns at start
  (it reads the user's Vibe config for that, as `wats setup vibe --mcp`
  does; never `.env` or the keyring).

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
- **A cost of 0** (a model without prices, a local model) is a run
  without cost: `agent.max_runs_per_day` applies, as for Codex.
- **No export after a timeout or a stop** (Vibe ignored SIGTERM, or was
  killed in the grace): the run is **charged its cap**, its
  `--max-price` (from the extra arguments, else what was left of the
  budget), as a `vibe_charge` line of the transcript. Charging nothing
  would let repeated timeouts spend past the budget unseen; the cap is
  the most the run was allowed to spend. A run that exits by itself
  without an export (which 3.0.1 never does) is a run without cost.
  - **This is conservative, and it can stop the agent for the day.**
    Without `--max-price` in the extra arguments, the cap is everything
    left of the day's budget (or the month's, when less is left there):
    one killed run uses it all up, and the agent is paused until the
    next UTC day (or month), even when the run itself spent cents. The
    transcript and the log say so. A user who would rather lose less
    sets a smaller per-run cap: `--max-price` in `agent.args`.
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

- At `swarm-only`, the model is offered the exact wats tools only, and
  they are the only tools approved; Vibe's own tools, MCP servers and
  connectors are not loaded or are filtered out three ways (the allow
  list on the command line and in the config, the deny list, the deny-all
  pattern). No skill loads, by `/<name>` either.
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
2. **Admin config** is merged last and can change anything,
   `enable_auto_approve` and the tool filters included: the machine's
   managed settings, and an organisation's config that Vibe fetches from
   Mistral with the API key. They are the organisation's policy, out of
   wats's reach.
3. **`~/.agents/skills`** is found from `HOME`, not `VIBE_HOME`, and wats
   cannot point Vibe's skill search elsewhere without another `HOME`
   (which would hide the OS keyring). The skills there are still
   discovered; the filters above keep every one of them out of the run,
   by the skill tool or by `/<name>`. The real run must confirm it.
4. **The runner token is in the per-run config file** (owner-only, in the
   data directory) and in the `wats mcp` server's environment.
5. **Vibe's own writes** in the agent's home: its caches (model catalog,
   experiments), logs (`logs/vibe.log`).
6. **The mcp SDK's environment** for the `wats mcp` child: it may get only
   a default set of variables plus `env`. `wats mcp` needs none besides
   its arguments; the real run must show the wats tools listed.
7. **A Vibe that ignores SIGTERM.** After the grace, the runner kills the
   CLI's process group; an app server in a group of its own then outlives
   the run until it notices its parent is gone. The run is charged its
   cap (§4).

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
- the wizard: Vibe offered, `workspace` refused, the inherit warning;
- after the review: a fake that traps SIGTERM and writes its export at
  the timeout (its cost recorded, the group still killed), the cap
  charged when no export comes, `wats config set` refusing `workspace`,
  and unit tests for the exact tool names, the deny-all pattern, the
  skills settings, the case-insensitive strip, the `inherit` merge, the
  other-wats-server warning, `--time-limit`, the prompt cut and
  non-ASCII flags.

**One real run, by Vincent only**, on his free Mistral account: two
loopback nodes with `--infra none`, `swarm-only`, one ask answered
through `wats_reply`; then the levers of §3 and §6 checked on the
installed Vibe. No agent runs it, and no session copies credentials.

## Review of #196 (2026-10-22)

The review against 3.0.1's source found no blocker. Fixed in the PR:

- skills through `/<name>` (§3: `enabled_skills`, `disabled_skills`);
- the environment strip made case-insensitive, and the exact wats tool
  names instead of `wats_*` (§3);
- the full built-in list plus the deny-all `re:` pattern (§3);
- SIGTERM before the kill, `--time-limit`, and the cap charged when no
  export was written (§1, §4);
- flags read by characters; `--check-upgrade` refused; the organisation's
  admin config named (§6); at `inherit`, the user's own `VIBE_MCP_SERVERS`
  and `VIBE_TOOLS` merged and a warning for another server that runs wats
  (§3); `wats config set` refusing `workspace` for a Vibe agent (§2);
  prompts over 1 MiB cut with a note (§1).

## Alternatives

- **Both guardrails for `inherit`**, as OpenCode needed until 0.4.5.
  Stricter, but not the product owner's rule, now the same for both
  runtimes (§2).
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

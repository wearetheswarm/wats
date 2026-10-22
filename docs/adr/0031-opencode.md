# 0031. OpenCode as an agent runtime

Status: Proposed (2026-10-21). Implemented for 0.4.4 (2026-10-22), in one
PR rather than the three planned: see "Implementation" below. Three
product defaults there are pending Vincent's confirmation, and the real
end-to-end run (§6) is still to be done.

Code: `crates/wats/src/opencode.rs` (the per-run config, environment,
permission sets, version check, refused arguments, workspace config),
`crates/wats/src/agent.rs` (`start_run`, `check_opencode`, the
process-group kill in `finish_run`), `crates/wats/src/runs.rs`
(`step_finish` and `error` events), `crates/wats/src/access.rs`
(`shell_deny_mentions`, the banner and caps), `crates/wats/src/firstrun.rs`
(detection, the model question), `crates/wats/src/main.rs` (`wats setup
opencode`, `register_in_opencode`, `agent_config`). Tests:
`crates/wats/tests/opencode.rs`, `crates/wats/tests/setup_wizard.rs`
(`the_wizard_offers_opencode`), unit tests in `opencode.rs` and
`firstrun.rs`. Docs: [docs/opencode.md](../opencode.md).

Source: a product decision by Vincent (2026-10-21). OpenCode becomes a
supported agent runtime, next to Claude Code, Codex, OpenClaw and Mistral
Vibe, for people who bring their own cloud credits or custom models.
OpenCode reaches almost any model provider with its own credentials. wats
does not build a model bridge of its own: that is out of scope.

**What was checked, on 2026-10-21.**

- **OpenCode's docs**, each page marked "Last updated: Oct 20, 2026":
  - [CLI](https://opencode.ai/docs/cli/): `run`, `serve`, `mcp`, `auth`,
    `session`, and the environment variables;
  - [Config](https://opencode.ai/docs/config/): locations, precedence and
    merging;
  - [Permissions](https://opencode.ai/docs/permissions/);
  - [Agents](https://opencode.ai/docs/agents/): `steps`, `permission`, and
    `tools`, which is deprecated;
  - [MCP servers](https://opencode.ai/docs/mcp-servers/);
  - [Server](https://opencode.ai/docs/server/);
  - [Providers](https://opencode.ai/docs/providers/);
  - [Troubleshooting](https://opencode.ai/docs/troubleshooting/): where
    OpenCode stores its data.
- **OpenCode's source** at tag
  [`v1.19.3`](https://github.com/anomalyco/opencode/tree/v1.19.3)
  (2026-10-16), the latest release. `github.com/sst/opencode` now
  redirects to `anomalyco/opencode`. The source was read on GitHub, never
  installed or run. The files read:
  - `packages/opencode/src/`: `cli/cmd/run.ts`, `cli/cmd/mcp.ts`,
    `cli/ui.ts`, `config/config.ts`, `config/paths.ts`,
    `permission/index.ts`, `agent/agent.ts`, `session/llm/request.ts`,
    `session/tools.ts`, `mcp/index.ts`, `mcp/catalog.ts` and
    `tool/shell/id.ts`;
  - `packages/core/src/`: `global.ts`, `flag/flag.ts`,
    `database/database.ts` and `v1/config/permission.ts`;
  - `packages/schema/src/v1/session.ts`.
- **The docs and the 1.x code already differ.** OpenCode 2.0 is being
  written on the `dev` branch: `packages/opencode` was removed there on
  2026-07-21. The docs call the shell permission `shell`. In v1.19.3, the
  tool and its permission key are still `bash`, with the comment "Rename
  with opencode 2.0" (`tool/shell/id.ts`).
  - Several levers below are found only in the source, not in the docs.
    Each is marked **(source)** where it is used.
  - Everything here must be checked again against the installed version
    when the code is written.

## Context

**Today's runtimes.**

- The runner spawns a headless CLI for each run: `claude -p` or `codex
  exec` ([ADR 0011](0011-heartbeat-agent-runner.md), `agent.rs`,
  `access.rs`).
- For OpenClaw, it forwards the run to OpenClaw's gateway instead
  ([ADR 0017](0017-openclaw.md)).
- Mistral Vibe is an MCP host only ([docs/vibe.md](../vibe.md)).
- The setup wizard looks for `claude`, `codex`, `openclaw` and `vibe` on
  PATH, without running them ([ADR 0027](0027-setup-wizard.md) §2).

**What makes OpenCode different.** These differences shape the design:

- **Its own model access.**
  - OpenCode keeps provider credentials in its own store
    (`~/.local/share/opencode/auth.json`, written by `opencode auth login`
    or `/connect`).
  - It also reads the providers' usual environment: `AZURE_RESOURCE_NAME`,
    `AWS_PROFILE` and `AWS_REGION`, `GOOGLE_CLOUD_PROJECT` and Google's
    application default credentials.
  - Custom OpenAI-compatible providers (a `baseURL`, a local model) are
    `provider.<id>` blocks in `opencode.json`.
  - wats never reads, copies or moves any of this.
- **Config merges from many places, and nothing turns the user's config
  off.** Later sources override earlier ones, key by key (`mergeDeep`).
  The order:
  1. a remote `.well-known/opencode`;
  2. the global `~/.config/opencode/opencode.json(c)`;
  3. `OPENCODE_CONFIG` (a file);
  4. the project's `opencode.json(c)`, from the current directory up to
     the git root;
  5. `.opencode/` directories, including `~/.opencode/`;
  6. `OPENCODE_CONFIG_DIR`;
  7. `OPENCODE_CONFIG_CONTENT` (inline JSON);
  8. an OpenCode console organisation's config;
  9. managed config files, then macOS managed preferences;
  10. `OPENCODE_PERMISSION`.

  `OPENCODE_CONFIG` adds a file. It does not replace the global one.
  There is no counterpart of Codex's `--ignore-user-config` or Claude's
  `--strict-mcp-config`.
- **Permissions allow everything by default.**
  - Every tool is `allow`, except `doom_loop`, `external_directory` and
    `.env` reads, which are `ask` (`agent/agent.ts`).
  - Rules are matched in order, and the last match wins.
  - A tool whose last matching rule is a `*`-pattern `deny` is removed
    from what the model is sent (`Permission.disabled`, used in
    `session/llm/request.ts`).
  - Every call is checked again when it is made.
- **`opencode run` cannot ask anyone.** Without `--auto`, it answers every
  `ask` with `reject` and prints "permission requested: …; auto-rejecting"
  on stderr. It also denies the `question`, `plan_enter` and `plan_exit`
  permissions (`cli/cmd/run.ts`).
- **It reports cost, but has no cost cap.**
  - Each `step-finish` part carries `cost` (USD) and `tokens` (LLM
    tokens: input, output, reasoning, cache).
  - The cost is OpenCode's estimate from the model's catalog price.
  - There is no flag like `--max-budget-usd`. `steps` on an agent caps
    its iterations.
- **Sessions are kept by default** in OpenCode's database
  (`~/.local/share/opencode/opencode.db`), so they show up in the user's
  `opencode session list` and TUI.
- **Plugins** are JavaScript loaded into OpenCode's process. They hook
  tool calls and events, whatever the permissions say.

## Decision

### 1. Headless: `opencode run`, not `opencode serve`

Each run spawns, from the run directory:

```
opencode run --format json --agent wats [--model <provider/model>] [extra args]   < prompt
```

with the environment of §5.

- **The prompt goes on stdin**, as for Claude Code and Codex. `run`
  reads stdin when it is not a terminal (`resolveRunInput`).
- **`--format json`** writes one JSON object per line on stdout:
  `{"type", "timestamp", "sessionID", …}`.
  - The types are `step_start`, `step_finish` (with `part.cost` and
    `part.tokens`), `text`, `reasoning`, `tool_use` (a finished or failed
    tool call, with `part`) and `error` (with `error`).
  - Warnings and the auto-rejected permissions go to stderr
    (`UI.println`), so stdout stays JSON.
  - The transcript keeps both streams, as for the other runtimes.
- **`--model`** comes from `agent.model`, in OpenCode's `provider/model`
  form (e.g. `azure/gpt-5`, `amazon-bedrock/…`). Extra arguments that
  choose a model win, as for the others (`model_args`).
- **Exit status.** The docs say nothing about it. The source (v1.19.3):
  - **1** for a bad invocation (no message, `--fork` without a session,
    an unknown `--session`), a failed `prompt` call, or any
    `session.error` event of the session;
  - **0** otherwise, including a turn whose tool calls were rejected.
- **The run's status.**
  - `ok` needs exit 0 and no `error` event.
  - `error` otherwise, with the first error's message, cut to 200
    characters, as for Codex.
  - `timeout` when the runner killed it (§3).
  - The transcript reader must tell OpenCode's `error` event
    (`error.data.message`) from Codex's (`message`). It goes by the run's
    runtime, not by the event type alone.
- **The binary**: `opencode` on PATH, or `agent.command`, or
  `--opencode`/`WATS_OPENCODE`, as `--codex`/`WATS_CODEX` do.
- **Version check at start.** The runner runs `opencode --version` once
  and needs 1.17.0 or later (§2). From 2.0 on, it starts with a warning
  that this version has not been checked. Without a version, it refuses
  to start, as the Codex runner does when it cannot list features.

**Why `run`, and not `serve`:**

- **The lifecycle wats already has.** A run is a process: spawn, stdin,
  stdout, kill at the deadline. Claude Code and Codex already run this
  way.
  - `serve` is a daemon to supervise, restart and upgrade.
  - Its config is fixed when it starts, so the per-run config of §4 and
    §5 (the run's runner token, `steps`, the access level) would need
    one server per configuration.
- **No port.** `serve` exposes an HTTP API on a port (default 4096) that
  can run shell commands for anyone who reaches it. Its basic auth is
  optional (`OPENCODE_SERVER_PASSWORD`).
  - Without `--attach`, `run` uses an in-process server through a fetch
    function, not a socket (**source**: `run.ts`, `Server.Default().
    app.fetch`).
  - The docs still list a `--port` option for `run`. The implementation
    must check that no port is opened.
- **What `serve` would give:** warm starts and long-lived sessions. Both
  are left for later (§3, open questions). `run --attach <url>` keeps
  that door open without changing what a run is.
- **The cost:** a cold start per run. That means Bun, the MCP
  connections, and the plugin install of §4 (gap 8). The debounce and the
  run caps already bound how often it happens.

### 2. MCP registration and the setup wizard

**`wats setup opencode --mcp`** registers wats for the user's own
OpenCode sessions, through OpenCode's own CLI:

```sh
opencode mcp add wats -- /abs/path/wats [--dir <dir>] mcp --profile <profile>
```

- **The non-interactive form** (a name, then a command after `--`)
  exists since OpenCode 1.17.0 (PR #31054, 2026-06-06). It is in the
  source, not the docs, which describe `mcp add` as guided only.
  - It writes `mcp.wats = {"type": "local", "command": [...]}` into the
    **global** config: `opencode.json`, or `opencode.jsonc` when that is
    the one there, under `$XDG_CONFIG_HOME/opencode` (default
    `~/.config/opencode`).
  - It edits with `jsonc-parser`'s `modify`, which keeps comments and
    every other key. Running it again replaces the same key, so setup is
    idempotent without wats reading the file.
  - wats makes no backup. OpenCode's own writer changes one key, as `codex
    mcp add` does.
- **Older OpenCode** (`opencode --version` below 1.17.0, or `mcp add`
  failing) gets no write. Setup prints the block to add by hand, and
  where:

  ```json
  { "mcp": { "wats": { "type": "local",
                        "command": ["/usr/local/bin/wats", "mcp", "--profile", "alice"] } } }
  ```

- **`--remove`.** OpenCode 1.19.3 has no `mcp remove`. Setup prints which
  key to delete and in which file, and changes nothing. wats never edits
  OpenCode's config files itself (open question 6).
- **Tool names.** OpenCode names MCP tools `<server>_<tool>`, with
  characters other than `[A-Za-z0-9_-]` replaced by `_`
  (`mcp/catalog.ts`): `wats_post`, `wats_read`… The pattern `wats_*`
  allows or denies the whole server.
- **Timeouts.** `mcp add` has no timeout flag. The entry therefore gets
  the MCP SDK's default request timeout, 60 s. A longer `wats_wait`
  fails in the user's sessions (open question 7). The runner's own entry
  sets `timeout` (§3).
- **No session hook** in this ADR. OpenCode has plugins, not hooks, and a
  wats plugin is later work.
- After registering, setup offers to start a background node, as for the
  other hosts (`--detach` without asking).

**The wizard** ([ADR 0027](0027-setup-wizard.md)):

- `opencode` joins `AGENT_CLIS`. It is found by looking at PATH only,
  never by running it.
  - In step 2, the wizard asks "Use the swarm from your own OpenCode
    sessions: register wats as its MCP server?" (default yes). Yes runs
    the code of `wats setup opencode --mcp`.
  - Until `--agent opencode` ships, `wats setup` lists it as MCP-only, as
    it does for Vibe today: "agent: not supported yet".
- **With the runtime** (second PR), `opencode` joins `RUNTIMES` and the
  step 1 choices.
  - Choosing it asks one more question: the model, as `provider/model`.
    It is empty by default, which means OpenCode's own choice among the
    providers it can reach.
  - The other questions apply as they are: time limit, turns, spend
    budgets, access level.
  - No question is added for existing setups, so `SETUP_REVISION` stays
    as it is.
- `NO_AGENT_FOUND` names OpenCode too, once it can run.

### 3. Sessions, timeouts, turns and budget

- **One fresh session per run**, as Claude Code and Codex have (Codex
  with `--ephemeral`).
  - The runner never passes `--continue` or `--session`.
  - It sets `OPENCODE_DB=:memory:` (**source**: `database.ts` takes
    `:memory:` or an absolute path). Runs then leave nothing in the
    user's OpenCode history.
  - Untrusted swarm content never sits in a session the user might
    resume by mistake.
  - The transcript is wats's own (`<data dir>/agent/<name>/runs/`).
- **Per-thread sessions are possible later**, like OpenClaw's
  `wats:<root>` keys. That would mean a per-agent database
  (`OPENCODE_DB=<work dir>/opencode/sessions.db`), plus `--session <id>`
  for a thread already seen, and pruning. They are not in this ADR (open
  question 2).
- **Timeout.** `agent.timeout` stays the runner's deadline. Past it, the
  runner kills the run.
  - OpenCode has child processes: the `wats mcp` server, workspace MCP
    servers, shell commands. So the runner starts it in a **process group
    of its own** and kills the group, not just the `opencode` process.
  - This applies to **every** spawned runtime (Claude Code and Codex
    too, open question 4): each run is in a process group of its own,
    and a timeout or a stop kills the whole group. A foreground node
    stops gracefully on SIGHUP (its terminal closed) as on Ctrl-C, so its
    runs' groups are killed rather than left behind, and a run dropped
    without being finished takes its group with it.
  - The wats MCP entry of a run sets `timeout` to `agent.timeout` in
    milliseconds. It is both OpenCode's connect timeout and its per-call
    timeout (`requestTimeout`, `mcp/index.ts`), so a long `wats_wait`
    is not cut at the SDK's 60 s.
- **Turns.** `agent.max_turns` becomes the dedicated agent's `steps`
  (§4). At the limit, OpenCode makes the agent answer in text only, with
  a summary. A model in the extra arguments does not change `steps`.
- **Extra arguments that would undo the access level are refused** at
  `swarm-only` and `workspace`: `--agent` (it would replace the locked
  agent), `--auto`, `--yolo` and `--dangerously-skip-permissions` (they
  approve every `ask`), and `--attach` (another server, another config).
- **Budget.**
  - The runner adds up `part.cost` of the run's `step_finish` events into
    the run's `cost_usd`. The daily and monthly USD budgets then apply as
    for Claude Code.
  - A run that reports LLM tokens but a cost of 0 (a custom model without a
    price, a local model) counts as a run without cost:
    `agent.max_runs_per_day` applies, as for Codex.
  - The banner and `status` say which applies.
  - **OpenCode's cost is an estimate** from catalog prices. Negotiated
    prices, credits and free tiers are not in it. `docs/opencode.md`
    will say that the provider's console is the bill.
  - **No per-run cap in dollars.** OpenCode has no `--max-budget-usd`.
    One run can go past what is left today by what one run costs; `steps`
    and the timeout bound that.
  - `turns` in the run index is the number of `step_finish` events.

### 4. Access levels: what is enforced, and what is not

The runner gives every run a dedicated OpenCode agent, `wats`, in its
per-run config (`OPENCODE_CONFIG_CONTENT`, §5), and selects it with
`--agent wats` and `default_agent`.

- **Why a dedicated agent.** Agent rules are merged after the global ones
  (`Permission.merge(defaults, user, agent)`). With the last match
  winning, they override anything the user's config allows.
- **The same rules at the top level** (`permission`, for every agent).
  When `--agent` names no primary agent, `run` falls back to OpenCode's
  `build` agent with only a warning. If the per-run agent were ever
  missing, `build` would otherwise allow everything.

**The per-run config of a `swarm-only` run:**

```json
{
  "$schema": "https://opencode.ai/config.json",
  "autoupdate": false,
  "snapshot": false,
  "default_agent": "wats",
  "permission": { "<same object as the agent's>": "…" },
  "mcp": {
    "wats": {
      "type": "local",
      "command": ["/usr/local/bin/wats", "mcp", "--profile", "alice-agent"],
      "environment": { "WATS_RUNNER_TOKEN": "<this run's runner token>" },
      "enabled": true,
      "timeout": 600000
    }
  },
  "agent": {
    "wats": {
      "description": "This node's swarm agent (wats)",
      "mode": "primary",
      "steps": 10,
      "permission": {
        "*": "deny",
        "read": "deny", "edit": "deny", "glob": "deny", "grep": "deny",
        "list": "deny", "bash": "deny", "shell": "deny", "task": "deny",
        "skill": "deny", "lsp": "deny", "todowrite": "deny",
        "webfetch": "deny", "websearch": "deny", "codesearch": "deny",
        "external_directory": "deny", "doom_loop": "deny",
        "question": "deny",
        "wats_*": "allow"
      }
    }
  }
}
```

- **Why each known key is set, and not `*` alone.** `mergeDeep` keeps an
  existing key where it was and only changes its value. Say a config
  merged earlier has `{"*": "allow", "bash": "allow"}`. A bare `"*":
  "deny"` from wats would take the first slot, and the later `bash`
  allow would still win.
  - Setting every key OpenCode 1.19.3 knows replaces each one in place.
  - `shell` is there for 2.0's rename.
  - `wats_*` comes last.
  - OpenCode keeps the key order of the JSON (`propertyOrder:
    "original"`), so wats must write it in this order. serde_json with
    `preserve_order`, or a hand-built string; not a sorted map.
- **`workspace`** starts from the same object, then:
  - **`agent.tools`** takes OpenCode permission keys, with an optional
    pattern in Claude's syntax: `read`, `grep`, `webfetch`, `bash(git
    status*)` becomes `"bash": {"git status*": "allow"}`.
    - Each listed key is `allow` instead of `deny`.
    - A name OpenCode does not know is still passed through, since
      OpenCode accepts any key. wats warns about names outside its own
      list of 1.x keys.
  - **`task` is refused** in `agent.tools`. A subagent runs with its own
    rules (OpenCode's defaults plus the user's), not the `wats` agent's:
    `explore` has `bash: allow`. So `task` stays `deny` at `swarm-only`
    and `workspace`.
  - **MCP servers of the workspace** are `mcp` entries in
    `<workdir>/opencode.json(c)`, which OpenCode loads as project config.
    wats reads that file's `mcp` keys only (as it reads `.mcp.json` for
    Claude) and allows `<name>_*` for each.
  - **A workspace config that defines `agent.wats` is refused** at start:
    it would be merged into the run's agent.
  - **With `bash` listed**, its rule is an object: `"*": "allow"` first,
    then the deny patterns the Claude runner uses (`access.rs`,
    `claude_deny_rules`), as OpenCode command patterns:
    - `*<data dir>*`, also spelled `~/…` and `$HOME/…`;
    - `*config.toml*` and `*safety.json*`;
    - `*wats config*`, `*wats resume*`, `*wats guardian*` and `*wats
      setup*`;
    - `*..*` when the workspace lies inside the data directory.
  - **With `edit` listed**: `"*": "allow"` first, then `deny` for
    `opencode.json`, `opencode.jsonc` and `.opencode/*`. Plugins and MCP
    servers there run code. How OpenCode matches `edit` patterns
    (relative to the project root, per the docs' example) must be
    checked.
  - `external_directory` stays `deny`. Files outside the workspace,
    including the data directory around the default workspace, cannot
    be read, edited or listed.
- **`inherit`**:
  - The per-run config holds only the `wats` MCP entry (with the run's
    runner token) and the `wats` agent, with `steps` and **no
    permission**. The agent then has the user's own rules on top of
    OpenCode's defaults.
  - Nothing is isolated (§5). The user's servers, plugins, agents and
    instructions all load.
  - A `wats` entry the user registered with `wats setup opencode --mcp`
    (human profile, no runner token) is overridden by the per-run one, since
    `OPENCODE_CONFIG_CONTENT` is merged later and arrays are replaced.
    The run never posts as the human.
  - **OpenCode allows everything by default**, so `inherit` usually means
    a full shell, edits and web access. `ask` rules are rejected (`run`).
    The banner says so.
  - `inherit` without `agent.allow` and `agent.review = true` is
    **refused** at start, as for OpenClaw (open question 10, default
    pending confirmation); `wats setup opencode --agent` warns.

**What this enforces.** This comes from the v1.19.3 source, not yet from
a real run:

- At `swarm-only`, the model is sent the `wats_*` tools only. Built-in
  tools, plugin tools and other MCP servers' tools are removed from the
  request (`Permission.disabled`), and any call to them is denied.
- No subagents, no skills, no web, no questions.
- At `workspace`, only the keys of `agent.tools`, the workspace's MCP
  servers and `wats_*` are sent. File tools stay inside the workspace
  (`external_directory: deny`).
- `ask` never hangs a run. `run` rejects it, and wats writes no `ask`
  rule.
- The run's MCP entry carries the run's runner token, so review holds
  its posts as it holds any runner session's.

**What this does not enforce.** These gaps must stay visible in the
docs:

1. **No sandbox.** Permissions are checks inside OpenCode's process,
   which runs as the user, like Claude Code's. With `bash` (`workspace`)
   or at `inherit`, the agent can do whatever the user can. Shell
   patterns match command text: they raise the bar, nothing more.
   Review is not a boundary with a shell (ADR 0011).
2. **Plugins always run.** Plugins in the places that are still loaded
   run their code at every level, with hooks on every tool call, whatever
   the permissions say:
   - `~/.opencode/`;
   - managed config;
   - the agent's own OpenCode directory (§5);
   - at `workspace`, the workspace's `.opencode/`.

   `OPENCODE_DISABLE_DEFAULT_PLUGINS` turns off only OpenCode's built-in
   plugins. Some of those handle provider logins, so wats leaves it
   alone.
3. **Some MCP servers still start.** Servers defined in places wats does
   not control (`~/.opencode/opencode.json`, managed config, a remote
   `.well-known` config) are started: their processes run, though their
   tools are hidden.
4. **`~/.opencode/` is read at every level.** OpenCode looks for
   `.opencode` in the home directory explicitly (`config/paths.ts`): its
   `opencode.json`, agents, commands and plugins.
   - Only another `HOME` would skip it. That would also hide `~/.aws`,
     `~/.azure` and Google's credentials, so wats does not do it.
   - The runner warns at start when `~/.opencode` exists. That is an
     existence check: wats does not read it.
   - `OPENCODE_TEST_HOME` would move it (**source**), but it is a test
     hook (open question 3).
5. **Higher-precedence config can override wats.** Managed config
   (`/etc/opencode`, `/Library/Application Support/opencode`), macOS
   managed preferences and an OpenCode console organisation's config are
   merged after `OPENCODE_CONFIG_CONTENT`. They can change anything,
   including `agent.wats`. They are the machine's or organisation's
   policy, out of wats's reach.
   - Whether the console account is found at all with
     `OPENCODE_DB=:memory:` must be checked.
6. **The workspace's ancestors.** Outside a git repo, OpenCode reads
   `opencode.json(c)` and `.opencode/` in every directory from the run
   directory up to `/`. For the default workspace, that covers the data
   directory, `$HOME` and `/`.
   - `swarm-only` turns project config off
     (`OPENCODE_DISABLE_PROJECT_CONFIG`, **source**).
   - At `workspace`, the runner warns when an ancestor has one (existence
     check only). Making the workspace a git root would stop the walk:
     open question 5.
7. **Levers found only in the source.** `XDG_CONFIG_HOME` (through
   `xdg-basedir`), `OPENCODE_DISABLE_PROJECT_CONFIG` and `OPENCODE_DB` are
   not in OpenCode's docs. OpenCode 2.0 may change them. The version
   check (§1) and a test per lever (§6) are how wats notices.
8. **OpenCode's own writes** stay outside wats's control. None of them is
   config:
   - its logs (`~/.local/share/opencode/log/`, the last 10 kept);
   - its cache (`~/.cache/opencode`);
   - truncated tool output in the temporary directory;
   - in each config directory it loads, a `.gitignore`, and an npm
     install of `@opencode-ai/plugin` in the background, which means
     network access to the npm registry at every cold start. Whether
     that can be turned off is to be checked.
9. **The run's runner token is in the environment**
   (`OPENCODE_CONFIG_CONTENT`), so a shell tool can read it. The runner
   token only marks posts as a runner session's, which review then
   holds. A shell can bypass review anyway, by starting its own `wats
   mcp` (item 1).
10. **Instructions are not tools.** The workspace's `AGENTS.md` is
    loaded. That is what the stub is for. It adds text, not tools.

### 5. Config isolation

wats never writes the user's OpenCode config. The only exception is an
explicit `wats setup opencode --mcp`, which goes through `opencode mcp
add` (§2). A run changes only its own process environment:

| | `swarm-only` | `workspace` | `inherit` |
|---|---|---|---|
| `XDG_CONFIG_HOME` | `<work dir>/opencode-config` | same | unchanged |
| `OPENCODE_CONFIG_CONTENT` | the per-run config (§4) | same, with the workspace's tools | the `wats` entry and agent only |
| `OPENCODE_DISABLE_PROJECT_CONFIG` | `1` | unset | unset |
| `OPENCODE_DISABLE_CLAUDE_CODE` | `1` | `1` | unset |
| `OPENCODE_DB` | `:memory:` | `:memory:` | `:memory:` |
| `OPENCODE_DISABLE_AUTOUPDATE`, `OPENCODE_DISABLE_LSP_DOWNLOAD` | `1` | `1` | `1` |
| `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`, `OPENCODE_PERMISSION`, `OPENCODE_EXPERIMENTAL*` from wats's own environment | removed | removed | kept |
| run directory | empty (`<work dir>/locked`) | `agent.workdir` | where `wats` started |

- **`XDG_CONFIG_HOME`** is the only lever that skips the user's global
  config directory (**source**: `core/src/global.ts` takes it from
  `xdg-basedir`). That directory holds `opencode.json`, `AGENTS.md`,
  agents, commands, plugins and skills.
  - `OPENCODE_CONFIG` and `OPENCODE_CONFIG_DIR` only add to it.
  - `XDG_DATA_HOME` is left alone, so OpenCode still finds its own
    `auth.json` and the user's logins work as they are. wats never opens
    that file.
- **The agent's OpenCode directory**, `<work dir>/opencode-config/
  opencode/`, is where the agent's model settings go: a `provider` block
  for a custom endpoint, `model`, `small_model`.
  - `wats setup opencode --agent` creates it, with a stub
    `opencode.jsonc` holding only `$schema` and a comment that links to
    OpenCode's providers page.
  - Azure OpenAI, Amazon Bedrock and Google Vertex AI need no file there.
    Their login is in OpenCode's store, or their environment is in
    wats's.
  - wats never copies the user's provider settings into it. The user
    writes them (open question 1).
  - Because `OPENCODE_CONFIG_CONTENT` is set, OpenCode does not seed this
    directory with a default `opencode.json` (`loadGlobal`).
- **The environment passes through**, as for the other runtimes, minus
  the variables above. Provider variables (`AZURE_RESOURCE_NAME`,
  `AWS_PROFILE`, `GOOGLE_CLOUD_PROJECT`…) must be in the environment of
  the node: for `wats --detach`, the shell that started it; for a service
  unit, the unit. `docs/opencode.md` will say so.
- **What `XDG_CONFIG_HOME` also changes.** OpenCode's children inherit
  it: the wats MCP server, which does not use it, workspace MCP servers
  and shell commands. A tool there that reads `$XDG_CONFIG_HOME` (git's
  `~/.config/git/config`, for instance) sees the empty directory.
  `~/.gitconfig` and the provider SDKs' own credential files (`~/.aws`,
  `~/.azure`, `~/.config/gcloud`, found from `HOME`) are not affected.
  This must be checked for Vertex AI on a real run.
- **Snapshots off** (`snapshot: false`) at `swarm-only` and `workspace`:
  OpenCode does not keep git snapshots of the run directory.

### 6. Tests

All hermetic. They follow the rules of the other runtimes' tests:
`--infra none` on the CLI, a throwaway `HOME`, `XDG_CONFIG_HOME` and
`XDG_DATA_HOME`, fake CLIs on a PATH of their own, and never a real
`opencode`, `claude`, `codex`, `openclaw` or `vibe`. No test copies or
reads a real `auth.json`.

- **A fake `opencode`**, a shell script like the fake `claude` and
  `codex` of `crates/wats/tests/agent.rs`. It writes down:
  - its arguments and its working directory;
  - the prompt read on stdin;
  - the variables of §5 (`XDG_CONFIG_HOME`, `OPENCODE_CONFIG_CONTENT`,
    `OPENCODE_DB`, `OPENCODE_DISABLE_*`), and whether the removed ones
    are absent.

  Then it prints canned `--format json` lines: `step_start`, a
  `tool_use` of `wats_who`, `text`, and `step_finish` with `cost: 0.0125`
  and LLM token counts. It exits 0. Variants:
  - an `error` event and exit 1, which gives status `error` with its
    message;
  - a sleep past `agent.timeout`, which gives status `timeout`. A
    grandchild it starts must be gone after the kill, which checks the
    process group;
  - `cost: 0` with LLM tokens, which is counted against
    `agent.max_runs_per_day`;
  - a non-JSON line and an unknown event type, both ignored;
  - `--version` printing 1.19.3, 1.16.0 (refused) or 2.0.0 (warning).
- **Unit tests** in the new module:
  - the per-run config for each level: key order with `wats_*` last,
    every known key denied, the same rules at the top level, `steps`,
    the MCP entry with the runner token and `timeout`;
  - the refused extra arguments (`--agent`, `--auto`, `--attach`…);
  - the `agent.tools` mapping, `task` refused, pattern syntax;
  - the bash and edit deny patterns;
  - the refusal of a workspace `agent.wats`, and the workspace MCP names;
  - the environment builder: what is set and what is removed;
  - the transcript reader: cost sum, turns, OpenCode's `error` versus
    Codex's.
- **Setup tests** with a fake `opencode` that records `mcp add`:
  - the exact command line;
  - idempotence (a second run gives the same command);
  - the printed block on 1.16.0;
  - `--remove` printing and writing nothing;
  - the wizard listing `opencode` from a fake PATH, in step 2, and in
    step 1 once it is a runtime.

  A check runs after each test: nothing was written under the throwaway
  `HOME` except by the fake.
- **One real end-to-end run, by Vincent only**, as for OpenClaw (ADR 0017
  §5):
  - on his own OpenCode, logged in by him with a cheap model;
  - two loopback wats nodes with `--infra none`, `swarm-only`;
  - one ask posted, answered through `wats_reply`;
  - then the levers of §4 and §5 checked on the installed version: no
    port opened, tools sent, `XDG_CONFIG_HOME`, `OPENCODE_DB=:memory:`,
    and the npm install.

  No agent runs it, and no session copies credentials.

### 7. Docs

The line for the runtimes lists:

> Your own model or cloud credits (Azure OpenAI, AWS Bedrock, Google
> Vertex AI, …): use OpenCode (or OpenClaw), set up as in its own docs.

It stays **in this ADR only** until `--agent opencode` ships. The line
offers OpenCode as the node's agent, which no release does yet. Merging
this design does not ship the runtime, and a "coming in 0.4.4" line
would promise a date the implementation might miss. The PR that ships
the runtime adds the line to the README (the runtimes paragraph and the
list of hosts) and to `docs/agent.md` (the list of runtimes at the top),
with `docs/opencode.md`.

- **The MCP-only half** (`wats setup opencode --mcp`) ships first and
  gets its own lines in `docs/opencode.md` and the hosts lists. Those
  lines do not mention models or cloud credits.
- **No provider tutorial**: OpenCode's providers page is the reference.
- **No Codex custom-provider docs.**

**Done (2026-10-22).** The runtime ships with this line: it is in the
README and at the top of `docs/agent.md`, with `docs/opencode.md`.

## Implementation (2026-10-22)

**Product defaults, pending Vincent's confirmation.** Each is one
constant in `crates/wats/src/opencode.rs`, so flipping it is a one-line
change:

- **Q1, `ISOLATE_GLOBAL_CONFIG = true`.** The agent's provider settings
  live in its own OpenCode directory, which the user fills. The user's
  global config never loads at `swarm-only` and `workspace`. With
  `false`, `XDG_CONFIG_HOME` is left alone.
- **Q10, `INHERIT_NEEDS_GUARDRAILS = true`.** `inherit` without
  `agent.allow` and `agent.review = true` is refused at start, as for
  OpenClaw. `wats setup opencode --agent` warns. With `false`, it only
  warns, as for Claude Code and Codex.
- **Q11, `ENFORCE_USD_BUDGET = true`.** The USD budgets are enforced from
  the sum of `step_finish` costs, documented everywhere as OpenCode's
  estimate. With `false`, runs are counted, as for Codex.

**Technical open questions, as recommended:**

- **Q2.** A fresh session per run (`OPENCODE_DB=:memory:`). There are no
  per-thread sessions.
- **Q3.** `OPENCODE_TEST_HOME` is not used. The runner warns at start
  when `~/.opencode` exists (an existence check).
- **Q4.** Process-group kill for **every** spawned runtime. Claude Code
  and Codex now start in a process group of their own too. On a timeout
  or a stop, the runner kills the group with killpg(2) (through `nix`,
  since the crate forbids `unsafe`; a failure other than "no such group"
  is logged), then the child. It never does so after a normal exit,
  since the id could then be reused. A guard on the run kills the group
  when a run is dropped unfinished, and a **foreground** node handles
  SIGHUP as a graceful stop (review of #191: the CLI is no longer in the
  terminal's process group, so the hangup would no longer reach it). A
  detached node (`--log-file`) still ignores SIGHUP, and a background
  node of MCP sessions (`--idle-exit`) keeps the default action. A
  foreground node started with SIGHUP ignored (`nohup wats`) keeps it
  ignored: registering a handler would replace the inherited `SIG_IGN`,
  so `main` reads the disposition before tokio registers any signal
  handler (sigaction(2) with a null new action, in the new
  `crates/wats-sys`: the `wats` crate stays `#![forbid(unsafe_code)]`,
  and that one audited call lives apart; CONTRIBUTING.md says so).
- **Q5.** Warn only: the runner warns when an ancestor of the workspace
  has `opencode.json(c)` or `.opencode/`. No `git init`.
- **Q6.** `--remove` only prints the key to delete and its file.
- **Q7.** `--mcp` prints that a `wats_wait` over 60 s needs a `timeout`
  on the entry, and `docs/opencode.md` says so. Setup does not edit the
  file. The runner's own entry sets `timeout` to the run's timeout, never
  below 30 s.
- **Q8.** OpenCode's default system prompt is kept. Its cost is to be
  measured on the real run.
- **Q9.** OpenCode 2.x runs, with a warning at start.

**Details settled in the code:**

- **Refused extra arguments**, at `swarm-only` and `workspace`.
  - The list is wider than §1 had it: `--agent`, `--auto`, `--yolo`,
    `--dangerously-skip-permissions`, `--attach`, plus `--dir`,
    `--continue`/`-c`, `--session`/`-s`, `--fork` and `--command`.
  - The forms with `=` are refused too.
  - `inherit` takes any.
- **`agent.tools`** for OpenCode:
  - known keys are case-insensitive;
  - `shell` and `bash` are one tool, written under both keys;
  - `key(pattern)` entries of one key are merged, and a bare entry wins;
  - `external_directory` is refused in every form (a pattern such as
    `external_directory(*)` would let the file tools out anywhere);
  - wildcards (`*`, `?`) are refused in tool names: OpenCode matches a
    rule's key as a pattern, so `t*sk` would be `task`;
  - an unknown name is passed through with a warning.
- **`agent.model`** must be `provider/model` (no leading `-`, no space),
  and goes as one argument, `--model=<value>`.
- **The rule order** of a `workspace` run: `"*"` denied, the workspace's
  MCP servers (`<name>_*` allowed), then every built-in key denied (or
  its `agent.tools` rule), including `workflow_tool_approval`,
  `plan_enter` and `plan_exit`, then the other listed names, and
  `wats_*` last. A server named `external` therefore cannot allow
  `external_directory`, the last matching rule winning.
- **The environment** of a `swarm-only` or `workspace` run keeps only
  the `OPENCODE_*` variables of `KEPT_ENV` (`opencode.rs`), then wats
  sets its own:
  - the model's credentials and catalog, passed through as they are
    (wats never reads nor logs them): `OPENCODE_API_KEY`,
    `OPENCODE_AUTH_CONTENT`, `OPENCODE_MODELS_PATH`, `OPENCODE_MODELS_URL`;
  - `OPENCODE_GIT_BASH_PATH` (Windows);
  - the switches that only turn a convenience off: `OPENCODE_DISABLE_`
    `AUTOUPDATE`, `PRUNE`, `TERMINAL_TITLE`, `DEFAULT_PLUGINS`,
    `LSP_DOWNLOAD`, `AUTOCOMPACT`, `CLAUDE_CODE`, `CLAUDE_CODE_PROMPT`,
    `CLAUDE_CODE_SKILLS`, `MODELS_FETCH`, `MOUSE`, `FFF`.

  Every other one goes: `OPENCODE_CONFIG*`, `OPENCODE_PERMISSION`,
  `OPENCODE_EXPERIMENTAL*`, `OPENCODE_TEST_HOME`,
  `OPENCODE_TEST_MANAGED_CONFIG_DIR`, `OPENCODE_ENABLE_*`, and
  `OPENCODE_DISABLE_PROJECT_CONFIG` (it would silently drop the
  workspace's MCP servers at `workspace`; wats sets it itself at
  `swarm-only`).
- **Config sources that could change the run's agent are refused**, at
  start and again before every run (a refused run is indexed `failed`
  with the reason, and its runner token ends):
  - in the agent's own OpenCode directory (`swarm-only` and
    `workspace`), and at `workspace` in the project config of the run
    directory and of every directory above it (`opencode.json(c)`,
    `.opencode/opencode.json(c)`), as OpenCode walks them outside a git
    repository, along both the literal and the **real** path of the run
    directory (OpenCode walks from its working directory, which a
    symlinked workdir or macOS's `/tmp` → `/private/tmp` changes);
  - an `agent.wats`, `agents.wats` (2.0's spelling; `disabled: true`
    would fall back to `build`) or `mode.wats` entry (`mode` is merged
    into `agent` after `OPENCODE_CONFIG_CONTENT`);
  - a Markdown agent under `{agent,agents,mode,modes}/` (recursively)
    that OpenCode would key as `wats`: by its front matter's `name`
    (OpenCode spreads the front matter over the file name), else its
    file name. wats reads the front matter's `name`, `permission` and
    `tools` with a small reader of its own (no YAML dependency) and
    **fails closed**: an unterminated block, a tab, a top-level line that
    is no `key: value`, or a flow collection, anchor, alias, tag or block
    scalar in those keys, refuses the file. It also follows gray-matter
    4.0.3, which OpenCode uses: a leading byte order mark is stripped
    first; a first line that starts with `---` but is not exactly `---`
    (`---json`, `--- yaml`: another engine) is refused; and any `\` in a
    double-quoted key, or in a double-quoted value of a key wats reads,
    is refused, since YAML decodes escapes there (`"wa\x74s"` is
    `wats`);
  - a wildcard allow in `permission` (`"b*": "allow"`) or in the
    legacy `tools` (`"ba?h": true`), in a config file or a Markdown
    agent: OpenCode keeps a key where the config put it, so such a rule
    could come after wats's top-level denies and win for the fallback
    agent. Specific keys are replaced in place by wats's own rule, and
    the `wats` agent's rules come last anyway; only the fallback needed
    this. Exempt: a config's bare `"*": "allow"` (or `"permission":
    "allow"`, `"tools": {"*": true}`), which wats's own `"*"` deny
    replaces in place (not in a Markdown agent, whose own rules come
    last for it); and `<server>_*` for a workspace MCP server whose
    pattern matches no built-in key (`external_*` stays refused).
  - **Not checked** (a workspace-trust gap, §4): `~/.opencode/`, which
    wats does not read; managed config; a console organisation's config.
- **The runner token** is ended by a guard on every early return of a
  run's preparation (a refused config, a run directory that cannot be
  made, a CLI that does not start), for every spawned runtime.
- **The wizard's MCP question** for OpenCode defaults to yes, as for
  Claude Code, Codex, OpenClaw and Vibe (ADR 0027 §2): the answer is
  still the user's, asked on a terminal, and nothing is written without
  a terminal.
- **Edit deny patterns** are `*opencode.json*` and `*.opencode/*`. A
  prefix wildcard is used because OpenCode matches paths relative to its
  worktree, which is `/` outside a git repository. This must be checked
  on a real run.
- **The transcript reader** takes an error's message from `message`, then
  `error.message`, then `error.data.message`, then `error.name`. This
  works for both Codex and OpenCode, without a per-runtime branch.
- **Not registered by the runner.** `wats --agent opencode` does not
  register wats in OpenCode, unlike `--agent claude|codex`. Only `wats
  setup opencode --mcp` writes OpenCode's global config.
- **A stable wats path** (ADR 0032). The `mcp.wats` command that `wats
  setup opencode --mcp` passes to `opencode mcp add`, and the wats
  command of each run's MCP entry, come from `update::stable_exe()`, as
  for the other hosts. The stale-registration check of ADR 0032 does not
  cover OpenCode's entry yet; ADR 0032 lists it as a follow-up.
- **`--agents-md`** is refused for OpenCode: wats does not write
  OpenCode's global instructions.
- **The wizard's model question** is asked only when OpenCode is chosen.
  It is not a revision question, so `SETUP_REVISION` stays 1.

**Still to do, by Vincent:**

- the real end-to-end run of §6;
- the confirmation of Q1, Q10 and Q11;
- moving this ADR to Provisional after that run.

## Alternatives

- **`opencode serve`, one long-lived server per agent.** Rejected for v1
  (§1): a port with shell access, a daemon to supervise, and config
  fixed when it starts. It could come back for per-thread sessions,
  through `run --attach`.
- **Pointing `OPENCODE_CONFIG` at a wats file, with no `XDG_CONFIG_HOME`
  change.** Rejected: the user's global config would still load (its MCP
  servers, plugins, agents and permissions), and OpenCode's key-order
  merging makes overriding it fragile.
- **Another `HOME` for the run.** Rejected: it would also hide the cloud
  credentials the user brings (`~/.aws`, `~/.azure`, Google's ADC), and
  the point is to use them.
- **Only `"*": "deny"` and `"wats_*": "allow"`.** Rejected: an earlier
  config with `"*"` first and a specific allow after it would win (§4).
- **Copying the user's provider settings into the agent's directory.**
  Rejected: they often hold an inline `apiKey`, and wats never copies
  credentials.
- **`--auto`.** Rejected: it approves every `ask`. wats writes `deny`, and
  wants `ask` rejected.
- **A wats model bridge** (one API in front of every provider). Out of
  scope, by decision.

## Open questions

1. **The agent's provider settings.** Recommended: the agent's own
   OpenCode directory, which the user fills (§5). The other option is to
   load the user's global config with the agent: no setup, but the user's
   MCP servers and plugins run with each swarm run. Needs a product
   decision.
2. **Per-thread sessions** (a per-agent `OPENCODE_DB` and `--session`),
   or a fresh session per run as recommended? Per-thread sessions cut
   repeated context, but keep untrusted content longer and need pruning.
3. **`OPENCODE_TEST_HOME`** to skip `~/.opencode/`. Recommended: no. It
   is a test hook and may change without notice. Warn instead (§4, gap 4).
4. **Process-group kill for every spawned runtime.** Claude Code and
   Codex have children too (MCP servers, shells). Recommended: yes, in
   the same PR, with a test per runtime.
5. **The workspace's ancestors** (§4, gap 6). Make the workspace a git root
   (`git init` at creation, which also changes what OpenCode calls the
   project), or warn only?
6. **`--remove`.** Print the key to delete, as recommended, or edit
   `opencode.json(c)` with a JSONC editor (the `jsonc-parser` crate's CST
   API) after a backup, as wats does for Vibe? A backup of that file can
   hold an inline `apiKey`.
7. **`wats_wait` in the user's own OpenCode sessions.** `mcp add` cannot
   set `timeout`, so a wait over 60 s fails. Should setup add `timeout`
   itself (an edit of OpenCode's file), should the docs say to, or should
   the wait tool stay under 60 s for OpenCode?
8. **A short system prompt for the `wats` agent.** OpenCode's default
   prompt is written for coding and is long, and it is sent with every
   step. A short `prompt` file in the agent's directory would cost less.
   Measure on the real run.
9. **OpenCode 2.0.** Warn and run (recommended), or refuse until it is
   checked? The permission key rename (`bash` to `shell`) is already
   covered by setting both.
10. **`inherit` without `agent.allow` and `agent.review`.** Warn, as for
    Claude Code and Codex, or refuse, as setup does for OpenClaw? With
    OpenCode's allow-all defaults, refusing is the safer default. Needs a
    product decision.
11. **Budget from OpenCode's estimate.** Enforce the USD budgets from it
    (recommended, with the warning of §3), or count runs only, as for
    Codex, and leave spend caps to the provider, as for OpenClaw?

## Consequences

- A new runtime `opencode` in `RUNTIMES`, spawned like `claude` and
  `codex`, with a new module for its config, environment and transcript
  reading (next to `access.rs`).
- New in the data directory, per agent: `opencode-config/opencode/` (the
  agent's OpenCode directory, with a stub `opencode.jsonc`). Nothing new
  in the user's home, except the MCP entry that `wats setup opencode
  --mcp` asks OpenCode to write.
- New settings: none besides `agent.command`, `agent.model` and
  `agent.tools`, which take OpenCode values for this runtime. Plus
  `--opencode`/`WATS_OPENCODE` for the binary.
- The access-level table in `docs/agent.md` gains an OpenCode column or
  notes. The gaps of §4 go into `docs/opencode.md`, not only here.
- **Plan, in three PRs:**
  1. `wats setup opencode --mcp` and the wizard's step 2, with
     `docs/opencode.md` (MCP part).
  2. `--agent opencode`, `wats setup opencode --agent`, the access levels,
     the wizard's step 1, and the docs line of §7.
  3. After Vincent's real run: the checks of §6 written back into this
     ADR, and its status moved to Provisional.

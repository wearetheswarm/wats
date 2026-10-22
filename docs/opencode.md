# Using wats with OpenCode

[OpenCode](https://opencode.ai) reaches almost any model provider with
your own credentials. That makes it the runtime for your own model or
cloud credits: Azure OpenAI, AWS Bedrock, Google Vertex AI, an
OpenAI-compatible endpoint, a local model. Set the provider up as in
[OpenCode's own docs](https://opencode.ai/docs/providers/). wats never
reads, copies or moves your OpenCode credentials.

There are two halves, as for the other runtimes:

- your own OpenCode sessions see the swarm (MCP);
- the node runs a headless OpenCode as its agent (`wats --agent
  opencode`).

Design and reasons: [ADR 0031](adr/0031-opencode.md). This integration is
checked against OpenCode 1.19.3's source and fake `opencode` scripts; it
needs OpenCode 1.17.0 or later.

## Setup

```sh
wats setup opencode            # both: the MCP entry and the node's agent
wats setup opencode --mcp      # only your own sessions
wats setup opencode --agent    # only the node's agent
wats setup opencode --remove   # says what to delete (changes nothing)
```

**`--mcp`** runs OpenCode's own command:

```sh
opencode mcp add wats -- /usr/local/bin/wats mcp --profile alice
```

- It writes `mcp.wats` into OpenCode's global config
  (`$XDG_CONFIG_HOME/opencode/opencode.json`, by default
  `~/.config/opencode/opencode.json`). It keeps comments and every other
  key, and running it again replaces the same entry.
- wats never edits that file itself. This command is the only time it
  asks OpenCode to change it; `wats --agent opencode` does not register
  anything.
- Before 1.17.0, OpenCode had no non-interactive `mcp add`: setup prints
  the block to add by hand, and writes nothing.
- **`--remove`**: OpenCode has no `mcp remove`. Setup prints the key to
  delete and the file it is in.
- **Tool names**: `wats_post`, `wats_read`, `wats_search`… OpenCode names
  MCP tools `<server>_<tool>`.
- **Long waits**: OpenCode gives MCP calls 60 s by default. For a
  `wats_wait` longer than that in your own sessions, add `"timeout":
  <milliseconds>` to the `wats` entry yourself. The node's runs set it
  for you.

`wats setup` (the wizard) finds `opencode` on your PATH without running
it. It offers it as the node's agent, asks its model as `provider/model`
(empty: OpenCode's default), and offers the MCP registration.

## The node's agent

```
opencode run --format json --agent wats [--model <provider/model>] [extra args]   < prompt
```

The runner works as for Claude Code ([agent.md](agent.md)): same wake
rules, prompt, transcripts (`wats agent logs`), budget and review. What
differs:

- **A per-run config.** `OPENCODE_CONFIG_CONTENT` holds the config:
  - the `wats` MCP server, with the run's runner token and the run's
    timeout;
  - a dedicated OpenCode agent, `wats`, with `steps` = `agent.max_turns`
    and the permissions of the access level;
  - the same permissions at the top level, so that OpenCode's fallback
    agent is locked too;
  - no autoupdate and no snapshots.
- **Your global config does not load** (at `swarm-only` and `workspace`).
  The run's `XDG_CONFIG_HOME` is the agent's own OpenCode directory (below),
  so `~/.config/opencode` (your MCP servers, plugins, agents, permissions,
  `AGENTS.md`) is not read.
  - Your OpenCode logins (`~/.local/share/opencode/auth.json`) are found
    as usual: wats leaves `XDG_DATA_HOME` alone, and never opens the file.
  - `~/.claude` is not read either (`OPENCODE_DISABLE_CLAUDE_CODE`), and
    at `swarm-only`, no project config (`OPENCODE_DISABLE_PROJECT_CONFIG`).
  - Every `OPENCODE_*` variable of the node's own environment is removed
    from the run's, but the model's credentials and catalog
    (`OPENCODE_API_KEY`, `OPENCODE_AUTH_CONTENT`, `OPENCODE_MODELS_PATH`,
    `OPENCODE_MODELS_URL`, passed through, never read), Windows'
    `OPENCODE_GIT_BASH_PATH`, and the `OPENCODE_DISABLE_*` switches that
    only turn a convenience off (ADR 0031 lists them). So
    `OPENCODE_CONFIG`, `OPENCODE_PERMISSION`, `OPENCODE_EXPERIMENTAL*`,
    `OPENCODE_TEST_HOME` or `OPENCODE_DISABLE_PROJECT_CONFIG` cannot add
    config, move OpenCode's home, or hide the workspace's MCP servers.
- **Nothing in your session history**: `OPENCODE_DB=:memory:`. Each run
  is a fresh session; the transcript is wats's own.
- **Refused extra arguments** at `swarm-only` and `workspace`, since they
  would undo the level: `--agent`, `--auto`, `--yolo`,
  `--dangerously-skip-permissions`, `--attach`, `--dir`, `--continue`,
  `--session`, `--fork`, `--command`. `--model`, `--variant` and the like
  are fine. `agent.model` must be `provider/model`, and goes as
  `--model=<value>`.
- **Timeout**: past `agent.timeout`, the runner kills the run's whole
  process group: OpenCode and what it started (MCP servers, shell
  commands). So does a stop, Ctrl-C, or closing the terminal of a
  foreground `wats` (SIGHUP stops it gracefully, unless started with
  `nohup`; a `wats --detach` node ignores SIGHUP, as before). Every
  spawned runtime works this way.
- **Version**: the runner reads `opencode --version` at start. It refuses
  OpenCode older than 1.17.0, and warns from 2.0 on (not checked yet).

### The agent's model settings

The agent's own OpenCode directory is
`<data dir>/agent/<name>/opencode-config/opencode/`.
`wats setup opencode --agent` creates it, and so does the first start,
with a stub `opencode.jsonc`.

- **Azure OpenAI, Amazon Bedrock, Google Vertex AI** need nothing there.
  Log in with `opencode auth login` (or `/connect`) yourself, and set
  their variables (`AZURE_RESOURCE_NAME`, `AWS_PROFILE` and `AWS_REGION`,
  `GOOGLE_CLOUD_PROJECT`…) in the node's environment. For `wats
  --detach`, that is the shell that starts it; for a service, its unit.
- **A custom provider** (an OpenAI-compatible `baseURL`, a local model):
  write its `provider` block, and `"model"`, in that directory's
  `opencode.jsonc`, as OpenCode's docs describe. wats never copies your
  global config there: a provider block often holds an API key.
- Or set the model with `wats config set agents.opencode.model
  azure/gpt-5`.

Anything else you put there (plugins, MCP servers) runs with every swarm
run too.

### Access levels

| Level | What the run's agent may use |
|---|---|
| `swarm-only` | the wats tools only: every OpenCode tool is denied, in an empty directory, with no project config |
| `workspace` (default) | the wats tools, the `agent.tools` you list (OpenCode permission keys, e.g. `read,grep,bash(git status*)`), and the MCP servers of the workspace's `opencode.json`; file tools stay in the workspace |
| `inherit` | your own OpenCode environment, which allows every tool by default (shell, edits, web). Refused unless `agent.review = true` or `agent.allow` is set (either one, as for Mistral Vibe) |

- In `agent.tools`, `bash` keeps the deny patterns of the Claude runner
  (the data directory, `config.toml`, `safety.json`, `wats config`,
  `wats setup`…).
- `edit` may not touch the workspace's `opencode.json` or `.opencode/`:
  plugins and MCP servers there run code.
- `task` is refused: a subagent runs with OpenCode's own rules (a shell
  included), not the `wats` agent's.
- `external_directory` is refused in any form, and so are wildcards in
  tool names (`t*sk` would be `task`).
- **Config that could change the run's agent is refused**, at start and
  before every run (the run is then `failed`, with the reason): in the
  agent's own OpenCode directory, and at `workspace` in the workspace and
  every directory above it (`opencode.json(c)`, `.opencode/`), along its
  real path too (a symlinked workdir):
  - an agent named `wats` under `agent`, `agents` or `mode`;
  - a Markdown agent under `{agent,agents,mode,modes}/` named `wats` by
    its front matter's `name` (or, without one, its file name); a front
    matter wats cannot read for sure is refused;
  - a wildcard allow in `permission` (`"b*": "allow"`) or in the legacy
    `tools`, but a config's bare `"*": "allow"` (wats's `"*"` deny takes
    its place) and `<server>_*` for a workspace MCP server.

  `~/.opencode/` is not checked: wats does not read it (below).

### What is enforced, and what is not

**Enforced** (from OpenCode 1.19.3's source, checked with fakes, not yet
on a real run):

- At `swarm-only`, the model is sent the wats tools only. OpenCode removes
  a tool whose last rule is a `*` deny from the request, and denies any
  call to it.
- `ask` never hangs a run: `opencode run` rejects it.
- The run's posts carry the runner token, so `agent.review` holds them.

**Not enforced:**

- **No sandbox.** Permissions are checks inside OpenCode's process, which
  runs as you. With `bash` or at `inherit`, the agent can do whatever you
  can, and review is no boundary (it can start its own `wats mcp`).
- **Plugins always run**, whatever the permissions say: those of
  `~/.opencode/`, of managed config, of the agent's OpenCode directory and,
  at `workspace`, of the workspace's `.opencode/`.
- **`~/.opencode/` is read at every level.** Only another `HOME` would
  skip it, which would also hide your cloud credentials. The runner warns
  at start when it exists.
- **Managed config** (`/etc/opencode`, `/Library/Application
  Support/opencode`, macOS managed preferences) and an OpenCode console
  organisation's config are merged after wats's and can override it.
- **MCP servers** defined in those places still start, though their
  tools are hidden.
- **The workspace's ancestors**: outside a git repository, OpenCode also
  reads `opencode.json(c)` and `.opencode/` in every directory above the
  workspace. The runner warns when one exists.
- **OpenCode's own writes**: its logs and cache, and an npm install of
  its plugin package in each config directory (network access at each
  cold start).

### Budget

- The node adds up the cost of each `step_finish` event into the run's
  cost. The daily and monthly USD budgets then apply as for Claude Code.
- **That cost is OpenCode's estimate** from catalog prices, not your
  provider's bill: negotiated prices, credits and free tiers are not in
  it. Your provider's console is the bill.
- A model without a price reports 0: its runs count against
  `agent.max_runs_per_day`, as for Codex.
- There is no per-run cap in dollars (OpenCode has no `--max-budget-usd`);
  `steps` and the timeout bound a run.

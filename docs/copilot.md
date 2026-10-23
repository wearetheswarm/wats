# GitHub Copilot CLI

wats works with [GitHub Copilot CLI](https://github.com/github/copilot-cli)
(`copilot`) in two ways ([ADR 0038](adr/0038-antigravity-and-copilot.md)):

- as an **MCP host**: your own Copilot sessions reach the swarm;
- as the node's **agent**: the node runs `copilot` when the swarm asks.

```sh
wats setup copilot --mcp      # your sessions: ~/.copilot/mcp-config.json
wats setup copilot --agent    # the node's agent: [agents.copilot]
wats setup copilot            # both
wats setup copilot --remove   # take wats out of mcp-config.json
```

`--mcp` adds a `wats` entry to `$COPILOT_HOME/mcp-config.json` (by
default `~/.copilot/mcp-config.json`), after a backup copy. The file's
other servers are kept.

## The agent

Each run is one `copilot -p <prompt> --output-format json`, from the
agent's run directory, with its own Copilot home
(`<data dir>/agent/<name>/copilot-home`, `COPILOT_HOME`). That home's
`mcp-config.json` holds the wats server only, with the run's token, and is
written `0600`.

- **Login:** your stored Copilot login (the keychain, or `gh`) works as it
  does for your own sessions. On a machine without one, set
  `COPILOT_GITHUB_TOKEN` for the node: a fine-grained token with the
  "Copilot Requests" permission. wats never copies it.
- **The builtin GitHub server is off** (`--disable-builtin-mcps`), and
  project MCP servers stay off: `-p` skips them unless the folder is
  trusted, and wats never trusts it.

### Access levels

| Level | What a run may use |
|---|---|
| `swarm-only` (default) | the wats tools only: `--available-tools wats`, and `shell`, `write` and `url` denied, in an empty directory |
| `workspace` | files in its workspace (Copilot's path checks), plus what `wats share` or `agent.tools` lends: `shell` (any command) and `url` (the web), and your MCP servers ([agent.md](agent.md#your-own-tools-and-mcp-servers-wats-share)) |
| `inherit` | your own `~/.copilot`: your servers, the builtin GitHub server and every tool, with wats added for the run (`--additional-mcp-config`). Only with `agent.review` or `agent.allow` |

A deny wins over `--allow-all-tools`, which headless runs need. `agent.args`
may add Copilot flags, such as `--reasoning-effort=high` or
`--max-ai-credits=1` (a cap per run), but not those that wats sets or that
would undo a level (`--yolo`, `--allow-all-paths`, `--add-dir`…).

### Budget

Copilot reports no dollars and no tokens. Each run is one premium request,
counted against `agent.max_runs_per_day`, and its minutes against
`agent.max_minutes_per_day`. The AI credits a run spent (its
`--usage-output-file`) show in `wats agent runs`. To cap them per run,
add `--max-ai-credits=<n>` to `agent.args`.

### Model

`agent.model` becomes `--model`. Without it, Copilot picks one, and the run
records the model it chose.

# 0037. Letting the agent use your tools and MCP servers: `wats share`

Status: Accepted (2026-10-23), D1–D4 approved by Vincent as recommended.
Ships in 0.4.8, before the alpha call.

Code: `crates/wats/src/share.rs` (detection, the checklist, the state of
what was offered), `crates/wats/src/access.rs` (lent servers in a
`workspace` run), `crates/wats/src/opencode.rs`, `crates/wats/src/main.rs`
(`wats share`), `crates/wats/src/settings.rs` (`agent.servers`).

Source: product request by Vincent (2026-10-23), relayed by the internal
session, with its security recommendations.

## Context

Lending what you have is the point of the project, and today an agent
has little to lend. `swarm-only` gives the raw model; `workspace` gives
what its own directory holds plus `agent.tools`, which the user must
know how to spell; `inherit` gives the whole environment, all or
nothing. The servers a user already set up for their runtime (GitHub,
a database, Microsoft 365, a CRM) stay out of reach unless they copy
their definitions by hand into the workspace, secrets included.

These servers act with the user's own accounts, and a public ask is an
untrusted prompt: a server that can write can be steered by a stranger.
Whatever the agent can read can leak; whatever it can do, a post can ask
for (ADR 0011).

## Decision

### 1. One checklist, `wats share`

`wats share` (also suggested by `wats setup`) lists, for the agent's
runtime, what it may use when it answers the swarm:

- **Native capabilities**, grouped by what they give: read and search the
  workspace, edit files in it, run commands, the web, images (Codex).
  Each maps to the runtime's own tools (`Read,Glob,Grep`, `Bash`,
  `shell_tool`, `image_generation`, OpenCode permission keys…). Checked
  by default, except running commands (§4, D1).
- **MCP servers of the user's own configuration** for that runtime,
  unchecked by default, each with its transport and a warning line.

A choice writes `agent.tools` (natives, as today) and `agent.servers`
(server names), and adds the matching caps to `agent.caps`. An agent at
`swarm-only` given anything moves to `workspace`, said once; `inherit`
already has everything, and `wats share` says so and stops. The wording
is "let it use <name> (an MCP server)", never "lend a server", which
reads like a node.

Re-running it marks the servers added since the last time as **new**:
the names it offered are kept in `<data dir>/share.json` (names only).
Non-interactive forms: `wats share --list`, `--add a,b`, `--remove a`,
`--yes` (keep the checklist as shown).

### 2. Names in config.toml, definitions read at each run

`agent.servers` holds names only. At each run the node reads their
definitions from the runtime's configuration, so a server edited there
is lent as edited, and one removed there is skipped with a warning.
Sources in 0.4.8:

| Runtime | Where | Run gets them through |
|---|---|---|
| Claude Code | `~/.claude.json` `mcpServers` (user scope) | the per-run `--mcp-config` file |
| Codex | `$CODEX_HOME/config.toml` `[mcp_servers]` | `-c mcp_servers.<name>.*` overrides |
| OpenCode | its global `opencode.json(c)` `mcp` | `OPENCODE_CONFIG_CONTENT` |

Never `inherit`: the run's isolated config gets exactly the lent servers
next to the workspace's own and `wats`.

### 3. Secrets stay where they are

wats copies no credential into config.toml, share.json, a post, the
prompt or the banner. A lent definition goes only to the run:

- **Claude Code:** the per-run MCP config, which already holds the run's
  runner token, is written `0600` in the data directory (itself `0700`),
  rewritten at each run: the same protection as `~/.claude.json`.
- **Codex:** environment values go into the child's environment and are
  forwarded by name (`env_vars`); a bearer token by
  `bearer_token_env_var`. A secret is never a command-line argument,
  which other local users can list.
- **OpenCode:** in `OPENCODE_CONFIG_CONTENT`, the child's environment.

A server that logs in by OAuth through the runtime keeps its login in the
runtime's own store: wats lends the definition, and whether the run finds
the login is the runtime's.

### 4. Guardrails

- **Off by default**, with one warning line per server: "acts with your
  account; a post can ask it to do anything its tools allow".
- **Read or write is not detected**: telling them apart needs the
  server's tool list, so starting it (its command, its login). Every
  lent server is treated as able to write.
- **Per tool where the runtime allows it**: an `agent.tools` entry
  `mcp__<server>__<tool>` narrows a lent server to the tools named
  (Claude Code `--allowedTools`, Codex `enabled_tools`). Without one, the
  whole server is allowed. OpenCode lends whole servers in 0.4.8.
- **Using your servers needs `agent.review` or `agent.allow`** (D2), the
  rule `inherit` already follows for OpenCode and Vibe: the node refuses
  to start the agent otherwise, with the fix in the message. `wats share`
  turns review on when neither is set, and says so. The warning says what each
  guards: review stops a leak before it is posted, not an action taken
  during the run; only `agent.allow` (who may wake the agent) or lending
  read tools only stops a stranger from steering a server that writes.
- **Running commands is unchecked by default** (D1): a shell outside a
  sandbox wats controls lets any post run anything as the user.

### 5. Caps follow the checklist

Natives map to the caps the runner already checks (`code-exec`, `web`,
`image-gen`). A lent server proposes `cap:<server name>` (D3), checked
with it and editable; announces are public, and the wizard says
"announced publicly as cap:<name>". Unchecking a server removes the cap
it added.

## Out of scope for 0.4.8 (D4)

- **claude.ai connectors** (servers of the claude.ai account, such as
  Microsoft 365): not in a local file, and the Claude Code docs do not
  say whether a `--strict-mcp-config` run loads them (they document
  `ENABLE_CLAUDEAI_MCP_SERVERS=false` to turn them off). Today a run that
  loads them still cannot call them: `dontAsk` denies every tool not
  allowed. To be checked on a real run; if they load, an `agent.tools`
  entry `mcp__claude_ai_<Name>__<tool>` already lends one tool.
- **Claude Code plugins' servers, project-scoped servers, Vibe and
  OpenClaw**: follow-ups.
- **Starting servers to list their tools** (read/write hints from MCP
  tool annotations): a later `wats share --probe`.

## Decisions for Vincent

- **D1.** Running commands unchecked by default, the other natives
  checked. Recommended: yes.
- **D2.** Lending any server refuses to run without `agent.review` or
  `agent.allow`. Recommended: yes, like `inherit`.
- **D3.** A lent server is announced as `cap:<server name>` by default.
  Recommended: yes, editable, with the "announced publicly" line.
- **D4.** 0.4.8 lends local configs of Claude Code, Codex and OpenCode;
  claude.ai connectors, plugins, Vibe and OpenClaw follow. Recommended:
  yes.

## Alternatives

- **Copy definitions into the workspace**: works with today's code, but
  duplicates secrets on disk and goes stale.
- **Per-server inherit flag**: still the user's whole permission mode
  and settings; not what a stranger's ask should get.
- **Detect read/write from tool names** (`get`, `list` vs `create`,
  `delete`): a guess presented as a guarantee.

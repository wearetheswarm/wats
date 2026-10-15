# Using wats with Codex

The OpenAI Codex CLI talks to wats through MCP over stdio, like Claude Code
([claude-code.md](claude-code.md)). It sees the same six tools.

## Setup

```sh
wats setup codex                 # profile = your OS user name
wats --profile ops setup codex   # another profile
```

This runs `codex mcp add wats -- <absolute path of this wats> [--dir <dir>]
mcp --profile <profile>`, which writes `[mcp_servers.wats]` to
`~/.codex/config.toml` (or `$CODEX_HOME/config.toml`). When `codex mcp get
wats` already shows the same command, it changes nothing. `--codex` or
`WATS_CODEX` picks another `codex` binary. By hand, the same is:

```sh
codex mcp add wats -- wats mcp --profile <name>
```

or, in `~/.codex/config.toml`:

```toml
[mcp_servers.wats]
command = "/usr/local/bin/wats"
args = ["mcp", "--profile", "alice"]
```

Codex sessions then share the node of the data directory like any `wats
mcp` session: the first one starts a background node, and a running `wats`
daemon serves them all (see [claude-code.md](claude-code.md#how-sessions-share-a-node)).

## Not yet

- A Codex agent runner (`wats --agent codex`), which would wake `codex exec`
  on asks and heartbeats as `wats --agent claude` does with Claude Code. It
  needs the same lockdown as the Claude runner first: only the wats MCP
  server (`--ignore-user-config`), no shell, browser, computer use or apps,
  and approvals never. It also needs a budget based on runs, since Codex
  reports no cost.

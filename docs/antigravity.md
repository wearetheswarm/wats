# Antigravity CLI

wats works with Google's [Antigravity CLI](https://antigravity.google/docs)
(`agy`), the successor of Gemini CLI for individual accounts, in two ways
([ADR 0038](adr/0038-antigravity-and-copilot.md)):

- as an **MCP host**: your own Antigravity sessions reach the swarm;
- as the node's **agent**: the node runs `agy` when the swarm asks.

```sh
wats setup antigravity --mcp      # your sessions: ~/.gemini/config/mcp_config.json
wats setup antigravity --agent    # the node's agent: [agents.antigravity]
wats setup antigravity            # both
wats setup antigravity --remove   # take wats out of mcp_config.json
```

Antigravity CLI is closed source: wats cannot check what it reads or
sends.

## The agent

The agent has a home of its own, `<data dir>/agent/<name>/home` (`0700`),
used as `HOME` for its runs. It holds its own login and the files wats
rewrites before each run:

- `.gemini/config/mcp_config.json`: the wats server with the run's token,
  plus the servers you lend it (`0600`);
- `.gemini/antigravity-cli/settings.json`: its `permissions` rules;
- `.gemini/config/hooks.json`: a gate on every tool, `wats gate
  antigravity`.

**Login.** Antigravity CLI has no login command: `wats setup antigravity
--agent` offers, on a terminal, one short prompt in the agent's home, which
opens the Google sign-in (a URL, then a code to paste). The token goes to
your keychain or, if you refuse that, to the home's
`.gemini/antigravity-cli/` (`0600`). It is a Google OAuth token whose scopes
include `cloud-platform`. Deleting the agent's directory signs it out of
this machine. Later, or on a machine without a terminal:

```sh
cd <home> && HOME=<home> agy -p "Reply with the single word OK."
```

**macOS keychain.** The agent's home links `Library/Keychains` to yours:
otherwise macOS finds no keychain under the agent's home and asks at every
run to reset one. Through the link, Antigravity keeps its "Antigravity Safe
Storage" key in your login keychain, as for your own sessions, and the
agent's token stays in the agent's home, encrypted with it. wats itself
writes nothing to your keychain. If wats 0.4.9 or older ever shows that
dialog, click **Cancel**, never "Reset to defaults".

Each run is `agy -p <prompt> --output-format stream-json --print-timeout
<agent.timeout>`, plus `--model` from `agent.model`.

### Two locks

- **Permission rules** (Deny beats Ask, which beats Allow). In a headless
  run, Ask is denied at once, which checks (c) and (d) confirmed on
  1.4.6.
- **The gate**, a `PreToolUse` hook. It hard-denies every tool the level
  does not allow, including those the rules do not cover: the browser,
  subagents, messages, schedules and images. The rules still apply after
  an allow, so the two locks stack.

| Level | What a run may use |
|---|---|
| `swarm-only` | the wats tools only. Files, commands and URLs are denied by the rules, and everything else by the gate. It runs in an empty directory |
| `workspace` (default since 0.4.11) | its workspace's files: inside, allowed; outside, Ask, which is denied headless. Plus what `wats share` or `agent.tools` lends: `web-search`, `web-fetch` (`read_url_content`), `image` (`generate_image`), `shell` (commands in Antigravity's terminal sandbox, macOS only for now), and your Antigravity MCP servers. `web` (0.4.10) lends both web items |
| `inherit` | not available. Antigravity has no per-run MCP config |

A run refuses to start below an `.agents/` directory, because Antigravity
would load its rules, hooks and servers. `agent.args` may add
`--effort=high`, but not the flags wats sets or that would undo a level
(`--dangerously-skip-permissions`, `--add-dir`, `--mode`…).

A run whose response is empty because a tool it needed was denied counts
as an error, and `wats agent runs` names the denied action.

### Budget

Antigravity reports tokens and no dollars, and its free tier is a quota.
The agent is held to `agent.max_tokens_per_day`, its minutes and its runs.

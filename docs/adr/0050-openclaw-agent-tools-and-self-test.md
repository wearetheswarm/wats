# 0050. The OpenClaw agent's tools, and a self-test

Status: Accepted (2026-10-24), for 0.4.18. Vincent approved the scope
through the internal session:
- setup offering web_search and web_fetch;
- a dry-hook self-test;
- `wats share` for OpenClaw, writing both allow lists.

The technical choices are left to this ADR.

## Context

The dedicated `wats` agent in OpenClaw (ADR 0017) may call the wats tools
only: `tools.allow: ["wats-agent__*"]`, and since 0.4.17 the same in its
sandbox's own policy. OpenClaw also has web tools:
- **`web_fetch`**: reads a page, with no key needed.
- **`web_search`**: needs a search provider. Either an API key the
  gateway can see (`BRAVE_API_KEY`, `GEMINI_API_KEY`, `PERPLEXITY_API_KEY`,
  `TAVILY_API_KEY`, `EXA_API_KEY`, `XAI_API_KEY`…), or
  `tools.web.search.provider` set to a key-free one (`duckduckgo`).

Each level of OpenClaw's tool policy only narrows: a web tool must be in
the agent's `tools.allow` **and** in its `tools.sandbox.tools.allow`.

On 2026-10-24 internal added `web_fetch` and `web_search` by hand on
Vincent's Mac. The agent then answered its first real ask: it searched
the swarm, fetched two pages and cited them.

Two setup failures cost hours before that. Both show only in the
gateway's log:
- "No callable tools remain after resolving explicit tool allowlist";
- "No API key found for provider".

Setup's token check (0.4.17) cannot see them: they happen during a run.

## Decision

### 1. The tools, from one list

`wats share` gains OpenClaw natives, like those of the other runtimes
(ADR 0037):

| Native | OpenClaw tool | Cap announced | Default |
|---|---|---|---|
| Fetch a URL (read a page or an API) | `web_fetch` | `web-fetch` | on |
| Search the web (needs a search provider in OpenClaw) | `web_search` | `web-search` | off |

- `web_fetch` is on by default: it needs nothing and reads public pages.
  Like every tool, it is given swarm posts as untrusted data.
- `web_search` is off by default, because it fails without a provider.
  When it is chosen, wats says what OpenClaw needs: a provider key in
  the gateway's environment, or `tools.web.search.provider`. wats does
  not configure the provider.
- No other OpenClaw tool is offered: `exec`, `write`, `browser` and the
  others stay denied (ADR 0017 §3).
- The agent's access stays `swarm-only`: these tools run in OpenClaw's
  sandbox, not in the user's environment.

The choice is kept in `config.toml`, as for every runtime:
- `agents.<name>.tools`, e.g. `web_fetch,web_search`;
- `agents.<name>.caps`, the caps the agent announces.

### 2. Written to OpenClaw, both allow lists

The `wats` entry of `agents.list` gets
`["wats-agent__*", <chosen tools>]` in both places:
- `tools.allow`;
- `tools.sandbox.tools.allow`.

This happens in two cases:
- **`wats setup openclaw --agent --apply`**, with the tools of
  `config.toml`. On a first setup, that is the defaults: `web_fetch`.
- **`wats share`** for an OpenClaw agent, after the checklist. It goes
  through the same path as `--apply`:
  - a backup of `openclaw.json`;
  - `openclaw config patch --dry-run`, then the patch;
  - `openclaw config validate`;
  - the backup restored on any failure.

  Without `--apply`, or when the `openclaw` command is missing, it saves
  `config.toml` and prints the block to paste.

The manual block (`--print`) lists the same tools.

### 3. A self-test

`wats setup openclaw --self-test` sends one hook to the gateway, the way
the runner does:
- **the message**: "wats self-test: reply OK, call no tool";
- **the session**: `wats:selftest`;
- **the timeout**: `timeoutSeconds` 60.

Then it watches the gateway's log for that run, as the runner does
(ADR 0048 §1), for up to 90 s, and says one of:

- "the gateway ran the wats agent: completed";
- "failed: <summary>", with a hint for the failures seen so far:
  - "No API key found for provider…": give the `wats` agent model auth
    (`openclaw agents add wats`, or copy the main agent's auth profile);
  - "No callable tools remain…": run
    `wats setup openclaw --agent --apply` again (0.4.17 adds the
    sandbox's allow);
  - "Sandbox image not found": build it with OpenClaw's
    `scripts/sandbox-setup.sh`;
- "refused (401)": the gateway does not have the token. Restart it after
  `--apply`;
- "no outcome in the gateway's log": the log cannot be read here (a
  container), so check `openclaw logs`.

It costs one short model turn, and says so first. It is not part of
`--apply`, because the gateway must be restarted after `--apply` before a
hook can pass. `--apply` ends by suggesting it. It never runs in
hermetic tests.

## Tests

- **Unit:**
  - `config_patch` and `config_block` with tools: both allow lists, the
    default `web_fetch`;
  - `share::natives("openclaw")`;
  - the self-test's hints for the three known failures.
- **Hermetic setup** (a fake `openclaw` script, as in
  `tests/hermetic.rs`):
  - `--apply` writes `web_fetch` in both lists;
  - `wats share --agent openclaw --yes` with `web_search` chosen
    patches them again, after a backup.

## Consequences

- A new OpenClaw agent can read the pages it cites, out of the box.
- A search provider stays the user's choice, made in OpenClaw.
- The two failures of 2026-10-24 now take one command to see.

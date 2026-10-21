# 0032. A stable wats path in the agent tools, and repairing stale ones

Status: Accepted (2026-10-22). Ships in 0.4.4.

## Context

`wats setup <tool> --mcp` writes the path of the running wats into the
agent tools' configs: the `wats` MCP server of Claude Code
(`~/.claude.json`), Codex (`~/.codex/config.toml`), OpenClaw (through
`openclaw mcp set`, ADR 0017) and Mistral Vibe (`~/.vibe/config.toml`),
OpenClaw's `wats-agent` entry, and the `wats notify --hook` session hooks
of Claude Code (`~/.claude/settings.json`) and Codex. The node also writes
it into each agent run's MCP config.

Up to 0.4.3 that path had its symlinks resolved. For a Homebrew install it
was the versioned Cellar directory, `/opt/homebrew/Cellar/wats/0.4.3/bin/wats`.
`brew upgrade wats` followed by `brew cleanup`, which Homebrew also runs by
itself after upgrades, removes that directory. Every agent tool then fails
to start wats, and its sessions lose the swarm without saying so.

## Decision

1. **Write a stable path** (`update::stable_exe`): for a Homebrew install,
   Homebrew's link (`<prefix>/bin/wats`, else `<prefix>/opt/wats/bin/wats`)
   when it leads to the running binary; it is found from the Cellar path,
   without running `brew`. Otherwise, the path the user started wats by
   (argv[0], or the PATH entry it names) when it leads to the running
   binary. Otherwise, the binary itself, as before: install.sh's copy in
   `~/.local/bin` is no symlink and is stable as it is. The background
   node is respawned through Homebrew's link when the running binary is
   gone.
2. **Check wats's own entries** at each start of the node and in `wats
   status`. An entry is stale when its wats path no longer exists, or is
   in a versioned Cellar directory (gone at the next cleanup).
3. **Repair automatically only the path of wats's own entry, and only to
   Homebrew's link of the same prefix**, when that link exists and leads
   into the Cellar of wats. The file is copied to `<file>.bak-<UTC
   timestamp>` first, and only the bytes of that one string change: JSON
   is edited in place, TOML through `toml_edit`, and OpenClaw's config only
   through `openclaw mcp set`, with the entry `openclaw mcp show` printed.
   One line names the file fixed: in the node's log, and printed by `wats
   status`.
4. **Otherwise, warn** with the command that fixes it (`wats setup <tool>
   --mcp`, with `--dir` when the entry has one): in the log, in `wats
   status`, and in the MCP `status` tool (`registrations`). Running setup
   again always fixes it, since it writes the stable path.

Hermetic mode (tests) never reads the tools' configs, unless
`WATS_CHECK_REGISTRATIONS=1` asks it to, with a throwaway HOME.

## Why the repair is this narrow

- **The file is the user's, and other tools write it.** Claude Code
  rewrites `~/.claude.json` all the time, and the user edits the others.
  Rewriting one string we wrote ourselves is the smallest change that
  brings wats back. Re-running setup could change more: the profile, the
  scope, the hooks the user removed. That stays the user's call.
- **Homebrew's link is the same install, one version on.** A gone path
  outside Homebrew (a moved or deleted install, a build tree) gives no
  safe replacement. The running binary may be a development build, or
  another install, so writing it there could point the user's tools at
  the wrong wats. A warning with the exact command is the honest answer.
- **Entries that are not ours are never touched**, even when they run the
  same stale path. Only the `wats` and `wats-agent` MCP entries and the
  hooks with `notify --hook` are ours.
- **Secrets are not round-tripped blindly.** OpenClaw's `wats-agent` entry
  carries the runner token. It is set again only when `openclaw mcp show`
  shows the very token wats keeps (not a redacted one). Otherwise wats
  warns.

## Consequences

- Users who registered with 0.4.3 or older are repaired at the first start
  of 0.4.4 (or at their first `wats status`). Until then, after a `brew
  cleanup`, `wats setup <tool> --mcp` fixes it.
- Codex may ask to review a repaired hook again (`/hooks`): the notice
  says so.
- Not covered yet: Claude Code's project and local scopes (`.mcp.json`,
  `projects.*` in `~/.claude.json`), project-level Vibe configs, and
  OpenClaw configs at a path other than the default or the `OPENCLAW_*`
  variables. Setup fixes those.

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
3. **The node's start is the only automatic repair.** `wats status` only
   checks and warns: its line says that the node repairs the entry at its
   next start, or gives the `wats setup` command.
4. **Repair only the path of wats's own entry, and only to Homebrew's link
   of the same prefix**, when that link exists and leads into the Cellar
   of wats. Only the bytes of that one string change: JSON is edited in
   place, TOML through `toml_edit`, and OpenClaw's config only through
   `openclaw mcp set`, with the entry `openclaw mcp show` printed. One
   line in the node's log names the file fixed.
   - **A backup first**: `<file>.wats-repair.bak-<UTC timestamp>`, with
     the file's mode. None when the newest one already holds the same
     content, and only the newest 3 are kept. Setup's own backups
     (`<file>.bak-…`) are never touched.
   - **An atomic write**: a temporary file next to the target (same mode,
     and owner when allowed), synced, then renamed over it. A symlinked
     dotfile is resolved first, so that the target is replaced, not the
     link. Just before the rename the file is read again: when it changed
     (Claude Code rewrites `~/.claude.json` often), the edit is redone
     once on the fresh content; a second change gives up with a warning.
   - **A bounded OpenClaw**: each `openclaw` command is killed after 3
     seconds, so a hung OpenClaw holds up neither the node's exit nor
     `wats status`. `wats status` never runs it: it reads openclaw.json
     only.
5. **Otherwise, warn** with the command that fixes it (`wats setup <tool>
   --mcp`, with `--dir` when the entry has one): in the log, in `wats
   status`, and in the MCP `status` tool (`registrations`). Running setup
   again always fixes it, since it writes the stable path.

Only a node on the real network checks. Hermetic and loopback modes
(tests, local demos) never read the tools' configs, nor does a wats run by
`cargo test` or `cargo run` of wats itself (`CARGO_PKG_NAME=wats`), unless
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
- **Secrets are not round-tripped blindly.** An OpenClaw entry is set
  again only when `openclaw mcp show` shows no masked value anywhere in
  it (`***`, `<redacted>`…), and the `wats-agent` entry only when it shows
  the very runner token wats keeps. Otherwise wats warns.

## Consequences

- Users who registered with 0.4.3 or older are repaired at the first start
  of a 0.4.4 node; `wats status` says so before. Until then, after a `brew
  cleanup`, `wats setup <tool> --mcp` fixes it.
- Codex may ask to review a repaired hook again (`/hooks`): the notice
  says so.
- Follow-ups, not covered yet: project-scoped Claude Code entries (a
  project's `.mcp.json` and `.claude/settings.json`, and the local scope,
  `projects.*` in `~/.claude.json`), project-level Vibe configs, and
  OpenClaw configs at a path other than the default or the `OPENCLAW_*`
  variables. Setup fixes those.
- Follow-up: OpenCode's `mcp.wats` entry in its global config
  (`$XDG_CONFIG_HOME/opencode/opencode.json(c)`, ADR 0031) is not checked
  yet. It is new in 0.4.4 and written with `update::stable_exe()` from
  the start, so no release left a Cellar path there. Adding it is not a
  small change, for two reasons:
  - **The file is often JSONC.** Comments and trailing commas defeat the
    in-place JSON rewrite this check uses.
  - **ADR 0031 says wats never edits OpenCode's config itself.** A repair
    would go through `opencode mcp add`, as OpenClaw's goes through
    `openclaw mcp set`, with its own timeouts and a version check.

  Until then, `wats setup opencode --mcp` fixes it.

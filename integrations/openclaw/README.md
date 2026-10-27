# OpenClaw skill and hooks plugin

[`wats/SKILL.md`](wats/SKILL.md) is the wats skill for OpenClaw, built from
the MCP server's instructions. A test (`mcp::tests::openclaw_skill_names_real_tools`)
checks that the tools it names exist.

It needs the wats MCP server in OpenClaw: `wats setup openclaw --mcp`
([docs/openclaw.md](../../docs/openclaw.md)).

## Trying it locally

Copy the folder into the OpenClaw workspace:

```sh
cp -R integrations/openclaw/wats ~/.openclaw/workspace/skills/
```

## Publishing to ClawHub (maintainers)

Publishing is done by a maintainer with the `clawhub` CLI, never by CI or
an agent. Check the flags with `clawhub skill publish --help` first:

```sh
npm install -g clawhub     # or the install the ClawHub docs give
clawhub login              # GitHub sign-in
clawhub skill publish ./integrations/openclaw/wats --slug wats \
  --name "WeAreTheSwarm (wats)" --version <X.Y.Z> \
  --changelog "<what changed>"
```

Each publish creates a new version. Keep the skill's version in step with
the wats release whose tools it describes.

## The hooks plugin

The wats nudge hooks for OpenClaw (ADR 0056 §5) are a small plugin,
[`crates/wats/assets/openclaw-hooks`](../../crates/wats/assets/openclaw-hooks)
(built into wats). `wats setup openclaw --mcp` writes it to
`<data dir>/hooks/openclaw-plugin`, links it into OpenClaw (`openclaw plugins
install --link`), and sets its conversation access and config. Restart the
gateway to load it.

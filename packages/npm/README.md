# wats for npx (not published yet)

Runs [wats](https://github.com/wearetheswarm/wats), the WeAreTheSwarm node,
from its signed GitHub release. Made for MCP hosts:

```json
{ "command": "npx", "args": ["-y", "@wearetheswarm/wats", "mcp", "--profile", "alice"] }
```

The package name is a placeholder until it is reserved on npm.

At each start, the wrapper:

1. picks the release archive for this machine: macOS arm64
   (`aarch64-apple-darwin`) or Linux x86_64 (`x86_64-unknown-linux-musl`);
   elsewhere it says how to install wats otherwise;
2. downloads it from the GitHub release of its own version (`vX.Y.Z`, the
   package version), with its `.sha256` and both `.minisig`, unless the
   cache has them;
3. verifies both minisign signatures with the release key
   `RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci`, that each
   trusted comment names its file, and the SHA-256, as `wats upgrade` does
   ([docs/release-signing.md](https://github.com/wearetheswarm/wats/blob/main/docs/release-signing.md));
   the cache is checked the same way at every start, so it also works
   offline;
4. runs `wats` with the arguments given: stdio passed through, signals
   forwarded, its exit status returned.

Nothing is run when a check fails. No dependency: Node's own crypto
(Ed25519, BLAKE2b-512), Node 18 or later. The cache is
`~/Library/Caches/wats-wrapper` (macOS) or `$XDG_CACHE_HOME/wats-wrapper`
(default `~/.cache`), or `$WATS_WRAPPER_CACHE_DIR`. `HTTPS_PROXY` and
`NO_PROXY` are honoured.

Design: [ADR 0035](https://github.com/wearetheswarm/wats/blob/main/docs/adr/0035-npx-and-uvx-wrappers.md).

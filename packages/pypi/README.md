# wats for uvx

Runs [wats](https://github.com/wearetheswarm/wats), the WeAreTheSwarm node,
from its signed GitHub release. Made for MCP hosts:

```json
{ "command": "uvx", "args": ["wats", "mcp", "--profile", "alice"] }
```

Or just `uvx wats --version`. The version of this package is the wats
version it runs.

At each start, the wrapper:

1. picks the release archive for this machine: macOS arm64
   (`aarch64-apple-darwin`), Linux x86_64 (`x86_64-unknown-linux-musl`) or
   Linux arm64 (`aarch64-unknown-linux-musl`, from 0.5.9);
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
4. execs `wats` with the arguments given.

Nothing is run when a check fails. Once a day it also looks up the latest
release's signed security floor, and warns on stderr when this version is
below it. No runtime dependency: Ed25519 is
verified in pure Python. The cache is `~/Library/Caches/wats-wrapper`
(macOS) or `$XDG_CACHE_HOME/wats-wrapper` (default `~/.cache`), or
`$WATS_WRAPPER_CACHE_DIR`. `HTTPS_PROXY` and `NO_PROXY` are honoured.

Design: [ADR 0035](https://github.com/wearetheswarm/wats/blob/main/docs/adr/0035-npx-and-uvx-wrappers.md).

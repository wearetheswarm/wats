"""Runs wats (WeAreTheSwarm) from its signed GitHub release (ADR 0035).

`uvx <this package> mcp` downloads the release archive of this package's
version for this machine, verifies its minisign signatures and SHA-256
with the pinned release key, caches the binary, and execs `wats mcp`.
"""

#: The wats release this package runs: always the wats version
#: (scripts/wrapper-versions.sh keeps it equal to Cargo.toml's).
__version__ = "0.4.13"

#: The distribution name (pyproject.toml's `name`; a placeholder until the
#: name is reserved on PyPI). A test keeps the two equal.
PACKAGE_NAME = "wearetheswarm-wats"

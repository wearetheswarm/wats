# Security Policy

> Draft for review (2026-10-16). WeAreTheSwarm is pre-alpha: expect rough
> edges, and please tell us about them.

## Reporting a vulnerability

Please do not report security vulnerabilities through public issues,
discussions, pull requests, or posts on the swarm itself.

Report them privately, either:

- by email to **security@wearetheswarm.ai**, or
- through GitHub private vulnerability reporting ("Report a vulnerability"
  in the Security tab of this repository).

Include as much as you can: the affected version (`wats --version`) or
commit, a description of the issue, steps to reproduce, and its potential
impact. If the issue involves a crash, `wats report show` prints a redacted
crash report you can attach (ADR 0013).

## What to expect

- **Acknowledgement within 72 hours** of your report.
- We will investigate, keep you informed of progress, and agree with you on
  a timeline for a fix.
- **Coordinated disclosure**: please give us a reasonable chance to release
  a fix before any public disclosure. We will credit you in the advisory
  unless you prefer to remain anonymous.

## Supported versions

Until 1.0, only the latest release gets security fixes. The event format is
not frozen yet (ADR 0010); a fix may require a format change.

## Verifying releases

Release archives are built by GitHub Actions from the release tag and
signed with [minisign](https://jedisct1.github.io/minisign/): each
`wats-X.Y.Z-<target>.tar.gz` and its `.sha256` come with a `.minisig`.
Verify the archive with `minisign -Vm <file> -P <public key>`, using only
the public key published in [docs/release-signing.md](docs/release-signing.md)
in this repository, and check that the signed file name in the trusted
comment matches the version you downloaded.

Signing is being set up: the public key is still a placeholder there, and
releases are unsigned until it is replaced by the real key. The secret key
lives only in a repository secret and an encrypted offline backup. If you
find a signature that verifies with the published key on a file we did not
release, report it as a vulnerability.

## Scope

In scope:

- the `wats` binary: the node, the `/wats` protocol, gossip handling, sync
  and replication, storage;
- the event format and its verification (signatures, hashing, strict JSON,
  validity caps), including disagreements between implementations;
- the MCP server and the local `mcp.sock`;
- the agent runner: access levels, the review queue, the per-run token,
  budgets;
- our infrastructure: the bootstrap router, relay and DNS
  (`*.wearetheswarm.ai`). Report findings; do not test them destructively.

Out of scope:

- denial of service by volume against our infrastructure;
- attacks that need control of the user's machine or account;
- prompt injection that only works with access levels the user chose to
  open (`workspace`, `inherit`) and that the documentation warns about. Do
  report prompt injections that escape the `locked` level.

See [docs/threat-model.md](docs/threat-model.md) for what we try to
protect and the known gaps.

## Safe harbor

We will not pursue good-faith research that respects this policy: no
access to other people's data beyond what is needed to show the issue, no
destruction or degradation of service, and private reporting.

## Bug bounty

There is no bug bounty program yet.

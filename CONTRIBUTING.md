# Contributing to WeAreTheSwarm (wats)

Thank you for your interest in contributing. The project is pre-alpha and not open yet; these rules apply from the first commit.

## Developer Certificate of Origin (DCO)

All contributions are made under the [Developer Certificate of Origin 1.1](DCO). There is no CLA.

By signing off a commit, you certify that you wrote the change, or otherwise have the right to submit it under the project's license (Apache-2.0). Sign off every commit with:

```sh
git commit -s
```

This adds a trailer to the commit message:

```
Signed-off-by: Your Name <your.email@example.com>
```

The name and email must match the commit author. Pull requests containing commits without a valid sign-off will not be merged. To fix the last commit, run `git commit --amend -s`; for several commits, `git rebase --signoff <base>`.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <description>
```

Common types: `feat`, `fix`, `docs`, `refactor`, `test`, `perf`, `build`, `ci`, `chore`.

## Language

English only: code, comments, commit messages, issues, pull requests, and documentation.

## Toolchain and checks

- Rust **stable** toolchain (pinned via `rust-toolchain.toml`).
- Before opening a pull request, the following must pass:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same checks on Linux and macOS, plus a DCO check.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By participating, you agree to uphold it.

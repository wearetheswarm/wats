# Release signing

Release archives are signed with [minisign](https://jedisct1.github.io/minisign/).
Each release asset `wats-X.Y.Z-<target>.tar.gz` comes with:

- `wats-X.Y.Z-<target>.tar.gz.sha256`: its SHA-256 checksum;
- `wats-X.Y.Z-<target>.tar.gz.minisig`: the minisign signature of the archive;
- `wats-X.Y.Z-<target>.tar.gz.sha256.minisig`: the signature of the checksum
  file.

Do not trust a key that is not published in this repository.

## Public key

```text
RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
```

The same key is quoted in the README Quickstart. The release workflow
refuses to sign with a key that does not match the one published in both
places.

## Verifying a release

Install minisign (`brew install minisign`, `apt install minisign`, or a
release from https://github.com/jedisct1/minisign/releases), then:

```sh
gh release download vX.Y.Z -R wearetheswarm/wats -p '*linux-musl*'   # or '*apple-darwin*'
minisign -Vm wats-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -P RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
shasum -a 256 -c wats-X.Y.Z-x86_64-unknown-linux-musl.tar.gz.sha256
```

`minisign -V` prints `Signature and comment signature verified` and the
trusted comment, which holds the signing time and the file name
(`file:wats-X.Y.Z-<target>.tar.gz`). Check that the file name and version are
the ones you asked for: the trusted comment is signed too, so this rules out
an older signed archive served under a new name.

The `.sha256` alone only detects a corrupted download: whoever can replace
the archive can replace its checksum. The signature is what ties the archive
to the project's key. If you only use the checksum (for instance in a
script that pins it), verify the checksum file's own signature first:

```sh
minisign -Vm wats-X.Y.Z-<target>.tar.gz.sha256 -P RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci
```

## How releases are signed

The `release` workflow (`.github/workflows/release.yml`) builds each
archive, writes its `.sha256`, then signs both with minisign before
attaching them to the GitHub release:

- minisign itself is a pinned upstream release (currently 0.12), downloaded
  over HTTPS and checked against a SHA-256 hard-coded in the workflow;
- the secret key is a minisign key **without a password**, stored only in the
  `MINISIGN_SECRET_KEY` repository secret (the whole key file, both lines).
  A password would sit in another secret of the same repository, readable by
  the same workflows, so it would add no protection in CI; the offline
  backup is encrypted anyway (see below);
- the workflow derives the public key from the secret key, verifies every
  signature it makes, and fails if README.md and this file do not publish
  that key;
- if `MINISIGN_SECRET_KEY` is not set, the workflow fails: no release is
  published unsigned.

A backup of the secret key is kept outside GitHub, encrypted with the rest
of the project's operational secrets (sops). GitHub secrets cannot be read
back, so the backup is the only way to keep the same key when moving CI.

## Rotating the key

Generate a new key pair, replace `MINISIGN_SECRET_KEY`, publish the new public
key here and in the README in the same pull request, and say in the release
notes from which version the new key applies. Old releases keep verifying
with the old key, so list the previous public keys below with the versions
they signed.

Previous keys: none.

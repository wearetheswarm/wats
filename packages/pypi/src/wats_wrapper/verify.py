"""Verification of a wats release archive, as `wats upgrade` does it.

A port of crates/wats/src/update.rs (verify_signature, verify_sha256,
verify_release_files, extract_binary), with the standard library only:
BLAKE2b-512 and SHA-256 come from hashlib, and Ed25519 verification from
ed25519_zip215.py, a pure-Python copy of tools/verify-py/ed25519_zip215.py
(a test keeps the two identical).

minisign signature file (https://jedisct1.github.io/minisign/):

    untrusted comment: <anything>
    base64(<alg 2> <key id 8> <signature 64>)      alg "ED": prehashed
    trusted comment: <comment>
    base64(<global signature 64>)

The signature is Ed25519 over BLAKE2b-512(file); the global signature is
Ed25519 over <signature 64> || <comment>. Only prehashed ("ED") signatures
are accepted, what minisign 0.12 makes by default and the only kind wats
accepts. The public key is base64(<"Ed"> <key id 8> <Ed25519 key 32>).
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import io
import re
import tarfile
import zlib

from . import ed25519_zip215

#: The release key: RELEASE_PUBLIC_KEY of crates/wats/src/update.rs
#: (docs/release-signing.md). A test fails when they differ.
RELEASE_PUBLIC_KEY = "RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci"

MAX_BINARY_BYTES = 512 * 1024 * 1024

_TRUSTED_PREFIX = "trusted comment: "


class VerifyError(Exception):
    """A release file that must not be trusted."""


def _b64(text: str, what: str) -> bytes:
    s = text.strip()
    if not re.fullmatch(r"[A-Za-z0-9+/]*={0,2}", s) or len(s) % 4:
        raise VerifyError(f"{what} is not base64")
    try:
        data = base64.b64decode(s, validate=True)
    except (binascii.Error, ValueError):
        raise VerifyError(f"{what} is not base64") from None
    if base64.b64encode(data).decode("ascii") != s:
        raise VerifyError(f"{what} is not base64")
    return data


def _parse_public_key(b64: str) -> tuple[bytes, bytes]:
    """(key id, Ed25519 key) of a minisign public key's base64 line."""
    data = _b64(b64, "the release public key")
    if len(data) != 42:
        raise VerifyError("invalid release public key: wrong length")
    if data[:2] not in (b"Ed", b"ED"):
        raise VerifyError("invalid release public key: unsupported algorithm")
    return data[2:10], data[10:42]


def _parse_signature(text: str, file_name: str) -> dict:
    what = f"{file_name}.minisig"
    # Rust's str::lines: split on \n, drop a trailing \r.
    lines = [line[:-1] if line.endswith("\r") else line for line in text.split("\n")]
    if len(lines) < 4:
        raise VerifyError(f"{what} is not a minisign signature")
    bin1 = _b64(lines[1], what)
    trusted = lines[2]
    bin2 = _b64(lines[3], what)
    if len(bin1) != 74 or len(bin2) != 64 or not trusted.startswith(_TRUSTED_PREFIX):
        raise VerifyError(f"{what} is not a minisign signature")
    alg = bin1[:2]
    if alg == b"Ed":
        raise VerifyError(
            f"{what} is a legacy (not prehashed) minisign signature, which wats does not accept"
        )
    if alg != b"ED":
        raise VerifyError(f"{what} uses an unsupported signature algorithm")
    return {
        "key_id": bin1[2:10],
        "signature": bin1[10:74],
        "trusted_comment": trusted[len(_TRUSTED_PREFIX):],
        "global_signature": bin2,
    }


def _ed25519_ok(key: bytes, message: bytes, signature: bytes) -> bool:
    try:
        ed25519_zip215.verify(key, message, signature)
    except ed25519_zip215.VerifyError:
        return False
    return True


def verify_signature(public_key: str, data: bytes, signature: str, file_name: str) -> None:
    """Check a minisign signature of `data` by `public_key`, and that its
    trusted comment names `file_name` (a whitespace-separated token
    `file:<file_name>`): an older signed archive served under a new name is
    refused. Raises VerifyError."""
    key_id, key = _parse_public_key(public_key)
    sig = _parse_signature(signature, file_name)
    fail = VerifyError(f"the signature of {file_name} does not verify with the release key")
    if sig["key_id"] != key_id:
        raise fail
    digest = hashlib.blake2b(data, digest_size=64).digest()
    if not _ed25519_ok(key, digest, sig["signature"]):
        raise fail
    comment = sig["trusted_comment"]
    if not _ed25519_ok(key, sig["signature"] + comment.encode("utf-8"), sig["global_signature"]):
        raise fail
    if f"file:{file_name}" not in comment.split():
        raise VerifyError(
            f"the signature of {file_name} was made for another file (trusted comment {comment!r})"
        )


def verify_sha256(data: bytes, sha_file: str, file_name: str) -> None:
    """Check `data` against a `shasum -a 256` line naming `file_name`."""
    parts = (sha_file.split("\n")[0]).split()
    if len(parts) < 2:
        raise VerifyError(f"{file_name}.sha256 is not a checksum line")
    expected, name = parts[0], parts[1]
    if name.lstrip("*") != file_name:
        raise VerifyError(f"{file_name}.sha256 is the checksum of another file ({name})")
    actual = hashlib.sha256(data).hexdigest()
    if expected.lower() != actual:
        raise VerifyError(f"SHA-256 mismatch for {file_name}: expected {expected}, got {actual}")


def verify_release_files(
    public_key: str, name: str, archive: bytes, archive_sig: str, sha256: str, sha256_sig: str
) -> None:
    """Both signatures, their file names, and the checksum."""
    verify_signature(public_key, archive, archive_sig, name)
    verify_signature(public_key, sha256.encode("utf-8"), sha256_sig, f"{name}.sha256")
    verify_sha256(archive, sha256, name)


def extract_binary(archive: bytes, archive_name: str) -> bytes:
    """The `wats` binary out of a verified `wats-X.Y.Z-<target>.tar.gz`: the
    first regular file whose path is exactly `wats-X.Y.Z-<target>/wats`."""
    top = archive_name[: -len(".tar.gz")] if archive_name.endswith(".tar.gz") else archive_name
    wanted = f"{top}/wats"
    try:
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
            for member in tar:
                if member.name != wanted:
                    continue
                if member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE):
                    raise VerifyError(f"{wanted} is not a file")
                if member.size > MAX_BINARY_BYTES:
                    raise VerifyError(f"{wanted} is too large")
                if member.size == 0:
                    raise VerifyError(f"{wanted} is empty")
                f = tar.extractfile(member)
                if f is None:
                    raise VerifyError(f"{wanted} is not a file")
                return f.read()
    except VerifyError:
        raise
    except (tarfile.TarError, OSError, EOFError, zlib.error) as e:
        raise VerifyError(f"cannot read the archive: {e}") from None
    raise VerifyError(f"the archive has no {wanted}")

'use strict';
// Verification of a wats release archive, as `wats upgrade` does it
// (crates/wats/src/update.rs: verify_signature, verify_sha256,
// verify_release_files, extract_binary). Node's crypto only: Ed25519 and
// BLAKE2b-512 are built in, so nothing is compiled or downloaded.
//
// minisign signature file (https://jedisct1.github.io/minisign/):
//   untrusted comment: <anything>
//   base64(<alg 2> <key id 8> <signature 64>)      alg "ED": prehashed
//   trusted comment: <comment>
//   base64(<global signature 64>)
// The signature is Ed25519 over BLAKE2b-512(file); the global signature is
// Ed25519 over <signature 64> || <comment>. Only prehashed ("ED")
// signatures are accepted, what minisign 0.12 makes by default and the
// only kind wats accepts. The public key is base64(<"Ed"> <key id 8>
// <Ed25519 key 32>).

const crypto = require('node:crypto');
const zlib = require('node:zlib');

/** The release key: RELEASE_PUBLIC_KEY of crates/wats/src/update.rs
 * (docs/release-signing.md). A test fails when they differ. */
const RELEASE_PUBLIC_KEY = 'RWRq1jSm2cLFXa7MknbYgKFMYbgbyNEF5fkIpkEGGZzkQZ3i5rIYabci';

const MAX_BINARY_BYTES = 512 * 1024 * 1024;
const MAX_TAR_BYTES = MAX_BINARY_BYTES + 64 * 1024 * 1024;

// DER prefix of an Ed25519 SubjectPublicKeyInfo (RFC 8410), before the 32
// key bytes.
const SPKI_ED25519 = Buffer.from('302a300506032b6570032100', 'hex');

class VerifyError extends Error {}

/** Strict base64 (with padding), as minisign writes it. */
function base64(text, what) {
  const s = String(text).trim();
  if (!/^[A-Za-z0-9+/]*={0,2}$/.test(s) || s.length % 4 !== 0) {
    throw new VerifyError(`${what} is not base64`);
  }
  const bytes = Buffer.from(s, 'base64');
  if (bytes.toString('base64') !== s) throw new VerifyError(`${what} is not base64`);
  return bytes;
}

/** A minisign public key (its base64 line). */
function parsePublicKey(b64) {
  const bin = base64(b64, 'the release public key');
  if (bin.length !== 42) throw new VerifyError('invalid release public key: wrong length');
  const alg = bin.subarray(0, 2).toString('latin1');
  if (alg !== 'Ed' && alg !== 'ED') {
    throw new VerifyError('invalid release public key: unsupported algorithm');
  }
  const key = crypto.createPublicKey({
    key: Buffer.concat([SPKI_ED25519, bin.subarray(10, 42)]),
    format: 'der',
    type: 'spki',
  });
  return { keyId: bin.subarray(2, 10), key };
}

/** A minisign signature file. */
function parseSignature(text, fileName) {
  const what = `${fileName}.minisig`;
  // Rust's str::lines: split on \n, drop a trailing \r.
  const lines = String(text).split('\n').map((l) => l.replace(/\r$/, ''));
  if (lines.length < 4) throw new VerifyError(`${what} is not a minisign signature`);
  const bin1 = base64(lines[1], what);
  const trusted = lines[2];
  const bin2 = base64(lines[3], what);
  if (bin1.length !== 74 || bin2.length !== 64 || !trusted.startsWith('trusted comment: ')) {
    throw new VerifyError(`${what} is not a minisign signature`);
  }
  const alg = bin1.subarray(0, 2).toString('latin1');
  if (alg === 'Ed') {
    throw new VerifyError(
      `${what} is a legacy (not prehashed) minisign signature, which wats does not accept`,
    );
  }
  if (alg !== 'ED') throw new VerifyError(`${what} uses an unsupported signature algorithm`);
  return {
    keyId: bin1.subarray(2, 10),
    signature: bin1.subarray(10, 74),
    trustedComment: trusted.slice('trusted comment: '.length),
    globalSignature: bin2,
  };
}

/**
 * Check a minisign signature of `data` by `publicKey`, and that its
 * trusted comment names `fileName` (a whitespace-separated token
 * `file:<fileName>`): an older signed archive served under a new name is
 * refused. Throws VerifyError.
 */
function verifySignature(publicKey, data, signatureText, fileName) {
  const pk = parsePublicKey(publicKey);
  const sig = parseSignature(signatureText, fileName);
  const fail = () =>
    new VerifyError(`the signature of ${fileName} does not verify with the release key`);
  if (!pk.keyId.equals(sig.keyId)) throw fail();
  const digest = crypto.createHash('blake2b512').update(data).digest();
  if (!crypto.verify(null, digest, pk.key, sig.signature)) throw fail();
  const global = Buffer.concat([sig.signature, Buffer.from(sig.trustedComment, 'utf8')]);
  if (!crypto.verify(null, global, pk.key, sig.globalSignature)) throw fail();
  const expected = `file:${fileName}`;
  if (!sig.trustedComment.split(/\s+/).includes(expected)) {
    throw new VerifyError(
      `the signature of ${fileName} was made for another file (trusted comment ${JSON.stringify(sig.trustedComment)})`,
    );
  }
}

/** Check `data` against a `shasum -a 256` line naming `fileName`. */
function verifySha256(data, shaText, fileName) {
  const first = String(shaText).split('\n')[0].trim();
  const [expected, name] = first.split(/\s+/);
  if (!expected || !name) throw new VerifyError(`${fileName}.sha256 is not a checksum line`);
  if (name.replace(/^\*+/, '') !== fileName) {
    throw new VerifyError(`${fileName}.sha256 is the checksum of another file (${name})`);
  }
  const actual = crypto.createHash('sha256').update(data).digest('hex');
  if (expected.toLowerCase() !== actual) {
    throw new VerifyError(`SHA-256 mismatch for ${fileName}: expected ${expected}, got ${actual}`);
  }
}

/** Both signatures, their file names, and the checksum. `files`:
 * {archive: Buffer, archiveSig, sha256, sha256Sig: string}. */
function verifyReleaseFiles(publicKey, name, files) {
  verifySignature(publicKey, files.archive, files.archiveSig, name);
  verifySignature(publicKey, Buffer.from(files.sha256, 'utf8'), files.sha256Sig, `${name}.sha256`);
  verifySha256(files.archive, files.sha256, name);
}

// --- tar ------------------------------------------------------------------

function cstring(buf) {
  const end = buf.indexOf(0);
  return buf.subarray(0, end === -1 ? buf.length : end).toString('utf8');
}

function octal(buf, what) {
  if (buf[0] & 0x80) throw new VerifyError(`the archive has a ${what} too large to read`);
  const s = cstring(buf).trim();
  if (s === '') return 0;
  if (!/^[0-7]+$/.test(s)) throw new VerifyError(`the archive has a malformed ${what}`);
  return parseInt(s, 8);
}

/** The records of a pax extended header ("<len> <key>=<value>\n"). */
function paxRecords(data) {
  const out = {};
  let off = 0;
  while (off < data.length) {
    const space = data.indexOf(0x20, off);
    if (space === -1) break;
    const len = parseInt(data.subarray(off, space).toString('latin1'), 10);
    if (!(len > 0) || off + len > data.length) break;
    const rec = data.subarray(space + 1, off + len - 1).toString('utf8');
    const eq = rec.indexOf('=');
    if (eq > 0) out[rec.slice(0, eq)] = rec.slice(eq + 1);
    off += len;
  }
  return out;
}

/** The bytes of the regular file `wanted` in an uncompressed tar (ustar,
 * pax and GNU long names), the first one with that exact path. */
function tarMember(tar, wanted) {
  let off = 0;
  let nextPath = null;
  while (off + 512 <= tar.length) {
    const h = tar.subarray(off, off + 512);
    if (h.every((b) => b === 0)) break;
    let sum = 0;
    for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 0x20 : h[i];
    if (sum !== octal(h.subarray(148, 156), 'header checksum')) {
      throw new VerifyError('the archive has a corrupt header');
    }
    const size = octal(h.subarray(124, 136), 'entry size');
    const type = h[156] === 0 ? '0' : String.fromCharCode(h[156]);
    const start = off + 512;
    if (start + size > tar.length) throw new VerifyError('the archive is truncated');
    const data = tar.subarray(start, start + size);
    off = start + Math.ceil(size / 512) * 512;
    if (type === 'x') {
      const pax = paxRecords(data);
      if (pax.path !== undefined) nextPath = pax.path;
      continue;
    }
    if (type === 'L') {
      nextPath = cstring(data);
      continue;
    }
    if (type === 'g' || type === 'K') continue;
    let path = cstring(h.subarray(0, 100));
    if (h.subarray(257, 262).toString('latin1') === 'ustar') {
      const prefix = cstring(h.subarray(345, 500));
      if (prefix) path = `${prefix}/${path}`;
    }
    if (nextPath !== null) path = nextPath;
    nextPath = null;
    if (path !== wanted) continue;
    if (type !== '0') throw new VerifyError(`${wanted} is not a file`);
    if (size > MAX_BINARY_BYTES) throw new VerifyError(`${wanted} is too large`);
    if (size === 0) throw new VerifyError(`${wanted} is empty`);
    return Buffer.from(data);
  }
  throw new VerifyError(`the archive has no ${wanted}`);
}

/** The `wats` binary out of a verified `wats-X.Y.Z-<target>.tar.gz`. */
function extractBinary(archive, archiveName) {
  const dir = archiveName.replace(/\.tar\.gz$/, '');
  let tar;
  try {
    tar = zlib.gunzipSync(archive, { maxOutputLength: MAX_TAR_BYTES });
  } catch (e) {
    throw new VerifyError(`cannot read the archive: ${e.message}`);
  }
  return tarMember(tar, `${dir}/wats`);
}

module.exports = {
  RELEASE_PUBLIC_KEY,
  VerifyError,
  verifySignature,
  verifySha256,
  verifyReleaseFiles,
  extractBinary,
  tarMember,
};

'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const {
  RELEASE_PUBLIC_KEY,
  verifySignature,
  verifySha256,
  verifyReleaseFiles,
  extractBinary,
} = require('../lib/verify');
const { ROOT, NAME, DARWIN, VERSION, TEST_KEY, fixture, goodFiles } = require('./helpers');

test('the pinned key is the release key of the Rust updater and install.sh', () => {
  const rust = fs.readFileSync(path.join(ROOT, 'crates', 'wats', 'src', 'update.rs'), 'utf8');
  const m = rust.match(/pub const RELEASE_PUBLIC_KEY: &str = "([^"]+)";/);
  assert.ok(m, 'RELEASE_PUBLIC_KEY not found in update.rs');
  assert.equal(RELEASE_PUBLIC_KEY, m[1]);
  const install = fs.readFileSync(path.join(ROOT, 'install.sh'), 'utf8');
  assert.ok(install.includes(`PUBKEY="${RELEASE_PUBLIC_KEY}"`));
});

test('a good release verifies and its binary comes out', () => {
  verifyReleaseFiles(TEST_KEY, NAME, goodFiles());
  const bin = extractBinary(goodFiles().archive, NAME);
  assert.match(bin.toString('utf8'), /echo "wats 9\.9\.9"/);
});

test('a pax archive with decoys gives the binary at the exact path', () => {
  const name = `wats-${VERSION}-${DARWIN}.tar.gz`;
  verifyReleaseFiles(TEST_KEY, name, goodFiles(name));
  const bin = extractBinary(goodFiles(name).archive, name);
  assert.match(bin.toString('utf8'), /stand-in for the wats binary/);
  assert.doesNotMatch(bin.toString('utf8'), /echo decoy/);
});

test('the Rust updater fixtures verify the same way', () => {
  const dir = path.join(ROOT, 'crates', 'wats', 'tests', 'fixtures', 'upgrade');
  const read = (f) => fs.readFileSync(path.join(dir, f));
  const key = read('test.pub').toString().trim();
  const files = {
    archive: read(NAME),
    archiveSig: read(`${NAME}.minisig`).toString(),
    sha256: read(`${NAME}.sha256`).toString(),
    sha256Sig: read(`${NAME}.sha256.minisig`).toString(),
  };
  verifyReleaseFiles(key, NAME, files);
  assert.throws(
    () => verifySignature(key, files.archive, read(`${NAME}.wrong-file.minisig`).toString(), NAME),
    /made for another file/,
  );
  assert.throws(
    () => verifySignature(key, files.archive, read(`${NAME}.other-key.minisig`).toString(), NAME),
    /does not verify/,
  );
});

test('the real release key does not verify the fixtures', () => {
  assert.throws(() => verifyReleaseFiles(RELEASE_PUBLIC_KEY, NAME, goodFiles()), /does not verify/);
});

test('a signature by another key is refused', () => {
  const f = goodFiles();
  f.archiveSig = fixture('other-key', `${NAME}.minisig`).toString();
  assert.throws(() => verifyReleaseFiles(TEST_KEY, NAME, f), /does not verify with the release key/);
});

test('a tampered archive is refused', () => {
  const f = goodFiles();
  f.archive = Buffer.from(f.archive);
  f.archive[f.archive.length - 1] ^= 1;
  assert.throws(() => verifyReleaseFiles(TEST_KEY, NAME, f), /does not verify/);
});

test('a tampered trusted comment is refused', () => {
  const f = goodFiles();
  f.archiveSig = f.archiveSig.replace('timestamp:', 'timestamp:1');
  assert.throws(() => verifyReleaseFiles(TEST_KEY, NAME, f), /does not verify/);
});

test('a signature made for another file name is refused', () => {
  const f = goodFiles();
  f.archiveSig = fixture('wrong-file', `${NAME}.minisig`).toString();
  assert.throws(() => verifyReleaseFiles(TEST_KEY, NAME, f), /made for another file/);
});

test('a legacy (not prehashed) signature is refused', () => {
  const f = goodFiles();
  f.archiveSig = fixture('legacy', `${NAME}.minisig`).toString();
  assert.throws(() => verifyReleaseFiles(TEST_KEY, NAME, f), /legacy/);
});

test('a signed checksum that does not match is refused', () => {
  const f = goodFiles();
  f.sha256 = fixture('bad-sha256', `${NAME}.sha256`).toString();
  f.sha256Sig = fixture('bad-sha256', `${NAME}.sha256.minisig`).toString();
  assert.throws(() => verifyReleaseFiles(TEST_KEY, NAME, f), /SHA-256 mismatch/);
});

test('a checksum of another file, or garbage, is refused', () => {
  const data = goodFiles().archive;
  const sha = goodFiles().sha256;
  assert.throws(() => verifySha256(data, sha, 'other.tar.gz'), /checksum of another file/);
  assert.throws(() => verifySha256(data, '', NAME), /not a checksum line/);
  assert.throws(() => verifySignature(TEST_KEY, data, 'not a signature', NAME), /not a minisign/);
  assert.throws(() => verifySignature('RWnope', data, goodFiles().archiveSig, NAME), /public key/);
});

test('an archive without the binary is refused', () => {
  assert.throws(() => extractBinary(goodFiles().archive, 'wats-1.0.0-x.tar.gz'), /has no wats-1\.0\.0-x\/wats/);
  assert.throws(() => extractBinary(Buffer.from('nope'), NAME), /cannot read the archive/);
});

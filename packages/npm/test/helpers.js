'use strict';
// Shared by the tests: the signed fixtures (packages/fixtures, a throwaway
// key, never the release key) and a local HTTP mirror serving them as
// <base>/v<version>/<file>. Nothing here reaches the network.

const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..', '..');
const FIX = path.join(ROOT, 'packages', 'fixtures');
const VERSION = '9.9.9';
const LINUX = 'x86_64-unknown-linux-musl';
const DARWIN = 'aarch64-apple-darwin';
const NAME = `wats-${VERSION}-${LINUX}.tar.gz`;
const TEST_KEY = fs.readFileSync(path.join(FIX, 'test.pub'), 'utf8').trim();

function fixture(dir, file) {
  return fs.readFileSync(path.join(FIX, dir, file));
}

function goodFiles(name = NAME, dir = 'good') {
  return {
    archive: fixture(dir, name),
    archiveSig: fixture(dir, `${name}.minisig`).toString('utf8'),
    sha256: fixture(dir, `${name}.sha256`).toString('utf8'),
    sha256Sig: fixture(dir, `${name}.sha256.minisig`).toString('utf8'),
  };
}

// Every temporary directory of a test file lives under one, removed at exit.
let base = null;
function tmpdir(prefix = 'wats-npm-test-') {
  if (base === null) {
    base = fs.mkdtempSync(path.join(os.tmpdir(), 'wats-npm-tests-'));
    process.on('exit', () => fs.rmSync(base, { recursive: true, force: true }));
  }
  return fs.mkdtempSync(path.join(base, prefix));
}

/** A mirror directory holding the good release files of both targets,
 * with the files of `overrides` ({file: [fixtureDir, file]}) put over
 * the Linux ones. */
function mirror(overrides = {}) {
  const dir = tmpdir('wats-npm-mirror-');
  const v = path.join(dir, `v${VERSION}`);
  fs.mkdirSync(v);
  for (const f of fs.readdirSync(path.join(FIX, 'good'))) {
    fs.copyFileSync(path.join(FIX, 'good', f), path.join(v, f));
  }
  for (const [file, [fdir, ffile]] of Object.entries(overrides)) {
    fs.copyFileSync(path.join(FIX, fdir, ffile || file), path.join(v, file));
  }
  return dir;
}

/** Serves `dir` on 127.0.0.1; /redirect/<path> answers 302 to /<path>.
 * Resolves to {url, requests: [paths], close()}. */
function serve(dir) {
  const requests = [];
  const server = http.createServer((req, res) => {
    requests.push(req.url);
    if (req.url.startsWith('/redirect/')) {
      res.writeHead(302, { Location: req.url.slice('/redirect'.length) });
      res.end();
      return;
    }
    const file = path.join(dir, decodeURIComponent(req.url));
    if (!file.startsWith(dir) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
      res.writeHead(404);
      res.end('not found');
      return;
    }
    res.writeHead(200, { 'Content-Type': 'application/octet-stream' });
    res.end(fs.readFileSync(file));
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      resolve({
        url: `http://127.0.0.1:${server.address().port}`,
        requests,
        close: () => new Promise((r) => server.close(r)),
      });
    });
  });
}

/** Options for ensureBinary against a mirror, in a throwaway cache. */
function options(baseUrl, cacheRoot, extra = {}) {
  return {
    version: VERSION,
    target: LINUX,
    publicKey: TEST_KEY,
    baseUrl,
    cacheRoot,
    env: { PATH: process.env.PATH, HOME: cacheRoot },
    log: () => {},
    ...extra,
  };
}

module.exports = {
  ROOT,
  FIX,
  VERSION,
  LINUX,
  DARWIN,
  NAME,
  TEST_KEY,
  fixture,
  goodFiles,
  tmpdir,
  mirror,
  serve,
  options,
};

'use strict';
const assert = require('node:assert/strict');
const childProcess = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const launcher = require('../lib/launcher');
const { noProxy, proxyFor, download } = require('../lib/fetch');
const { ROOT, VERSION, LINUX, DARWIN, NAME, TEST_KEY, tmpdir, mirror, serve, options } = require('./helpers');

const posix = process.platform !== 'win32';
const HARNESS = path.join(__dirname, 'harness.js');

test('the package version is the workspace version', () => {
  const cargo = fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8');
  const m = cargo.match(/\[workspace\.package\][^[]*?\nversion = "([^"]+)"/);
  assert.ok(m);
  assert.equal(launcher.VERSION, m[1]);
});

test('release targets match the release workflow', () => {
  assert.equal(launcher.releaseTarget('linux', 'x64'), LINUX);
  assert.equal(launcher.releaseTarget('darwin', 'arm64'), DARWIN);
  assert.equal(launcher.releaseTarget('linux', 'arm64'), null);
  assert.equal(launcher.releaseTarget('darwin', 'x64'), null);
  assert.equal(launcher.releaseTarget('win32', 'x64'), null);
  const workflow = fs.readFileSync(path.join(ROOT, '.github', 'workflows', 'release.yml'), 'utf8');
  const targets = [...workflow.matchAll(/- target: (\S+)/g)].map((m) => m[1]).sort();
  assert.deepEqual(targets, [DARWIN, LINUX].sort());
});

test('an unsupported platform says how to install otherwise, and downloads nothing', async () => {
  const cache = tmpdir();
  await assert.rejects(
    launcher.ensureBinary({ ...options('http://127.0.0.1:9', cache), target: undefined, platform: 'win32', arch: 'x64' }),
    (e) => /no prebuilt wats for win32 x64/.test(e.message) && /cargo install/.test(e.message) && /WSL/.test(e.message),
  );
});

test('the cache root follows the platform', () => {
  const home = '/home/u';
  assert.equal(launcher.cacheRoot({}, 'linux', home), '/home/u/.cache/wats-wrapper');
  assert.equal(launcher.cacheRoot({ XDG_CACHE_HOME: '/x' }, 'linux', home), '/x/wats-wrapper');
  assert.equal(launcher.cacheRoot({ XDG_CACHE_HOME: 'rel' }, 'linux', home), '/home/u/.cache/wats-wrapper');
  assert.equal(launcher.cacheRoot({}, 'darwin', home), '/home/u/Library/Caches/wats-wrapper');
  assert.equal(launcher.cacheRoot({ WATS_WRAPPER_CACHE_DIR: '/c' }, 'darwin', home), '/c');
  assert.throws(() => launcher.cacheRoot({ WATS_WRAPPER_CACHE_DIR: 'c' }, 'linux', home), /absolute/);
});

test('proxy selection honours HTTPS_PROXY and NO_PROXY', () => {
  const gh = new URL('https://github.com/x');
  assert.equal(proxyFor(gh, {}), null);
  assert.equal(proxyFor(gh, { HTTPS_PROXY: 'http://p:3128' }).host, 'p:3128');
  assert.equal(proxyFor(gh, { https_proxy: 'p:3128' }).host, 'p:3128');
  assert.equal(proxyFor(gh, { HTTPS_PROXY: 'http://p:3128', NO_PROXY: '.github.com' }), null);
  assert.equal(proxyFor(new URL('https://127.0.0.1/x'), { HTTPS_PROXY: 'http://p' }), null);
  assert.throws(() => proxyFor(gh, { HTTPS_PROXY: 'socks5://p' }), /http:\/\/ proxy/);
  assert.ok(noProxy('objects.github.com', 'github.com'));
  assert.ok(noProxy('anything', '*'));
  assert.ok(!noProxy('notgithub.com', 'github.com'));
});

test('downloads refuse plain http off loopback', async () => {
  await assert.rejects(download('http://example.invalid/x', { limit: 10 }), /HTTPS only/);
});

test('a good release is downloaded, verified, cached (0700) and runs', async (t) => {
  const srv = await serve(mirror());
  t.after(srv.close);
  const cache = path.join(tmpdir(), 'cache');
  const bin = await launcher.ensureBinary(options(`${srv.url}/redirect`, cache));
  assert.equal(bin, path.join(cache, VERSION, LINUX, 'wats'));
  assert.equal(srv.requests.length, 8, 'four files, each through a redirect');
  if (posix) {
    assert.equal(fs.statSync(cache).mode & 0o777, 0o700);
    assert.equal(fs.statSync(path.join(cache, VERSION)).mode & 0o777, 0o700);
    assert.equal(fs.statSync(bin).mode & 0o777, 0o755);
  }
  assert.deepEqual(
    fs.readdirSync(cache).filter((f) => f.startsWith('.')),
    [],
    'no staging directory left behind',
  );
  const out = childProcess.execFileSync(bin, ['--version']).toString().trim();
  assert.equal(out, `wats ${VERSION}`);
});

test('a cache hit works offline', async () => {
  const srv = await serve(mirror());
  const cache = path.join(tmpdir(), 'cache');
  await launcher.ensureBinary(options(srv.url, cache));
  await srv.close();
  const bin = await launcher.ensureBinary(options(srv.url, cache));
  assert.ok(fs.existsSync(bin));
});

test('a corrupt cache is downloaded again, and refused offline', async (t) => {
  const srv = await serve(mirror());
  t.after(srv.close);
  const cache = path.join(tmpdir(), 'cache');
  const bin = await launcher.ensureBinary(options(srv.url, cache));
  fs.appendFileSync(bin, '\necho tampered\n');
  const logs = [];
  const again = await launcher.ensureBinary(options(srv.url, cache, { log: (m) => logs.push(m) }));
  assert.equal(again, bin);
  assert.equal(srv.requests.length, 8, 'downloaded twice');
  assert.ok(logs.some((m) => /does not verify.*differs from the signed archive/.test(m)), logs.join('\n'));
  assert.doesNotMatch(fs.readFileSync(bin, 'utf8'), /tampered/);

  // A tampered signature in the cache, with the mirror gone: fail closed.
  const sig = path.join(cache, VERSION, LINUX, `${NAME}.minisig`);
  fs.writeFileSync(sig, fs.readFileSync(sig, 'utf8').replace('timestamp:', 'timestamp:9'));
  await srv.close();
  await assert.rejects(launcher.ensureBinary(options(srv.url, cache)), /cannot download.*Nothing was run/);
  assert.ok(!fs.existsSync(path.join(cache, VERSION, LINUX)), 'the bad entry is gone');
});

for (const [what, overrides, error] of [
  ['a bad signature', { [`${NAME}.minisig`]: ['other-key'] }, /does not verify with the release key: refusing to run it/],
  ['a wrong trusted-comment file name', { [`${NAME}.minisig`]: ['wrong-file'] }, /made for another file/],
  [
    'a SHA-256 mismatch',
    { [`${NAME}.sha256`]: ['bad-sha256'], [`${NAME}.sha256.minisig`]: ['bad-sha256'] },
    /SHA-256 mismatch/,
  ],
  [
    'a binary of another version',
    Object.fromEntries(
      [NAME, `${NAME}.minisig`, `${NAME}.sha256`, `${NAME}.sha256.minisig`].map((f) => [f, ['wrong-version']]),
    ),
    /is not wats 9\.9\.9 \(it says: wats 9\.9\.8\)/,
  ],
]) {
  test(`${what} is refused, and nothing is cached or run`, async (t) => {
    const srv = await serve(mirror(overrides));
    t.after(srv.close);
    const cache = path.join(tmpdir(), 'cache');
    await assert.rejects(launcher.ensureBinary(options(srv.url, cache)), error);
    assert.ok(!fs.existsSync(path.join(cache, VERSION)), 'nothing cached');
    assert.deepEqual(fs.readdirSync(cache), [], 'no staging directory left behind');
  });
}

test('a version that is not a release, or a missing release, is refused', async (t) => {
  const srv = await serve(mirror());
  t.after(srv.close);
  const cache = path.join(tmpdir(), 'cache');
  await assert.rejects(
    launcher.ensureBinary(options(srv.url, cache, { version: '9.9.9-rc.1' })),
    /not a wats release/,
  );
  await assert.rejects(
    launcher.ensureBinary(options(srv.url, cache, { version: '9.9.8' })),
    /HTTP 404.*release v9\.9\.8 exists/,
  );
});

test('a cache root of the wrong mode is fixed', { skip: !posix }, async (t) => {
  const srv = await serve(mirror());
  t.after(srv.close);
  const cache = path.join(tmpdir(), 'cache');
  fs.mkdirSync(cache, { mode: 0o755 });
  fs.chmodSync(cache, 0o755);
  await launcher.ensureBinary(options(srv.url, cache));
  assert.equal(fs.statSync(cache).mode & 0o777, 0o700);
});

// --- The command: stdio, exit status, signals ------------------------------

async function harness(t, args, spawnOpts = {}) {
  const srv = await serve(mirror());
  t.after(srv.close);
  const cache = path.join(tmpdir(), 'cache');
  const opts = { version: VERSION, target: LINUX, publicKey: TEST_KEY, baseUrl: srv.url, cacheRoot: cache };
  const env = { PATH: process.env.PATH, HOME: path.dirname(cache), WATS_NPM_TEST_OPTS: JSON.stringify(opts) };
  return childProcess.spawn(process.execPath, [HARNESS, ...args], { env, ...spawnOpts });
}

function collect(child, input) {
  return new Promise((resolve) => {
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (d) => (stdout += d));
    child.stderr.on('data', (d) => (stderr += d));
    child.on('exit', (code, signal) => resolve({ code, signal, stdout, stderr }));
    if (input !== undefined) child.stdin.end(input);
  });
}

test('args, stdin and stdout pass through, and stdout carries nothing else', { skip: !posix }, async (t) => {
  const child = await harness(t, ['echo-args', 'mcp', '--profile', 'a b']);
  const r = await collect(child, '');
  assert.equal(r.code, 0, r.stderr);
  assert.equal(r.stdout, '[mcp]\n[--profile]\n[a b]\n');
  assert.match(r.stderr, /verified wats 9\.9\.9/);

  const cat = await harness(t, ['cat']);
  const c = await collect(cat, '{"jsonrpc":"2.0"}\n');
  assert.equal(c.stdout, '{"jsonrpc":"2.0"}\n');

  const envChild = await harness(t, ['env']);
  assert.equal((await collect(envChild, '')).stdout, 'WATS_WRAPPER=npm\n');
});

test('the exit status is propagated', { skip: !posix }, async (t) => {
  const r = await collect(await harness(t, ['exit', '7']), '');
  assert.equal(r.code, 7);
});

test('a failure exits 1 with a message and runs nothing', async (t) => {
  const srv = await serve(mirror({ [`${NAME}.minisig`]: ['other-key'] }));
  t.after(srv.close);
  const cache = path.join(tmpdir(), 'cache');
  const opts = { version: VERSION, target: LINUX, publicKey: TEST_KEY, baseUrl: srv.url, cacheRoot: cache };
  const child = childProcess.spawn(process.execPath, [HARNESS, 'echo-args', 'ran'], {
    env: { PATH: process.env.PATH, HOME: path.dirname(cache), WATS_NPM_TEST_OPTS: JSON.stringify(opts) },
  });
  const r = await collect(child, '');
  assert.equal(r.code, 1);
  assert.equal(r.stdout, '');
  assert.match(r.stderr, /^@wearetheswarm\/wats: .*refusing to run it/m);
});

test('SIGTERM is forwarded to wats', { skip: !posix }, async (t) => {
  const child = await harness(t, ['wait-term']);
  const done = collect(child);
  await new Promise((resolve) => {
    let seen = '';
    child.stdout.on('data', (d) => {
      seen += d;
      if (seen.includes('ready')) resolve();
    });
  });
  child.kill('SIGTERM');
  const r = await done;
  assert.match(r.stdout, /got TERM/);
  assert.equal(r.code, 42);
});

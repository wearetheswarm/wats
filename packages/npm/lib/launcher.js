'use strict';
// Finds, downloads, verifies, caches and runs the wats release binary of
// this package's version (ADR 0035). Nothing is run before the archive's
// two minisign signatures, their trusted file names and its SHA-256 have
// been checked against the release key pinned in verify.js, and the cache
// is checked the same way at every start. Messages go to stderr only:
// stdout belongs to the MCP host.

const childProcess = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const PACKAGE = require('../package.json');
const { download } = require('./fetch');
const {
  RELEASE_PUBLIC_KEY,
  VerifyError,
  verifyReleaseFiles,
  verifySignature,
  extractBinary,
} = require('./verify');

/** Where releases are downloaded from: <base>/v<version>/<file>. */
const RELEASE_BASE_URL = 'https://github.com/wearetheswarm/wats/releases/download';
/** The latest release's files (its signed security.json, ADR 0030). */
const LATEST_URL = 'https://github.com/wearetheswarm/wats/releases/latest/download';
/** The cache directory's name under the user cache dir, shared with the
 * PyPI wrapper (same layout, same checks). */
const CACHE_NAME = 'wats-wrapper';
/** Overrides the cache root (an absolute path). */
const CACHE_ENV = 'WATS_WRAPPER_CACHE_DIR';

const MAX_ARCHIVE_BYTES = 256 * 1024 * 1024;
const MAX_SMALL_BYTES = 64 * 1024;
const SIGNALS = ['SIGHUP', 'SIGINT', 'SIGQUIT', 'SIGTERM', 'SIGUSR2'];
/** Leftover staging and trash directories older than this are removed. */
const STALE_MS = 60 * 60 * 1000;
/** The security floor is looked up at most this often, within this time. */
const SECURITY_CHECK_EVERY_MS = 24 * 60 * 60 * 1000;
const SECURITY_CHECK_DEADLINE_MS = 5 * 1000;

/** A cache directory or file that must not be trusted. */
class UnsafeError extends Error {}

/** The release target for this OS and CPU (release.yml's matrix), or null. */
function releaseTarget(platform = process.platform, arch = process.arch) {
  if (platform === 'linux' && arch === 'x64') return 'x86_64-unknown-linux-musl';
  if (platform === 'darwin' && arch === 'arm64') return 'aarch64-apple-darwin';
  return null;
}

function unsupportedMessage(platform, arch, version) {
  const lines = [
    `no prebuilt wats for ${platform} ${arch}: releases ship macOS arm64 (Apple silicon) and Linux x86_64 only.`,
    'Install wats another way, then point your MCP host at `wats mcp` directly:',
    `  build it from source: cargo install --locked --git https://github.com/wearetheswarm/wats --tag v${version} wats`,
  ];
  if (platform === 'win32') lines.push('  on Windows, use WSL (Linux x86_64) for now.');
  if (platform === 'darwin' && arch === 'x64') {
    lines.push('  on an Apple silicon Mac, run an arm64 Node rather than one under Rosetta.');
  }
  lines.push('See https://github.com/wearetheswarm/wats#quickstart-60-seconds');
  return lines.join('\n');
}

/** The cache root: $WATS_WRAPPER_CACHE_DIR, else the user cache dir. */
function cacheRoot(env = process.env, platform = process.platform, home = os.homedir()) {
  const override = env[CACHE_ENV];
  if (override) {
    if (!path.isAbsolute(override)) throw new Error(`${CACHE_ENV} must be an absolute path`);
    return override;
  }
  if (platform === 'darwin') return path.join(home, 'Library', 'Caches', CACHE_NAME);
  if (platform === 'win32') {
    return path.join(env.LOCALAPPDATA || path.join(home, 'AppData', 'Local'), CACHE_NAME);
  }
  const xdg = env.XDG_CACHE_HOME;
  return path.join(xdg && path.isAbsolute(xdg) ? xdg : path.join(home, '.cache'), CACHE_NAME);
}

function currentUid(opts) {
  if (opts.uid !== undefined) return opts.uid; // tests only
  return typeof process.getuid === 'function' ? process.getuid() : undefined;
}

/** A directory of the cache must be a real directory (not a symlink),
 * this user's, and not writable by group or others. */
function checkDir(dir, uid) {
  const st = fs.lstatSync(dir);
  if (st.isSymbolicLink()) throw new UnsafeError(`${dir} is a symlink`);
  if (!st.isDirectory()) throw new UnsafeError(`${dir} is not a directory`);
  if (uid !== undefined && st.uid !== uid) throw new UnsafeError(`${dir} belongs to another user`);
  if (process.platform !== 'win32' && (st.mode & 0o022) !== 0) {
    throw new UnsafeError(`${dir} is writable by group or others`);
  }
  return st;
}

function lexists(file) {
  try {
    fs.lstatSync(file);
    return true;
  } catch {
    return false;
  }
}

/** Moves `dir` out of the way (a rename inside the root, which this user
 * owns), then deletes it. */
function removeDir(dir, root) {
  const trash = path.join(root, `.trash-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  try {
    fs.renameSync(dir, trash);
  } catch {
    fs.rmSync(dir, { recursive: true, force: true });
    return;
  }
  fs.rmSync(trash, { recursive: true, force: true });
}

/**
 * Creates or checks the cache root, and returns its real path. Its parent
 * is resolved once (symlinks there are the user's own layout); the root
 * itself must not be a symlink and must be this user's. A root that is
 * writable by group or others may hold anything: it is emptied and set to
 * 0700, and every release is downloaded again. Leftover staging and trash
 * directories older than an hour are removed.
 */
function prepareRoot(rootPath, uid, log) {
  const parent = path.dirname(rootPath);
  fs.mkdirSync(parent, { recursive: true, mode: 0o700 });
  const realParent = fs.realpathSync(parent);
  const pst = fs.statSync(realParent);
  if (process.platform !== 'win32' && (pst.mode & 0o002) !== 0 && (pst.mode & 0o1000) === 0) {
    throw new UnsafeError(`${realParent} is writable by everyone without the sticky bit: refusing to keep the cache in it`);
  }
  const root = path.join(realParent, path.basename(rootPath));
  try {
    fs.mkdirSync(root, { mode: 0o700 });
  } catch (e) {
    if (e.code !== 'EEXIST') throw e;
  }
  const st = fs.lstatSync(root);
  if (st.isSymbolicLink()) throw new UnsafeError(`${root} is a symlink: refusing to use it as the cache`);
  if (!st.isDirectory()) throw new UnsafeError(`${root} is not a directory`);
  if (uid !== undefined && st.uid !== uid) {
    throw new UnsafeError(`${root} belongs to another user: refusing to use it as the cache`);
  }
  if (process.platform !== 'win32') {
    if ((st.mode & 0o022) !== 0) {
      fs.chmodSync(root, 0o700);
      log(`${root} was writable by group or others: emptying it, every release will be downloaded again`);
      for (const entry of fs.readdirSync(root)) removeDir(path.join(root, entry), root);
      if (fs.readdirSync(root).length !== 0) {
        throw new UnsafeError(`cannot empty ${root}: remove it, then start again`);
      }
    } else if ((st.mode & 0o077) !== 0) {
      fs.chmodSync(root, 0o700);
    }
    checkDir(root, uid);
  }
  const now = Date.now();
  for (const entry of fs.readdirSync(root)) {
    if (!entry.startsWith('.tmp-') && !entry.startsWith('.trash-')) continue;
    const p = path.join(root, entry);
    try {
      if (now - fs.lstatSync(p).mtimeMs > STALE_MS) fs.rmSync(p, { recursive: true, force: true });
    } catch {
      // Another start is cleaning it up.
    }
  }
  return root;
}

function archiveName(version, target) {
  return `wats-${version}-${target}.tar.gz`;
}

function releaseFileNames(name) {
  return [name, `${name}.minisig`, `${name}.sha256`, `${name}.sha256.minisig`];
}

function readLimited(file, limit) {
  const st = fs.lstatSync(file);
  if (!st.isFile()) throw new VerifyError(`${path.basename(file)} is not a file`);
  if (st.size > limit) throw new VerifyError(`${path.basename(file)} is too large`);
  return fs.readFileSync(file);
}

/**
 * Checks a cache entry as a fresh download: the directories from the root
 * down (real, this user's, not writable by others), the signatures, file
 * names and SHA-256, and that `wats` is byte for byte the archive's binary,
 * read from the descriptor that is then executed (or compared with the
 * path just before exec). Returns {path, fd, dev, ino}; the caller closes
 * fd.
 */
function verifyCached(dir, name, publicKey, uid) {
  checkDir(path.dirname(dir), uid);
  checkDir(dir, uid);
  const [archive, archiveSig, sha256, sha256Sig] = releaseFileNames(name).map((f, i) =>
    readLimited(path.join(dir, f), i === 0 ? MAX_ARCHIVE_BYTES : MAX_SMALL_BYTES),
  );
  verifyReleaseFiles(publicKey, name, {
    archive,
    archiveSig: archiveSig.toString('utf8'),
    sha256: sha256.toString('utf8'),
    sha256Sig: sha256Sig.toString('utf8'),
  });
  const expected = extractBinary(archive, name);
  const bin = path.join(dir, 'wats');
  const fd = fs.openSync(bin, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW || 0));
  try {
    const st = fs.fstatSync(fd);
    if (!st.isFile() || (st.mode & 0o100) === 0) {
      throw new VerifyError('the cached wats is not an executable file');
    }
    if (uid !== undefined && st.uid !== uid) throw new UnsafeError('the cached wats belongs to another user');
    if (process.platform !== 'win32' && (st.mode & 0o022) !== 0) {
      throw new UnsafeError('the cached wats is writable by group or others');
    }
    if (st.size !== expected.length) throw new VerifyError('the cached wats differs from the signed archive');
    const got = Buffer.alloc(expected.length);
    let off = 0;
    while (off < got.length) {
      const n = fs.readSync(fd, got, off, got.length - off, off);
      if (n === 0) break;
      off += n;
    }
    if (off !== got.length || !got.equals(expected)) {
      throw new VerifyError('the cached wats differs from the signed archive');
    }
    return { path: bin, fd, dev: st.dev, ino: st.ino };
  } catch (e) {
    fs.closeSync(fd);
    throw e;
  }
}

/** Just before exec: the path still names the verified file. */
function checkSame(handle) {
  const st = fs.lstatSync(handle.path);
  const fst = fs.fstatSync(handle.fd);
  if (!st.isFile() || st.dev !== handle.dev || st.ino !== handle.ino || fst.ino !== handle.ino) {
    throw new UnsafeError(`${handle.path} changed after it was verified: refusing to run it`);
  }
}

function writeFile(file, data, mode) {
  const fd = fs.openSync(file, 'wx', mode);
  try {
    fs.writeSync(fd, data);
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.chmodSync(file, mode);
}

function settings(opts) {
  return {
    env: opts.env || process.env,
    version: opts.version || PACKAGE.version,
    platform: opts.platform || process.platform,
    arch: opts.arch || process.arch,
    publicKey: opts.publicKey || RELEASE_PUBLIC_KEY,
    baseUrl: opts.baseUrl || RELEASE_BASE_URL,
    latestUrl: opts.latestUrl || LATEST_URL,
    allowLoopbackHttp: opts.allowLoopbackHttp === true, // tests only
    uid: currentUid(opts),
    log: opts.log || ((msg) => process.stderr.write(`${PACKAGE.name}: ${msg}\n`)),
  };
}

/**
 * A verified wats binary of `version` for `target`, from the cache or
 * downloaded into it: {path, fd, dev, ino}, fd open on the verified file.
 * Throws with a message for the user. Options (all but the tests' leave
 * them unset): version, target, publicKey, baseUrl, cacheRoot, env,
 * platform, arch, log, and the tests' uid and allowLoopbackHttp.
 */
async function ensureVerified(opts = {}) {
  const s = settings(opts);
  const { version, publicKey, uid, log } = s;
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(`this package's version ${version} is not a wats release (X.Y.Z)`);
  }
  const target = opts.target || releaseTarget(s.platform, s.arch);
  if (!target) throw new Error(unsupportedMessage(s.platform, s.arch, version));

  const root = prepareRoot(opts.cacheRoot || cacheRoot(s.env, s.platform), uid, log);
  const name = archiveName(version, target);
  const versionDir = path.join(root, version);
  const dir = path.join(versionDir, target);

  if (lexists(dir) || lexists(versionDir)) {
    try {
      return verifyCached(dir, name, publicKey, uid);
    } catch (e) {
      let bad = dir;
      try {
        checkDir(versionDir, uid);
      } catch {
        bad = versionDir;
      }
      if (lexists(bad)) {
        log(`the cached wats ${version} (${bad}) does not verify: ${e.message}; downloading it again`);
        removeDir(bad, root);
      }
    }
  }

  log(`downloading wats ${version} for ${target}`);
  const fetched = {};
  for (const [i, file] of releaseFileNames(name).entries()) {
    const url = `${s.baseUrl}/v${version}/${file}`;
    try {
      fetched[file] = await download(url, {
        limit: i === 0 ? MAX_ARCHIVE_BYTES : MAX_SMALL_BYTES,
        env: s.env,
        allowLoopbackHttp: s.allowLoopbackHttp,
      });
    } catch (e) {
      throw new Error(
        `cannot download ${url}: ${e.message}. Check your connection or proxy (HTTPS_PROXY), and that release v${version} exists for ${target}. Nothing was run.`,
      );
    }
  }
  const files = {
    archive: fetched[name],
    archiveSig: fetched[`${name}.minisig`].toString('utf8'),
    sha256: fetched[`${name}.sha256`].toString('utf8'),
    sha256Sig: fetched[`${name}.sha256.minisig`].toString('utf8'),
  };
  let binary;
  try {
    verifyReleaseFiles(publicKey, name, files);
    binary = extractBinary(files.archive, name);
  } catch (e) {
    throw new Error(`${e.message}: refusing to run it. Nothing was run or cached.`);
  }

  const staging = fs.mkdtempSync(path.join(root, '.tmp-'));
  try {
    fs.chmodSync(staging, 0o700);
    for (const file of releaseFileNames(name)) writeFile(path.join(staging, file), fetched[file], 0o600);
    const bin = path.join(staging, 'wats');
    writeFile(bin, binary, 0o755);
    let says = '';
    try {
      says = childProcess
        .execFileSync(bin, ['--version'], { timeout: 30000, stdio: ['ignore', 'pipe', 'ignore'] })
        .toString()
        .trim();
    } catch {
      // Reported below.
    }
    if (says !== `wats ${version}`) {
      throw new Error(
        `the downloaded binary does not run here, or is not wats ${version} (it says: ${says || 'nothing'}). Nothing was cached.`,
      );
    }
    if (lexists(versionDir)) {
      try {
        checkDir(versionDir, uid);
      } catch {
        removeDir(versionDir, root);
      }
    }
    try {
      fs.mkdirSync(versionDir, { mode: 0o700 });
    } catch (e) {
      if (e.code !== 'EEXIST') throw e;
    }
    try {
      fs.renameSync(staging, dir);
    } catch (e) {
      // Another start won the race: its entry is checked below.
      if (!lexists(dir)) throw e;
    }
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
  // Checked again where it now lives, and opened for exec.
  const handle = verifyCached(dir, name, publicKey, uid);
  log(`verified wats ${version} (minisign signatures and SHA-256) and cached it in ${dir}`);
  return handle;
}

/** The path of a verified binary (ensureVerified, descriptor closed). */
async function ensureBinary(opts = {}) {
  const handle = await ensureVerified(opts);
  fs.closeSync(handle.fd);
  return handle.path;
}

function parseVersion(v) {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(String(v));
  return m ? m.slice(1).map(Number) : null;
}

function olderThan(a, b) {
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] < b[i];
  return false;
}

/**
 * The latest release's signed security floor (security.json, ADR 0030),
 * looked up at most once a day within 5 s, best effort: any failure is
 * silent. Returns a warning when `version` is below the floor, else null.
 * Never stops wats from running: the version is pinned on purpose.
 */
async function securityWarning(opts = {}) {
  const s = settings(opts);
  try {
    const root = opts.root;
    const json = path.join(root, 'security.json');
    const sig = `${json}.minisig`;
    const stamp = path.join(root, '.security-checked');
    const now = opts.now || Date.now();
    let due = true;
    try {
      due = now - fs.lstatSync(stamp).mtimeMs > SECURITY_CHECK_EVERY_MS;
    } catch {
      // Never checked.
    }
    if (due) {
      fs.writeFileSync(stamp, '', { mode: 0o600 });
      try {
        const get = (file) =>
          download(`${s.latestUrl}/${file}`, {
            limit: MAX_SMALL_BYTES,
            env: s.env,
            allowLoopbackHttp: s.allowLoopbackHttp,
            deadlineMs: SECURITY_CHECK_DEADLINE_MS,
          });
        const [j, m] = await Promise.all([get('security.json'), get('security.json.minisig')]);
        verifySignature(s.publicKey, j, m.toString('utf8'), 'security.json');
        for (const [file, data] of [[json, j], [sig, m]]) {
          const tmp = `${file}.tmp-${process.pid}`;
          fs.writeFileSync(tmp, data, { mode: 0o600 });
          fs.renameSync(tmp, file);
        }
      } catch {
        // Best effort: offline, rate-limited, or a release without one.
      }
    }
    const j = readLimited(json, MAX_SMALL_BYTES);
    verifySignature(s.publicKey, j, readLimited(sig, MAX_SMALL_BYTES).toString('utf8'), 'security.json');
    const file = JSON.parse(j.toString('utf8'));
    const floor = parseVersion(file.security_floor);
    const latest = parseVersion(file.version);
    const ours = parseVersion(s.version);
    if (!floor || !latest || !ours || olderThan(latest, floor) || !olderThan(ours, floor)) return null;
    return (
      `SECURITY: wats ${s.version} is older than ${file.security_floor}, the security floor of the latest release ` +
      `(wats ${file.version}): later releases fix security issues. Use ${PACKAGE.name}@${file.version} ` +
      `(or at least ${file.security_floor}). Running ${s.version} anyway, as asked.`
    );
  } catch {
    return null;
  }
}

/** Runs the verified binary with `args`: stdio passed through, signals
 * forwarded, its exit status (or signal) becomes this process's. On Linux
 * the verified descriptor itself is executed (/proc/self/fd), elsewhere the
 * path, once checked to still name the verified file. */
function exec(handle, args, env = process.env) {
  checkSame(handle);
  const childEnv = { ...env, WATS_WRAPPER: 'npm' };
  const viaFd = process.platform === 'linux' && fs.existsSync('/proc/self/fd');
  const child = viaFd
    ? childProcess.spawn('/proc/self/fd/3', args, {
        stdio: ['inherit', 'inherit', 'inherit', handle.fd],
        argv0: handle.path,
        env: childEnv,
      })
    : childProcess.spawn(handle.path, args, { stdio: 'inherit', env: childEnv });
  fs.closeSync(handle.fd);
  const handlers = {};
  for (const sig of SIGNALS) {
    handlers[sig] = () => {
      try {
        child.kill(sig);
      } catch {
        // The child is gone already.
      }
    };
    process.on(sig, handlers[sig]);
  }
  const unhook = () => {
    for (const sig of SIGNALS) process.removeListener(sig, handlers[sig]);
  };
  child.once('error', (e) => {
    unhook();
    process.stderr.write(`${PACKAGE.name}: cannot run ${handle.path}: ${e.message}\n`);
    process.exit(1);
  });
  child.once('exit', (code, signal) => {
    unhook();
    if (signal) {
      process.kill(process.pid, signal);
      setTimeout(() => process.exit(128 + (os.constants.signals[signal] || 0)), 200).unref();
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

/** The command: `npx -y <package> mcp …` runs `wats mcp …`. */
async function main(args = process.argv.slice(2), opts = {}) {
  const log = opts.log || ((msg) => process.stderr.write(`${PACKAGE.name}: ${msg}\n`));
  let handle;
  try {
    handle = await ensureVerified(opts);
    const warning = await securityWarning({ ...opts, root: path.dirname(path.dirname(path.dirname(handle.path))) });
    if (warning) log(warning);
    exec(handle, args, opts.env || process.env);
  } catch (e) {
    process.stderr.write(`${PACKAGE.name}: ${e.message}\n`);
    process.exit(1);
  }
}

module.exports = {
  PACKAGE_NAME: PACKAGE.name,
  VERSION: PACKAGE.version,
  RELEASE_BASE_URL,
  LATEST_URL,
  CACHE_ENV,
  UnsafeError,
  releaseTarget,
  unsupportedMessage,
  cacheRoot,
  prepareRoot,
  ensureVerified,
  ensureBinary,
  verifyCached,
  checkSame,
  securityWarning,
  exec,
  main,
};

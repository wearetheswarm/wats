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
const { RELEASE_PUBLIC_KEY, VerifyError, verifyReleaseFiles, extractBinary } = require('./verify');

/** Where releases are downloaded from: <base>/v<version>/<file>. */
const RELEASE_BASE_URL = 'https://github.com/wearetheswarm/wats/releases/download';
/** The cache directory's name under the user cache dir, shared with the
 * PyPI wrapper (same layout, same checks). */
const CACHE_NAME = 'wats-wrapper';
/** Overrides the cache root (an absolute path). */
const CACHE_ENV = 'WATS_WRAPPER_CACHE_DIR';

const MAX_ARCHIVE_BYTES = 256 * 1024 * 1024;
const MAX_SMALL_BYTES = 64 * 1024;
const SIGNALS = ['SIGHUP', 'SIGINT', 'SIGQUIT', 'SIGTERM', 'SIGUSR2'];

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

/** Creates the cache root (0700) and checks that it is this user's. */
function prepareRoot(root) {
  fs.mkdirSync(root, { recursive: true, mode: 0o700 });
  const st = fs.statSync(root);
  if (!st.isDirectory()) throw new Error(`${root} is not a directory`);
  if (typeof process.getuid === 'function') {
    if (st.uid !== process.getuid()) {
      throw new Error(`${root} belongs to another user: refusing to use it as the cache`);
    }
    if ((st.mode & 0o077) !== 0) fs.chmodSync(root, 0o700);
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

/** Checks a cache entry as a fresh download: signatures, file names,
 * SHA-256, and that `wats` is byte for byte the archive's binary. */
function verifyCached(dir, name, publicKey) {
  const st = fs.lstatSync(dir);
  if (!st.isDirectory()) throw new VerifyError(`${dir} is not a directory`);
  const [archive, archiveSig, sha256, sha256Sig] = releaseFileNames(name).map((f, i) =>
    readLimited(path.join(dir, f), i === 0 ? MAX_ARCHIVE_BYTES : MAX_SMALL_BYTES),
  );
  const files = {
    archive,
    archiveSig: archiveSig.toString('utf8'),
    sha256: sha256.toString('utf8'),
    sha256Sig: sha256Sig.toString('utf8'),
  };
  verifyReleaseFiles(publicKey, name, files);
  const expected = extractBinary(archive, name);
  const bin = path.join(dir, 'wats');
  const binStat = fs.lstatSync(bin);
  if (!binStat.isFile() || (binStat.mode & 0o100) === 0) {
    throw new VerifyError('the cached wats is not an executable file');
  }
  if (binStat.size !== expected.length || !readLimited(bin, expected.length).equals(expected)) {
    throw new VerifyError('the cached wats differs from the signed archive');
  }
  return bin;
}

function lexists(file) {
  try {
    fs.lstatSync(file);
    return true;
  } catch {
    return false;
  }
}

function removeDir(dir, root) {
  const trash = path.join(root, `.trash-${process.pid}-${Date.now()}`);
  try {
    fs.renameSync(dir, trash);
  } catch {
    fs.rmSync(dir, { recursive: true, force: true });
    return;
  }
  fs.rmSync(trash, { recursive: true, force: true });
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

/**
 * The path of a verified wats binary of `version` for `target`, from the
 * cache or downloaded into it. Throws with a message for the user.
 * Options (all but the tests' leave them unset): version, target,
 * publicKey, baseUrl, cacheRoot, env, platform, arch, log.
 */
async function ensureBinary(opts = {}) {
  const env = opts.env || process.env;
  const version = opts.version || PACKAGE.version;
  const platform = opts.platform || process.platform;
  const arch = opts.arch || process.arch;
  const publicKey = opts.publicKey || RELEASE_PUBLIC_KEY;
  const baseUrl = opts.baseUrl || RELEASE_BASE_URL;
  const log = opts.log || ((msg) => process.stderr.write(`${PACKAGE.name}: ${msg}\n`));
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(`this package's version ${version} is not a wats release (X.Y.Z)`);
  }
  const target = opts.target || releaseTarget(platform, arch);
  if (!target) throw new Error(unsupportedMessage(platform, arch, version));

  const root = prepareRoot(opts.cacheRoot || cacheRoot(env, platform));
  const name = archiveName(version, target);
  const dir = path.join(root, version, target);

  if (lexists(dir)) {
    try {
      return verifyCached(dir, name, publicKey);
    } catch (e) {
      log(`the cached wats ${version} (${dir}) does not verify: ${e.message}; downloading it again`);
      removeDir(dir, root);
    }
  }

  log(`downloading wats ${version} for ${target}`);
  const fetched = {};
  for (const [i, file] of releaseFileNames(name).entries()) {
    const url = `${baseUrl}/v${version}/${file}`;
    try {
      fetched[file] = await download(url, {
        limit: i === 0 ? MAX_ARCHIVE_BYTES : MAX_SMALL_BYTES,
        env,
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
    fs.mkdirSync(path.join(root, version), { recursive: true, mode: 0o700 });
    try {
      fs.renameSync(staging, dir);
    } catch (e) {
      // Another start won the race: use its entry if it verifies.
      if (!lexists(dir)) throw e;
      return verifyCached(dir, name, publicKey);
    }
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
  log(`verified wats ${version} (minisign signatures and SHA-256) and cached it in ${dir}`);
  return path.join(dir, 'wats');
}

/** Runs `bin` with `args`: stdio passed through, signals forwarded, its
 * exit status (or signal) becomes this process's. */
function exec(bin, args, env = process.env) {
  const child = childProcess.spawn(bin, args, {
    stdio: 'inherit',
    env: { ...env, WATS_WRAPPER: 'npm' },
  });
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
    process.stderr.write(`${PACKAGE.name}: cannot run ${bin}: ${e.message}\n`);
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
  let bin;
  try {
    bin = await ensureBinary(opts);
  } catch (e) {
    process.stderr.write(`${PACKAGE.name}: ${e.message}\n`);
    process.exit(1);
  }
  exec(bin, args, opts.env || process.env);
}

module.exports = {
  PACKAGE_NAME: PACKAGE.name,
  VERSION: PACKAGE.version,
  RELEASE_BASE_URL,
  CACHE_ENV,
  releaseTarget,
  unsupportedMessage,
  cacheRoot,
  ensureBinary,
  verifyCached,
  exec,
  main,
};

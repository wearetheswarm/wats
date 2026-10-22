'use strict';
// Downloads over HTTPS with Node's own modules: redirects (GitHub sends
// release assets to its CDN), a size limit, an idle timeout, and
// HTTPS_PROXY / NO_PROXY through an HTTP CONNECT tunnel, an overall
// deadline. HTTPS only: plain http:// to a loopback host only with the
// tests' allowLoopbackHttp option. No token is ever sent: release
// downloads need none.

const http = require('node:http');
const https = require('node:https');
const tls = require('node:tls');

const MAX_REDIRECTS = 5;
const IDLE_TIMEOUT_MS = 60 * 1000;
const DEADLINE_MS = 10 * 60 * 1000;

function isLoopback(hostname) {
  const h = hostname.replace(/^\[|\]$/g, '');
  return h === 'localhost' || h === '::1' || /^127\.\d+\.\d+\.\d+$/.test(h);
}

/** Whether NO_PROXY (comma-separated hosts or domains, `*` for all)
 * covers `hostname`. */
function noProxy(hostname, list) {
  const host = hostname.toLowerCase().replace(/^\[|\]$/g, '');
  return String(list || '')
    .split(',')
    .map((e) => e.trim().toLowerCase())
    .filter(Boolean)
    .some((e) => {
      if (e === '*') return true;
      const bare = e.replace(/:\d+$/, '').replace(/^\*?\./, '');
      return host === bare || host.endsWith(`.${bare}`);
    });
}

/** The proxy URL for an https:// `url`, or null. */
function proxyFor(url, env) {
  if (url.protocol !== 'https:' || isLoopback(url.hostname)) return null;
  const raw = env.HTTPS_PROXY || env.https_proxy || '';
  if (!raw || noProxy(url.hostname, env.NO_PROXY ?? env.no_proxy)) return null;
  let proxy;
  try {
    proxy = new URL(raw.includes('://') ? raw : `http://${raw}`);
  } catch {
    throw new Error(`HTTPS_PROXY is not a URL: ${raw}`);
  }
  if (proxy.protocol !== 'http:') {
    throw new Error(`HTTPS_PROXY must be an http:// proxy URL, got ${proxy.protocol}//`);
  }
  return proxy;
}

/** An https.Agent whose connections go through `proxy` (CONNECT). */
function tunnelAgent(proxy) {
  const agent = new https.Agent({ keepAlive: false });
  agent.createConnection = (options, callback) => {
    const headers = { Host: `${options.host}:${options.port}` };
    if (proxy.username) {
      const auth = `${decodeURIComponent(proxy.username)}:${decodeURIComponent(proxy.password)}`;
      headers['Proxy-Authorization'] = `Basic ${Buffer.from(auth).toString('base64')}`;
    }
    const req = http.request({
      host: proxy.hostname.replace(/^\[|\]$/g, ''),
      port: proxy.port || 80,
      method: 'CONNECT',
      path: `${options.host}:${options.port}`,
      headers,
    });
    req.setTimeout(IDLE_TIMEOUT_MS, () => req.destroy(new Error('the proxy did not answer')));
    req.once('connect', (res, socket) => {
      if (res.statusCode !== 200) {
        socket.destroy();
        callback(new Error(`the proxy refused the connection (HTTP ${res.statusCode})`));
        return;
      }
      callback(null, tls.connect({ ...options, socket, servername: options.servername || options.host }));
    });
    req.once('error', (e) => callback(e));
    req.end();
  };
  return agent;
}

function getOnce(url, env, ca, track) {
  return new Promise((resolve, reject) => {
    const proxy = proxyFor(url, env);
    const mod = url.protocol === 'https:' ? https : http;
    const req = mod.get(url, {
      agent: proxy ? tunnelAgent(proxy) : undefined,
      ...(ca ? { ca } : {}),
      headers: { 'User-Agent': 'wats-npm-wrapper', Accept: 'application/octet-stream' },
    });
    track(req);
    req.setTimeout(IDLE_TIMEOUT_MS, () => req.destroy(new Error('the connection timed out')));
    req.once('response', resolve);
    req.once('error', reject);
  });
}

/**
 * GET `url` into a Buffer of at most `limit` bytes, following redirects,
 * within `deadlineMs` in all (10 minutes by default; each hop also aborts
 * after 60 s without data). Rejects on a non-200 answer, a non-https URL,
 * or more than `limit` bytes. Test-only options: `ca` replaces the trusted
 * certificates, `allowLoopbackHttp` accepts plain http:// to 127.0.0.1.
 */
async function download(url, { limit, env = process.env, ca, allowLoopbackHttp = false, deadlineMs = DEADLINE_MS } = {}) {
  let active = null;
  let timer;
  const expired = new Promise((_, reject) => {
    timer = setTimeout(() => {
      const e = new Error(`the download took longer than ${Math.round(deadlineMs / 1000)} s`);
      if (active) active.destroy(e);
      reject(e);
    }, deadlineMs);
  });
  try {
    return await Promise.race([fetchAll(), expired]);
  } finally {
    clearTimeout(timer);
  }

  async function fetchAll() {
    let current = new URL(url);
    for (let hop = 0; ; hop++) {
      const loopbackHttp = allowLoopbackHttp && current.protocol === 'http:' && isLoopback(current.hostname);
      if (current.protocol !== 'https:' && !loopbackHttp) {
        throw new Error(`refusing to download over ${current.protocol}// (${current.href}): HTTPS only`);
      }
      const res = await getOnce(current, env, ca, (req) => (active = req));
      if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location) {
        res.resume();
        if (hop >= MAX_REDIRECTS) throw new Error(`too many redirects for ${url}`);
        current = new URL(res.headers.location, current);
        continue;
      }
      if (res.statusCode !== 200) {
        res.resume();
        throw new Error(`HTTP ${res.statusCode} for ${current.href}`);
      }
      const declared = Number(res.headers['content-length']);
      if (declared > limit) {
        res.destroy();
        throw new Error(`${current.href} is larger than ${limit} bytes`);
      }
      return await new Promise((resolve, reject) => {
        const chunks = [];
        let size = 0;
        res.on('data', (chunk) => {
          size += chunk.length;
          if (size > limit) {
            res.destroy(new Error(`${current.href} is larger than ${limit} bytes`));
            return;
          }
          chunks.push(chunk);
        });
        res.once('end', () => resolve(Buffer.concat(chunks)));
        res.once('error', reject);
        res.once('aborted', () => reject(new Error(`the download of ${current.href} was cut`)));
      });
    }
  }
}

module.exports = { download, noProxy, proxyFor, isLoopback };

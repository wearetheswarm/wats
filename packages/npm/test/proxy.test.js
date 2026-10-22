'use strict';
// HTTPS_PROXY end to end, on loopback only: a CONNECT proxy and an HTTPS
// server with a throwaway self-signed certificate for `wats.test` (made
// with the openssl CLI; skipped without it). The client never resolves
// wats.test: the proxy maps it to 127.0.0.1.
const assert = require('node:assert/strict');
const childProcess = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const net = require('node:net');
const path = require('node:path');
const test = require('node:test');

const { download } = require('../lib/fetch');
const { tmpdir } = require('./helpers');

function makeCert() {
  const dir = tmpdir('wats-npm-tls-');
  const key = path.join(dir, 'key.pem');
  const cert = path.join(dir, 'cert.pem');
  const r = childProcess.spawnSync(
    'openssl',
    ['req', '-x509', '-newkey', 'rsa:2048', '-nodes',
      '-keyout', key, '-out', cert, '-subj', '/CN=wats.test',
      '-addext', 'subjectAltName=DNS:wats.test', '-days', '1'],
    { stdio: 'ignore' },
  );
  if (r.status !== 0) return null;
  return { key: fs.readFileSync(key), cert: fs.readFileSync(cert) };
}

const tls = makeCert();

function listen(server) {
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server.address().port)));
}

test('downloads go through an HTTPS_PROXY tunnel, with proxy auth', { skip: !tls && 'no openssl' }, async (t) => {
  const origin = https.createServer({ key: tls.key, cert: tls.cert }, (req, res) => {
    res.end(`hello ${req.url}`);
  });
  const originPort = await listen(origin);
  const connects = [];
  const sockets = new Set();
  const proxy = http.createServer();
  proxy.on('connect', (req, client, head) => {
    connects.push({ target: req.url, auth: req.headers['proxy-authorization'] });
    if (req.url === 'refused.test:443') {
      client.end('HTTP/1.1 403 Forbidden\r\n\r\n');
      return;
    }
    const upstream = net.connect(originPort, '127.0.0.1', () => {
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      upstream.write(head);
      upstream.pipe(client);
      client.pipe(upstream);
    });
    for (const s of [client, upstream]) {
      sockets.add(s);
      s.on('error', () => {});
    }
  });
  const proxyPort = await listen(proxy);
  t.after(() => {
    for (const s of sockets) s.destroy();
    origin.closeAllConnections?.();
    origin.close();
    proxy.close();
  });

  const env = { HTTPS_PROXY: `http://u%40x:p@127.0.0.1:${proxyPort}` };
  const body = await download(`https://wats.test:${originPort}/file`, { limit: 100, env, ca: tls.cert });
  assert.equal(body.toString(), 'hello /file');
  assert.deepEqual(connects, [
    { target: `wats.test:${originPort}`, auth: `Basic ${Buffer.from('u@x:p').toString('base64')}` },
  ]);

  // The certificate is checked inside the tunnel.
  await assert.rejects(download(`https://wats.test:${originPort}/file`, { limit: 100, env }), /certificate/);
  // A proxy that refuses says so.
  await assert.rejects(download('https://refused.test/file', { limit: 100, env, ca: tls.cert }), /HTTP 403/);
});

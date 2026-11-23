#!/usr/bin/env node
// SPDX-License-Identifier: AGPL-3.0-only
//
// A tiny static file server for the demo. No npm dependencies, no bundler.
// Serves this directory over plain HTTP on a loopback address, which browsers
// treat as a secure context (so WebTransport and WebCodecs are available).
//
//   node serve.mjs [--port 8080] [--host 127.0.0.1]

import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(fileURLToPath(new URL('.', import.meta.url)));

const args = process.argv.slice(2);
const port = numberArg('--port', 8080);
const host = stringArg('--host', '127.0.0.1');

const mime = new Map([
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.mjs', 'text/javascript; charset=utf-8'],
  ['.css', 'text/css; charset=utf-8'],
  ['.json', 'application/json; charset=utf-8'],
  ['.wasm', 'application/wasm'],
  ['.map', 'application/json; charset=utf-8'],
  ['.png', 'image/png'],
  ['.svg', 'image/svg+xml'],
  ['.ico', 'image/x-icon'],
  ['.txt', 'text/plain; charset=utf-8'],
  ['.md', 'text/markdown; charset=utf-8'],
]);

const server = createServer(async (request, response) => {
  const method = request.method ?? 'GET';
  if (method !== 'GET' && method !== 'HEAD') {
    respond(response, 405, 'text/plain; charset=utf-8', 'method not allowed');
    return;
  }
  let pathname;
  try {
    pathname = decodeURIComponent(new URL(request.url ?? '/', 'http://localhost').pathname);
  } catch {
    respond(response, 400, 'text/plain; charset=utf-8', 'bad request');
    return;
  }
  const candidate = resolve(root, `.${pathname}`);
  if (candidate !== root && !candidate.startsWith(root + sep)) {
    respond(response, 403, 'text/plain; charset=utf-8', 'forbidden');
    return;
  }

  let file = candidate;
  try {
    const info = await stat(file);
    if (info.isDirectory()) file = resolve(file, 'index.html');
  } catch {
    respond(response, 404, 'text/plain; charset=utf-8', 'not found');
    return;
  }

  try {
    const body = await readFile(file);
    const type = mime.get(extname(file).toLowerCase()) ?? 'application/octet-stream';
    response.writeHead(200, {
      'content-type': type,
      'content-length': body.length,
      'cache-control': 'no-store',
      // The demo page and the node API are different origins (and different
      // ports), so cross-origin fetches need CORS on the node side; this header
      // only relaxes isolation for the pkg/ module imports.
      'cross-origin-resource-policy': 'same-origin',
    });
    if (method === 'HEAD') response.end();
    else response.end(body);
  } catch (error) {
    console.error(`read ${file}:`, error);
    respond(response, 500, 'text/plain; charset=utf-8', 'internal error');
  }
});

server.listen(port, host, () => {
  console.log(`Ozma web demo: http://${host}:${port}/`);
  console.log('Serve from localhost so the page is a secure context for WebTransport.');
  console.log('Build web/pkg first with ./build.sh.');
});

function respond(response, status, type, text) {
  response.writeHead(status, { 'content-type': type, 'cache-control': 'no-store' });
  response.end(text);
}

function numberArg(name, fallback) {
  const index = args.indexOf(name);
  if (index === -1) return fallback;
  const value = Number.parseInt(args[index + 1] ?? '', 10);
  if (!Number.isFinite(value) || value <= 0) {
    console.error(`${name} needs a positive integer`);
    process.exit(2);
  }
  return value;
}

function stringArg(name, fallback) {
  const index = args.indexOf(name);
  return index === -1 ? fallback : args[index + 1] ?? fallback;
}

/* Runs the in-app 400 KiB benchmark through Chromium CDP with real worker time. */
import { spawn } from 'node:child_process';
import { rm } from 'node:fs/promises';

const root = process.cwd();
const cdpPort = 9300 + (process.pid % 500);
const profile = `/tmp/opencode/amte-benchmark-${process.pid}`;
const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
let server;
let browser;
let socket;

async function waitFor(getter, label, timeout = 20_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    try {
      const value = await getter();
      if (value) return value;
    } catch { /* service is still starting */ }
    await sleep(50);
  }
  throw new Error(`Timed out waiting for ${label}`);
}

async function cleanup() {
  try { socket?.close(); } catch { /* already closed */ }
  browser?.kill();
  server?.kill();
  await sleep(250);
  await rm(profile, { recursive: true, force: true });
}

try {
  server = spawn(process.execPath, ['node_modules/vite/bin/vite.js', '--config', 'vite.config.js', '--host', '127.0.0.1', '--port', '0'], { cwd: root, stdio: ['ignore', 'pipe', 'pipe'] });
  let viteOutput = '';
  server.stdout.on('data', (chunk) => { viteOutput += chunk; });
  server.stderr.on('data', (chunk) => { viteOutput += chunk; });
  const viteUrl = await waitFor(() => viteOutput.match(/http:\/\/127\.0\.0\.1:(\d+)\//)?.[0], 'Vite');

  browser = spawn('chromium', ['--headless=new', '--no-sandbox', '--disable-gpu', `--remote-debugging-port=${cdpPort}`, `--user-data-dir=${profile}`, 'about:blank'], { stdio: 'ignore' });
  const targets = await waitFor(async () => {
    const response = await fetch(`http://127.0.0.1:${cdpPort}/json/list`);
    return response.ok ? response.json() : null;
  }, 'Chromium CDP');
  const page = targets.find((target) => target.type === 'page');
  if (!page) throw new Error('Chromium did not expose a page target');

  socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
  let nextId = 1;
  const pending = new Map();
  socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (!message.id) return;
    const request = pending.get(message.id);
    pending.delete(message.id);
    if (!request) return;
    if (message.error) request.reject(new Error(message.error.message));
    else request.resolve(message.result);
  };
  const cdp = (method, params = {}) => {
    const id = nextId++;
    socket.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
  };
  const evaluate = async (expression) => {
    const result = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.text);
    return result.result.value;
  };

  await cdp('Runtime.enable');
  await cdp('Page.navigate', { url: `${viteUrl}?benchmark=1` });
  const encoded = await waitFor(() => evaluate('document.body.dataset.amteBenchmark || null'), 'benchmark completion', 20_000);
  const result = JSON.parse(encoded);
  if (!result.converged || result.workerVersion !== result.sourceVersion) {
    throw new Error(`Worker did not converge (${result.workerVersion}/${result.sourceVersion})`);
  }
  console.log(result);
} catch (error) {
  console.error(`Benchmark failed: ${error.message}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}

/* Runs the in-app 400 KiB benchmark through Chromium CDP with real worker time. */
import { spawn } from 'node:child_process';
import { rm } from 'node:fs/promises';

const root = process.cwd();
const searchCheck = process.argv.includes('--search-check');
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

function assertBrowser(condition, message) {
  if (!condition) throw new Error(message);
}

try {
  const viteArgs = searchCheck
    ? ['node_modules/vite/bin/vite.js', 'preview', '--host', '127.0.0.1', '--port', '0']
    : ['node_modules/vite/bin/vite.js', '--config', 'vite.config.js', '--host', '127.0.0.1', '--port', '0'];
  server = spawn(process.execPath, viteArgs, { cwd: root, stdio: ['ignore', 'pipe', 'pipe'] });
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
  const press = async (key, { code = key, modifiers = 0, virtualKeyCode = 0 } = {}) => {
    const text = key === 'Enter' ? '\r' : (key.length === 1 && !modifiers ? key : undefined);
    await cdp('Input.dispatchKeyEvent', { type: 'keyDown', key, code, modifiers, windowsVirtualKeyCode: virtualKeyCode, nativeVirtualKeyCode: virtualKeyCode, text, unmodifiedText: text });
    await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key, code, modifiers, windowsVirtualKeyCode: virtualKeyCode, nativeVirtualKeyCode: virtualKeyCode });
  };
  const ctrlPress = async (key, { code = key, virtualKeyCode = 0 } = {}) => {
    await cdp('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Control', code: 'ControlLeft', windowsVirtualKeyCode: 17, nativeVirtualKeyCode: 17 });
    await press(key, { code, modifiers: 2, virtualKeyCode });
    await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Control', code: 'ControlLeft', windowsVirtualKeyCode: 17, nativeVirtualKeyCode: 17 });
  };

  await cdp('Runtime.enable');
  if (searchCheck) {
    await cdp('Page.navigate', { url: viteUrl });
    await waitFor(() => evaluate('window.__amteReady?.then(() => true)'), 'application startup');
    await evaluate(`window.__amteReady.then((app) => {
      app.replaceDocument('alpha beta ALPHA alpha');
      app.view.focus();
      return true;
    })`);

    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    await waitFor(() => evaluate('document.querySelector("#search-dialog")?.open'), 'search dialog opening');
    assertBrowser(await evaluate('document.activeElement?.id === "search-term"'), 'Search input did not receive focus');
    assertBrowser(await evaluate('(() => { const dialog = document.querySelector("#search-dialog"); const box = dialog.getBoundingClientRect(); return box.width > 0 && box.width <= innerWidth && box.height > 0 && box.height <= innerHeight && getComputedStyle(dialog).backgroundColor !== "rgba(0, 0, 0, 0)"; })()'), 'Monokai search dialog was not visible and bounded');
    await cdp('Input.insertText', { text: 'alpha' });
    await press('Tab', { code: 'Tab', virtualKeyCode: 9 });
    await press('Tab', { code: 'Tab', virtualKeyCode: 9 });
    await press('Tab', { code: 'Tab', virtualKeyCode: 9 });
    assertBrowser(await evaluate('document.querySelector("#search-dialog").contains(document.activeElement)'), 'Tab escaped the search dialog');
    await press('Enter', { code: 'Enter', virtualKeyCode: 13 });
    await waitFor(async () => !(await evaluate('document.querySelector("#search-dialog")?.open')), 'search dialog closing');
    assertBrowser(await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)'), 'Search submit did not select the first match');
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === '0:5', 'First match selection was incorrect');
    assertBrowser(await evaluate('Boolean(document.querySelector(".cm-searchMatch-selected"))'), 'Current match was not decorated');

    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === '11:16', 'F7 did not move to the next case-insensitive match');
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === '0:5', 'F7 did not wrap');
    await press('F6', { code: 'F6', virtualKeyCode: 117 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === '17:22', 'F6 did not wrap to the previous match');
    const beforeModified = await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)');
    await press('F7', { code: 'F7', modifiers: 2, virtualKeyCode: 118 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === beforeModified, 'Modified F7 was handled');

    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    await press('Escape', { code: 'Escape', virtualKeyCode: 27 });
    await waitFor(async () => !(await evaluate('document.querySelector("#search-dialog")?.open')), 'Escape closing search dialog');
    assertBrowser(await evaluate('document.activeElement?.classList.contains("cm-content")'), 'Search dialog did not restore editor focus');

    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    await ctrlPress('a', { code: 'KeyA', virtualKeyCode: 65 });
    await press('Backspace', { code: 'Backspace', virtualKeyCode: 8 });
    await press('Enter', { code: 'Enter', virtualKeyCode: 13 });
    const emptySearchSelection = await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)');
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === emptySearchSelection, 'F7 changed selection after an empty search');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    await cdp('Input.insertText', { text: 'missing' });
    await press('Enter', { code: 'Enter', virtualKeyCode: 13 });
    const noMatchSelection = await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)');
    await press('F6', { code: 'F6', virtualKeyCode: 117 });
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === noMatchSelection, 'No-match navigation changed selection');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    await cdp('Input.insertText', { text: 'alpha' });
    await press('Enter', { code: 'Enter', virtualKeyCode: 13 });
    await waitFor(async () => !(await evaluate('document.querySelector("#search-dialog")?.open')), 'search dialog closing after no-match recovery');

    await ctrlPress('p', { code: 'KeyP', virtualKeyCode: 80 });
    await waitFor(() => evaluate('!document.querySelector("#preview").hidden'), 'preview mode').catch(async () => {
      throw new Error(`Preview shortcut failed (dialog=${await evaluate('document.querySelector("#search-dialog").open')}, focus=${await evaluate('document.activeElement?.className || document.activeElement?.id')})`);
    });
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser(await evaluate('document.querySelector("#preview").hidden'), 'F7 did not return a matching preview to source mode');
    assertBrowser(await evaluate('document.documentElement.scrollHeight <= window.innerHeight && document.documentElement.scrollWidth <= window.innerWidth'), 'Search introduced root scrolling');

    await evaluate('window.__amteReady.then((app) => { app.replaceDocument("replacement text"); return true; })');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    assertBrowser((await evaluate('document.querySelector("#search-term").value')) === '', 'Document replacement retained a stale search term');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    const replacementSelection = await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)');
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser((await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}`)')) === replacementSelection, 'F7 changed a document with no active term');

    await evaluate('window.__amteReady.then((app) => { app.replaceDocument("alpha alpha"); app.view.focus(); return true; })');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    await cdp('Input.insertText', { text: 'alpha' });
    await press('Enter', { code: 'Enter', virtualKeyCode: 13 });
    await evaluate('window.__amteReady.then((app) => { app.view.dispatch({ changes: { from: 0, insert: "x " } }); return true; })');
    await press('F7', { code: 'F7', virtualKeyCode: 118 });
    assertBrowser(await evaluate('window.__amteReady.then((app) => app.view.state.sliceDoc(app.view.state.selection.main.from, app.view.state.selection.main.to))'), 'Active search did not update safely after editing');

    await ctrlPress('t', { code: 'KeyT', virtualKeyCode: 84 });
    await waitFor(() => evaluate('document.documentElement.dataset.theme === "vintage-msdos-edit"'), 'vintage theme');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    assertBrowser(await evaluate('(() => { const box = document.querySelector("#search-dialog").getBoundingClientRect(); return box.width > 0 && box.width <= innerWidth && getComputedStyle(document.querySelector("#search-dialog")).backgroundColor !== "rgba(0, 0, 0, 0)"; })()'), 'Vintage search dialog was not visible and bounded');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    console.log({ searchCheck: 'passed', themes: ['monokai', 'vintage-msdos-edit'] });
  } else {
    await cdp('Page.navigate', { url: `${viteUrl}?benchmark=1` });
    const encoded = await waitFor(() => evaluate('document.body.dataset.amteBenchmark || null'), 'benchmark completion', 20_000);
    const result = JSON.parse(encoded);
    if (!result.converged || result.workerVersion !== result.sourceVersion) {
      throw new Error(`Worker did not converge (${result.workerVersion}/${result.sourceVersion})`);
    }
    console.log(result);
  }
} catch (error) {
  console.error(`Benchmark failed: ${error.message}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}

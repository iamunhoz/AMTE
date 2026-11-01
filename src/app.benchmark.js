/* Runs the in-app 400 KiB benchmark through Chromium CDP with real worker time. */
import { spawn } from 'node:child_process';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';

const root = process.cwd();
const searchCheck = process.argv.includes('--search-check');
const projectOpenCheck = process.argv.includes('--project-open-check');
const pickerFixturePath = new URL('./project-picker-response.fixture.json', import.meta.url);
const activationFixturePath = new URL('./project-activation-response.fixture.json', import.meta.url);
const pickerFixture = JSON.parse(await readFile(pickerFixturePath, 'utf8'));
const activationFixture = JSON.parse(await readFile(activationFixturePath, 'utf8'));
const INPUT_P95_LIMIT_MS = 8;
// A buffer handoff replaces the live CodeMirror state and is allowed two
// 60 Hz frames; above that it is no longer responsive.
const SWITCH_P95_LIMIT_MS = 32;
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
  await rm(pickerFixture.root, { recursive: true, force: true });
}

function assertBrowser(condition, message) {
  if (!condition) throw new Error(message);
}

try {
  // Measure the production bundle in both modes. This local server exists only
  // for Chromium CDP; the packaged Tauri release embeds these same assets.
  const viteArgs = ['node_modules/vite/bin/vite.js', 'preview', '--host', '127.0.0.1', '--port', '0'];
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
    if (!message.id) {
      if (message.method === 'Runtime.exceptionThrown') console.error('Browser exception:', message.params.exceptionDetails.exception?.description || message.params.exceptionDetails.text);
      return;
    }
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
  await cdp('Page.enable');
  await cdp('DOM.enable');
  await cdp('Accessibility.enable');
  const axState = async (selector) => {
    try {
      const { root } = await cdp('DOM.getDocument');
      const { nodeId } = await cdp('DOM.querySelector', { nodeId: root.nodeId, selector });
      if (!nodeId) return [{ absent: true }];
      const { nodes } = await cdp('Accessibility.getPartialAXTree', { nodeId, fetchRelatives: false });
      return nodes.map((node) => ({ ignored: node.ignored, role: node.role?.value, name: node.name?.value, reasons: (node.ignoredReasons || []).map((reason) => reason.name) }));
    } catch (error) {
      // Chromium omits display:none nodes from the AX tree altogether. That is
      // the desired result; other builds retain an ignored placeholder above.
      if (/Could not find node/.test(String(error))) return [{ absent: true }];
      throw error;
    }
  };
  if (searchCheck || projectOpenCheck) {
    await mkdir(pickerFixture.root, { recursive: true });
    await writeFile(`${pickerFixture.root}/root-entry.md`, '# Disposable picker entry\n');
    await cdp('Page.addScriptToEvaluateOnNewDocument', {
      source: `globalThis.__TAURI__ = { core: { invoke: async (command, args = {}) => {
        const picker = globalThis.__AMTE_PICKER_TEST__;
        if (command === 'restore_last_project') return null;
        if (command === 'pick_project') { picker.pickCalls += 1; return picker.mode === 'cancel' ? null : picker.candidate; }
        if (command === 'activate_picked_project') {
          picker.activationCalls += 1; picker.activationArgs.push(args);
          if (picker.mode === 'fail') throw new Error('injected activation failure');
          return picker.project;
        }
        if (command === 'list_project_directory') return picker.entries;
        if (command === 'save_document' || command === 'autosave_document') return { fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true, reconciliationPending: false, writeRevision: args.revision, baselineHash: 'accepted', document: { relativePath: 'check.md', documentId: args.documentId, writeRevision: args.revision, baselineHash: 'accepted' } };
        if (command === 'save_document_as') return { fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true, reconciliationPending: false, writeRevision: args.revision, baselineHash: 'accepted', document: { relativePath: 'saved-as.md', documentId: 2, writeRevision: args.revision, baselineHash: 'accepted' } };
        return {};
      } } }; globalThis.__AMTE_PICKER_TEST__ = { mode: 'open', pickCalls: 0, activationCalls: 0, activationArgs: [], candidate: ${JSON.stringify(pickerFixture)}, project: ${JSON.stringify(activationFixture)}, entries: [{ name: 'root-entry.md', relativePath: 'root-entry.md', kind: 'file' }] };`,
    });
    await cdp('Page.navigate', { url: `${viteUrl}?browser-check=1` });
    const started = await waitFor(() => evaluate('window.__amteReady?.then(() => true, (error) => `startup error: ${error?.stack || error}`)'), 'application startup');
    if (typeof started === 'string') throw new Error(started);
    if (projectOpenCheck) {
      const opened = await evaluate(`window.__amteReady.then(async (app) => {
        await app.openProject();
        return {
          path: document.querySelector('#project-path').textContent,
          entries: [...document.querySelectorAll('#project-tree [role=treeitem]')].map((row) => row.textContent),
          errorHidden: document.querySelector('#project-error').hidden,
          calls: globalThis.__AMTE_PICKER_TEST__,
        };
      })`);
      assertBrowser(opened.path === activationFixture.root, `Picker activation did not install the realpath (${JSON.stringify(opened)})`);
      assertBrowser(opened.entries.some((entry) => entry.includes('root-entry.md')), `Picker activation did not install the disposable Markdown root entry (${JSON.stringify(opened)})`);
      assertBrowser(opened.errorHidden && opened.calls.pickCalls === 1 && opened.calls.activationCalls === 1 && opened.calls.activationArgs[0]?.candidateId === pickerFixture.candidateId, `Picker response was not consumed exactly once (${JSON.stringify(opened)})`);

      const existing = await evaluate(`window.__amteReady.then(async (app) => {
        const test = globalThis.__AMTE_PICKER_TEST__; test.mode = 'open'; const before = test.activationCalls;
        await app.openProject();
        return { path: document.querySelector('#project-path').textContent, activations: test.activationCalls - before };
      })`);
      assertBrowser(existing.path === activationFixture.root && existing.activations === 1, `Existing project was not replaced by one selected project (${JSON.stringify(existing)})`);

      const cancelled = await evaluate(`window.__amteReady.then(async (app) => {
        const test = globalThis.__AMTE_PICKER_TEST__; test.mode = 'cancel';
        await app.openProject();
        return { path: document.querySelector('#project-path').textContent, calls: test };
      })`);
      assertBrowser(cancelled.path === activationFixture.root && cancelled.calls.activationCalls === 2, `Cancelled picker changed the active project (${JSON.stringify(cancelled)})`);

      const failed = await evaluate(`window.__amteReady.then(async (app) => {
        const test = globalThis.__AMTE_PICKER_TEST__; test.mode = 'fail';
        await app.openProject();
        const error = document.querySelector('#project-error');
        return { path: document.querySelector('#project-path').textContent, hidden: error.hidden, role: error.getAttribute('role'), text: error.textContent, calls: test };
      })`);
      assertBrowser(failed.path === activationFixture.root && !failed.hidden && failed.role === 'alert' && /injected activation failure/.test(failed.text), `Activation failure was not persistent and accessible while retaining the old project (${JSON.stringify(failed)})`);

      const serialized = await evaluate(`window.__amteReady.then(async (app) => {
        const test = globalThis.__AMTE_PICKER_TEST__; test.mode = 'open'; const before = test.activationCalls;
        await Promise.all([app.openProject(), app.openProject()]);
        const error = document.querySelector('#project-error');
        return { activations: test.activationCalls - before, errorHidden: error.hidden, path: document.querySelector('#project-path').textContent };
      })`);
      assertBrowser(serialized.activations === 1 && serialized.errorHidden && serialized.path === activationFixture.root, `Duplicate Ctrl+O activation was not serialized (${JSON.stringify(serialized)})`);
      console.log({ projectOpenCheck: 'passed', root: activationFixture.root, markdown: 'root-entry.md' });
    }
    const exerciseActivity = async (path) => {
      await evaluate(`window.__amteReady.then((app) => { app.benchmarkSwitchDocument({ projectId: 1, root: '/mock-project' }, '${path}', ''); app.view.focus(); return true; })`);
      await cdp('Input.insertText', { text: 'hello' });
      await press('Backspace', { code: 'Backspace', virtualKeyCode: 8 });
      await ctrlPress('a', { code: 'KeyA', virtualKeyCode: 65 });
      await cdp('Input.insertText', { text: 'world' });
      await ctrlPress('z', { code: 'KeyZ', virtualKeyCode: 90 });
      await ctrlPress('y', { code: 'KeyY', virtualKeyCode: 89 });
      const activityFooter = await evaluate('document.querySelector("#daily-count").textContent');
      assertBrowser(activityFooter === '(-): 3 (+): 4', `Typing, deletion, replacement, undo, and redo did not produce gross activity (${activityFooter})`);
    };
    await exerciseActivity('monokai.md');
    assertBrowser(await evaluate('document.documentElement.scrollHeight <= window.innerHeight && document.documentElement.scrollWidth <= window.innerWidth'), 'Monokai activity footer introduced root scrolling');
    const saveIndicatorStyle = () => evaluate(`(() => {
      const element = document.querySelector('#save-indicator');
      const style = getComputedStyle(element); const box = element.getBoundingClientRect();
      const metrics = document.querySelector('#document-metrics').getBoundingClientRect();
      const overlapsMetricsText = [...document.querySelector('#document-metrics').querySelectorAll('#total-count, #daily-count')].some((node) => { const rect = node.getBoundingClientRect(); return box.left < rect.right && box.right > rect.left && box.top < rect.bottom && box.bottom > rect.top; });
      return { hidden: element.hidden, display: style.display, visibility: style.visibility, opacity: style.opacity, color: style.color, zIndex: style.zIndex, right: innerWidth - box.right, bottom: innerHeight - box.bottom, width: box.width, height: box.height, overlapsMetricsText, footerBottom: innerHeight - metrics.bottom, rootScroll: document.documentElement.scrollHeight > innerHeight || document.documentElement.scrollWidth > innerWidth };
    })()`);
    const assertVisibleSaveIndicator = async (theme) => {
      const style = await saveIndicatorStyle();
      assertBrowser(!style.hidden && style.display !== 'none' && style.visibility === 'visible' && Number(style.opacity) > 0 && style.color !== 'rgba(0, 0, 0, 0)' && Number(style.zIndex) >= 20 && style.width > 0 && style.height > 0 && style.right >= 0 && style.bottom >= 0 && !style.overlapsMetricsText && !style.rootScroll, `${theme} save indicator was not visibly painted at the bottom-right (${JSON.stringify(style)})`);
      const screenshot = await cdp('Page.captureScreenshot', { format: 'png' });
      assertBrowser(typeof screenshot.data === 'string' && screenshot.data.length > 100, `${theme} save indicator screenshot was unavailable`);
    };
    await evaluate(`window.__amteReady.then(async (app) => { app.benchmarkSwitchDocument({ projectId: 1, root: '/mock-project' }, 'check.md', 'saved'); return app.browserCheckSave('manual'); })`);
    await assertVisibleSaveIndicator('Monokai manual save');
    // Start and inspect the 4999ms boundary entirely in the production page;
    // CDP screenshot round-trips must not consume the five-second interval.
    const visibleAt4999 = await evaluate(`window.__amteReady.then(async (app) => {
      await app.browserCheckSave('autosave');
      await new Promise((resolve) => setTimeout(resolve, 4999));
      const element = document.querySelector('#save-indicator'); const style = getComputedStyle(element);
      return !element.hidden && style.display !== 'none' && style.visibility === 'visible' && Number(style.opacity) > 0;
    })`);
    assertBrowser(visibleAt4999, 'Save indicator hid before 5000ms');
    await new Promise((resolve) => setTimeout(resolve, 25));
    const expiredSaveIndicator = await saveIndicatorStyle();
    assertBrowser(expiredSaveIndicator.hidden, `Save indicator remained visible after 5000ms (${JSON.stringify(expiredSaveIndicator)})`);
    await evaluate(`window.__amteReady.then((app) => app.browserCheckSave('autosave'))`);
    await assertVisibleSaveIndicator('Monokai autosave retrigger');
    await evaluate(`window.__amteReady.then((app) => app.browserCheckSave('save-all'))`);
    await assertVisibleSaveIndicator('Monokai Save All');
    await evaluate(`window.__amteReady.then((app) => app.browserCheckSave('save-as'))`);
    await assertVisibleSaveIndicator('Monokai Save As');
    await evaluate(`window.__amteReady.then((app) => { app.benchmarkSwitchDocument({ projectId: 1, root: '/mock-project' }, 'other.md', 'disk content'); return true; })`);
    assertBrowser(await evaluate('document.querySelector("#daily-count").textContent === "(-): 0 (+): 0"'), 'Opening another file counted disk content as editing');
    await evaluate(`window.__amteReady.then((app) => { app.benchmarkSwitchDocument({ projectId: 1, root: '/mock-project' }, 'monokai.md', 'disk content'); return true; })`);
    assertBrowser(await evaluate('document.querySelector("#daily-count").textContent === "(-): 3 (+): 4"'), 'File switching did not restore isolated activity');
    await evaluate('window.__amteReady.then((app) => { app.replaceDocument("restored disk content"); return true; })');
    assertBrowser(await evaluate('document.querySelector("#daily-count").textContent === "(-): 0 (+): 0"'), 'Document replacement/restoration counted as editing');
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
    { const selection = await evaluate('window.__amteReady.then((app) => `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}:${app.view.state.doc.toString()}`)'); assertBrowser(selection.startsWith('0:5:'), `First match selection was incorrect (${selection})`); }
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
    // A dialog's `close` event can be queued after its `open` state flips.
    // Let its production focus-restoration handler finish before exercising an
    // unrelated preview transition, exactly as separate user input tasks do.
    await evaluate('new Promise((resolve) => setTimeout(resolve, 0))');

    // CDP's Control modifier delivery is flaky under some headless Chromium
    // builds; keyboard routing is source-tested, while this exercises preview
    // state preservation deterministically.
    let previewVisible = await evaluate('window.__amteReady.then((app) => { app.togglePreview(); return !document.querySelector("#preview").hidden; })');
    if (!previewVisible) previewVisible = await evaluate('window.__amteReady.then((app) => { app.togglePreview(); return !document.querySelector("#preview").hidden; })');
    await waitFor(() => evaluate('!document.querySelector("#preview").hidden'), 'preview mode').catch(async () => {
      throw new Error(`Preview shortcut failed (initial=${previewVisible}, hidden=${await evaluate('document.querySelector("#preview").hidden')}, search=${await evaluate('document.querySelector("#search-dialog").open')}, dirty=${await evaluate('document.querySelector("#dirty-dialog").open')}, focus=${await evaluate('document.activeElement?.className || document.activeElement?.id')})`);
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
    await evaluate(`window.__amteReady.then((app) => app.browserCheckSave('autosave'))`);
    await assertVisibleSaveIndicator('Vintage autosave');
    await exerciseActivity('vintage.md');
    assertBrowser(await evaluate('document.documentElement.scrollHeight <= window.innerHeight && document.documentElement.scrollWidth <= window.innerWidth'), 'Vintage activity footer introduced root scrolling');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    assertBrowser(await evaluate('(() => { const box = document.querySelector("#search-dialog").getBoundingClientRect(); return box.width > 0 && box.width <= innerWidth && getComputedStyle(document.querySelector("#search-dialog")).backgroundColor !== "rgba(0, 0, 0, 0)"; })()'), 'Vintage search dialog was not visible and bounded');
    await press('F5', { code: 'F5', virtualKeyCode: 116 });
    const state = () => evaluate('window.__amteReady.then((app) => ({ doc: app.view.state.doc.toString(), selection: `${app.view.state.selection.main.from}:${app.view.state.selection.main.to}` }))');
    const replaceEditorDocument = (document, selection = null) => evaluate(`window.__amteReady.then((app) => { app.replaceDocument(${JSON.stringify(document)}); if (${JSON.stringify(selection)}) app.view.dispatch({ selection: ${JSON.stringify(selection)} }); app.view.focus(); return true; })`);
    const assertPairBehavior = async (open, close) => {
      await replaceEditorDocument('');
      await evaluate('window.__amteReady.then((app) => { const dispatch = app.view.dispatch.bind(app.view); let changes = 0; app.view.dispatch = (...args) => { changes += 1; return dispatch(...args); }; app.__pairDispatches = () => changes; return true; })');
      await press(open);
      let snapshot = await state();
      assertBrowser(snapshot.doc === `${open}${close}` && snapshot.selection === '1:1', `Pair insertion/caret failed for ${open}${close}`);
      assertBrowser(await evaluate('window.__amteReady.then((app) => app.__pairDispatches())') === 1, `Pair insertion dispatched more than once for ${open}${close}`);
      await press(close);
      snapshot = await state();
      assertBrowser(snapshot.doc === `${open}${close}` && snapshot.selection === '2:2', `Pair closing skip failed for ${open}${close}`);
      await replaceEditorDocument('');
      await press(open);
      await press('Backspace', { code: 'Backspace', virtualKeyCode: 8 });
      assertBrowser((await state()).doc === '', `Pair backspace failed for ${open}${close}`);
      await replaceEditorDocument('');
      await press(open);
      await ctrlPress('z', { code: 'KeyZ', virtualKeyCode: 90 });
      assertBrowser((await state()).doc === '', `Pair undo failed for ${open}${close}`);
      await ctrlPress('y', { code: 'KeyY', virtualKeyCode: 89 });
      assertBrowser((await state()).doc === `${open}${close}`, `Pair redo failed for ${open}${close}`);
    };
    for (const [open, close] of [['(', ')'], ['[', ']'], ['{', '}'], ['"', '"'], ["'", "'"], ['`', '`']]) await assertPairBehavior(open, close);
    await replaceEditorDocument('text', { anchor: 0, head: 4 });
    await press('(');
    assertBrowser((await state()).doc === '(text)', 'Selected text was not wrapped by the CodeMirror pair extension');

    // Exercise the real worker -> patched DOM path, not a synthetic tree. This
    // runs in vintage first, then Monokai below, at the narrow sidebar width.
    await replaceEditorDocument('# Parent\n## Child\n### Grandchild\n## Sibling\n# Tail\n###### Six');
    await waitFor(() => evaluate('document.querySelectorAll("#chapter-list [role=treeitem]").length === 6'), 'H1-H6 heading tree');
    const treeSemantics = await evaluate(`(() => {
      const tree = document.querySelector('#chapter-list');
      const items = [...tree.querySelectorAll('[role=treeitem]')];
      const parent = items[0];
      return {
        role: tree.getAttribute('role'), levels: items.map((item) => item.getAttribute('aria-level')).join(','),
        expanded: parent.getAttribute('aria-expanded'), disclosure: parent.querySelector('.chapter-disclosure')?.getAttribute('aria-label'),
        titleButton: parent.querySelector('.chapter-title')?.tagName, rootScroll: document.documentElement.scrollHeight > innerHeight || document.documentElement.scrollWidth > innerWidth,
      };
    })()`);
    assertBrowser(treeSemantics.role === 'tree' && treeSemantics.levels === '1,2,3,2,1,6' && treeSemantics.expanded === 'true' && /^Collapse /.test(treeSemantics.disclosure || '') && treeSemantics.titleButton === 'BUTTON' && !treeSemantics.rootScroll, `Heading tree ARIA was incomplete (${JSON.stringify(treeSemantics)})`);
    await press('F3', { code: 'F3', virtualKeyCode: 114 });
    assertBrowser(await evaluate('document.activeElement?.classList.contains("chapter-title")'), 'F3 did not focus the roving heading title');
    await press('ArrowDown', { code: 'ArrowDown', virtualKeyCode: 40 });
    assertBrowser(await evaluate('window.__amteReady.then((app) => app.view.state.selection.main.anchor === 9)'), 'Down did not navigate immediately to the next visible heading');
    // A short test-only scrollport makes scrollHeight report card contribution
    // rather than its normal viewport-filling minimum.
    await evaluate('document.querySelector("#chapter-list").style.maxHeight = "1px"');
    // Fold the nested child first, then the parent. The child state must
    // survive the parent round trip even though its row is removed from layout.
    const fullyExpanded = await evaluate('document.querySelector("#chapter-list").scrollHeight');
    await evaluate('document.querySelector(".chapter-card[data-chapter-index=\\\"1\\\"] .chapter-disclosure").click()');
    assertBrowser(await evaluate('document.querySelector(".chapter-card[data-chapter-index=\\\"1\\\"]").getAttribute("aria-expanded") === "false"'), 'Nested child did not collapse before parent');
    await evaluate('document.querySelector(".chapter-card[data-chapter-index=\\\"0\\\"] .chapter-disclosure").click()');
    const beforeFold = await evaluate(`(() => {
      const tree = document.querySelector('#chapter-list');
      return { scrollHeight: tree.scrollHeight, visibleCards: [...tree.querySelectorAll('.chapter-card:not([hidden])')].length, cards: tree.querySelectorAll('.chapter-card').length };
    })()`);
    const folded = await evaluate(`(() => {
      const parent = document.querySelector('.chapter-card[data-chapter-index="0"]');
      const hidden = [...document.querySelectorAll('.chapter-card[hidden]')];
      const tree = document.querySelector('#chapter-list');
      return {
        expanded: parent.getAttribute('aria-expanded'), label: parent.querySelector('.chapter-disclosure').getAttribute('aria-label'),
        hidden: hidden.length, active: document.querySelector('[aria-current="true"]')?.dataset.chapterIndex,
        scrollHeight: tree.scrollHeight, visibleCards: [...tree.querySelectorAll('.chapter-card:not([hidden])')].length, cards: tree.querySelectorAll('.chapter-card').length,
        rows: hidden.map((row) => {
          const box = row.getBoundingClientRect(); const frame = row.querySelector('.box-frame'); const frameBox = frame.getBoundingClientRect();
          return { display: getComputedStyle(row).display, width: box.width, height: box.height, rects: row.getClientRects().length, ariaHidden: row.getAttribute('aria-hidden'), inert: row.inert, buttonTabs: [...row.querySelectorAll('button')].map((button) => button.tabIndex), frameDisplay: getComputedStyle(frame).display, frameWidth: frameBox.width, frameHeight: frameBox.height, frameRects: frame.getClientRects().length };
        }),
      };
    })()`);
    const hiddenAx = await Promise.all([1, 2, 3].map((index) => axState(`.chapter-card[data-chapter-index="${index}"]`)));
    assertBrowser(
      folded.expanded === 'false' && /^Expand /.test(folded.label || '') && folded.hidden === 3 && folded.active === '0'
      && folded.cards === beforeFold.cards && folded.visibleCards === 3 && folded.scrollHeight < fullyExpanded
      && folded.rows.every((row) => row.display === 'none' && row.width === 0 && row.height === 0 && row.rects === 0 && row.ariaHidden === 'true' && row.inert && row.buttonTabs.every((tabIndex) => tabIndex < 0) && row.frameDisplay === 'none' && row.frameWidth === 0 && row.frameHeight === 0 && row.frameRects === 0)
      && hiddenAx.every((nodes) => nodes.every((node) => node.absent || node.ignored || node.role === 'none')),
      `Disclosure did not structurally remove descendants (${JSON.stringify({ beforeFold, folded, hiddenAx })})`,
    );
    await press('F3', { code: 'F3', virtualKeyCode: 114 });
    await press('ArrowDown', { code: 'ArrowDown', virtualKeyCode: 40 });
    assertBrowser(await evaluate('document.activeElement?.closest(".chapter-card")?.dataset.chapterIndex === "4"'), 'Collapsed descendants remained in F3/visible-item traversal');
    await press('ArrowRight', { code: 'ArrowRight', virtualKeyCode: 39 });
    await evaluate('document.querySelector(".chapter-card[data-chapter-index=\\\"0\\\"] .chapter-title").focus()');
    await press('ArrowRight', { code: 'ArrowRight', virtualKeyCode: 39 });
    const expanded = await evaluate(`(() => {
      const tree = document.querySelector('#chapter-list'); const child = document.querySelector('.chapter-card[data-chapter-index="1"]'); const grandchild = document.querySelector('.chapter-card[data-chapter-index="2"]');
      return { childExpanded: child.getAttribute('aria-expanded'), childHidden: child.hidden, grandchildHidden: grandchild.hidden, childDisplay: getComputedStyle(child).display, childHeight: child.getBoundingClientRect().height, scrollHeight: tree.scrollHeight, visibleCards: [...tree.querySelectorAll('.chapter-card:not([hidden])')].length };
    })()`);
    assertBrowser(expanded.childExpanded === 'false' && !expanded.childHidden && expanded.grandchildHidden && expanded.childDisplay !== 'none' && expanded.childHeight > 0 && expanded.scrollHeight > folded.scrollHeight && expanded.visibleCards === 5, `Parent expand did not restore intrinsic cards or retain nested fold (${JSON.stringify(expanded)})`);
    await evaluate('document.querySelector(".chapter-card[data-chapter-index=\\\"0\\\"] .chapter-title").click()');
    assertBrowser(await evaluate('window.__amteReady.then((app) => app.view.state.selection.main.anchor === 0)'), 'Heading title did not navigate to its exact source position');
    await cdp('Emulation.setDeviceMetricsOverride', { width: 480, height: 640, deviceScaleFactor: 1, mobile: false });
    assertBrowser(await evaluate('document.documentElement.scrollHeight <= window.innerHeight && document.documentElement.scrollWidth <= window.innerWidth && [...document.querySelectorAll(".chapter-label")].every((label) => getComputedStyle(label).overflowWrap === "anywhere")'), 'Narrow heading tree introduced root scrolling or non-wrapping labels');
    await cdp('Emulation.clearDeviceMetricsOverride');
    await ctrlPress('t', { code: 'KeyT', virtualKeyCode: 84 });
    await waitFor(() => evaluate('document.documentElement.dataset.theme === "monokai"'), 'Monokai heading tree');
    assertBrowser(await evaluate('document.querySelector("#chapter-list").getAttribute("role") === "tree" && document.documentElement.scrollHeight <= window.innerHeight && document.documentElement.scrollWidth <= window.innerWidth'), 'Monokai heading tree lost semantics or introduced root scrolling');

    // A test-only startup injection verifies the disabled config branch without
    // changing the bundled production default.
    await cdp('Page.addScriptToEvaluateOnNewDocument', { source: 'globalThis.__AMTE_TEST_CONFIG__ = { editor: { autoClosePairs: false } };' });
    await cdp('Page.navigate', { url: `${viteUrl}?browser-check=1&auto-close-pairs=false` });
    const falseStarted = await waitFor(() => evaluate('window.__amteReady?.then(() => true, (error) => `startup error: ${error?.stack || error}`)'), 'false-config application startup');
    if (typeof falseStarted === 'string') throw new Error(falseStarted);
    assertBrowser(await evaluate('globalThis.__AMTE_TEST_CONFIG__?.editor?.autoClosePairs === false'), 'False-config injection was not available at startup');
    for (const character of ['(', '[', '{', '"', "'", '`']) {
      await replaceEditorDocument('');
      await press(character);
      assertBrowser((await state()).doc === character, `False autoClosePairs config did not keep ${character} literal`);
    }
    console.log({ searchCheck: 'passed', themes: ['monokai', 'vintage-msdos-edit'] });
  } else {
    await cdp('Page.navigate', { url: `${viteUrl}?benchmark=1` });
    const encoded = await waitFor(() => evaluate('document.body.dataset.amteBenchmark || null'), 'benchmark completion', 20_000);
    const result = JSON.parse(encoded);
    if (!result.converged || result.workerVersion !== result.sourceVersion) {
      throw new Error(`Worker did not converge (${result.workerVersion}/${result.sourceVersion})`);
    }
    if (result.p95Ms > INPUT_P95_LIMIT_MS || result.bufferSwitchP95Ms > SWITCH_P95_LIMIT_MS) {
      throw new Error(`Performance regression: input p95 ${result.p95Ms}ms (limit ${INPUT_P95_LIMIT_MS}ms), buffer-switch p95 ${result.bufferSwitchP95Ms}ms (limit ${SWITCH_P95_LIMIT_MS}ms)`);
    }
    console.log(result);
  }
} catch (error) {
  console.error(`Benchmark failed: ${error.message}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}

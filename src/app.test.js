import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { applyChanges, classifyWordActivity, extractHeadings, reconcileCollapsedChapterIdentities, wordCount } from './analysis.js';
import { AnalysisEngine } from './analysis-worker.js';
import { DEFAULT_THEME_ID, applyTheme, chooseTheme, isThemeToggleShortcut, validateTheme } from './theme-selection.js';
import { SearchQuery } from '@codemirror/search';
import { EditorState } from '@codemirror/state';
import { BufferSession } from './buffer-session.js';

const root = new URL('..', import.meta.url);
const read = (path) => readFileSync(new URL(path, root), 'utf8');
const source = read('src/app.js');
const html = read('src/index.html');
const styles = read('src/styles.css');
const config = JSON.parse(read('src/config.json'));
const theme = JSON.parse(read('src/theme.json'));
const vintage = JSON.parse(read('src/themes/vintage-msdos-edit.json'));
const controllerSource = source.match(/\/\* AUTOSAVE_CONTROLLER_START \*\/([\s\S]*?)\/\* AUTOSAVE_CONTROLLER_END \*\//)?.[1];
assert.ok(controllerSource);
const createAutosaveController = new Function(`const AUTOSAVE_INTERVAL_MS = 300000;${controllerSource}; return createAutosaveController;`)();
const configSource = source.match(/\/\* APP_CONFIG_START \*\/([\s\S]*?)\/\* APP_CONFIG_END \*\//)?.[1];
assert.ok(configSource);
const { validateAppConfig, loadAppConfig, applyAppConfig, paintsVintageAsciiFrames, clearBoxFramePaint } = new Function(`${configSource}; return { validateAppConfig, loadAppConfig, applyAppConfig, paintsVintageAsciiFrames, clearBoxFramePaint };`)();
const activitySource = source.match(/\/\* DAILY_WORD_ACTIVITY_START \*\/([\s\S]*?)\/\* DAILY_WORD_ACTIVITY_END \*\//)?.[1];
assert.ok(activitySource);
const { createDailyWordActivityStore, readDailyWordActivity } = new Function(`${activitySource}; return { createDailyWordActivityStore, readDailyWordActivity };`)();

function activityDelta(before, changes) {
  let shift = 0;
  const ranges = [...changes].sort((left, right) => left.from - right.from).map((change) => {
    const fromB = change.from + shift;
    const inserted = String(change.inserted || '');
    shift += inserted.length - (change.to - change.from);
    return { ...change, inserted, fromB, toB: fromB + inserted.length };
  });
  const after = applyChanges(before, ranges);
  return {
    after,
    ...classifyWordActivity({
      beforeLength: before.length,
      afterLength: after.length,
      readBefore: (from, to) => before.slice(from, to),
      readAfter: (from, to) => after.slice(from, to),
      changes: ranges,
    }),
  };
}

function memoryStorage(entries = {}) {
  const values = new Map(Object.entries(entries));
  return { getItem: (key) => values.get(key) || null, setItem: (key, value) => values.set(key, value), values };
}

test('bundled appearance and editor config defaults safely and applies the vintage frame switch at startup', async () => {
  assert.deepEqual(config, { appearance: { vintageAsciiFrames: false }, editor: { autoClosePairs: true } });
  assert.deepEqual(validateAppConfig({ appearance: { vintageAsciiFrames: true }, editor: { autoClosePairs: false } }), { appearance: { vintageAsciiFrames: true }, editor: { autoClosePairs: false } });
  for (const invalid of [null, {}, { appearance: {} }, { appearance: { vintageAsciiFrames: 'true' } }, { editor: { autoClosePairs: 'true' } }, { editor: { autoClosePairs: null } }, []]) {
    assert.deepEqual(validateAppConfig(invalid), { appearance: { vintageAsciiFrames: false }, editor: { autoClosePairs: true } });
  }
  assert.deepEqual(await loadAppConfig('missing', async () => ({ ok: false })), { appearance: { vintageAsciiFrames: false }, editor: { autoClosePairs: true } });
  assert.deepEqual(await loadAppConfig('malformed', async () => ({ ok: true, json: async () => { throw new Error('bad JSON'); } })), { appearance: { vintageAsciiFrames: false }, editor: { autoClosePairs: true } });
  const root = { dataset: {} };
  applyAppConfig({ appearance: { vintageAsciiFrames: true } }, root);
  assert.equal(root.dataset.vintageAsciiFrames, 'true');
  applyAppConfig(null, root);
  assert.equal(root.dataset.vintageAsciiFrames, 'false');
  root.dataset.theme = 'vintage-msdos-edit';
  assert.equal(paintsVintageAsciiFrames(root), false);
  applyAppConfig({ appearance: { vintageAsciiFrames: true } }, root);
  assert.equal(paintsVintageAsciiFrames(root), true);
  assert.match(source, /const appConfig = globalThis\.__AMTE_TEST_CONFIG__ === undefined[\s\S]*?await loadAppConfig\(appConfigUrl\)/);
  assert.match(source, /import appConfigUrl from '\.\/config\.json\?url'/);
});

test('auto-close pairs use the official CodeMirror extension once in the shared state factory', () => {
  assert.match(source, /import \{ closeBrackets, closeBracketsKeymap \} from '@codemirror\/autocomplete'/);
  assert.match(source, /appConfig\.editor\.autoClosePairs \? \[[\s\S]*?EditorState\.languageData\.of\(\(\) => \[\{ closeBrackets: \{ brackets: AUTO_CLOSE_BRACKETS \} \}\]\),[\s\S]*?closeBrackets\(\),[\s\S]*?keymap\.of\(closeBracketsKeymap\)/);
  assert.equal((source.match(/closeBrackets\(\)/g) || []).length, 1);
  assert.equal((source.match(/function createState\(/g) || []).length, 1);
  assert.match(source, /import \{ BufferSession, clampRestoredSelection \} from '\.\/buffer-session\.js'/);
  assert.match(source, /const safeRestored = \{ \.\.\.restored, selection: clampRestoredSelection\(restored\.selection, document\) \}/);
  assert.match(source, /existing\.state = createState\(document, safeRestored\.selection\)/);
  assert.match(source, /buffers\.add\([^\n]*createState\(document, safeRestored\.selection\)/);
  assert.match(source, /view\.setState\(entry\.state\)/);
});

test('vintage ASCII bottom tracks are cleared while false and fully gated while true', () => {
  assert.match(html, /class="box-frame"/);
  assert.match(source, /function decorateBoxFrame/);
  const parts = [{ textContent: '═══' }, { textContent: '╝' }];
  clearBoxFramePaint({ querySelectorAll: () => parts });
  assert.deepEqual(parts.map((part) => part.textContent), ['', '']);
  assert.match(source, /if \(!paintsVintageAsciiFrames\(root\)\) \{[\s\S]*?clearBoxFramePaint\(frame\);[\s\S]*?return;/);
  assert.match(styles, /\.box-frame \{ display: none; \}/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\]\[data-vintage-ascii-frames="true"\] \.box-frame \{[\s\S]*?display: block;/);
});

test('chapter selector has no left border and card spacing is exactly 75 percent without shadow overlap', () => {
  assert.match(styles, /\.chapter-sidebar \{[\s\S]*?border-left: 0;/);
  assert.match(styles, /\.chapter-list \{[\s\S]*?gap: 0\.375rem;/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.chapter-list \{[\s\S]*?gap: 16\.5px;/);
  assert.equal(6 / 8, 0.75);
  assert.equal(16.5 / 22, 0.75);
  assert.ok(16.5 >= 10, 'vintage gap clears the 10px hard shadow');
});

test('visible chapter cards retain intrinsic content tracks while collapsed treeitem wrappers are removed from layout', () => {
  assert.match(styles, /\.chapter-list \{[\s\S]*?display: grid;[\s\S]*?grid-auto-rows: max-content;/);
  assert.match(styles, /\.chapter-card \{[\s\S]*?min-height: 2\.5rem;[\s\S]*?align-self: start;[\s\S]*?overflow: visible;/);
  assert.match(styles, /\.chapter-title \{ grid-column: 2;[\s\S]*?overflow-wrap: anywhere;/);
  assert.match(styles, /\.chapter-disclosure \{ grid-column: 1;/);
  assert.match(styles, /\.chapter-card\[hidden\] \{ display: none !important; \}/);
  assert.match(source, /row\.hidden = !visible\.has\(identity\);[\s\S]*?row\.toggleAttribute\('inert', row\.hidden\);[\s\S]*?row\.setAttribute\('aria-hidden', String\(row\.hidden\)\)/);
  assert.match(source, /const visible = !row\.hidden;[\s\S]*?row\.querySelector\('\.chapter-title'\)\.tabIndex = visible/);
  assert.match(source, /while \(parent\) \{[\s\S]*?if \(collapsedChapterIdentities\.has\(parent\)\) return false;[\s\S]*?parent = chapterByIdentity\(parent\)\?\.parentIdentity \|\| null;/);
});

test('theme assets validate and markdown editor remains CodeMirror based', () => {
  assert.equal(validateTheme(theme), theme);
  assert.equal(validateTheme(vintage), vintage);
  assert.equal(chooseTheme(new Map([[DEFAULT_THEME_ID, theme]]), 'unknown'), theme);
  assert.match(source, /new EditorView/);
  assert.match(source, /@codemirror\/lang-markdown/);
  assert.doesNotMatch(html, /textarea|highlight-layer/);
});

test('project UI is a three-column lazy accessible tree with patching', () => {
  assert.match(html, /class="project-panel"/);
  assert.match(html, /id="project-tree"[^>]*role="tree"/);
  assert.match(styles, /grid-template-columns:\s*minmax\(11rem, var\(--project-width\)\) minmax\(0, 1fr\) minmax/);
  assert.match(source, /invoke\('list_project_directory', \{ projectId, relativePath: path \}\)/);
  assert.match(source, /treeRows\.get\(path\)/);
  assert.match(source, /aria-expanded/);
  assert.match(source, /aria-selected/);
  assert.match(source, /ArrowUp.*ArrowDown.*ArrowLeft.*ArrowRight/s);
  assert.match(styles, /\.project-tree \{ min-height: 0; overflow: auto/);
});

test('project transitions use Save, Discard, Cancel and scoped commands', () => {
  assert.match(html, /id="dirty-dialog"/);
  assert.match(source, /async function allowTransition/);
  assert.match(source, /action === 'cancel'/);
  assert.match(source, /async function saveAllBuffers/);
  assert.match(source, /return \(await saveAllBuffers\(dirtyEntries\)\)\.complete/);
  assert.match(source, /invoke\('pick_project'/);
  assert.match(source, /invoke\('activate_picked_project'/);
  assert.match(source, /invoke\('open_project_document', \{ projectId, relativePath \}\)/);
  assert.match(source, /invoke\('save_document_as', \{ projectId: currentProject\.projectId, sessionToken: currentProject\.sessionToken, revision: snapshot\.revision, clientEditVersion: snapshot\.clientEditVersion, content: snapshot\.content, sourceRecovery:/s);
  assert.match(source, /buffers\.get\(relativePath\)/);
  assert.match(source, /activateBuffer\(cached\)/);
});

test('plain F1/F2/F3 focus routing prevents defaults and preserves existing Ctrl commands', () => {
  assert.match(source, /!event\.ctrlKey && !event\.altKey && !event\.metaKey && !event\.shiftKey && \['F1', 'F2', 'F3'\]/);
  assert.match(source, /event\.preventDefault\(\);\n      if \(event\.key === 'F1'\) focusTree\(\)/);
  assert.match(source, /else if \(event\.key === 'F2'\).*view\.focus\(\)/s);
  for (const command of ['key === \'p\'', 'key === \'s\'', 'key === \'o\'', 'BracketLeft', 'BracketRight', 'NumpadAdd']) assert.match(source, new RegExp(command.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  assert.equal(isThemeToggleShortcut({ ctrlKey: true, altKey: false, metaKey: false, key: 't' }), true);
});

test('active-document search uses CodeMirror literal queries, native commands, modal focus handling, and reset wiring', () => {
  const state = EditorState.create({ doc: 'Alpha [a] alpha' });
  const query = new SearchQuery({ search: '[a]', caseSensitive: false, literal: true, regexp: false });
  const matches = [...query.getCursor(state)].map(({ from, to }) => [from, to]);
  assert.deepEqual(matches, [[6, 9]]);
  assert.equal(new SearchQuery({ search: '', literal: true }).valid, false);
  assert.match(source, /import \{ SearchQuery, closeSearchPanel, findNext, findPrevious, openSearchPanel, search, setSearchQuery \} from '@codemirror\/search'/);
  assert.match(source, /search\(\{ createPanel: createSearchDecorationPanel \}\)/);
  assert.match(source, /new SearchQuery\(\{ search: term, caseSensitive: false, literal: true, regexp: false \}\)/);
  assert.match(source, /navigateSearch\(event\.key === 'F6' \? findPrevious : findNext\)/);
  assert.match(source, /buffers\.active\(\)\.searchTerm = term/);
  assert.match(source, /searchDialog\.addEventListener\('cancel'/);
  assert.match(source, /event\.key !== 'Tab'/);
  assert.match(html, /id="search-dialog"[^>]*aria-modal="true"[^>]*aria-labelledby="search-dialog-title"/);
  assert.match(html, /<label for="search-term">Search term<\/label>/);
  assert.match(styles, /\.cm-searchMatch\.cm-searchMatch-selected/);
});

test('chapter outline is an accessible delegated H1-H6 tree with exact navigation', () => {
  assert.match(html, /id="chapter-list"[^>]*role="tree"[^>]*aria-label="Document headings"/);
  assert.match(source, /row\.setAttribute\('role', 'treeitem'\)/);
  assert.match(source, /row\.setAttribute\('aria-level', String\(chapter\.level\)\)/);
  assert.match(source, /row\.setAttribute\('aria-expanded'/);
  assert.match(source, /row\.setAttribute\('aria-current'/);
  assert.match(source, /className = 'chapter-disclosure'/);
  assert.match(source, /className = 'chapter-title'/);
  assert.match(source, /chapterList\.addEventListener\('click'/);
  assert.match(source, /chapterList\.addEventListener\('keydown'/);
  assert.match(source, /\['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'Enter', ' '\]/);
  assert.match(source, /if \(event\.ctrlKey \|\| event\.altKey \|\| event\.metaKey \|\| event\.shiftKey\) return;/);
  assert.match(source, /event\.preventDefault\(\);/);
  assert.match(source, /function toggleChapter\(identity\)/);
  assert.match(source, /reconcileCollapsedChapterIdentities/);
  assert.match(source, /preview\.querySelectorAll\('h1, h2, h3, h4, h5, h6'\)/);
  assert.match(source, /view\.dispatch\(\{ selection: \{ anchor: chapter\.position \}, effects: EditorView\.scrollIntoView\(chapter\.position, \{ y: 'center' \}\) \}\);\s*if \(focusEditor\) view\.focus\(\);/s);
  assert.match(styles, /\.chapter-title:focus-visible, \.chapter-disclosure:focus-visible/);
  assert.match(styles, /overflow-x: hidden/);
});

test('top bar has realpath-only accessible visual truncation and bounded internal scroll owners', () => {
  assert.match(html, /id="project-path"/);
  assert.doesNotMatch(html, /theme-label|Ctrl\+T theme/);
  assert.match(source, /currentProject\?\.root \|\| 'No project open'/);
  assert.match(styles, /text-overflow: ellipsis/);
  assert.match(styles, /html,\s*body\s*\{[\s\S]*?overflow: hidden/);
  assert.match(styles, /\.cm-scroller \{ overflow: auto/);
  assert.match(styles, /\.preview \{[\s\S]*?overflow: auto/);
  assert.match(styles, /\.chapter-list \{[\s\S]*?overflow-y: auto/);
});

test('vintage frame reserves and clips space at the actual source and preview scroll owners', () => {
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.paper \{[\s\S]*?--vintage-frame-stroke-block: 1rem;[\s\S]*?--vintage-frame-cell-width: 0\.58rem;[\s\S]*?--vintage-content-gap-block: clamp\(/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] #editor,\s*\[data-theme="vintage-msdos-edit"\] \.preview \{[\s\S]*?inset-block: var\(--vintage-frame-stroke-block\);[\s\S]*?inset-inline: var\(--vintage-frame-stroke-inline\);[\s\S]*?overflow: hidden;/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.cm-scroller \{\s*overflow: auto;/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.cm-content \{[\s\S]*?padding-block: var\(--vintage-content-gap-block\);[\s\S]*?padding-inline: var\(--vintage-content-gap-inline\);/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.preview \{[\s\S]*?padding-block: var\(--vintage-content-gap-block\);[\s\S]*?padding-inline: var\(--vintage-content-gap-inline\);[\s\S]*?overflow: auto;/);
});

test('vintage panel bounds derive from the shared content row with one shadow clearance', () => {
  assert.match(styles, /--theme-layout-top-bar-height: 1\.8rem;/);
  assert.match(styles, /--content-footer-gap: clamp\(/);
  assert.match(styles, /\.writing-layout \{[\s\S]*?height: calc\(var\(--page-row-height\) - var\(--content-footer-gap\)\);/);
  assert.match(styles, /\.project-panel \{\s*height: 100%;/);
  assert.match(styles, /\.editor-workspace \{[^}]*height: 100%;/);
  assert.match(styles, /\.chapter-sidebar \{\s*height: 100%;/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.chapter-sidebar \{[\s\S]*?padding-top: 0;/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.project-panel \{ height: auto; margin: 0 0\.5rem var\(--theme-shadow-offset\);/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.editor-workspace \{ padding-bottom: var\(--theme-shadow-offset\);/);
});

test('restoration and autosave carry project and document identity', () => {
  assert.match(source, /invoke\('restore_last_project'\)/);
  assert.match(source, /projectId: currentProject\.projectId/);
  assert.match(source, /invoke\('autosave_document', \{ projectId: saved\.projectId, sessionToken: saved\.sessionToken, documentId: saved\.documentId, revision: saved\.revision, clientEditVersion: saved\.clientEditVersion, content: saved\.content \}\)/);
  assert.match(source, /buffers\.matchesBinding\(saved\.entry, saved\)/);
  assert.match(source, /buffers\.applySaveOutcome/);
  assert.match(source, /write_recovery_buffer/);
  assert.match(source, /clientEditVersion: snapshot\.clientEditVersion/);
  assert.match(source, /dirty: true,/);
  assert.doesNotMatch(source, /saveReceipt/);
  assert.doesNotMatch(source, /recoverySequence: sequence/);
  assert.match(source, /buffers\.autosaveEligible\(\)/);
  assert.match(source, /invoke\('reconcile_save_operation', \{/);
  assert.match(source, /operationKey: entry\.reconciliationOperationKey/);
  assert.match(source, /writeRevision: snapshot\.writeRevision/);
  assert.match(source, /buffers\.drainRecovery\(\);\s*await flushRecovery\(\);/s);
  assert.match(source, /if \(browserCheckMode \|\| buffers\.persistenceSuppressed\) return;/);
  assert.match(source, /buffers\.suppressPersistence\(\);/);
});

test('failed project activation retains the live frontend session until backend success', () => {
  const activation = source.indexOf("await invoke('activate_picked_project'");
  const replacement = source.indexOf('setProject(project); replaceDocument', activation);
  assert.ok(activation >= 0 && replacement > activation);
  assert.match(source, /catch \(error\) \{ console\.error\('Could not open project:', error\); \}/);
  assert.match(source, /if \(decision === 'cancel' \|\| decision === 'failed'\) return;/);
});

test('autosave remains single, exact and non-overlapping', async () => {
  const writes = []; let release;
  const blocked = new Promise((resolve) => { release = resolve; });
  const controller = createAutosaveController({ snapshot: () => ({ text: 'exact' }), write: async (value) => { writes.push(value); await blocked; }, onSuccess: () => {}, onError: () => {}, setIntervalFn: () => 1, clearIntervalFn: () => {} });
  const first = controller.tick(); await controller.tick();
  assert.equal(writes.length, 1); release(); await first;
  assert.deepEqual(writes, [{ text: 'exact' }]);
});

test('autosave aggregates mixed outcomes instead of abandoning later buffers', async () => {
  const completed = [];
  const controller = createAutosaveController({
    snapshot: () => [{ entry: { key: 'one' } }, { entry: { key: 'two' } }, { entry: { key: 'three' } }],
    write: async (value) => { if (value.entry.key === 'two') throw new Error('disk full'); return value.entry.key; },
    onSuccess: (value) => completed.push(value.entry.key), onError: () => {},
    setIntervalFn: () => 1, clearIntervalFn: () => {},
  });
  const outcomes = await controller.tick();
  assert.deepEqual(completed, ['one', 'three']);
  assert.equal(outcomes.one.result, 'one');
  assert.match(outcomes.two.error, /disk full/);
  assert.equal(outcomes.three.result, 'three');
});

test('an old autosave completion cannot schedule recovery after a session switch, while a failed switch can resume', async () => {
  const timers = []; let release; let started;
  const startedWrite = new Promise((resolve) => { started = resolve; });
  const blocked = new Promise((resolve) => { release = resolve; });
  const session = new BufferSession({ project: { projectId: 1, root: '/old' }, schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: () => {} });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'draft' }, { doc: { toString: () => 'draft' } }); session.markEdit(entry);
  const saved = { entry, ...session.writeSnapshot(entry) };
  const controller = createAutosaveController({ snapshot: () => [saved], write: async () => { started(); await blocked; return { writeRevision: 1 }; }, onSuccess: (snapshot, result) => { if (session.matchesBinding(snapshot.entry, snapshot)) { session.applyWriteAck(snapshot.entry, snapshot, result); session.scheduleRecovery(snapshot.entry, async () => {}); } }, onError: () => {}, setIntervalFn: () => 1, clearIntervalFn: () => {} });
  const tick = controller.tick(); await startedWrite;
  session.setProject({ projectId: 2, root: '/new' }); release(); await tick;
  assert.equal(timers.length, 0, 'stale completion could not queue an old recovery write');
  controller.resume(); await controller.quiesce(); controller.resume();
  const replacement = session.add({ relativePath: 'b.md', documentId: 2, content: 'b' }, { doc: { toString: () => 'b' } });
  session.markEdit(replacement);
  session.scheduleRecovery(replacement, async () => {});
  assert.equal(timers.length, 1, 'a canceled/failed transition leaves the current session persistable');
});

test('save and autosave snapshots carry a monotonic revision so stale completion stays dirty', () => {
  assert.match(source, /buffers\.writeSnapshot\(entry\)/);
  assert.match(read('src/buffer-session.js'), /allocateRevision\(entry\)/);
  assert.match(source, /let currentWriteRevision = 0/);
  const state = { currentRevision: 9, dirty: true, contentVersion: 10 };
  const complete = (revision) => {
    if (revision < state.currentRevision) return false;
    state.currentRevision = revision;
    if (revision === state.contentVersion) state.dirty = false;
    return true;
  };
  assert.equal(complete(10), true);
  state.dirty = true;
  assert.equal(complete(9), false);
  assert.equal(state.dirty, true);
});

test('analysis behavior preserves worker, headings and word semantics', async () => {
  assert.equal(wordCount('# A\n\nHello **world**'), 3);
  assert.deepEqual(extractHeadings('# One\n### Three').map(({ level, text }) => [level, text]), [[1, 'One'], [3, 'Three']]);
  assert.equal(applyChanges('abcd', [{ from: 1, to: 3, inserted: 'X' }]), 'aXd');
  const results = []; const engine = new AnalysisEngine((result) => results.push(result), 1);
  engine.receive({ type: 'init', version: 1, document: '# Kept' }); await new Promise((resolve) => setTimeout(resolve, 10)); engine.terminate();
  assert.equal(results[0].headings[0].text, 'Kept');
});

test('H1-H6 outline extraction tracks exact occurrences, fences, hierarchy, and compact numbering', () => {
  const markdown = [
    '# Root', '###### Deep', '### Middle', '##### Five', '###### Six',
    '```md', '# fenced', '###### also fenced', '```', '####### invalid', 'text # not heading', '# Root',
  ].join('\n');
  const headings = extractHeadings(markdown);
  assert.deepEqual(headings.map(({ level, text, number, parentIndex }) => [level, text, number, parentIndex]), [
    [1, 'Root', '1', null], [6, 'Deep', '1.1', 0], [3, 'Middle', '1.1', 0],
    [5, 'Five', '1.1.1', 2], [6, 'Six', '1.1.1.1', 3], [1, 'Root', '2', null],
  ]);
  assert.equal(headings[1].position, markdown.indexOf('###### Deep'));
  assert.notEqual(headings[0].identity, headings.at(-1).identity, 'duplicate occurrences must remain distinct');
  assert.match(headings[0].identity, /^h1:[0-9a-f]{16}:1$/);
});

test('collapsed identities reconcile only surviving unambiguous parents after edits and offset shifts', () => {
  const before = extractHeadings('# Root\n## Fold\n### Child\n# Tail');
  const folded = [before[1].identity];
  const shifted = extractHeadings('Opening prose\n# Root\n## Fold\n### Child\n# Tail');
  assert.deepEqual(reconcileCollapsedChapterIdentities(before, shifted, folded), [shifted[1].identity]);
  const removed = extractHeadings('# Root\n# Tail');
  assert.deepEqual(reconcileCollapsedChapterIdentities(before, removed, folded), []);
  const duplicates = extractHeadings('# Root\n## Same\n### A\n## Same\n### B');
  const changed = extractHeadings('# Root\n## Same\n### New\n## Same\n### A\n## Same\n### B');
  assert.deepEqual(reconcileCollapsedChapterIdentities(duplicates, changed, [duplicates[1].identity]), [], 'ambiguous duplicate context is fail-closed');
});

test('word activity classifies local token births, deaths, replacement, and Unicode punctuation deterministically', () => {
  let document = '';
  let added = 0; let deleted = 0;
  for (const character of 'word') {
    const result = activityDelta(document, [{ from: document.length, to: document.length, inserted: character }]);
    document = result.after; added += result.added; deleted += result.deleted;
  }
  assert.deepEqual({ document, added, deleted }, { document: 'word', added: 1, deleted: 0 });
  for (let index = document.length - 1; index >= 0; index -= 1) {
    const result = activityDelta(document, [{ from: index, to: index + 1, inserted: '' }]);
    document = result.after; added += result.added; deleted += result.deleted;
  }
  assert.deepEqual({ document, added, deleted }, { document: '', added: 1, deleted: 1 });
  assert.deepEqual(activityDelta('old', [{ from: 0, to: 3, inserted: 'new' }]), { after: 'new', added: 1, deleted: 1 });
  assert.deepEqual(activityDelta('casa', [{ from: 1, to: 2, inserted: 'o' }]), { after: 'cosa', added: 0, deleted: 0 });
  assert.deepEqual(activityDelta('Olá, mundo!', [{ from: 0, to: 10, inserted: 'ação rápida' }]), { after: 'ação rápida!', added: 2, deleted: 2 });
  assert.deepEqual(activityDelta('', [{ from: 0, to: 0, inserted: 'primeiro' }]), { after: 'primeiro', added: 1, deleted: 0 });
  assert.deepEqual(activityDelta('último', [{ from: 0, to: 6, inserted: '' }]), { after: '', added: 0, deleted: 1 });
});

test('word activity handles paste/cut, whitespace split/join, multi-range changes, and undo/redo as new activity', () => {
  assert.deepEqual(activityDelta('', [{ from: 0, to: 0, inserted: 'one two\nthree' }]), { after: 'one two\nthree', added: 3, deleted: 0 });
  assert.deepEqual(activityDelta('one two three', [{ from: 0, to: 13, inserted: '' }]), { after: '', added: 0, deleted: 3 });
  assert.deepEqual(activityDelta('hello', [{ from: 2, to: 2, inserted: ' ' }]), { after: 'he llo', added: 1, deleted: 0 });
  assert.deepEqual(activityDelta('he llo', [{ from: 2, to: 3, inserted: '' }]), { after: 'hello', added: 0, deleted: 1 });
  assert.deepEqual(activityDelta('aa bb', [
    { from: 0, to: 0, inserted: 'x' },
    { from: 3, to: 3, inserted: 'y' },
  ]), { after: 'xaa ybb', added: 0, deleted: 0 });
  const typed = activityDelta('', [{ from: 0, to: 0, inserted: 'undoable' }]);
  const undone = activityDelta(typed.after, [{ from: 0, to: typed.after.length, inserted: '' }]);
  const redone = activityDelta(undone.after, [{ from: 0, to: 0, inserted: 'undoable' }]);
  assert.deepEqual([typed.added, undone.deleted, redone.added], [1, 1, 1]);
});

test('daily word activity persists only per file/day, rolls over, and safely transfers an unsaved session after Save As', () => {
  let day = '2026-10-25';
  const storage = memoryStorage({
    'amte.daily-baseline.v1:/project/a.md': JSON.stringify({ day, total: 999 }),
    'amte.daily-word-activity.v1:/project/bad.md': '{not json',
  });
  const first = createDailyWordActivityStore({ storage, getDay: () => day });
  first.setIdentity('/project/a.md');
  first.record({ added: 2, deleted: 1 });
  assert.deepEqual(first.snapshot(), { day, added: 2, deleted: 1, identity: '/project/a.md' });
  const reload = createDailyWordActivityStore({ storage, getDay: () => day });
  reload.setIdentity('/project/a.md');
  assert.deepEqual(reload.snapshot(), { day, added: 2, deleted: 1, identity: '/project/a.md' });
  reload.setIdentity('/project/bad.md');
  assert.deepEqual(reload.snapshot(), { day, added: 0, deleted: 0, identity: '/project/bad.md' });
  reload.setIdentity('/project/other.md'); reload.record({ added: 1, deleted: 0 });
  reload.setIdentity('/project/a.md');
  assert.deepEqual(reload.snapshot(), { day, added: 2, deleted: 1, identity: '/project/a.md' });
  day = '2026-10-26';
  assert.deepEqual(reload.snapshot(), { day, added: 0, deleted: 0, identity: '/project/a.md' });
  const unsaved = createDailyWordActivityStore({ storage, getDay: () => day });
  unsaved.record({ added: 3, deleted: 2 });
  assert.equal(storage.values.has('amte.daily-word-activity.v1:/project/new.md'), false, 'cancelled/failed Save As leaves session-only activity unpersisted');
  unsaved.adoptIdentity('/project/new.md');
  assert.deepEqual(unsaved.snapshot(), { day, added: 3, deleted: 2, identity: '/project/new.md' });
  assert.deepEqual(readDailyWordActivity(storage, '/project/new.md', day), { day, added: 3, deleted: 2 });
});

test('activity is synchronous to the active transaction; opens, restores, and stale worker results cannot create counts', () => {
  assert.doesNotMatch(source, /daily-baseline\.v1/);
  assert.match(source, /dailyActivity\.setIdentity\(currentPath\);[\s\S]*?view\.setState\(entry\.state\);/);
  assert.match(source, /const delta = classifyWordActivity\([\s\S]*?dailyActivity\.record\(delta\);/);
  assert.match(source, /data\.activation !== activationGeneration/);
  assert.match(source, /data\.version !== sourceVersion/);
  assert.match(source, /if \(savingUntitled\) dailyActivity\.adoptIdentity\(currentPath\);/);
  assert.match(html, /id="daily-count">\(-\): 0 \(\+\): 0/);
  assert.match(source, /dailyCount\.textContent = `\(-\): \$\{activity\.deleted\} \(\+\): \$\{activity\.added\}`;/);
  assert.match(source, /totalCount\.textContent = `total: \$\{total\}`;/);
});

test('both physical page edges and middle-width calculations remain available', () => {
  assert.match(html, /page-resize-handle-left[\s\S]*page-resize-handle-right/);
  assert.match(source, /layoutWidth - sidebarWidth - projectWidth/);
  assert.match(source, /resizeStartWidth \+ resizeDirection/);
  assert.match(styles, /\.editor-workspace .*place-items: center/);
});

test('footer status is accessible, fixed-size, and distinguishes recovery from file saves', () => {
  assert.match(html, /id="save-indicator"[^>]*role="status"[^>]*aria-live="polite"[^>]*aria-atomic="true"[^>]*aria-label="No save activity"/);
  assert.match(html, /data-save-icon[^>]*aria-hidden="true"/);
  assert.match(html, /data-save-label>No save activity/);
  assert.match(styles, /\.save-indicator \{[\s\S]*?position: fixed;[\s\S]*?width: 13rem;[\s\S]*?display: flex;[\s\S]*?z-index: 20;/);
  assert.match(styles, /\.save-indicator\[hidden\] \{ visibility: hidden; opacity: 0; \}/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.save-indicator \{ bottom: var\(--metrics-padding-block\); color: var\(--theme-bar-foreground\); \}/);
  assert.match(source, /function notifySuccessfulProjectWrite\(entry, snapshot\) \{[\s\S]*?buffers\.matchesBinding\(entry, snapshot\)[\s\S]*?saveIndicator\.show\(\);/);
  assert.match(source, /if \(!saved\?\.fileCommitted\) \{/);
  assert.match(source, /if \(!notifySuccessfulProjectWrite\(entry, snapshot\)\) return false;/);
  assert.match(source, /onSuccess\(saved, result\) \{\s*if \(!result\?\.fileCommitted\)/);
  assert.match(source, /reconciliationPending: entry\.reconciliationPending/);
  assert.match(source, /scheduleSaveReconciliation/);
  assert.doesNotMatch(source.match(/async function persistRecovery[\s\S]*?function scheduleRecovery/)?.[0] || '', /notifySuccessfulProjectWrite/);
  assert.match(source, /recoveryDurable: entry\.dirty && entry\.recoveryState === 'durable'/);
  assert.match(source, /beforeunload', \(\) => \{ saveIndicator\.destroy\(\); autosave\.stop\(\);/);
});

test('native close registration is listener-first, acknowledge-gated, and always reopens a retained close request', () => {
  assert.match(source, /const unlisten = await events\.listen\('amte:\/\/close-requested', async \(\{ payload \}\) => \{/);
  assert.match(source, /await invoke\('close_listener_ready'\);/);
  assert.match(source, /await invoke\('acknowledge_close_request', \{ requestId \}\)/);
  assert.match(source, /if \(!closed\) \{[\s\S]*?await invoke\('finish_close_request', \{ requestId \}\)/);
  assert.doesNotMatch(source.match(/function askDirty\(\)[\s\S]*?async function saveDocument/)?.[0] || '', /if \(!dirty\)/);
  assert.match(html, /Save all changed buffers\?<\/p>[\s\S]*?Save All[\s\S]*?Discard All[\s\S]*?Cancel/);
});

test('the browser benchmark flushes and bounds worker convergence before reporting', () => {
  assert.match(source, /app\.worker\.postMessage\(\{ type: 'flush', version: app\.sourceVersion \}\)/);
  assert.match(source, /performance\.now\(\) \+ 5000/);
  assert.match(source, /converged: app\.analysisVersion === app\.sourceVersion/);
  assert.match(source, /document\.querySelector\('#total-count'\)/);
  assert.match(source, /buffers: \[\.\.\.app\.buffers\.entries\.values\(\)\]/);
  assert.match(source, /editorViews: editorHost\.querySelectorAll\('\.cm-editor'\)\.length/);
});

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { applyChanges, extractHeadings, wordCount } from './analysis.js';
import { AnalysisEngine } from './analysis-worker.js';
import { DEFAULT_THEME_ID, applyTheme, chooseTheme, isThemeToggleShortcut, validateTheme } from './theme-selection.js';
import { SearchQuery } from '@codemirror/search';
import { EditorState } from '@codemirror/state';

const root = new URL('..', import.meta.url);
const read = (path) => readFileSync(new URL(path, root), 'utf8');
const source = read('src/app.js');
const html = read('src/index.html');
const styles = read('src/styles.css');
const theme = JSON.parse(read('src/theme.json'));
const vintage = JSON.parse(read('src/themes/vintage-msdos-edit.json'));
const controllerSource = source.match(/\/\* AUTOSAVE_CONTROLLER_START \*\/([\s\S]*?)\/\* AUTOSAVE_CONTROLLER_END \*\//)?.[1];
assert.ok(controllerSource);
const createAutosaveController = new Function(`const AUTOSAVE_INTERVAL_MS = 300000;${controllerSource}; return createAutosaveController;`)();

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
  assert.match(source, /await saveDocument\(\)/);
  assert.match(source, /invoke\('open_project'/);
  assert.match(source, /invoke\('open_project_document', \{ projectId, relativePath \}\)/);
  assert.match(source, /invoke\('save_document_as', \{ projectId: currentProject\.projectId, revision: saveRevision, content \}\)/s);
  assert.match(source, /invoke\('save_document', \{ projectId: currentProject\.projectId, documentId: currentDocumentId, revision: saveRevision, content \}\)/s);
  assert.match(source, /transitionInFlight/);
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
  assert.match(source, /clearSearchState\(\{ closeDialog: true \}\);[\s\S]*?view\.setState\(createState\(document\)\)/);
  assert.match(source, /searchDialog\.addEventListener\('cancel'/);
  assert.match(source, /event\.key !== 'Tab'/);
  assert.match(html, /id="search-dialog"[^>]*aria-modal="true"[^>]*aria-labelledby="search-dialog-title"/);
  assert.match(html, /<label for="search-term">Search term<\/label>/);
  assert.match(styles, /\.cm-searchMatch\.cm-searchMatch-selected/);
});

test('chapter keyboard navigation keeps a selected card and reuses exact chapter navigation', () => {
  assert.match(source, /let selectedChapterIdentity = null/);
  assert.match(source, /card\.setAttribute\('aria-current', String\(identity === selectedChapterIdentity\)\)/);
  assert.match(source, /function focusChapter\(\) \{\s*focusChapterCard\(selectedChapterCard\(\) \|\| chapterList\.querySelector\('\.chapter-card'\)\);/s);
  assert.match(source, /card\.focus\(\{ preventScroll: true \}\);\s*ensureChapterCardVisible\(card\);/s);
  assert.match(source, /function ensureChapterCardVisible\(card\) \{[\s\S]*?chapterList\.scrollTop \+= cardBounds\.top - listBounds\.top;[\s\S]*?chapterList\.scrollTop \+= cardBounds\.bottom - listBounds\.bottom;/);
  assert.match(source, /chapterList\.addEventListener\('keydown', \(event\) => \{[\s\S]*?event\.ctrlKey \|\| event\.altKey \|\| event\.metaKey \|\| event\.shiftKey[\s\S]*?\['ArrowUp', 'ArrowDown'\]/);
  assert.match(source, /event\.preventDefault\(\);[\s\S]*?Math\.max\(0, Math\.min\(cards\.length - 1, index \+ \(event\.key === 'ArrowDown' \? 1 : -1\)\)\);[\s\S]*?if \(nextIndex === index\) return;/);
  assert.match(source, /focusChapterCard\(nextCard\);\s*navigateToChapter\(Number\(nextCard\.dataset\.chapterIndex\), nextCard\.dataset\.identity, \{ focusEditor: false \}\);/s);
  assert.match(source, /function navigateToChapter\(index, identity = null, \{ focusEditor = true \} = \{\}\)/);
  assert.match(source, /view\.dispatch\(\{ selection: \{ anchor: chapter\.position \}, effects: EditorView\.scrollIntoView\(chapter\.position, \{ y: 'center' \}\) \}\);\s*if \(focusEditor\) view\.focus\(\);/s);
  assert.match(source, /const target = preview\.querySelector\(`\[data-chapter-index="\$\{index\}"\]`\);/);
  assert.match(source, /const framePaddingTop = root\.dataset\.theme === 'vintage-msdos-edit'[\s\S]*?getComputedStyle\(preview\)\.paddingTop[\s\S]*?preview\.scrollTop = Math\.max\(0, headingBounds\.top - previewBounds\.top \+ preview\.scrollTop - framePaddingTop\);/);
  assert.match(source, /chapters = \[\];\s*selectedChapterIdentity = null;\s*pendingSourceChapterIdentity = null;\s*patchChapters\(chapters\);/s);
  assert.match(styles, /\.chapter-card\[aria-current="true"\]/);
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

test('primary panels share the content-row bottom geometry instead of letting the project tree overflow the footer', () => {
  assert.match(styles, /--theme-layout-top-bar-height: 1\.8rem;/);
  assert.match(styles, /--content-footer-gap: clamp\(/);
  assert.match(styles, /\.writing-layout \{[\s\S]*?height: calc\(var\(--page-row-height\) - var\(--content-footer-gap\)\);/);
  assert.match(styles, /\.project-panel \{\s*height: 100%;/);
  assert.match(styles, /\.editor-workspace \{[^}]*height: 100%;/);
  assert.match(styles, /\.chapter-sidebar \{\s*height: 100%;/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.project-panel \{ height: auto; margin: 0\.5rem 0\.5rem var\(--theme-shadow-offset\);/);
  assert.match(styles, /\[data-theme="vintage-msdos-edit"\] \.editor-workspace \{ padding-bottom: var\(--theme-shadow-offset\);/);
});

test('restoration and autosave carry project and document identity', () => {
  assert.match(source, /invoke\('restore_last_project'\)/);
  assert.match(source, /projectId: currentProject\.projectId/);
  assert.match(source, /invoke\('autosave_document', \{ projectId, documentId, revision, content: text \}\)/);
  assert.match(source, /documentGeneration === saved\.documentGeneration/);
  assert.match(source, /currentProject\?\.projectId === saved\.projectId/);
});

test('autosave remains single, exact and non-overlapping', async () => {
  const writes = []; let release;
  const blocked = new Promise((resolve) => { release = resolve; });
  const controller = createAutosaveController({ snapshot: () => ({ text: 'exact' }), write: async (value) => { writes.push(value); await blocked; }, onSuccess: () => {}, onError: () => {}, setIntervalFn: () => 1, clearIntervalFn: () => {} });
  const first = controller.tick(); await controller.tick();
  assert.equal(writes.length, 1); release(); await first;
  assert.deepEqual(writes, [{ text: 'exact' }]);
});

test('save and autosave snapshots carry a monotonic revision so stale completion stays dirty', () => {
  assert.match(source, /const saveRevision = Math\.max\(saveVersion, currentWriteRevision\)/);
  assert.match(source, /revision: Math\.max\(contentVersion, currentWriteRevision\)/);
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

test('both physical page edges and middle-width calculations remain available', () => {
  assert.match(html, /page-resize-handle-left[\s\S]*page-resize-handle-right/);
  assert.match(source, /layoutWidth - sidebarWidth - projectWidth/);
  assert.match(source, /resizeStartWidth \+ resizeDirection/);
  assert.match(styles, /\.editor-workspace .*place-items: center/);
});

test('the browser benchmark flushes and bounds worker convergence before reporting', () => {
  assert.match(source, /app\.worker\.postMessage\(\{ type: 'flush', version: app\.sourceVersion \}\)/);
  assert.match(source, /performance\.now\(\) \+ 5000/);
  assert.match(source, /converged: app\.analysisVersion === app\.sourceVersion/);
  assert.match(source, /document\.querySelector\('#total-count'\)/);
});

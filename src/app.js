import { EditorState } from '@codemirror/state';
import { history, historyKeymap, defaultKeymap, indentWithTab } from '@codemirror/commands';
import { markdown } from '@codemirror/lang-markdown';
import { syntaxHighlighting, HighlightStyle } from '@codemirror/language';
import { EditorView, keymap } from '@codemirror/view';
import { tags } from '@lezer/highlight';
import themeUrl from './theme.json?url';
import vintageThemeUrl from './themes/vintage-msdos-edit.json?url';
import {
  DEFAULT_THEME_ID,
  applyTheme,
  chooseTheme,
  getStoredThemeId,
  isThemeToggleShortcut,
  persistThemeId,
  validateTheme,
} from './theme-selection.js';

// Vite emits this JSON alongside the hashed production assets. A plain
// runtime './theme.json' fetch works in dev but is absent from frontendDist.
const THEME_URLS = new Map([[DEFAULT_THEME_ID, themeUrl], ['vintage-msdos-edit', vintageThemeUrl]]);
const DEFAULT_DOCUMENT = '# Untitled\n\nBegin writing here.\n\nUse **Markdown** freely, then press Ctrl+P to read it.';
const MIN_ZOOM_STEPS = 5;
const MAX_ZOOM_STEPS = 20;
const PAGE_DEFAULT_MAX_WIDTH = 736;
const PAGE_MIN_WIDTH = 360;
const PAGE_MAX_WIDTH = 960;
const PAGE_WIDTH_INCREMENT = 32;
const SIDEBAR_DEFAULT_WIDTH = 224;
const RESIZE_HANDLE_HIT_WIDTH = 16;
const PAGE_VIEWPORT_HEIGHT_RATIO = 0.9;
const WORKSPACE_BREATHING_GAP_MIN = 12;
const WORKSPACE_BREATHING_GAP_MAX = 32;
const WORKSPACE_BREATHING_GAP_RATIO = 0.02;
const METRICS_HEIGHT = 20;
const AUTOSAVE_INTERVAL_MS = 300000;

// This small controller deliberately has no editor or worker dependencies: an
// autosave always receives the synchronous CodeMirror snapshot taken at tick
// time, never an eventually-consistent analysis result.
/* AUTOSAVE_CONTROLLER_START */
function createAutosaveController({ snapshot, write, onSuccess, onError, setIntervalFn, clearIntervalFn }) {
  let timer = null;
  let writeInFlight = false;

  async function tick() {
    if (writeInFlight) return;
    const pending = snapshot();
    if (!pending) return;
    writeInFlight = true;
    try {
      await write(pending);
      onSuccess(pending);
    } catch (error) {
      onError(error);
    } finally {
      writeInFlight = false;
    }
  }

  return {
    start() {
      if (timer !== null) return;
      timer = setIntervalFn(() => { void tick(); }, AUTOSAVE_INTERVAL_MS);
    },
    stop() {
      if (timer === null) return;
      clearIntervalFn(timer);
      timer = null;
    },
    tick,
    get writing() { return writeInFlight; },
  };
}
/* AUTOSAVE_CONTROLLER_END */

async function loadTheme(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`Theme request failed (${response.status}).`);
  return validateTheme(await response.json());
}

async function loadThemes() {
  const themes = new Map();
  // Monokai is mandatory because it is the safe baseline. Vintage is optional
  // at recovery time, so an interrupted/corrupt bundled asset cannot block writing.
  themes.set(DEFAULT_THEME_ID, await loadTheme(THEME_URLS.get(DEFAULT_THEME_ID)));
  try {
    themes.set('vintage-msdos-edit', await loadTheme(THEME_URLS.get('vintage-msdos-edit')));
  } catch (error) {
    console.warn('AMTE could not load the vintage theme; using Monokai instead.', error);
  }
  return themes;
}

function showThemeError(error) {
  const message = document.querySelector('#theme-error');
  message.hidden = false;
  message.textContent = `AMTE could not load its theme safely: ${error.message}`;
  document.body.classList.remove('theme-loading');
}

function localDay() {
  const now = new Date();
  return `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`;
}

function invoke(command, args) {
  const core = window.__TAURI__?.core;
  if (!core) return Promise.reject(new Error('Tauri is unavailable outside the desktop application.'));
  return core.invoke(command, args);
}

let cachedEditorTheme;
function editorTheme() {
  if (cachedEditorTheme) return cachedEditorTheme;
  return (cachedEditorTheme = [
    EditorView.theme({
      '&': { height: '100%', backgroundColor: 'transparent', color: 'var(--theme-ink)' },
      '.cm-scroller': { overflow: 'auto', font: '1rem/1.75 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace', letterSpacing: '.005em', tabSize: '2' },
      '.cm-content': { padding: 'clamp(2rem, 8%, 4.5rem)', minHeight: '100%', caretColor: 'var(--theme-caret)' },
      '.cm-gutters': { display: 'none' },
      '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--theme-caret)' },
      '.cm-selectionBackground, ::selection': { backgroundColor: 'var(--theme-selection) !important' },
      '.cm-focused': { outline: 'none' },
    }),
    syntaxHighlighting(HighlightStyle.define([
      { tag: [tags.heading, tags.heading1], color: 'var(--theme-syntax-h1)', fontWeight: '700' },
      { tag: tags.heading2, color: 'var(--theme-syntax-h2)', fontWeight: '700' },
      { tag: tags.heading3, color: 'var(--theme-syntax-h3)', fontWeight: '700' },
      { tag: tags.heading4, color: 'var(--theme-syntax-h4)', fontWeight: '700' },
      { tag: tags.heading5, color: 'var(--theme-syntax-h5)', fontWeight: '700' },
      { tag: tags.heading6, color: 'var(--theme-syntax-h6)', fontWeight: '700' },
      { tag: [tags.punctuation, tags.contentSeparator], color: 'var(--theme-syntax-marker)' },
      { tag: [tags.emphasis, tags.strong, tags.quote], color: 'var(--theme-syntax-emphasis)' },
      { tag: [tags.link, tags.url], color: 'var(--theme-syntax-link)', textDecoration: 'underline' },
      { tag: [tags.monospace, tags.string], color: 'var(--theme-syntax-code)' },
      { tag: tags.strikethrough, color: 'var(--theme-syntax-strikethrough)', textDecoration: 'line-through' },
      { tag: tags.list, color: 'var(--theme-syntax-list)' },
    ])),
  ]);
}

async function start() {
  let themes;
  let activeThemeId;
  try {
    themes = await loadThemes();
    const requestedThemeId = getStoredThemeId();
    const activeTheme = chooseTheme(themes, requestedThemeId);
    activeThemeId = themes.has(requestedThemeId) ? requestedThemeId : DEFAULT_THEME_ID;
    applyTheme(activeThemeId, activeTheme);
    document.body.classList.remove('theme-loading');
  } catch (error) {
    showThemeError(error);
    return null;
  }

  const editorHost = document.querySelector('#editor');
  const preview = document.querySelector('#preview');
  const totalCount = document.querySelector('#total-count');
  const dailyCount = document.querySelector('#daily-count');
  const chapterList = document.querySelector('#chapter-list');
  const pageResizeHandles = [
    { element: document.querySelector('#page-resize-handle-left'), direction: -1 },
    { element: document.querySelector('#page-resize-handle-right'), direction: 1 },
  ];
  const writingLayout = document.querySelector('.writing-layout');
  const chapterSidebar = document.querySelector('.chapter-sidebar');
  const projectPanel = document.querySelector('.project-panel');
  const projectTree = document.querySelector('#project-tree');
  const dirtyDialog = document.querySelector('#dirty-dialog');
  const projectPathLabel = document.querySelector('#project-path');
  const documentShell = document.querySelector('#document-shell');
  const themeChrome = document.querySelector('#theme-chrome');
  const boxFrames = new Set(document.querySelectorAll('.box-frame'));
  const root = document.documentElement;
  let previewing = false;
  let currentProject = null;
  let currentRelativePath = null;
  let currentPath = null;
  let currentDocumentId = null;
  let currentWriteRevision = 0;
  let transitionInFlight = false;
  const treeNodes = new Map();
  const treeRows = new Map();
  let selectedTreePath = null;
  let documentGeneration = 0;
  let contentVersion = 0;
  let dirty = false;
  let lastSavedText = null;
  let previewRenderVersion = 0;
  let pendingPreviewChapter = null;
  let sourceVersion = 0;
  let analysisVersion = -1;
  let latestTotal = 0;
  let chapters = [];
  let selectedChapterIdentity = null;
  let pendingSourceChapterIdentity = null;
  let pendingSourceChapterFocusEditor = true;
  let zoomSteps = 10;
  let pageWidth = getDefaultPageWidth();
  let resizePointerId = null;
  let resizeStartX = 0;
  let resizeStartWidth = pageWidth;
  let resizeDirection = 0;
  let resizeHandle = null;
  let resizeFrame = null;
  const requestFrame = window.requestAnimationFrame?.bind(window) || ((callback) => window.setTimeout(callback, 16));
  const cancelFrame = window.cancelAnimationFrame?.bind(window) || ((frame) => window.clearTimeout(frame));
  const worker = new Worker(new URL('./analysis-worker.js', import.meta.url), { type: 'module' });
  const benchmarkMode = new URLSearchParams(window.location.search).has('benchmark');
  const updateSamples = [];
  let view;

  function refreshBoxFrames() {
    if (root.dataset.theme !== 'vintage-msdos-edit') return;
    const computed = getComputedStyle(root);
    const horizontal = computed.getPropertyValue('--theme-box-horizontal').trim();
    const vertical = computed.getPropertyValue('--theme-box-vertical').trim();
    const boxGlyph = (name) => computed.getPropertyValue(name).trim().replace(/^(['"])(.*)\1$/, '$2');
    const corners = {
      'top-left': boxGlyph('--theme-box-top-left'),
      'top-right': boxGlyph('--theme-box-top-right'),
      'bottom-right': boxGlyph('--theme-box-bottom-right'),
      'bottom-left': boxGlyph('--theme-box-bottom-left'),
    };
    if (!horizontal || !vertical || Object.values(corners).some((corner) => !corner)) return;
    for (const frame of boxFrames) {
      const bounds = frame.parentElement.getBoundingClientRect();
      const fontSize = Number.parseFloat(getComputedStyle(frame).fontSize) || 16;
      const horizontalCount = Math.max(1, Math.ceil(bounds.width / Math.max(1, fontSize * 0.58)));
      const verticalCount = Math.max(1, Math.ceil(bounds.height / Math.max(1, fontSize)));
      frame.querySelector('[data-box-edge="top"]').textContent = horizontal.repeat(horizontalCount);
      frame.querySelector('[data-box-edge="bottom"]').textContent = horizontal.repeat(horizontalCount);
      frame.querySelector('[data-box-edge="left"]').textContent = `${vertical}\n`.repeat(verticalCount);
      frame.querySelector('[data-box-edge="right"]').textContent = `${vertical}\n`.repeat(verticalCount);
      for (const [position, glyph] of Object.entries(corners)) frame.querySelector(`[data-box-corner="${position}"]`).textContent = glyph;
    }
  }

  function decorateBoxFrame(element) {
    const frame = document.createElement('span');
    frame.className = 'box-frame';
    frame.setAttribute('aria-hidden', 'true');
    for (const edge of ['top', 'right', 'bottom', 'left']) {
      const track = document.createElement('span');
      track.className = `box-frame__edge box-frame__edge--${edge}`;
      track.dataset.boxEdge = edge;
      frame.append(track);
    }
    for (const corner of ['top-left', 'top-right', 'bottom-right', 'bottom-left']) {
      const cornerElement = document.createElement('span');
      cornerElement.className = `box-frame__corner box-frame__corner--${corner}`;
      cornerElement.dataset.boxCorner = corner;
      frame.append(cornerElement);
    }
    element.append(frame);
    boxFrames.add(frame);
    return frame;
  }

  function updateThemeChrome() {
    const path = currentProject?.root || 'No project open';
    projectPathLabel.textContent = path;
    projectPathLabel.title = path;
    projectPathLabel.setAttribute('aria-label', path);
    themeChrome.hidden = false;
  }

  function activateTheme(themeId, persist = false) {
    const theme = chooseTheme(themes, themeId);
    activeThemeId = themes.has(themeId) ? themeId : DEFAULT_THEME_ID;
    applyTheme(activeThemeId, theme, root);
    updateThemeChrome();
    if (persist) persistThemeId(activeThemeId);
    requestAnimationFrame(refreshBoxFrames);
  }

  updateThemeChrome();
  const boxFrameObserver = typeof ResizeObserver === 'function' ? new ResizeObserver(() => requestAnimationFrame(refreshBoxFrames)) : null;
  for (const frame of boxFrames) boxFrameObserver?.observe(frame.parentElement);
  document.fonts?.ready?.then(() => refreshBoxFrames());

  function documentText() { return view.state.doc.toString(); }

  function updateDaily(total) {
    if (!currentPath) return 0;
    const key = `amte.daily-baseline.v1:${currentPath}`;
    const today = localDay();
    let baseline;
    try { baseline = JSON.parse(localStorage.getItem(key)); } catch { baseline = null; }
    if (!baseline || baseline.day !== today || !Number.isFinite(baseline.total)) {
      baseline = { day: today, total };
      try { localStorage.setItem(key, JSON.stringify(baseline)); } catch (error) { console.error('Could not persist the daily word baseline:', error); }
    }
    return Math.max(total - baseline.total, 0);
  }

  function updateMetrics(total) {
    latestTotal = total;
    totalCount.textContent = `total: ${total}`;
    dailyCount.textContent = `${updateDaily(total)}/250`;
  }

  function patchChapters(nextChapters) {
    const oldByIdentity = new Map();
    for (const card of chapterList.querySelectorAll('.chapter-card')) oldByIdentity.set(card.dataset.identity, card);
    const occurrences = new Map();
    const cards = nextChapters.map((chapter, index) => {
      const base = `${chapter.level}\u0000${chapter.text}`;
      const occurrence = occurrences.get(base) || 0;
      occurrences.set(base, occurrence + 1);
      const identity = `${base}\u0000${occurrence}`;
      let card = oldByIdentity.get(identity);
      if (!card) {
        card = document.createElement('button');
        card.type = 'button';
        card.className = 'chapter-card';
        card.append(document.createElement('span'), document.createElement('span'));
        card.children[0].className = 'chapter-number';
        decorateBoxFrame(card);
        boxFrameObserver?.observe(card);
      }
      card.dataset.identity = identity;
      card.dataset.chapterIndex = String(index);
      card.dataset.position = String(chapter.position);
      card.style.setProperty('--chapter-indent', `${Math.min(chapter.level - 1, 4) * 7}px`);
      card.children[0].textContent = `${chapter.number} `;
      card.children[1].textContent = chapter.text || 'Untitled heading';
      card.setAttribute('aria-label', `Go to heading ${chapter.number}: ${chapter.text || 'Untitled heading'}`);
      card.setAttribute('aria-current', String(identity === selectedChapterIdentity));
      return card;
    });
    for (const card of oldByIdentity.values()) {
      if (cards.includes(card)) continue;
      const frame = card.querySelector('.box-frame');
      if (frame) boxFrames.delete(frame);
      boxFrameObserver?.unobserve(card);
      card.remove();
    }
    for (const card of cards) chapterList.append(card);
    requestAnimationFrame(refreshBoxFrames);
  }

  function selectedChapterCard() {
    return selectedChapterIdentity
      ? [...chapterList.querySelectorAll('.chapter-card')].find((card) => card.dataset.identity === selectedChapterIdentity)
      : null;
  }

  function markCurrentChapter(card) {
    selectedChapterIdentity = card?.dataset.identity || null;
    for (const chapterCard of chapterList.querySelectorAll('.chapter-card')) {
      chapterCard.setAttribute('aria-current', String(chapterCard === card));
    }
  }

  function ensureChapterCardVisible(card) {
    const listBounds = chapterList.getBoundingClientRect();
    const cardBounds = card.getBoundingClientRect();
    if (cardBounds.top < listBounds.top) chapterList.scrollTop += cardBounds.top - listBounds.top;
    else if (cardBounds.bottom > listBounds.bottom) chapterList.scrollTop += cardBounds.bottom - listBounds.bottom;
  }

  function focusChapterCard(card) {
    if (!card) return;
    card.focus({ preventScroll: true });
    ensureChapterCardVisible(card);
  }

  function navigateToChapter(index, identity = null, { focusEditor = true } = {}) {
    const chapter = chapters[index];
    if (!chapter) return;
    const card = identity
      ? [...chapterList.querySelectorAll('.chapter-card')].find((chapterCard) => chapterCard.dataset.identity === identity)
      : chapterList.querySelector(`.chapter-card[data-chapter-index="${index}"]`);
    if (card) markCurrentChapter(card);
    if (analysisVersion !== sourceVersion) {
      pendingSourceChapterIdentity = card?.dataset.identity || identity;
      pendingSourceChapterFocusEditor = focusEditor;
      worker.postMessage({ type: 'flush', version: sourceVersion });
      return;
    }
    if (!previewing) {
      view.dispatch({ selection: { anchor: chapter.position }, effects: EditorView.scrollIntoView(chapter.position, { y: 'center' }) });
      if (focusEditor) view.focus();
      return;
    }
    const target = preview.querySelector(`[data-chapter-index="${index}"]`);
    if (target) scrollPreviewToHeading(target);
    else pendingPreviewChapter = index;
  }

  chapterList.addEventListener('click', (event) => {
    const card = event.target.closest?.('.chapter-card');
    if (card && chapterList.contains(card)) navigateToChapter(Number(card.dataset.chapterIndex), card.dataset.identity);
  });

  chapterList.addEventListener('keydown', (event) => {
    if (event.ctrlKey || event.altKey || event.metaKey || event.shiftKey || !['ArrowUp', 'ArrowDown'].includes(event.key)) return;
    const card = event.target.closest?.('.chapter-card');
    if (!card || !chapterList.contains(card)) return;
    event.preventDefault();
    const cards = [...chapterList.querySelectorAll('.chapter-card')];
    const index = cards.indexOf(card);
    const nextIndex = Math.max(0, Math.min(cards.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)));
    if (nextIndex === index) return;
    const nextCard = cards[nextIndex];
    focusChapterCard(nextCard);
    navigateToChapter(Number(nextCard.dataset.chapterIndex), nextCard.dataset.identity, { focusEditor: false });
  });

  function scrollPreviewToHeading(heading) {
    const previewBounds = preview.getBoundingClientRect();
    const headingBounds = heading.getBoundingClientRect();
    const framePaddingTop = root.dataset.theme === 'vintage-msdos-edit'
      ? Number.parseFloat(getComputedStyle(preview).paddingTop) || 0
      : 0;
    preview.scrollTop = Math.max(0, headingBounds.top - previewBounds.top + preview.scrollTop - framePaddingTop);
  }

  function markPreviewChapters() {
    preview.querySelectorAll('h1, h2, h3, h4, h5').forEach((heading, index) => { heading.dataset.chapterIndex = String(index); });
    if (pendingPreviewChapter !== null) {
      const target = preview.querySelector(`[data-chapter-index="${pendingPreviewChapter}"]`);
      if (target) scrollPreviewToHeading(target);
      pendingPreviewChapter = null;
    }
  }

  async function renderPreview() {
    const version = ++previewRenderVersion;
    const markdown = documentText();
    preview.replaceChildren(Object.assign(document.createElement('p'), { textContent: 'Rendering…' }));
    try {
      const html = await invoke('render_markdown', { markdown });
      if (previewing && version === previewRenderVersion) {
        preview.innerHTML = html;
        markPreviewChapters();
      }
    } catch (error) {
      if (previewing && version === previewRenderVersion) preview.textContent = `Preview could not render: ${String(error)}`;
    }
  }

  function togglePreview() {
    previewing = !previewing;
    documentShell.classList.toggle('previewing', previewing);
    preview.hidden = !previewing;
    editorHost.hidden = previewing;
    if (previewing) void renderPreview();
    else view.focus();
  }

  function sendInitialAnalysis(document) {
    sourceVersion += 1;
    analysisVersion = -1;
    worker.postMessage({ type: 'init', version: sourceVersion, document });
  }

  function handleUpdate(update) {
    if (!update.docChanged) return;
    const started = benchmarkMode ? performance.now() : 0;
    contentVersion += 1;
    dirty = true;
    sourceVersion += 1;
    const changes = [];
    update.changes.iterChanges((from, to, _fromB, _toB, inserted) => changes.push({ from, to, inserted: inserted.toString() }));
    worker.postMessage({ type: 'changes', version: sourceVersion, changes });
    if (benchmarkMode) updateSamples.push(performance.now() - started);
  }

  function createState(document) {
    return EditorState.create({
      doc: document,
      extensions: [
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        markdown(),
        EditorView.lineWrapping,
        editorTheme(),
        EditorView.updateListener.of(handleUpdate),
      ],
    });
  }

  view = new EditorView({ state: createState(''), parent: editorHost });
  worker.onmessage = ({ data }) => {
    if (data.type !== 'analysis' || data.version !== sourceVersion) return;
    analysisVersion = data.version;
    chapters = data.headings;
    patchChapters(chapters);
    updateMetrics(data.total);
      if (pendingSourceChapterIdentity) {
      const currentCard = [...chapterList.querySelectorAll('.chapter-card')]
        .find((card) => card.dataset.identity === pendingSourceChapterIdentity);
        pendingSourceChapterIdentity = null;
        if (currentCard) navigateToChapter(Number(currentCard.dataset.chapterIndex), currentCard.dataset.identity, { focusEditor: pendingSourceChapterFocusEditor });
      }
  };

  function setProject(project) {
    currentProject = project;
    currentRelativePath = null;
    currentPath = null;
    currentDocumentId = null;
    currentWriteRevision = 0;
    selectedTreePath = null;
    treeNodes.clear();
    treeRows.clear();
    projectTree.replaceChildren();
    updateThemeChrome();
  }

  function replaceDocument(document, { relativePath = null, documentId = null, writeRevision = 0, clean = false } = {}) {
    documentGeneration += 1;
    contentVersion += 1;
    currentRelativePath = relativePath;
    currentPath = currentProject && relativePath ? `${currentProject.root}/${relativePath}` : null;
    currentDocumentId = documentId;
    currentWriteRevision = writeRevision;
    lastSavedText = clean ? document : null;
    dirty = !clean;
    chapters = [];
    selectedChapterIdentity = null;
    pendingSourceChapterIdentity = null;
    patchChapters(chapters);
    view.setState(createState(document));
    sendInitialAnalysis(document);
  }

  function applyDocument(document) {
    if (!document || typeof document.content !== 'string' || !Number.isSafeInteger(document.documentId)) return false;
    replaceDocument(document.content, { relativePath: document.relativePath, documentId: document.documentId, writeRevision: document.writeRevision || 0, clean: true });
    selectedTreePath = document.relativePath;
    patchTree();
    if (previewing) void renderPreview();
    return true;
  }

  function visibleTreeNodes() {
    const result = [];
    const add = (path, depth) => {
      const node = treeNodes.get(path); if (!node) return;
      result.push({ path, node, depth });
      if (node.entry.kind === 'directory' && node.expanded) for (const child of node.children || []) add(child, depth + 1);
    };
    for (const path of treeNodes.get('')?.children || []) add(path, 0);
    return result;
  }

  function treeRowKey(event) { return String(event.key || ''); }
  function patchTree() {
    const visible = visibleTreeNodes();
    const live = new Set();
    for (const { path, node, depth } of visible) {
      live.add(path);
      let row = treeRows.get(path);
      if (!row) {
        row = document.createElement('button'); row.type = 'button'; row.className = 'tree-row'; row.setAttribute('role', 'treeitem');
        row.append(Object.assign(document.createElement('span'), { className: 'tree-row__disclosure' }), Object.assign(document.createElement('span'), { className: 'tree-row__label' }));
        row.addEventListener('click', () => { if (node.entry.kind === 'directory') void toggleDirectory(path); else void requestOpenDocument(path); });
        row.addEventListener('keydown', onTreeKeydown);
        treeRows.set(path, row);
      }
      row.dataset.path = path; row.style.paddingLeft = `${0.35 + depth * 1}rem`; row.children[0].textContent = node.entry.kind === 'directory' ? (node.expanded ? '▾' : '▸') : '·'; row.children[1].textContent = node.entry.name;
      row.setAttribute('aria-level', String(depth + 1)); row.setAttribute('aria-selected', String(selectedTreePath === path));
      if (node.entry.kind === 'directory') row.setAttribute('aria-expanded', String(Boolean(node.expanded))); else row.removeAttribute('aria-expanded');
      projectTree.append(row);
    }
    for (const [path, row] of treeRows) if (!live.has(path)) { row.remove(); treeRows.delete(path); }
  }

  async function loadDirectory(path = '') {
    if (!currentProject) return;
    const projectId = currentProject.projectId;
    const existing = treeNodes.get(path);
    if (existing?.loaded) return;
    const entries = await invoke('list_project_directory', { projectId, relativePath: path });
    if (!currentProject || currentProject.projectId !== projectId) return;
    const parent = treeNodes.get(path) || { entry: { kind: 'directory', name: '' }, expanded: true };
    parent.loaded = true; parent.children = [];
    treeNodes.set(path, parent);
    for (const entry of entries) { treeNodes.set(entry.relativePath, { entry, loaded: false, expanded: false, children: [] }); parent.children.push(entry.relativePath); }
    patchTree();
  }

  async function toggleDirectory(path) {
    const node = treeNodes.get(path); if (!node || node.entry.kind !== 'directory') return;
    node.expanded = !node.expanded; patchTree();
    if (node.expanded) { try { await loadDirectory(path); } catch (error) { console.error('Could not list project directory:', error); } }
  }

  function focusTree(path = selectedTreePath) { const row = treeRows.get(path) || treeRows.get(visibleTreeNodes()[0]?.path); row?.focus(); }
  function focusChapter() {
    focusChapterCard(selectedChapterCard() || chapterList.querySelector('.chapter-card'));
  }
  function onTreeKeydown(event) {
    const row = event.currentTarget; const path = row.dataset.path; const nodes = visibleTreeNodes(); const index = nodes.findIndex((item) => item.path === path); const key = treeRowKey(event);
    if (!['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Enter', ' '].includes(key)) return;
    event.preventDefault();
    if (key === 'ArrowUp') focusTree(nodes[Math.max(0, index - 1)]?.path);
    else if (key === 'ArrowDown') focusTree(nodes[Math.min(nodes.length - 1, index + 1)]?.path);
    else if (key === 'ArrowRight' && nodes[index]?.node.entry.kind === 'directory') { if (!nodes[index].node.expanded) void toggleDirectory(path); else focusTree(nodes[index + 1]?.path); }
    else if (key === 'ArrowLeft' && nodes[index]?.node.entry.kind === 'directory' && nodes[index].node.expanded) void toggleDirectory(path);
    else if (key === 'Enter' || key === ' ') { if (nodes[index]?.node.entry.kind === 'directory') void toggleDirectory(path); else void requestOpenDocument(path); }
  }

  function askDirty() {
    if (!dirty) return Promise.resolve('discard');
    return new Promise((resolve) => {
      const close = () => { dirtyDialog.removeEventListener('close', close); resolve(dirtyDialog.returnValue || 'cancel'); };
      dirtyDialog.addEventListener('close', close, { once: true }); dirtyDialog.showModal();
    });
  }

  async function saveDocument() {
    if (!currentProject) { console.info('Open a project before saving.'); return false; }
    const saveGeneration = documentGeneration; const saveVersion = contentVersion; const saveRevision = Math.max(saveVersion, currentWriteRevision); const content = documentText();
    try {
      const saved = currentDocumentId === null
        ? await invoke('save_document_as', { projectId: currentProject.projectId, revision: saveRevision, content })
        : await invoke('save_document', { projectId: currentProject.projectId, documentId: currentDocumentId, revision: saveRevision, content });
      if (!saved || documentGeneration !== saveGeneration) return false;
      currentRelativePath = saved.relativePath; currentPath = `${currentProject.root}/${saved.relativePath}`; currentDocumentId = saved.documentId; currentWriteRevision = saved.writeRevision; selectedTreePath = saved.relativePath;
      lastSavedText = content; if (contentVersion === saveVersion) dirty = false;
      if (currentDocumentId && treeNodes.has('')) { treeNodes.get('').loaded = false; await loadDirectory(''); }
      if (analysisVersion === sourceVersion) updateMetrics(latestTotal);
      return true;
    } catch (error) { console.error('Could not save document:', error); return false; }
  }

  async function allowTransition() {
    if (!dirty) return true;
    const action = await askDirty();
    if (action === 'cancel') return false;
    return action === 'discard' || await saveDocument();
  }

  async function requestOpenDocument(relativePath) {
    if (!currentProject || transitionInFlight || relativePath === currentRelativePath) return;
    transitionInFlight = true;
    try {
      if (!await allowTransition()) return;
      const projectId = currentProject.projectId;
      const document = await invoke('open_project_document', { projectId, relativePath });
      if (currentProject?.projectId === projectId) applyDocument(document);
    } catch (error) { console.error('Could not open project document:', error); } finally { transitionInFlight = false; }
  }

  async function openProject() {
    if (transitionInFlight) return;
    transitionInFlight = true;
    try {
      if (!await allowTransition()) return;
      const project = await invoke('open_project');
      if (!project) return;
      setProject(project); replaceDocument(DEFAULT_DOCUMENT, { clean: true });
      treeNodes.set('', { entry: { kind: 'directory', name: '' }, loaded: false, expanded: true, children: [] });
      await loadDirectory('');
    } catch (error) { console.error('Could not open project:', error); } finally { transitionInFlight = false; }
  }

  async function restoreLastProject() {
    try {
      const restored = await invoke('restore_last_project');
      if (!restored?.project) return false;
      setProject(restored.project); replaceDocument(restored.document?.content || DEFAULT_DOCUMENT, { relativePath: restored.document?.relativePath || null, documentId: restored.document?.documentId || null, writeRevision: restored.document?.writeRevision || 0, clean: true });
      if (restored.document) selectedTreePath = restored.document.relativePath;
      treeNodes.set('', { entry: { kind: 'directory', name: '' }, loaded: false, expanded: true, children: [] });
      await loadDirectory('');
      return true;
    } catch (error) { console.error('Could not restore project:', error); return false; }
  }

  const autosave = createAutosaveController({
    snapshot() {
      if (!dirty || !currentProject || currentDocumentId === null) return null;
      const text = documentText(); if (text === lastSavedText) { dirty = false; return null; }
      return { documentGeneration, projectId: currentProject.projectId, documentId: currentDocumentId, revision: Math.max(contentVersion, currentWriteRevision), path: currentPath, contentVersion, text };
    },
    write({ projectId, documentId, revision, text }) { return invoke('autosave_document', { projectId, documentId, revision, content: text }); },
    onSuccess(saved) { if (documentGeneration === saved.documentGeneration && currentProject?.projectId === saved.projectId && currentDocumentId === saved.documentId && currentPath === saved.path) { lastSavedText = saved.text; if (contentVersion === saved.contentVersion) dirty = false; } },
    onError(error) { console.error('Could not autosave document:', error); }, setIntervalFn: window.setInterval.bind(window), clearIntervalFn: window.clearInterval.bind(window),
  });

  function setApplicationZoom(direction) {
    const next = Math.min(MAX_ZOOM_STEPS, Math.max(MIN_ZOOM_STEPS, zoomSteps + direction));
    if (next === zoomSteps) return;
    zoomSteps = next;
    void invoke('set_application_zoom', { zoom: zoomSteps / 10 }).catch((error) => console.error('Could not set application zoom:', error));
  }

  function getViewportWidth() { return window.innerWidth || document.documentElement.clientWidth || PAGE_DEFAULT_MAX_WIDTH + SIDEBAR_DEFAULT_WIDTH + RESIZE_HANDLE_HIT_WIDTH; }
  function getViewportHeight() { return window.innerHeight || document.documentElement.clientHeight || 900; }
  function getWorkspaceBreathingGap() { return Math.min(WORKSPACE_BREATHING_GAP_MAX, Math.max(WORKSPACE_BREATHING_GAP_MIN, getViewportHeight() * WORKSPACE_BREATHING_GAP_RATIO)); }
  function getPageRowMaximumHeight() { return Math.min(getViewportHeight() * PAGE_VIEWPORT_HEIGHT_RATIO, getViewportHeight() - (getWorkspaceBreathingGap() * 2) - METRICS_HEIGHT); }
  function getEditorWorkspaceWidth() {
    const layoutWidth = writingLayout?.getBoundingClientRect().width;
    const sidebarWidth = chapterSidebar?.getBoundingClientRect().width || SIDEBAR_DEFAULT_WIDTH;
    const projectWidth = projectPanel?.getBoundingClientRect().width || SIDEBAR_DEFAULT_WIDTH;
    if (layoutWidth) return layoutWidth - sidebarWidth - projectWidth;
    return getViewportWidth() - sidebarWidth - projectWidth;
  }
  function getPageMaximumWidth() { return Math.min(PAGE_MAX_WIDTH, Math.max(PAGE_MIN_WIDTH, Math.floor(getEditorWorkspaceWidth() - RESIZE_HANDLE_HIT_WIDTH))); }
  function clampPageWidth(width) { return Math.min(getPageMaximumWidth(), Math.max(PAGE_MIN_WIDTH, Math.round(width))); }
  function getDefaultPageWidth() { return clampPageWidth(Math.min(getViewportWidth() * 0.62, PAGE_DEFAULT_MAX_WIDTH, getPageRowMaximumHeight() * 210 / 297)); }
  function applyPageWidth() { pageWidth = clampPageWidth(pageWidth); document.documentElement.style.setProperty('--page-width', `${pageWidth}px`); }
  function schedulePageWidth() {
    if (resizeFrame !== null) return;
    resizeFrame = requestFrame(() => { resizeFrame = null; applyPageWidth(); });
  }
  function flushPageWidth() {
    if (resizeFrame !== null) { cancelFrame(resizeFrame); resizeFrame = null; }
    applyPageWidth();
  }
  function changePageWidth(direction) { pageWidth = clampPageWidth(pageWidth + direction * PAGE_WIDTH_INCREMENT); applyPageWidth(); }
  function resetPageWidth() { pageWidth = getDefaultPageWidth(); applyPageWidth(); }
  function stopPageResize(event) {
    if (resizePointerId === null || (event.pointerId !== undefined && event.pointerId !== resizePointerId)) return;
    flushPageWidth();
    if (resizeHandle?.hasPointerCapture?.(resizePointerId)) resizeHandle.releasePointerCapture(resizePointerId);
    resizePointerId = null;
    resizeHandle = null;
    resizeDirection = 0;
    document.body.classList.remove('page-resizing');
  }

  function movePageResize(event) {
    if (resizePointerId === null || event.pointerId !== resizePointerId) return;
    event.preventDefault();
    pageWidth = clampPageWidth(resizeStartWidth + resizeDirection * (event.clientX - resizeStartX));
    schedulePageWidth();
  }

  for (const { element, direction } of pageResizeHandles) {
    element.addEventListener('pointerdown', (event) => {
      if (event.button !== 0 || resizePointerId !== null) return;
      event.preventDefault();
      resizePointerId = event.pointerId;
      resizeStartX = event.clientX;
      resizeStartWidth = pageWidth;
      resizeDirection = direction;
      resizeHandle = element;
      element.setPointerCapture?.(resizePointerId);
      document.body.classList.add('page-resizing');
    });
  }
  window.addEventListener('pointermove', movePageResize);
  window.addEventListener('pointerup', stopPageResize);
  window.addEventListener('pointercancel', stopPageResize);
  window.addEventListener('resize', applyPageWidth);
  window.addEventListener('keydown', (event) => {
    if (!event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && ['F1', 'F2', 'F3'].includes(event.key)) {
      event.preventDefault();
      if (event.key === 'F1') focusTree();
      else if (event.key === 'F2') { if (previewing) togglePreview(); view.focus(); }
      else focusChapter();
      return;
    }
    if (!event.ctrlKey || event.altKey || event.metaKey) return;
    const key = String(event.key || '').toLowerCase();
    // Shift is intentionally excluded so the host/browser's Ctrl+Shift+T
    // reopen-tab convention remains available; plain Ctrl+T is AMTE's toggle.
    if (isThemeToggleShortcut(event) && !event.shiftKey) {
      event.preventDefault();
      event.stopPropagation();
      activateTheme(activeThemeId === DEFAULT_THEME_ID ? 'vintage-msdos-edit' : DEFAULT_THEME_ID, true);
      return;
    }
    const widthDecrease = event.code === 'BracketLeft' || key === '[';
    const widthIncrease = event.code === 'BracketRight' || key === ']';
    const widthReset = event.code === 'Backspace' || key === 'backspace';
    const zoomIn = key === '+' || (key === '=' && event.shiftKey) || event.code === 'NumpadAdd';
    const zoomOut = key === '-' || key === 'subtract' || event.code === 'NumpadSubtract';
    if (widthDecrease || widthIncrease || widthReset || zoomIn || zoomOut || key === 'p' || key === 's' || key === 'o') {
      event.preventDefault();
      event.stopPropagation();
      if (widthReset) resetPageWidth();
      else if (widthDecrease || widthIncrease) changePageWidth(widthIncrease ? 1 : -1);
      else if (zoomIn || zoomOut) setApplicationZoom(zoomIn ? 1 : -1);
      else if (key === 'p') togglePreview();
      else if (key === 's') void saveDocument();
      else void openProject();
    }
  }, true);

  applyPageWidth();
  if (!await restoreLastProject()) { updateThemeChrome(); replaceDocument(DEFAULT_DOCUMENT, { clean: true }); }
  view.focus();
  autosave.start();
  window.addEventListener('beforeunload', () => autosave.stop(), { once: true });
  const app = { view, worker, get sourceVersion() { return sourceVersion; }, get analysisVersion() { return analysisVersion; }, togglePreview, replaceDocument };
  if (benchmarkMode) void runBrowserBenchmark(app, updateSamples, editorHost);
  return app;
}

async function runBrowserBenchmark(app, updateSamples, editorHost) {
  const source = (`# Benchmark\n\n${'alpha beta gamma delta epsilon zeta eta theta iota kappa\n'.repeat(10000)}`).slice(0, 400 * 1024);
  app.replaceDocument(source);
  updateSamples.length = 0;
  const counts = [];
  for (const position of [0, Math.floor(source.length / 2), source.length - 1]) {
    app.view.dispatch({ selection: { anchor: position }, effects: EditorView.scrollIntoView(position, { y: 'center' }) });
    await new Promise((resolve) => requestAnimationFrame(resolve));
    counts.push(editorHost.querySelectorAll('*').length);
  }
  for (let index = 0; index < 100; index += 1) app.view.dispatch({ changes: { from: app.view.state.doc.length, insert: String(index % 10) } });
  const sorted = [...updateSamples].sort((left, right) => left - right);
  const p95 = sorted[Math.ceil(sorted.length * 0.95) - 1] || 0;
  app.worker.postMessage({ type: 'flush', version: app.sourceVersion });
  const deadline = performance.now() + 5000;
  while (app.analysisVersion !== app.sourceVersion && performance.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  const result = {
    bytes: source.length,
    samples: sorted.length,
    p95Ms: Number(p95.toFixed(3)),
    domNodesTopMiddleEnd: counts,
    workerVersion: app.analysisVersion,
    sourceVersion: app.sourceVersion,
    converged: app.analysisVersion === app.sourceVersion,
    total: document.querySelector('#total-count')?.textContent,
    chapters: document.querySelectorAll('.chapter-card').length,
  };
  document.body.dataset.amteBenchmark = JSON.stringify(result);
  const output = document.createElement('pre');
  output.id = 'benchmark-result';
  output.textContent = JSON.stringify(result);
  document.body.append(output);
}

window.__amteReady = start();

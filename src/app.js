import { EditorState } from '@codemirror/state';
import { history, historyKeymap, defaultKeymap, indentWithTab } from '@codemirror/commands';
import { markdown } from '@codemirror/lang-markdown';
import { syntaxHighlighting, HighlightStyle } from '@codemirror/language';
import { EditorView, keymap } from '@codemirror/view';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { SearchQuery, closeSearchPanel, findNext, findPrevious, openSearchPanel, search, setSearchQuery } from '@codemirror/search';
import { tags } from '@lezer/highlight';
import appConfigUrl from './config.json?url';
import themeUrl from './theme.json?url';
import vintageThemeUrl from './themes/vintage-msdos-edit.json?url';
import { classifyWordActivity, reconcileCollapsedChapterIdentities } from './analysis.js';
import { BufferSession, clampRestoredSelection } from './buffer-session.js';
import { createCloseRequestGuard, runCloseFlow } from './close-flow.js';
import { createSaveIndicatorController } from './save-indicator.js';
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
const AUTO_CLOSE_BRACKETS = Object.freeze(['(', '[', '{', '"', "'", '`']);

/* APP_CONFIG_START */
const DEFAULT_APP_CONFIG = Object.freeze({
  appearance: Object.freeze({ vintageAsciiFrames: false }),
  editor: Object.freeze({ autoClosePairs: true }),
});

function validateAppConfig(value) {
  const vintageAsciiFrames = value
    && typeof value === 'object'
    && !Array.isArray(value)
    && value.appearance
    && typeof value.appearance === 'object'
    && !Array.isArray(value.appearance)
    && typeof value.appearance.vintageAsciiFrames === 'boolean'
    ? value.appearance.vintageAsciiFrames
    : false;
  const autoClosePairs = value
    && typeof value === 'object'
    && !Array.isArray(value)
    && value.editor
    && typeof value.editor === 'object'
    && !Array.isArray(value.editor)
    && typeof value.editor.autoClosePairs === 'boolean'
    ? value.editor.autoClosePairs
    : true;
  return { appearance: { vintageAsciiFrames }, editor: { autoClosePairs } };
}

async function loadAppConfig(url, fetchFn = fetch) {
  try {
    const response = await fetchFn(url);
    if (!response?.ok) return DEFAULT_APP_CONFIG;
    return validateAppConfig(await response.json());
  } catch {
    return DEFAULT_APP_CONFIG;
  }
}

function applyAppConfig(config, root = document.documentElement) {
  root.dataset.vintageAsciiFrames = String(validateAppConfig(config).appearance.vintageAsciiFrames);
}

function paintsVintageAsciiFrames(root = document.documentElement) {
  return root.dataset.theme === 'vintage-msdos-edit' && root.dataset.vintageAsciiFrames === 'true';
}

function clearBoxFramePaint(frame) {
  for (const part of frame.querySelectorAll('[data-box-edge], [data-box-corner]')) part.textContent = '';
}
/* APP_CONFIG_END */

// This small controller deliberately has no editor or worker dependencies: an
// autosave always receives the synchronous CodeMirror snapshot taken at tick
// time, never an eventually-consistent analysis result.
/* AUTOSAVE_CONTROLLER_START */
function createAutosaveController({ snapshot, write, onSuccess, onError, setIntervalFn, clearIntervalFn }) {
  let timer = null;
  let writeInFlight = false;
  let work = null;
  let paused = false;

  async function tick() {
    if (paused || writeInFlight) return;
    const pending = snapshot();
    if (!pending || (Array.isArray(pending) && !pending.length)) return;
    writeInFlight = true;
    work = (async () => {
      try {
      if (Array.isArray(pending)) {
        for (const item of pending) {
          try { onSuccess(item, await write(item)); } catch (error) { onError(error, item); }
        }
      } else onSuccess(pending, await write(pending));
      } catch (error) {
        onError(error);
      } finally {
        writeInFlight = false;
      }
    })();
    try { await work; } finally { work = null; }
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
    async quiesce() { paused = true; this.stop(); if (work) await work; },
    resume() { paused = false; this.start(); },
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

/* DAILY_WORD_ACTIVITY_START */
const DAILY_WORD_ACTIVITY_PREFIX = 'amte.daily-word-activity.v1:';

function validActivityCount(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function addActivityCounts(left, right) {
  return Math.min(Number.MAX_SAFE_INTEGER, left + right);
}

function readDailyWordActivity(storage, key, day) {
  try {
    const value = JSON.parse(storage.getItem(`${DAILY_WORD_ACTIVITY_PREFIX}${key}`));
    if (value?.day === day && validActivityCount(value.added) && validActivityCount(value.deleted)) return value;
  } catch { /* Treat unavailable or malformed local storage as an empty day. */ }
  return { day, added: 0, deleted: 0 };
}

function createDailyWordActivityStore({ storage, getDay }) {
  let identity = null;
  let activity = { day: getDay(), added: 0, deleted: 0 };
  const blank = (day = getDay()) => ({ day, added: 0, deleted: 0 });
  const persist = () => {
    if (!identity) return;
    try { storage.setItem(`${DAILY_WORD_ACTIVITY_PREFIX}${identity}`, JSON.stringify(activity)); } catch (error) { console.error('Could not persist daily word activity:', error); }
  };
  const rollOver = () => {
    const day = getDay();
    if (activity.day === day) return;
    activity = blank(day);
    persist();
  };
  return {
    setIdentity(nextIdentity) {
      const next = nextIdentity || null;
      if (next === identity) { rollOver(); return this.snapshot(); }
      identity = next;
      activity = identity ? readDailyWordActivity(storage, identity, getDay()) : blank();
      return this.snapshot();
    },
    // A successful first Save As gives transient activity a durable file key.
    // Existing activity for that path (if any) is retained and combined.
    adoptIdentity(nextIdentity) {
      if (!nextIdentity) return this.snapshot();
      rollOver();
      if (identity) return this.setIdentity(nextIdentity);
      const saved = readDailyWordActivity(storage, nextIdentity, activity.day);
      identity = nextIdentity;
      activity = { day: activity.day, added: addActivityCounts(saved.added, activity.added), deleted: addActivityCounts(saved.deleted, activity.deleted) };
      persist();
      return this.snapshot();
    },
    record(delta) {
      rollOver();
      const added = validActivityCount(delta?.added) ? delta.added : 0;
      const deleted = validActivityCount(delta?.deleted) ? delta.deleted : 0;
      if (added || deleted) {
        activity = { ...activity, added: addActivityCounts(activity.added, added), deleted: addActivityCounts(activity.deleted, deleted) };
        persist();
      }
      return this.snapshot();
    },
    restore(snapshot) {
      if (!snapshot || snapshot.day !== getDay() || !validActivityCount(snapshot.added) || !validActivityCount(snapshot.deleted)) return this.snapshot();
      activity = { day: snapshot.day, added: snapshot.added, deleted: snapshot.deleted };
      persist();
      return this.snapshot();
    },
    snapshot() { rollOver(); return { ...activity, identity }; },
  };
}
/* DAILY_WORD_ACTIVITY_END */

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
      '.cm-searchMatch': { backgroundColor: 'color-mix(in srgb, var(--theme-accent) 42%, transparent)', outline: '1px solid color-mix(in srgb, var(--theme-accent) 68%, transparent)' },
      '.cm-searchMatch.cm-searchMatch-selected': { backgroundColor: 'var(--theme-accent)', color: 'var(--theme-paper)', outlineColor: 'var(--theme-ink)' },
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
  // Configuration is a bundled, offline asset. Its failure must never block
  // writing, so malformed or unavailable content falls back to safe defaults.
  // The global is intentionally test-only and is absent in normal launches.
  const appConfig = globalThis.__AMTE_TEST_CONFIG__ === undefined
    ? await loadAppConfig(appConfigUrl)
    : validateAppConfig(globalThis.__AMTE_TEST_CONFIG__);
  applyAppConfig(appConfig);
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
  const searchDialog = document.querySelector('#search-dialog');
  const searchForm = document.querySelector('#search-form');
  const searchInput = document.querySelector('#search-term');
  const searchClose = document.querySelector('#search-close');
  const searchStatus = document.querySelector('#search-status');
  const projectPathLabel = document.querySelector('#project-path');
  const documentShell = document.querySelector('#document-shell');
  const themeChrome = document.querySelector('#theme-chrome');
  const saveIndicator = createSaveIndicatorController({ element: document.querySelector('#save-indicator') });
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
   let activationGeneration = 0;
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
  let focusedChapterIdentity = null;
  let collapsedChapterIdentities = new Set();
  const chapterRows = new Map();
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
  const query = new URLSearchParams(window.location.search);
  const benchmarkMode = query.has('benchmark');
  const browserCheckMode = benchmarkMode || query.has('browser-check');
  const updateSamples = [];
  let view;
  let activeSearchTerm = '';
  let searchRestoreTarget = null;
  let searchShouldRestoreFocus = false;
  let searchRestoreToEditor = false;
   const dailyActivity = createDailyWordActivityStore({ storage: localStorage, getDay: localDay });
   const buffers = new BufferSession();
   let selectedOpenRequest = 0;

  function refreshBoxFrames() {
    // The edge tracks use text glyphs, so empty the existing tracks before
    // returning when frames are disabled. This prevents a prior true setting
    // from leaving a bottom edge available to paint after a false setting.
    if (!paintsVintageAsciiFrames(root)) {
      for (const frame of boxFrames) clearBoxFramePaint(frame);
      return;
    }
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

  function updateActivityMetric() {
    const activity = dailyActivity.snapshot();
    dailyCount.textContent = `(-): ${activity.deleted} (+): ${activity.added}`;
  }

  function updateMetrics(total) {
    latestTotal = total;
    totalCount.textContent = `total: ${total}`;
    updateActivityMetric();
  }

  function chapterByIdentity(identity) { return chapters.find((chapter) => chapter.identity === identity) || null; }
  function chapterChildren(identity) { return chapters.filter((chapter) => chapter.parentIdentity === identity); }
  function isChapterVisible(chapter) {
    let parent = chapter?.parentIdentity;
    while (parent) {
      if (collapsedChapterIdentities.has(parent)) return false;
      parent = chapterByIdentity(parent)?.parentIdentity || null;
    }
    return Boolean(chapter);
  }
  function visibleChapters() { return chapters.filter(isChapterVisible); }
  function nearestVisibleChapter(identity) {
    let chapter = chapterByIdentity(identity);
    while (chapter && !isChapterVisible(chapter)) chapter = chapterByIdentity(chapter.parentIdentity);
    return chapter || visibleChapters()[0] || null;
  }
  function isChapterDescendant(identity, ancestor) {
    let chapter = chapterByIdentity(identity);
    while (chapter?.parentIdentity) {
      if (chapter.parentIdentity === ancestor) return true;
      chapter = chapterByIdentity(chapter.parentIdentity);
    }
    return false;
  }

  function createChapterRow() {
    const row = document.createElement('div');
    row.className = 'chapter-card';
    row.setAttribute('role', 'treeitem');
    const disclosure = document.createElement('button');
    disclosure.type = 'button'; disclosure.className = 'chapter-disclosure'; disclosure.tabIndex = -1;
    const title = document.createElement('button');
    title.type = 'button'; title.className = 'chapter-title';
    title.append(Object.assign(document.createElement('span'), { className: 'chapter-number' }), Object.assign(document.createElement('span'), { className: 'chapter-label' }));
    row.append(disclosure, title);
    decorateBoxFrame(row);
    boxFrameObserver?.observe(row);
    return row;
  }

  function syncChapterRovingTabIndex() {
    const fallback = nearestVisibleChapter(focusedChapterIdentity || selectedChapterIdentity)?.identity || null;
    focusedChapterIdentity = fallback;
    for (const [identity, row] of chapterRows) {
      const visible = !row.hidden;
      row.querySelector('.chapter-title').tabIndex = visible && identity === fallback ? 0 : -1;
      row.querySelector('.chapter-disclosure').tabIndex = -1;
    }
  }

  function patchChapters(nextChapters = chapters) {
    const visible = new Set(visibleChapters().map((chapter) => chapter.identity));
    const live = new Set();
    for (const chapter of nextChapters) {
      const identity = chapter.identity;
      live.add(identity);
      let row = chapterRows.get(identity);
      if (!row) { row = createChapterRow(); chapterRows.set(identity, row); }
      const hasChildren = chapter.childCount > 0;
      const title = row.querySelector('.chapter-title');
      const disclosure = row.querySelector('.chapter-disclosure');
      row.dataset.identity = identity;
      row.dataset.chapterIndex = String(chapter.index);
      row.dataset.position = String(chapter.position);
      row.style.setProperty('--chapter-level', String(chapter.level));
      row.setAttribute('aria-level', String(chapter.level));
      row.setAttribute('aria-current', String(identity === selectedChapterIdentity));
      row.hidden = !visible.has(identity);
      row.toggleAttribute('inert', row.hidden);
      // `hidden` removes the treeitem wrapper (and therefore its frame and
      // shadow) from layout. Keep the DOM accessibility state explicit too:
      // inert only prevents interaction, while aria-hidden prevents a cached
      // accessibility tree from exposing a collapsed descendant between paints.
      row.setAttribute('aria-hidden', String(row.hidden));
      if (hasChildren) {
        row.setAttribute('aria-expanded', String(!collapsedChapterIdentities.has(identity)));
        disclosure.hidden = false;
        disclosure.textContent = collapsedChapterIdentities.has(identity) ? '▸' : '▾';
        disclosure.setAttribute('aria-label', `${collapsedChapterIdentities.has(identity) ? 'Expand' : 'Collapse'} ${chapter.text || 'untitled heading'}`);
      } else {
        row.removeAttribute('aria-expanded');
        disclosure.hidden = true;
        disclosure.setAttribute('aria-label', 'No subheadings');
      }
      title.children[0].textContent = `${chapter.number} `;
      title.children[1].textContent = chapter.text || 'Untitled heading';
      title.setAttribute('aria-label', `Go to heading ${chapter.number}: ${chapter.text || 'Untitled heading'}`);
      chapterList.append(row);
    }
    for (const [identity, row] of chapterRows) {
      if (live.has(identity)) continue;
      const frame = row.querySelector('.box-frame');
      if (frame) boxFrames.delete(frame);
      boxFrameObserver?.unobserve(row);
      row.remove(); chapterRows.delete(identity);
    }
    const resolved = nearestVisibleChapter(selectedChapterIdentity);
    if (selectedChapterIdentity !== resolved?.identity) markCurrentChapter(resolved?.identity || null, false);
    syncChapterRovingTabIndex();
    requestAnimationFrame(refreshBoxFrames);
  }

  function selectedChapterCard() { return chapterRows.get(selectedChapterIdentity) || null; }

  function markCurrentChapter(identity, persist = true) {
    selectedChapterIdentity = identity || null;
    const entry = buffers.active();
    if (entry) {
      entry.chapterIdentity = selectedChapterIdentity;
      if (persist) scheduleRecovery(entry);
    }
    for (const [chapterIdentity, row] of chapterRows) row.setAttribute('aria-current', String(chapterIdentity === selectedChapterIdentity));
  }

  function ensureChapterCardVisible(card) {
    const listBounds = chapterList.getBoundingClientRect();
    const cardBounds = card.getBoundingClientRect();
    if (cardBounds.top < listBounds.top) chapterList.scrollTop += cardBounds.top - listBounds.top;
    else if (cardBounds.bottom > listBounds.bottom) chapterList.scrollTop += cardBounds.bottom - listBounds.bottom;
  }

  function focusChapterCard(card) {
    if (!card) return;
    focusedChapterIdentity = card.dataset.identity;
    syncChapterRovingTabIndex();
    card.querySelector('.chapter-title').focus({ preventScroll: true });
    ensureChapterCardVisible(card);
  }

  function navigateToChapter(index, identity = null, { focusEditor = true } = {}) {
    const chapter = chapters[index];
    if (!chapter) return;
    const card = chapterRows.get(identity || chapter.identity);
    if (card) markCurrentChapter(card.dataset.identity);
    if (analysisVersion !== sourceVersion) {
      pendingSourceChapterIdentity = card?.dataset.identity || identity;
      pendingSourceChapterFocusEditor = focusEditor;
      worker.postMessage({ type: 'flush', activation: activationGeneration, version: sourceVersion });
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
    if (!card || !chapterList.contains(card)) return;
    if (event.target.closest('.chapter-disclosure')) {
      toggleChapter(card.dataset.identity);
      return;
    }
    if (event.target.closest('.chapter-title')) navigateToChapter(Number(card.dataset.chapterIndex), card.dataset.identity);
  });

  chapterList.addEventListener('keydown', (event) => {
    if (event.ctrlKey || event.altKey || event.metaKey || event.shiftKey) return;
    const card = event.target.closest?.('.chapter-card');
    if (!card || !chapterList.contains(card)) return;
    const key = event.key === 'Spacebar' ? ' ' : event.key;
    if (!['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'Enter', ' '].includes(key)) return;
    event.preventDefault();
    const identity = card.dataset.identity;
    const chapter = chapterByIdentity(identity);
    const visible = visibleChapters();
    const index = visible.findIndex((item) => item.identity === identity);
    const move = (target) => {
      if (!target || target.identity === identity) return;
      const next = chapterRows.get(target.identity);
      focusChapterCard(next);
      navigateToChapter(target.index, target.identity, { focusEditor: false });
    };
    if (key === 'ArrowUp') move(visible[Math.max(0, index - 1)]);
    else if (key === 'ArrowDown') move(visible[Math.min(visible.length - 1, index + 1)]);
    else if (key === 'Home') move(visible[0]);
    else if (key === 'End') move(visible.at(-1));
    else if (key === 'ArrowRight') {
      if (chapter?.childCount && collapsedChapterIdentities.has(identity)) toggleChapter(identity);
      else if (chapter?.childCount) move(chapterChildren(identity)[0]);
    } else if (key === 'ArrowLeft') {
      if (chapter?.childCount && !collapsedChapterIdentities.has(identity)) toggleChapter(identity);
      else move(chapterByIdentity(chapter?.parentIdentity));
    } else if (key === ' ') {
      if (chapter?.childCount) toggleChapter(identity);
    } else if (key === 'Enter') navigateToChapter(chapter.index, identity, { focusEditor: false });
  });

  function toggleChapter(identity) {
    const chapter = chapterByIdentity(identity);
    if (!chapter?.childCount) return;
    const collapsing = !collapsedChapterIdentities.has(identity);
    if (collapsing) collapsedChapterIdentities.add(identity); else collapsedChapterIdentities.delete(identity);
    if (collapsing && (isChapterDescendant(selectedChapterIdentity, identity) || isChapterDescendant(focusedChapterIdentity, identity))) {
      markCurrentChapter(identity, false);
      focusedChapterIdentity = identity;
    }
    const entry = buffers.active();
    if (entry) {
      entry.collapsedChapterIdentities = [...collapsedChapterIdentities].sort();
      entry.chapterIdentity = selectedChapterIdentity;
      scheduleRecovery(entry);
    }
    patchChapters();
    if (collapsing && focusedChapterIdentity === identity) focusChapterCard(chapterRows.get(identity));
  }

  function scrollPreviewToHeading(heading) {
    const previewBounds = preview.getBoundingClientRect();
    const headingBounds = heading.getBoundingClientRect();
    const framePaddingTop = root.dataset.theme === 'vintage-msdos-edit'
      ? Number.parseFloat(getComputedStyle(preview).paddingTop) || 0
      : 0;
    preview.scrollTop = Math.max(0, headingBounds.top - previewBounds.top + preview.scrollTop - framePaddingTop);
  }

  function markPreviewChapters() {
    preview.querySelectorAll('h1, h2, h3, h4, h5, h6').forEach((heading, index) => { heading.dataset.chapterIndex = String(index); });
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
    const entry = buffers.active(); if (entry) { entry.previewing = previewing; scheduleRecovery(entry); }
    if (previewing) void renderPreview();
    else view.focus();
  }

  function createSearchDecorationPanel() {
    const panel = document.createElement('div');
    panel.className = 'search-decoration-panel';
    panel.setAttribute('aria-hidden', 'true');
    return { dom: panel };
  }

  function isVisibleFocusTarget(target) {
    return target instanceof HTMLElement
      && target !== document.body
      && target.isConnected
      && !target.closest('[hidden]')
      && target.getClientRects().length > 0;
  }

  function restoreSearchFocus() {
    const restoreTarget = searchRestoreTarget;
    const focusEditor = searchRestoreToEditor;
    const shouldRestore = searchShouldRestoreFocus;
    searchRestoreTarget = null;
    searchRestoreToEditor = false;
    searchShouldRestoreFocus = false;
    if (!shouldRestore) return;
    if (focusEditor) {
      if (previewing) togglePreview();
      view.focus();
    } else if (isVisibleFocusTarget(restoreTarget)) {
      restoreTarget.focus({ preventScroll: true });
    } else if (previewing) {
      preview.tabIndex = -1;
      preview.focus({ preventScroll: true });
    } else {
      view.focus();
    }
  }

  function closeSearchDialog({ restoreFocus = true, focusEditor = false } = {}) {
    if (!searchDialog.open) return;
    searchShouldRestoreFocus = restoreFocus;
    searchRestoreToEditor = focusEditor;
    searchDialog.close();
  }

  function openSearchDialog() {
    if (searchDialog.open || dirtyDialog.open) return;
    searchRestoreTarget = document.activeElement;
    searchShouldRestoreFocus = true;
    searchRestoreToEditor = false;
    searchInput.value = activeSearchTerm;
    searchStatus.textContent = '';
    searchDialog.showModal();
    searchInput.focus();
    searchInput.select();
  }

  function clearSearchState({ closeDialog = false } = {}) {
    activeSearchTerm = '';
    if (buffers.active()) { buffers.active().searchTerm = ''; scheduleRecovery(buffers.active()); }
    searchInput.value = '';
    searchStatus.textContent = '';
    if (view) {
      view.dispatch({ effects: setSearchQuery.of(new SearchQuery({ search: '', caseSensitive: false, literal: true })) });
      closeSearchPanel(view);
    }
    if (closeDialog) closeSearchDialog({ restoreFocus: false });
  }

  function setActiveSearch(term) {
    activeSearchTerm = term;
    if (buffers.active()) { buffers.active().searchTerm = term; scheduleRecovery(buffers.active()); }
    // The official highlighter is enabled by an open search panel. This panel
    // is deliberately empty—the accessible application dialog owns the UI.
    openSearchPanel(view);
    view.dispatch({
      effects: setSearchQuery.of(new SearchQuery({ search: term, caseSensitive: false, literal: true, regexp: false })),
    });
  }

  function navigateSearch(command) {
    if (!activeSearchTerm) return false;
    // Commands provide native query matching, wrapping, selection, and match
    // announcements. Run before leaving preview so a no-match key is harmless.
    if (!command(view)) return false;
    if (previewing) {
      togglePreview();
      view.dispatch({ effects: EditorView.scrollIntoView(view.state.selection.main, { y: 'center' }) });
    } else {
      view.focus();
    }
    return true;
  }

  function submitSearch() {
    const term = searchInput.value;
    if (!term) {
      clearSearchState();
      closeSearchDialog();
      return;
    }
    setActiveSearch(term);
    // Enter always returns to source editing. It selects the first native
    // result at/after the caret (wrapping), or leaves the document untouched.
    if (previewing) togglePreview();
    if (!findNext(view)) searchStatus.textContent = 'No matches in this document.';
    else searchStatus.textContent = '';
    closeSearchDialog({ focusEditor: true });
  }

  function focusableSearchControls() {
    return [...searchDialog.querySelectorAll('button:not([disabled]), input:not([disabled])')]
      .filter((element) => isVisibleFocusTarget(element));
  }

  searchDialog.addEventListener('cancel', (event) => {
    event.preventDefault();
    closeSearchDialog();
  });
  searchDialog.addEventListener('close', restoreSearchFocus);
  searchForm.addEventListener('submit', (event) => {
    event.preventDefault();
    submitSearch();
  });
  searchClose.addEventListener('click', () => closeSearchDialog());
  searchDialog.addEventListener('keydown', (event) => {
    if (event.key !== 'Tab') return;
    const controls = focusableSearchControls();
    if (!controls.length) return;
    const first = controls[0];
    const last = controls[controls.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  });

  function sendInitialAnalysis(document) {
    sourceVersion += 1;
    analysisVersion = -1;
    worker.postMessage({ type: 'init', activation: activationGeneration, version: sourceVersion, document });
  }

  function handleUpdate(update) {
    if (!update.docChanged) return;
    const started = benchmarkMode ? performance.now() : 0;
    contentVersion += 1;
    dirty = true;
    const entry = buffers.active();
    if (entry) {
      entry.state = update.state;
      buffers.markEdit(entry);
      entry.dirty = true;
      entry.previewing = previewing;
    }
    sourceVersion += 1;
    const changes = [];
    update.changes.iterChanges((from, to, fromB, toB, inserted) => changes.push({ from, to, fromB, toB, inserted: inserted.toString() }));
    // This is intentionally synchronous and document-local: worker analysis is
    // delayed, but activity belongs to this exact CodeMirror transaction.
    const delta = classifyWordActivity({
      beforeLength: update.startState.doc.length,
      afterLength: update.state.doc.length,
      readBefore: (from, to) => update.startState.sliceDoc(from, to),
      readAfter: (from, to) => update.state.sliceDoc(from, to),
      changes,
    });
    dailyActivity.record(delta);
    if (entry) entry.activity = dailyActivity.snapshot();
    updateActivityMetric();
    worker.postMessage({ type: 'changes', activation: activationGeneration, version: sourceVersion, changes });
    if (entry) scheduleRecovery(entry);
    if (benchmarkMode) updateSamples.push(performance.now() - started);
  }

  function createState(document, selection = null) {
    return EditorState.create({
      doc: document,
      selection: selection || undefined,
      extensions: [
        history(),
        ...(appConfig.editor.autoClosePairs ? [
          EditorState.languageData.of(() => [{ closeBrackets: { brackets: AUTO_CLOSE_BRACKETS } }]),
          closeBrackets(),
          keymap.of(closeBracketsKeymap),
        ] : []),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        markdown(),
        search({ createPanel: createSearchDecorationPanel }),
        EditorView.lineWrapping,
        editorTheme(),
        EditorView.updateListener.of(handleUpdate),
      ],
    });
  }

  function captureActiveBuffer() {
    const entry = buffers.active();
    if (!entry || !view) return;
    entry.state = view.state;
    entry.sourceScroll = { top: view.scrollDOM.scrollTop, left: view.scrollDOM.scrollLeft };
    entry.selection = { anchor: view.state.selection.main.anchor, head: view.state.selection.main.head };
    entry.previewScroll = preview.scrollTop;
    entry.previewing = previewing;
    entry.searchTerm = activeSearchTerm;
    entry.chapterIdentity = selectedChapterIdentity;
    entry.chapterScroll = chapterList.scrollTop;
    entry.collapsedChapterIdentities = [...collapsedChapterIdentities].sort();
    entry.headings = chapters;
    entry.total = latestTotal;
    entry.activity = dailyActivity.snapshot();
  }

  async function persistRecovery(entry, sequence, binding) {
    if (!entry || !binding?.projectId || !buffers.matchesBinding(entry, binding)) return;
    const current = entry === buffers.active();
    const state = current ? view.state : entry.state;
    return invoke('write_recovery_buffer', {
      projectId: binding.projectId,
      buffer: {
        relativePath: entry.relativePath, documentId: entry.documentId, writeRevision: entry.writeRevision, recoverySequence: sequence, dirty: entry.dirty,
        content: entry.dirty ? state.doc.toString() : null, baselineHash: entry.baselineHash,
        sourceScroll: entry.sourceScroll, selection: entry.selection, previewScroll: entry.previewScroll, previewing: entry.previewing,
        searchTerm: entry.searchTerm, chapterIdentity: entry.chapterIdentity, chapterScroll: entry.chapterScroll,
        collapsedChapterIdentities: entry.collapsedChapterIdentities,
        activity: entry.activity,
      },
    });
  }
  function scheduleRecovery(entry) {
    if (!window.__TAURI__?.core) return;
    buffers.scheduleRecovery(entry, persistRecovery);
  }
  // Project-file writes alone reach this notifier. Recovery snapshots deliberately
  // bypass it, and bindings reject stale or transitioned buffers.
  function notifySuccessfulProjectWrite(entry, snapshot) {
    if (!buffers.matchesBinding(entry, snapshot)) return false;
    saveIndicator.show();
    return true;
  }
  async function flushRecovery() {
    if (buffers.persistenceSuppressed) return;
    captureActiveBuffer();
    await buffers.flushRecovery(persistRecovery);
    const project = buffers.project;
    if (project && !buffers.persistenceSuppressed) await invoke('write_recovery_manifest', { projectId: project.projectId, activePath: buffers.activePath });
  }

  function activateBuffer(entry) {
    if (buffers.active() !== entry) captureActiveBuffer();
    buffers.activate(entry.relativePath);
    activationGeneration += 1;
    documentGeneration += 1;
    currentRelativePath = entry.relativePath;
    currentPath = currentProject && entry.relativePath ? `${currentProject.root}/${entry.relativePath}` : null;
    currentDocumentId = entry.documentId;
    currentWriteRevision = entry.writeRevision;
    dirty = entry.dirty;
    lastSavedText = entry.lastSavedText;
    contentVersion = entry.editSequence;
    previewing = entry.previewing;
    activeSearchTerm = entry.searchTerm;
    selectedChapterIdentity = entry.chapterIdentity;
    focusedChapterIdentity = entry.chapterIdentity;
    collapsedChapterIdentities = new Set(entry.collapsedChapterIdentities || []);
    chapters = entry.headings || [];
    latestTotal = entry.total || 0;
    dailyActivity.setIdentity(currentPath);
    if (entry.activity) dailyActivity.restore(entry.activity);
    view.setState(entry.state);
    documentShell.classList.toggle('previewing', previewing); preview.hidden = !previewing; editorHost.hidden = previewing;
    patchChapters(chapters); updateMetrics(latestTotal);
    selectedTreePath = entry.relativePath; patchTree();
    requestFrame(() => { view.scrollDOM.scrollTop = entry.sourceScroll?.top || 0; view.scrollDOM.scrollLeft = entry.sourceScroll?.left || 0; chapterList.scrollTop = entry.chapterScroll || 0; });
    if (activeSearchTerm) setActiveSearch(activeSearchTerm); else clearSearchState();
    sendInitialAnalysis(view.state.doc.toString());
    if (currentProject && entry.documentId) void invoke('select_project_document', { projectId: currentProject.projectId, documentId: entry.documentId }).catch(() => {});
    if (previewing) { void renderPreview(); requestFrame(() => { preview.scrollTop = entry.previewScroll || 0; }); }
    if (currentProject && entry.documentId) void invoke('revalidate_project_document', { projectId: currentProject.projectId, documentId: entry.documentId }).then((status) => { if (buffers.active() === entry && status.conflict) { entry.conflict = status.conflict; dirty = true; console.error(status.conflict); } }).catch(() => {});
    scheduleRecovery(entry);
  }

  view = new EditorView({ state: createState(''), parent: editorHost });
  view.scrollDOM.addEventListener('scroll', () => { const entry = buffers.active(); if (entry) { entry.sourceScroll = { top: view.scrollDOM.scrollTop, left: view.scrollDOM.scrollLeft }; scheduleRecovery(entry); } }, { passive: true });
  preview.addEventListener('scroll', () => { const entry = buffers.active(); if (entry) { entry.previewScroll = preview.scrollTop; scheduleRecovery(entry); } }, { passive: true });
  chapterList.addEventListener('scroll', () => { const entry = buffers.active(); if (entry) { entry.chapterScroll = chapterList.scrollTop; scheduleRecovery(entry); } }, { passive: true });
  worker.onmessage = ({ data }) => {
    if (data.type !== 'analysis' || data.activation !== activationGeneration || data.version !== sourceVersion) return;
    analysisVersion = data.version;
    const previousChapters = chapters;
    chapters = data.headings;
    const entry = buffers.active();
    collapsedChapterIdentities = new Set(reconcileCollapsedChapterIdentities(previousChapters, chapters, collapsedChapterIdentities));
    if (entry) {
      entry.headings = data.headings; entry.total = data.total; entry.analysisVersion = data.version;
      entry.collapsedChapterIdentities = [...collapsedChapterIdentities];
      scheduleRecovery(entry);
    }
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
    clearSearchState({ closeDialog: true });
    buffers.setProject(project);
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

  function replaceDocument(document, { relativePath = null, documentId = null, writeRevision = 0, baselineHash = null, clean = false, restored = {} } = {}) {
    clearSearchState({ closeDialog: true });
    const safeRestored = { ...restored, selection: clampRestoredSelection(restored.selection, document) };
    const existing = buffers.get(relativePath);
    if (existing && existing.state.doc.toString() === document) { activateBuffer(existing); return; }
    if (existing) {
      existing.state = createState(document, safeRestored.selection);
      existing.documentId = documentId;
      existing.writeRevision = writeRevision;
      existing.baselineHash = baselineHash;
      existing.lastSavedText = clean ? document : existing.lastSavedText;
      existing.dirty = !clean || Boolean(safeRestored.dirty);
      existing.searchTerm = ''; existing.headings = []; existing.total = 0;
      activateBuffer(existing);
      return;
    }
    const entry = buffers.add({ relativePath, documentId, writeRevision, baselineHash, content: document }, createState(document, safeRestored.selection), { ...safeRestored, dirty: !clean || safeRestored.dirty });
    entry.lastSavedText = clean ? document : entry.lastSavedText;
    entry.dirty = !clean || Boolean(safeRestored.dirty);
    activateBuffer(entry);
  }

  function applyDocument(document) {
    if (!document || typeof document.content !== 'string' || !Number.isSafeInteger(document.documentId)) return false;
    const existing = buffers.get(document.relativePath);
    if (existing) activateBuffer(existing);
    else replaceDocument(document.content, { relativePath: document.relativePath, documentId: document.documentId, writeRevision: document.writeRevision || 0, baselineHash: document.baselineHash, clean: !document.conflict, restored: { dirty: Boolean(document.conflict), conflict: document.conflict } });
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
    const chapter = nearestVisibleChapter(selectedChapterIdentity);
    focusChapterCard(chapter && chapterRows.get(chapter.identity));
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
    return new Promise((resolve) => {
      const close = () => { dirtyDialog.removeEventListener('close', close); resolve(dirtyDialog.returnValue || 'cancel'); };
      dirtyDialog.addEventListener('close', close, { once: true }); dirtyDialog.showModal();
    });
  }

  async function saveDocument() {
    if (!currentProject) { console.info('Open a project before saving.'); return false; }
    return saveBuffer(buffers.active(), true);
  }
  async function saveBuffer(entry, allowSaveAs = false) {
    if (!currentProject || !entry || entry.conflict) { if (entry?.conflict) console.error(entry.conflict); return false; }
    captureActiveBuffer();
    const isActive = entry === buffers.active();
    const snapshot = buffers.writeSnapshot(entry); const savingUntitled = entry.documentId === null;
    if (savingUntitled && !allowSaveAs) return false;
    try {
      const saved = savingUntitled
        ? await invoke('save_document_as', { projectId: currentProject.projectId, revision: snapshot.revision, content: snapshot.content })
        : await invoke('save_document', { projectId: currentProject.projectId, documentId: entry.documentId, revision: snapshot.revision, content: snapshot.content });
      if (!saved || !notifySuccessfulProjectWrite(entry, snapshot)) return false;
      entry.documentId = saved.documentId; entry.relativePath = saved.relativePath; entry.baselineHash = saved.baselineHash;
      if (savingUntitled) { buffers.entries.delete('__untitled__'); buffers.entries.set(entry.relativePath, entry); entry.key = entry.relativePath; if (isActive) buffers.activePath = entry.relativePath; }
      buffers.applyWriteAck(entry, snapshot, saved);
      if (isActive) { currentRelativePath = entry.relativePath; currentPath = `${currentProject.root}/${entry.relativePath}`; currentDocumentId = entry.documentId; currentWriteRevision = entry.writeRevision; dirty = entry.dirty; if (savingUntitled) dailyActivity.adoptIdentity(currentPath); }
      if (treeNodes.has('')) { treeNodes.get('').loaded = false; await loadDirectory(''); }
      scheduleRecovery(entry); await flushRecovery();
      return true;
    } catch (error) { if (/CONFLICT/.test(String(error))) buffers.markConflict(entry, error); console.error('Could not save document:', error); return false; }
  }

  async function allowTransition() {
    const dirtyEntries = [...buffers.entries.values()].filter((entry) => entry.dirty);
    if (!dirtyEntries.length) return 'clean';
    const action = await askDirty();
    if (action === 'cancel') return 'cancel';
    if (action === 'discard') return 'discard';
    for (const entry of dirtyEntries) if (!await saveBuffer(entry, entry === buffers.active())) return 'failed';
    return 'saved';
  }

  async function requestOpenDocument(relativePath) {
    if (!currentProject || transitionInFlight || relativePath === currentRelativePath) return;
    const cached = buffers.get(relativePath);
    if (cached) { activateBuffer(cached); return; }
    const request = ++selectedOpenRequest;
    try {
      const projectId = currentProject.projectId;
      const document = await invoke('open_project_document', { projectId, relativePath });
      if (currentProject?.projectId === projectId && request === selectedOpenRequest) applyDocument(document);
    } catch (error) { console.error('Could not open project document:', error); }
  }

  async function openProject() {
    if (transitionInFlight) return;
    transitionInFlight = true;
    let autosaveQuiesced = false;
    try {
      // Picking is side-effect free: picker cancellation and Save All failure
      // retain the old registry, buffers, and recovery data.
      const candidate = await invoke('pick_project');
      if (!candidate) return;
      const decision = await allowTransition();
      if (decision === 'cancel' || decision === 'failed') return;
      // Freeze the old session before activation.  This waits out old writes,
      // snapshots every buffer under its immutable old project binding, and
      // leaves that recovery set intact if candidate activation fails.
      await autosave.quiesce();
      autosaveQuiesced = true;
      await buffers.drainRecovery();
      await flushRecovery();
      const project = await invoke('activate_picked_project', { candidateId: candidate.candidateId, discardPreviousRecovery: decision === 'discard' });
      setProject(project); replaceDocument(DEFAULT_DOCUMENT, { clean: true });
      autosave.resume();
      autosaveQuiesced = false;
      treeNodes.set('', { entry: { kind: 'directory', name: '' }, loaded: false, expanded: true, children: [] });
      await loadDirectory('');
    } catch (error) { console.error('Could not open project:', error); } finally { if (autosaveQuiesced) autosave.resume(); transitionInFlight = false; }
  }

  async function restoreLastProject() {
    try {
      const restored = await invoke('restore_last_project');
      if (!restored?.project) return false;
      setProject(restored.project);
      const documents = restored.documents || (restored.document ? [restored.document] : []);
      for (const document of documents) replaceDocument(document.content, { relativePath: document.relativePath, documentId: document.documentId, writeRevision: document.writeRevision || 0, baselineHash: document.baselineHash, clean: !document.recovery?.dirty && !document.conflict, restored: { ...(document.recovery || {}), dirty: Boolean(document.recovery?.dirty || document.conflict), conflict: document.conflict } });
      const active = buffers.get(restored.activeRelativePath) || buffers.active();
      if (active) activateBuffer(active); else replaceDocument(DEFAULT_DOCUMENT, { clean: true });
      if (active) selectedTreePath = active.relativePath;
      treeNodes.set('', { entry: { kind: 'directory', name: '' }, loaded: false, expanded: true, children: [] });
      await loadDirectory('');
      return true;
    } catch (error) { console.error('Could not restore project:', error); return false; }
  }

  const autosave = createAutosaveController({
    snapshot() {
      captureActiveBuffer();
      if (!buffers.project || buffers.persistenceSuppressed) return [];
      return buffers.autosaveEligible().map((entry) => {
        const snapshot = buffers.writeSnapshot(entry);
        return snapshot && { entry, ...snapshot };
      }).filter(Boolean);
    },
    write(saved) {
      if (!buffers.matchesBinding(saved.entry, saved)) throw new Error('Stale autosave session');
      return invoke('autosave_document', { projectId: saved.projectId, documentId: saved.documentId, revision: saved.revision, content: saved.content });
    },
    onSuccess(saved, result) {
      if (!notifySuccessfulProjectWrite(saved.entry, saved)) return;
      buffers.applyWriteAck(saved.entry, saved, result);
      if (saved.entry === buffers.active()) { dirty = saved.entry.dirty; currentWriteRevision = saved.entry.writeRevision; }
      scheduleRecovery(saved.entry);
    },
    onError(error, saved) {
      if (saved && buffers.matchesBinding(saved.entry, saved) && /CONFLICT/.test(String(error))) buffers.markConflict(saved.entry, error);
      if (!/Stale autosave session/.test(String(error))) console.error('Could not autosave document:', error);
    }, setIntervalFn: window.setInterval.bind(window), clearIntervalFn: window.clearInterval.bind(window),
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
    const plainFunctionKey = !event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey;
    if (searchDialog.open) {
      if (plainFunctionKey && (event.key === 'F5' || event.key === 'Escape')) {
        event.preventDefault();
        closeSearchDialog();
      } else if (plainFunctionKey && /^F[1-7]$/.test(event.key)) {
        event.preventDefault();
      }
      return;
    }
    if (dirtyDialog.open) return;
    if (plainFunctionKey && event.key === 'F5') {
      event.preventDefault();
      openSearchDialog();
      return;
    }
    if (plainFunctionKey && (event.key === 'F6' || event.key === 'F7')) {
      event.preventDefault();
      navigateSearch(event.key === 'F6' ? findPrevious : findNext);
      return;
    }
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
  async function handleNativeClose(requestId) {
    let closed = false;
    let closingDiscard = false;
    let terminalDiscardProjectId = null;
    const restoreDiscardRecovery = async () => {
      closingDiscard = false;
      buffers.resumePersistence();
      autosave.resume();
      try { await flushRecovery(); } catch (recoveryError) { console.error('Could not restore recovery after close failure:', recoveryError); }
    };
    try {
      await runCloseFlow({
        hasDirty: () => [...buffers.entries.values()].some((entry) => entry.dirty),
        decide: askDirty,
        saveAll: async () => { for (const entry of [...buffers.entries.values()].filter((item) => item.dirty)) if (!await saveBuffer(entry, entry === buffers.active())) return false; return true; },
        discardAll: async () => {
          // This terminal barrier prevents an already-resolved write callback
          // or beforeunload from recreating the recovery set after deletion.
          closingDiscard = true;
          buffers.suppressPersistence();
          try {
            await autosave.quiesce();
            await buffers.drainRecovery();
            terminalDiscardProjectId = currentProject?.projectId ?? null;
            return true;
          } catch (error) {
            // Keep the window open and restore recoverability if deletion was
            // rejected; the discard was not final.
            await restoreDiscardRecovery();
            console.error('Could not discard recovery:', error);
            return false;
          }
        },
        flush: async () => { if (!closingDiscard) await flushRecovery(); return true; },
        close: () => terminalDiscardProjectId === null
          ? invoke('complete_close')
          : invoke('discard_recovery_and_exit', { projectId: terminalDiscardProjectId }),
      }).then(async (outcome) => {
        if (outcome === 'closed') { closed = true; return; }
        if (terminalDiscardProjectId !== null) await restoreDiscardRecovery();
        console.info(`Window close retained: ${outcome}`);
      });
    } catch (error) { console.error('Could not complete window close:', error); }
    finally {
      // Cancel/save failures reopen the native close gate. A later Super+W
      // therefore receives a fresh event instead of being prevented forever.
      if (!closed) {
        try { await invoke('finish_close_request', { requestId }); } catch (error) { console.error('Could not finish window-close request:', error); }
      }
    }
  }
  const nativeClose = createCloseRequestGuard(handleNativeClose);
  async function installNativeCloseListener() {
    const events = window.__TAURI__?.event;
    if (!events?.listen) return false;
    try {
      const unlisten = await events.listen('amte://close-requested', async ({ payload }) => {
        const requestId = Number(payload);
        if (!Number.isSafeInteger(requestId)) return;
        try {
          if (await invoke('acknowledge_close_request', { requestId })) await nativeClose.request(requestId);
        } catch (error) { console.error('Could not acknowledge window-close request:', error); }
      });
      try {
        await invoke('close_listener_ready');
        return unlisten;
      } catch (error) {
        unlisten?.();
        throw error;
      }
    } catch (error) {
      // Rust remains fail-open until close_listener_ready succeeds, so this
      // cannot leave a broken frontend window permanently uncloseable.
      console.error('Could not install window-close listener:', error);
      return false;
    }
  }
  // Tauri v2 intercepts titlebar/window-manager CloseRequested in Rust only
  // after this listener has acknowledged installation.
  await installNativeCloseListener();
  window.addEventListener('beforeunload', () => { saveIndicator.destroy(); autosave.stop(); void flushRecovery(); }, { once: true });
  const app = {
    view,
    worker,
    buffers,
    get sourceVersion() { return sourceVersion; },
    get analysisVersion() { return analysisVersion; },
    togglePreview,
    replaceDocument,
    // CDP coverage uses this in benchmark mode to exercise file-scoped
    // storage without granting the browser test any filesystem capability.
    benchmarkSwitchDocument(project, relativePath, content) {
      if (!browserCheckMode) return;
      if (currentProject?.projectId !== project.projectId) setProject(project);
      const cached = buffers.get(relativePath);
      if (cached) activateBuffer(cached);
      else replaceDocument(content, { relativePath, documentId: 1, clean: true });
    },
    async browserCheckSave(kind) {
      if (!browserCheckMode) return false;
      const entry = buffers.active();
      if (!entry) return false;
      entry.dirty = true;
      entry.editSequence += 1;
      if (kind === 'autosave') { await autosave.tick(); return true; }
      if (kind === 'save-as') {
        const untitled = buffers.add({ content: entry.state.doc.toString() }, createState(entry.state.doc.toString()));
        buffers.activate(null);
        return saveBuffer(untitled, true);
      }
      if (kind === 'save-all') {
        for (const candidate of [...buffers.entries.values()].filter((item) => item.dirty)) if (!await saveBuffer(candidate, candidate === buffers.active())) return false;
        return true;
      }
      return saveBuffer(entry, false);
    },
  };
  if (benchmarkMode) void runBrowserBenchmark(app, updateSamples, editorHost);
  return app;
}

async function runBrowserBenchmark(app, updateSamples, editorHost) {
  const source = (`# Benchmark\n\n${'alpha beta gamma delta epsilon zeta eta theta iota kappa\n'.repeat(10000)}`).slice(0, 400 * 1024);
  const bufferPaths = Array.from({ length: 10 }, (_, index) => `benchmark-${index}.md`);
  for (const path of bufferPaths) app.benchmarkSwitchDocument({ projectId: 99, root: '/benchmark-project' }, path, source);
  const switchSamples = [];
  for (const path of [...bufferPaths, ...bufferPaths].reverse()) {
    const started = performance.now();
    app.benchmarkSwitchDocument({ projectId: 99, root: '/benchmark-project' }, path, source);
    switchSamples.push(performance.now() - started);
  }
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
    buffers: [...app.buffers.entries.values()].filter((entry) => entry.relativePath).length,
    editorViews: editorHost.querySelectorAll('.cm-editor').length,
    bufferSwitchP95Ms: Number([...switchSamples].sort((left, right) => left - right)[Math.ceil(switchSamples.length * 0.95) - 1].toFixed(3)),
    approximateHeapBytes: performance.memory?.usedJSHeapSize ?? null,
  };
  document.body.dataset.amteBenchmark = JSON.stringify(result);
  const output = document.createElement('pre');
  output.id = 'benchmark-result';
  output.textContent = JSON.stringify(result);
  document.body.append(output);
}

window.__amteReady = start();

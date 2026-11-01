export const SAVE_INDICATOR_DURATION_MS = 5000;

export const SAVE_STATUS = Object.freeze({
  dirty: { priority: 20, text: 'Unsaved changes', icon: '●' },
  recoveryPending: { priority: 30, text: 'Protecting draft…', icon: '◌' },
  recoveryDurable: { priority: 40, text: 'Draft protected', icon: '◉' },
  fileSaving: { priority: 50, text: 'Saving file…', icon: '◌' },
  fileSaved: { priority: 60, text: 'File saved', icon: '✓', transient: true },
  reconciliationPending: { priority: 80, text: 'Arquivo salvo; recuperação pendente', icon: '!' },
  reconciliationError: { priority: 90, text: 'Arquivo salvo; recuperação com erro', icon: '!' },
  error: { priority: 100, text: 'Recovery or save error', icon: '!' },
  conflict: { priority: 110, text: 'File conflict needs attention', icon: '!' },
});

export function reduceSaveStatus(state = {}) {
  return Object.entries(state).filter(([, enabled]) => Boolean(enabled))
    .map(([name]) => ({ name, ...SAVE_STATUS[name] })).filter((value) => value.priority !== undefined)
    .sort((a, b) => b.priority - a.priority)[0] || null;
}

/** Fixed-size footer slot; only content changes, never footer geometry. */
export function createSaveIndicatorController({ element, setTimeoutFn = globalThis.setTimeout, clearTimeoutFn = globalThis.clearTimeout, duration = SAVE_INDICATOR_DURATION_MS }) {
  let timer = null; let destroyed = false; let current = null; let fileSavedActive = false; let latestState = {};
  const clear = () => { if (timer !== null) { clearTimeoutFn(timer); timer = null; } };
  const render = (status) => {
    current = status; element.hidden = false;
    if ('dataset' in element) element.dataset.status = status?.name || 'idle';
    if ('title' in element) element.title = status?.text || 'No save activity';
    element.setAttribute?.('aria-label', status?.text || 'No save activity');
    const icon = element.querySelector?.('[data-save-icon]'); if (icon) icon.textContent = status?.icon || '·';
    const label = element.querySelector?.('[data-save-label]'); if (label) label.textContent = status?.text || 'No save activity';
  };
  return {
    update(state) {
      if (destroyed) return null;
      latestState = { ...state };
      // Background recovery acknowledgements must not cancel the accepted
      // file-save confirmation. Only a persistent error/conflict supersedes it.
      if (state.error || state.conflict) { fileSavedActive = false; clear(); }
      if (state.fileSaved) { fileSavedActive = true; clear(); }
      const status = reduceSaveStatus({ ...latestState, fileSaved: fileSavedActive });
      render(status);
      if (fileSavedActive && timer === null) timer = setTimeoutFn(() => {
        timer = null; if (destroyed) return;
        fileSavedActive = false;
        const fallback = reduceSaveStatus({ ...latestState, fileSaved: false });
        if (fallback) render(fallback); else { current = null; element.hidden = true; }
      }, duration);
      return status?.name || null;
    },
    show() { return this.update({ fileSaved: true }); },
    hide() { clear(); fileSavedActive = false; latestState = {}; element.hidden = true; current = null; },
    get status() { return current?.name || null; },
    destroy() { destroyed = true; this.hide(); },
  };
}

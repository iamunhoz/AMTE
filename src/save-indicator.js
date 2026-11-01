export const SAVE_INDICATOR_DURATION_MS = 5000;

export const SAVE_STATUS = Object.freeze({
  dirty: { priority: 20, text: 'Alterações não salvas', icon: '●' },
  recoveryPending: { priority: 30, text: 'Protegendo rascunho…', icon: '◌' },
  recoveryDurable: { priority: 40, text: 'Rascunho protegido', icon: '◉' },
  fileSaving: { priority: 50, text: 'Salvando arquivo…', icon: '◌' },
  fileSaved: { priority: 60, text: 'Arquivo salvo', icon: '✓', transient: true },
  fileSaveError: { priority: 100, text: 'Falha ao salvar arquivo', icon: '!' },
  recoveryError: { priority: 101, text: 'Falha na recuperação do rascunho', icon: '!' },
  // A committed file is never a total save failure. Keep reconciliation above
  // residual per-channel errors until its durable outcome is resolved.
  reconciliationPending: { priority: 105, text: 'Arquivo salvo; recuperação pendente', icon: '!' },
  reconciliationError: { priority: 106, text: 'Arquivo salvo; recuperação com erro', icon: '!' },
  conflict: { priority: 110, text: 'Conflito no arquivo requer atenção', icon: '!' },
});

export function reduceSaveStatus(state = {}) {
  return Object.entries(state).filter(([, enabled]) => Boolean(enabled))
    .map(([name]) => ({ name, ...SAVE_STATUS[name] })).filter((value) => value.priority !== undefined)
    .sort((a, b) => b.priority - a.priority)[0] || null;
}

/** Fixed-size footer slot; only content changes, never footer geometry. */
export function createSaveIndicatorController({ element, setTimeoutFn = globalThis.setTimeout, clearTimeoutFn = globalThis.clearTimeout, duration = SAVE_INDICATOR_DURATION_MS }) {
  let timer = null; let destroyed = false; let current = null; let fileSavedActive = false; let persistentState = {};
  const clear = () => { if (timer !== null) { clearTimeoutFn(timer); timer = null; } };
  const render = () => {
    const persistent = reduceSaveStatus(persistentState);
    const status = persistent || (fileSavedActive ? { name: 'fileSaved', ...SAVE_STATUS.fileSaved } : null);
    current = status;
    if (!status) {
      element.hidden = true;
      if ('dataset' in element) { element.dataset.status = 'idle'; element.dataset.savePulse = 'false'; }
      if ('title' in element) element.title = 'No save activity';
      element.setAttribute?.('aria-label', 'No save activity');
      return;
    }
    element.hidden = false;
    if ('dataset' in element) {
      element.dataset.status = status.name;
      element.dataset.savePulse = fileSavedActive ? 'true' : 'false';
    }
    if ('title' in element) element.title = status.text;
    element.setAttribute?.('aria-label', status.text);
    // The pulse is global visual confirmation only. Persistent text and ARIA
    // always describe the active buffer, even when an inactive save succeeds.
    const icon = element.querySelector?.('[data-save-icon]'); if (icon) icon.textContent = fileSavedActive ? SAVE_STATUS.fileSaved.icon : status.icon;
    const label = element.querySelector?.('[data-save-label]'); if (label) label.textContent = status.text;
  };
  const pulse = () => {
    if (destroyed) return null;
    fileSavedActive = true;
    clear();
    render();
    timer = setTimeoutFn(() => {
      timer = null; if (destroyed) return;
      fileSavedActive = false;
      render();
    }, duration);
    return current?.name || null;
  };
  return {
    update(state = {}) {
      if (destroyed) return null;
      const { fileSaved, ...nextPersistent } = state;
      persistentState = nextPersistent;
      if (fileSaved) return pulse();
      render();
      return current?.name || null;
    },
    show() { return pulse(); },
    hide() { clear(); fileSavedActive = false; persistentState = {}; element.hidden = true; current = null; },
    get status() { return current?.name || null; },
    destroy() { destroyed = true; this.hide(); },
  };
}

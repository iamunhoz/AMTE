export const SAVE_INDICATOR_DURATION_MS = 5000;

/** Controls the transient, non-interactive confirmation shown after a project write. */
export function createSaveIndicatorController({ element, setTimeoutFn = globalThis.setTimeout, clearTimeoutFn = globalThis.clearTimeout, duration = SAVE_INDICATOR_DURATION_MS }) {
  let timer = null;
  let destroyed = false;

  function hide() {
    if (timer !== null) {
      clearTimeoutFn(timer);
      timer = null;
    }
    element.hidden = true;
  }

  return {
    show() {
      if (destroyed) return;
      if (timer !== null) clearTimeoutFn(timer);
      element.hidden = false;
      timer = setTimeoutFn(() => {
        timer = null;
        element.hidden = true;
      }, duration);
    },
    hide,
    destroy() {
      destroyed = true;
      hide();
    },
  };
}

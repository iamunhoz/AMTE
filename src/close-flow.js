/** Serializes the native CloseRequested decision without relying on beforeunload. */
export async function runCloseFlow({ hasDirty, decide, saveAll, discardAll, flush, close }) {
  if (hasDirty()) {
    const action = await decide();
    if (action === 'cancel') return 'cancel';
    if (action === 'save' && !await saveAll()) return 'save-failed';
    if (action === 'discard' && !await discardAll()) return 'discard-failed';
  }
  if (!await flush()) return 'flush-failed';
  try { await close(); } catch { return 'close-failed'; }
  return 'closed';
}

/** Coalesces duplicate compositor close events while one decision is visible. */
export function createCloseRequestGuard(handle) {
  let inFlight = null;
  return {
    request(requestId) {
      if (inFlight) return inFlight;
      inFlight = Promise.resolve().then(() => handle(requestId)).finally(() => { inFlight = null; });
      return inFlight;
    },
    get inFlight() { return inFlight !== null; },
  };
}

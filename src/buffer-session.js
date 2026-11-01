/**
 * Runtime-only document session.  EditorState deliberately remains in memory:
 * CodeMirror history is valid for a running process but is not serializable.
 */
export const RECOVERY_SEQUENCE_MAX = Number.MAX_SAFE_INTEGER;

// `recoverySequence` is the wire and in-memory contract.  The Rust backend
// used `sequence` in version-one manifests and responses, so accept that
// persisted/older response shape only while restoring it.
export function recoverySequenceFrom(restored = {}) {
  const hasCanonical = Object.hasOwn(restored, 'recoverySequence');
  const hasLegacy = Object.hasOwn(restored, 'sequence');
  const value = hasCanonical ? restored.recoverySequence : (hasLegacy ? restored.sequence : 0);
  if (!Number.isSafeInteger(value) || value < 0 || value > RECOVERY_SEQUENCE_MAX
    || (hasCanonical && hasLegacy && restored.recoverySequence !== restored.sequence)) {
    throw new RangeError('Recovery sequence must be one unambiguous safe nonnegative integer');
  }
  return value;
}

// CodeMirror document positions are UTF-16 code units, matching JavaScript
// string length rather than Rust's UTF-8 byte count. Backend validation is the
// authority; this is a final defensive clamp before EditorState construction.
export function clampRestoredSelection(selection, content) {
  if (!selection || !Number.isSafeInteger(selection.anchor) || !Number.isSafeInteger(selection.head)) return null;
  const maximum = String(content ?? '').length;
  return {
    anchor: Math.max(0, Math.min(selection.anchor, maximum)),
    head: Math.max(0, Math.min(selection.head, maximum)),
  };
}

export class BufferSession {
  constructor({ project = null, schedule = (...args) => globalThis.setTimeout(...args), cancel = (...args) => globalThis.clearTimeout(...args), recoveryDelay = 750, recoveryMaxDelay = 5000 } = {}) {
    this.project = project;
    this.entries = new Map();
    this.activePath = null;
    this.schedule = schedule;
    this.cancel = cancel;
    this.recoveryDelay = recoveryDelay;
    this.recoveryMaxDelay = recoveryMaxDelay;
    this.manifestDirty = false;
    this.persistenceSuppressed = false;
    this.generation = 0;
    this.recoveryWrites = new Set();
  }

  setProject(project) {
    this.cancelRecovery();
    this.generation += 1;
    this.persistenceSuppressed = false;
    this.project = project;
    this.entries.clear();
    this.activePath = null;
    this.manifestDirty = true;
  }
  cancelRecovery() {
    for (const entry of this.entries.values()) {
      if (entry.recoveryTimer !== null) this.cancel(entry.recoveryTimer);
      entry.recoveryTimer = null;
      entry.recoveryPending = false;
    }
  }
  suppressPersistence() { this.persistenceSuppressed = true; this.cancelRecovery(); }
  resumePersistence() { this.persistenceSuppressed = false; }
  async drainRecovery() {
    this.cancelRecovery();
    while (this.recoveryWrites.size) await Promise.allSettled([...this.recoveryWrites]);
  }

  add(document, state, restored = {}) {
    const path = document.relativePath || null;
    const key = path || '__untitled__';
    const existing = this.entries.get(key);
    if (existing) return existing;
    const entry = {
      key, relativePath: path, state,
      documentId: document.documentId ?? null,
      writeRevision: document.writeRevision || 0,
      baselineHash: document.baselineHash || null,
      lastSavedText: document.content ?? '', dirty: Boolean(restored.dirty),
      editSequence: 0, nextRevision: document.writeRevision || 0,
      saving: false, conflict: restored.conflict || null,
      sourceScroll: restored.sourceScroll || { top: 0, left: 0 }, selection: restored.selection || { anchor: 0, head: 0 },
      previewScroll: restored.previewScroll || 0, previewing: Boolean(restored.previewing),
      searchTerm: restored.searchTerm || '', chapterIdentity: restored.chapterIdentity || null,
      chapterScroll: restored.chapterScroll || 0,
      collapsedChapterIdentities: Array.isArray(restored.collapsedChapterIdentities) ? [...new Set(restored.collapsedChapterIdentities)] : [],
      headings: restored.headings || [],
      total: restored.total || 0, analysisVersion: -1, activity: restored.activity || null,
      // `recoverySequence` is diagnostic data returned by SQLite. It is never
      // allocated by this process. `snapshotVersion` fences an immutable local
      // capture and covers metadata-only snapshots too.
      recoverySequence: recoverySequenceFrom(restored), snapshotVersion: Number.isSafeInteger(restored.clientEditVersion) ? restored.clientEditVersion : 0,
      lastDurableEditVersion: Number.isSafeInteger(restored.clientEditVersion) ? restored.clientEditVersion : 0,
      recoveryState: restored.recoveryError ? 'error' : 'pending', recoveryTimer: null,
      recoveryFirstChange: 0, recoveryWriting: false, recoveryPending: false,
      // A committed manuscript can still need backend reconciliation. This is
      // intentionally not folded into `dirty`: an older acknowledgement must
      // never erase a newer edit, and a clean buffer must still expose a
      // persistent recovery/selector warning.
      reconciliationPending: Boolean(restored.reconciliationPending),
      reconciliationError: restored.reconciliationError || null,
      reconciliationAttempts: 0,
      reconciliationOperationKey: restored.reconciliationOperationKey || null,
      reconciliationSnapshot: restored.reconciliationOperationKey ? {
        revision: document.writeRevision || 0,
        clientEditVersion: restored.reconciliationClientEditVersion || restored.clientEditVersion || 0,
        content: document.content ?? '',
        // A restored editor has no pre-restart edit counter. The version and
        // exact content checks in applySaveOutcome fence newer recovered text.
        editSequence: 0,
      } : null,
    };
    this.entries.set(key, entry);
    this.manifestDirty = true;
    return entry;
  }

  get(path = this.activePath) { return this.entries.get(path || '__untitled__') || null; }
  contains(entry) { return Boolean(entry) && this.entries.get(entry.key) === entry; }
  bindingFor(entry) {
    if (!this.contains(entry)) return null;
    return { projectId: this.project?.projectId, projectRoot: this.project?.root, sessionToken: this.project?.sessionToken, generation: this.generation, entryKey: entry.key };
  }
  matchesBinding(entry, binding) {
    return !this.persistenceSuppressed && this.contains(entry) && binding?.generation === this.generation && binding?.projectId === this.project?.projectId && binding?.projectRoot === this.project?.root && binding?.sessionToken === this.project?.sessionToken && binding?.entryKey === entry.key;
  }
  active() { return this.get(); }
  activate(path) {
    const key = path || '__untitled__';
    if (!this.entries.has(key)) return null;
    this.activePath = key === '__untitled__' ? null : key;
    this.manifestDirty = true;
    return this.entries.get(key);
  }
  capture(entry, capture) { if (entry) Object.assign(entry, capture); }
  markEdit(entry) {
    entry.dirty = true;
    entry.editSequence += 1;
    entry.snapshotVersion += 1;
    entry.recoveryState = 'pending';
    this.manifestDirty = true;
  }
  allocateRevision(entry) {
    entry.nextRevision = Math.max(entry.nextRevision + 1, entry.writeRevision + 1);
    return entry.nextRevision;
  }
  writeSnapshot(entry) {
    const binding = this.bindingFor(entry);
    if (!binding) return null;
    return { ...binding, documentId: entry.documentId, revision: this.allocateRevision(entry), editSequence: entry.editSequence, clientEditVersion: entry.snapshotVersion, content: entry.state.doc.toString() };
  }
  applyWriteAck(entry, snapshot, saved) {
    if (!entry || snapshot.revision < entry.writeRevision) return false;
    entry.writeRevision = saved?.writeRevision ?? snapshot.revision;
    entry.nextRevision = Math.max(entry.nextRevision, entry.writeRevision);
    entry.baselineHash = saved?.baselineHash ?? entry.baselineHash;
    entry.lastSavedText = snapshot.content;
    if (entry.editSequence === snapshot.editSequence) {
      entry.dirty = false;
      if (entry.recoveryTimer !== null) { this.cancel(entry.recoveryTimer); entry.recoveryTimer = null; }
      entry.recoveryPending = false;
      entry.lastDurableEditVersion = Math.max(entry.lastDurableEditVersion, snapshot.clientEditVersion);
      entry.recoveryState = 'durable';
    }
    return !entry.dirty;
  }
  applySaveOutcome(entry, snapshot, outcome) {
    if (!outcome?.fileCommitted) return false;
    const saved = outcome.document || outcome;
    const canClearDirty = snapshot.content === entry.state.doc.toString()
      && snapshot.clientEditVersion === entry.snapshotVersion;
    this.applyWriteAck(entry, { ...snapshot, editSequence: canClearDirty ? entry.editSequence : -1 }, saved);
    entry.baselineHash = outcome.baselineHash || saved.baselineHash || entry.baselineHash;
    entry.writeRevision = outcome.writeRevision ?? saved.writeRevision ?? entry.writeRevision;
    entry.nextRevision = Math.max(entry.nextRevision, entry.writeRevision);
    entry.reconciliationPending = Boolean(outcome.reconciliationPending);
    entry.reconciliationError = outcome.reconciliationErrorMessage || outcome.reconciliationErrorCode || null;
    entry.reconciliationOperationKey = outcome.operationKey || entry.reconciliationOperationKey;
    if (entry.reconciliationPending) {
      entry.recoveryState = entry.reconciliationError ? 'error' : 'pending';
      // The existing dirty recovery history remains durable in SQLite; do not
      // erase it merely because the manuscript rename succeeded.
      entry.recoveryPending = true;
      entry.reconciliationSnapshot = { ...snapshot };
    } else {
      entry.reconciliationError = null;
      entry.reconciliationAttempts = 0;
      entry.reconciliationSnapshot = null;
      entry.reconciliationOperationKey = null;
    }
    this.manifestDirty = true;
    return !entry.dirty && !entry.reconciliationPending;
  }
  markConflict(entry, reason) { entry.conflict = String(reason || 'File changed on disk'); entry.dirty = true; }
  autosaveEligible() { return [...this.entries.values()].filter((entry) => (entry.dirty || entry.reconciliationPending) && entry.documentId !== null && !entry.conflict); }

  scheduleRecovery(entry, persist) {
    if (!this.contains(entry) || this.persistenceSuppressed) return;
    if (!entry.dirty) { entry.recoveryState = 'durable'; return; }
    entry.recoveryPending = true;
    entry.snapshotVersion += 1;
    entry.recoveryState = 'pending';
    const now = Date.now();
    if (!entry.recoveryFirstChange) entry.recoveryFirstChange = now;
    if (entry.recoveryTimer !== null) this.cancel(entry.recoveryTimer);
    const wait = Math.max(0, Math.min(this.recoveryDelay, this.recoveryMaxDelay - (now - entry.recoveryFirstChange)));
    const binding = this.bindingFor(entry);
    entry.recoveryTimer = this.schedule(() => {
      entry.recoveryTimer = null;
      const work = this.persistRecovery(entry, persist, binding);
      this.recoveryWrites.add(work);
      void work.catch((error) => { entry.recoveryError = String(error); }).finally(() => this.recoveryWrites.delete(work));
    }, wait);
  }
  async persistRecovery(entry, persist, binding = this.bindingFor(entry)) {
    if (!this.matchesBinding(entry, binding)) return;
    if (entry.recoveryWriting) { entry.recoveryPending = true; return; }
    entry.recoveryWriting = true;
    entry.recoveryPending = false;
    entry.recoveryState = 'persisting';
    const snapshot = {
      ...binding, documentId: entry.documentId, writeRevision: entry.writeRevision,
      editSequence: entry.editSequence, clientEditVersion: entry.snapshotVersion,
      dirty: entry.dirty, content: entry.state.doc.toString(), baselineHash: entry.baselineHash,
      sourceScroll: entry.sourceScroll, selection: entry.selection, previewScroll: entry.previewScroll,
      previewing: entry.previewing, searchTerm: entry.searchTerm, chapterIdentity: entry.chapterIdentity,
      chapterScroll: entry.chapterScroll, collapsedChapterIdentities: [...entry.collapsedChapterIdentities], activity: entry.activity,
    };
    try {
      const result = await persist(entry, snapshot, binding);
      if (!this.matchesBinding(entry, binding)) return;
      if (result?.clientEditVersion !== snapshot.clientEditVersion || result?.durable !== true) throw new Error('Recovery acknowledgement was stale or not durable');
      entry.recoverySequence = result.recoverySequence;
      entry.lastDurableEditVersion = snapshot.clientEditVersion;
      entry.recoveryState = 'durable';
      entry.recoveryFirstChange = 0;
      entry.recoveryError = null;
    } catch (error) {
      entry.recoveryError = String(error);
      entry.recoveryState = 'error';
      throw error;
    } finally {
      entry.recoveryWriting = false;
      if (entry.recoveryPending || entry.snapshotVersion > snapshot.clientEditVersion) this.scheduleRecovery(entry, persist);
    }
  }
  async flushRecovery(persist) {
    if (this.persistenceSuppressed) return;
    await Promise.all([...this.entries.values()].map(async (entry) => {
      if (entry.recoveryTimer !== null) { this.cancel(entry.recoveryTimer); entry.recoveryTimer = null; }
      // Clean entries keep only UI metadata (not manuscript text), which lets a
      // normal close restore positions without rewriting a large document.
      if (!entry.dirty || (entry.snapshotVersion <= entry.lastDurableEditVersion && !entry.recoveryPending)) return;
      await this.persistRecovery(entry, persist);
    }));
  }
  manifest() {
    return {
      activePath: this.activePath,
      entries: [...this.entries.values()].map((entry) => ({
        relativePath: entry.relativePath, documentId: entry.documentId, dirty: entry.dirty,
      recoverySequence: entry.recoverySequence, sourceScroll: entry.sourceScroll, selection: entry.selection,
         previewScroll: entry.previewScroll, previewing: entry.previewing, searchTerm: entry.searchTerm,
         chapterIdentity: entry.chapterIdentity, chapterScroll: entry.chapterScroll,
         collapsedChapterIdentities: entry.collapsedChapterIdentities,
        reconciliationPending: entry.reconciliationPending,
        reconciliationOperationKey: entry.reconciliationOperationKey,
        reconciliationClientEditVersion: entry.reconciliationSnapshot?.clientEditVersion || null,
      })),
    };
  }
}

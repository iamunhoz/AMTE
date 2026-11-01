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

function nextRecoverySequence(sequence) {
  const current = recoverySequenceFrom({ recoverySequence: sequence });
  if (current >= RECOVERY_SEQUENCE_MAX) throw new RangeError('Recovery sequence cannot be incremented safely');
  return current + 1;
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
      recoverySequence: recoverySequenceFrom(restored), recoveryTimer: null,
      recoveryFirstChange: 0, recoveryWriting: false, recoveryPending: false,
    };
    this.entries.set(key, entry);
    this.manifestDirty = true;
    return entry;
  }

  get(path = this.activePath) { return this.entries.get(path || '__untitled__') || null; }
  contains(entry) { return Boolean(entry) && this.entries.get(entry.key) === entry; }
  bindingFor(entry) {
    if (!this.contains(entry)) return null;
    return { projectId: this.project?.projectId, projectRoot: this.project?.root, generation: this.generation, entryKey: entry.key };
  }
  matchesBinding(entry, binding) {
    return !this.persistenceSuppressed && this.contains(entry) && binding?.generation === this.generation && binding?.projectId === this.project?.projectId && binding?.projectRoot === this.project?.root && binding?.entryKey === entry.key;
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
    this.manifestDirty = true;
  }
  allocateRevision(entry) {
    entry.nextRevision = Math.max(entry.nextRevision + 1, entry.writeRevision + 1);
    return entry.nextRevision;
  }
  writeSnapshot(entry) {
    const binding = this.bindingFor(entry);
    if (!binding) return null;
    return { ...binding, documentId: entry.documentId, revision: this.allocateRevision(entry), editSequence: entry.editSequence, content: entry.state.doc.toString() };
  }
  applyWriteAck(entry, snapshot, saved) {
    if (!entry || snapshot.revision < entry.writeRevision) return false;
    entry.writeRevision = saved?.writeRevision ?? snapshot.revision;
    entry.nextRevision = Math.max(entry.nextRevision, entry.writeRevision);
    entry.baselineHash = saved?.baselineHash ?? entry.baselineHash;
    entry.lastSavedText = snapshot.content;
    if (entry.editSequence === snapshot.editSequence) entry.dirty = false;
    return !entry.dirty;
  }
  markConflict(entry, reason) { entry.conflict = String(reason || 'File changed on disk'); entry.dirty = true; }
  autosaveEligible() { return [...this.entries.values()].filter((entry) => entry.dirty && entry.documentId !== null && !entry.conflict); }

  scheduleRecovery(entry, persist) {
    if (!this.contains(entry) || this.persistenceSuppressed) return;
    entry.recoveryPending = true;
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
    const sequence = nextRecoverySequence(entry.recoverySequence);
    try {
      const result = await persist(entry, sequence, binding);
      if (!this.matchesBinding(entry, binding)) return;
      entry.recoverySequence = sequence;
      entry.recoveryFirstChange = 0;
      entry.recoveryError = result?.cleanupError ? String(result.cleanupError) : null;
    } catch (error) {
      entry.recoveryError = String(error);
      throw error;
    } finally {
      entry.recoveryWriting = false;
      if (entry.recoveryPending) this.scheduleRecovery(entry, persist);
    }
  }
  async flushRecovery(persist) {
    if (this.persistenceSuppressed) return;
    await Promise.all([...this.entries.values()].map(async (entry) => {
      if (entry.recoveryTimer !== null) { this.cancel(entry.recoveryTimer); entry.recoveryTimer = null; }
      // Clean entries keep only UI metadata (not manuscript text), which lets a
      // normal close restore positions without rewriting a large document.
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
      })),
    };
  }
}

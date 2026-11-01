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
      if (entry.reconciliationTimer !== null) this.cancel(entry.reconciliationTimer);
      entry.recoveryTimer = null;
      entry.reconciliationTimer = null;
      entry.recoveryPending = false;
      entry.recoveryQueuedSnapshot = null;
      entry.recoveryCancellationEpoch += 1;
      if (entry.recoveryWriting) entry.recoveryState = 'pending';
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
      // Every live entry carries the capability that created it. Never infer a
      // restored document's session from a late acknowledgement.
      projectId: this.project?.projectId ?? null,
      projectRoot: this.project?.root ?? null,
      sessionToken: this.project?.sessionToken ?? null,
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
      // Error epochs fence a late save acknowledgement from hiding a recovery
      // failure that occurred after its immutable snapshot was captured.
      stateEpoch: 0,
      conflictEpoch: 0,
      errorEpoch: restored.recoveryError ? 1 : 0,
      recoveryErrorEpoch: restored.recoveryError ? 1 : 0,
      saveErrorEpoch: 0,
      recoveryError: restored.recoveryError || null,
      saveError: null,
      recoveryState: restored.recoveryError ? 'error' : 'pending', recoveryTimer: null,
      recoveryFirstChange: 0, recoveryWriting: false, recoveryPending: false,
      // A queued immutable snapshot lets a flush drain the newest state behind
      // an older in-flight write without allowing parallel writers.
      recoveryGeneration: 0, recoveryDurableGeneration: 0,
      recoveryQueuedSnapshot: null, recoveryWritePromise: null,
      recoveryCancellationEpoch: 0,
      // A committed manuscript can still need backend reconciliation. This is
      // intentionally not folded into `dirty`: an older acknowledgement must
      // never erase a newer edit, and a clean buffer must still expose a
      // persistent recovery/selector warning.
      reconciliationPending: Boolean(restored.reconciliationPending),
      reconciliationError: restored.reconciliationError || null,
      reconciliationAttempts: 0,
      reconciliationTimer: null,
      reconciliationOperationKey: restored.reconciliationOperationKey || null,
      reconciliationSnapshot: restored.reconciliationOperationKey ? {
        projectId: this.project?.projectId ?? null,
        projectRoot: this.project?.root ?? null,
        sessionToken: this.project?.sessionToken ?? null,
        generation: this.generation,
        entryKey: key,
        documentId: document.documentId ?? null,
        revision: document.writeRevision || 0,
        clientEditVersion: restored.reconciliationClientEditVersion || restored.clientEditVersion || 0,
        content: document.content ?? '',
        // A restored editor has no pre-restart edit counter. The version and
        // exact content checks in applySaveOutcome fence newer recovered text.
        editSequence: 0,
        stateEpoch: 0,
        conflictEpoch: 0,
        operationKey: restored.reconciliationOperationKey,
        restoredReconciliation: true,
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
    return { projectId: entry.projectId, projectRoot: entry.projectRoot, sessionToken: entry.sessionToken, generation: this.generation, entryKey: entry.key };
  }
  matchesBinding(entry, binding) {
    return !this.persistenceSuppressed && this.contains(entry)
      && Number.isSafeInteger(entry.projectId) && entry.projectId > 0
      && typeof entry.projectRoot === 'string' && Boolean(entry.projectRoot)
      && typeof entry.sessionToken === 'string' && Boolean(entry.sessionToken)
      && binding?.generation === this.generation
      && binding?.projectId === entry.projectId && binding?.projectRoot === entry.projectRoot
      && binding?.sessionToken === entry.sessionToken && binding?.entryKey === entry.key
      && this.project?.projectId === entry.projectId && this.project?.root === entry.projectRoot
      && this.project?.sessionToken === entry.sessionToken;
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
    entry.stateEpoch += 1;
    // A committed operation remains safe to reconcile after later typing. Its
    // immutable content/version still prevents dirty clearing, while this
    // current state fence allows its independent journal acknowledgement to
    // retire the pending operation rather than stranding all later saves.
    if (entry.reconciliationSnapshot) entry.reconciliationSnapshot = {
      ...entry.reconciliationSnapshot,
      stateEpoch: entry.stateEpoch,
      conflictEpoch: entry.conflictEpoch,
    };
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
    return { ...binding, documentId: entry.documentId, revision: this.allocateRevision(entry), editSequence: entry.editSequence, clientEditVersion: entry.snapshotVersion, stateEpoch: entry.stateEpoch, conflictEpoch: entry.conflictEpoch, errorEpoch: entry.errorEpoch, operationKey: entry.reconciliationOperationKey, content: entry.state.doc.toString() };
  }
  recordRecoveryError(entry, error, binding = this.bindingFor(entry)) {
    if (!this.matchesBinding(entry, binding)) return false;
    entry.recoveryError = String(error);
    entry.recoveryErrorEpoch = ++entry.errorEpoch;
    entry.stateEpoch += 1;
    if (entry.reconciliationSnapshot) entry.reconciliationSnapshot = {
      ...entry.reconciliationSnapshot,
      stateEpoch: entry.stateEpoch,
      conflictEpoch: entry.conflictEpoch,
    };
    entry.recoveryState = 'error';
    return true;
  }
  recordSaveError(entry, error, binding = this.bindingFor(entry)) {
    if (!this.matchesBinding(entry, binding)) return false;
    entry.saveError = String(error);
    entry.saveErrorEpoch = ++entry.errorEpoch;
    entry.stateEpoch += 1;
    if (entry.reconciliationSnapshot) entry.reconciliationSnapshot = {
      ...entry.reconciliationSnapshot,
      stateEpoch: entry.stateEpoch,
      conflictEpoch: entry.conflictEpoch,
    };
    return true;
  }
  recordReconciliationError(entry, error, binding = this.bindingFor(entry)) {
    if (!this.matchesBinding(entry, binding) || !entry.reconciliationPending || !entry.reconciliationOperationKey) return false;
    // A timer failure must not invalidate the committed operation snapshot: a
    // later exact retry still has to converge without another picker.
    entry.reconciliationError = String(error);
    entry.recoveryPending = true;
    entry.recoveryState = 'error';
    this.manifestDirty = true;
    return true;
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
    // Both fresh and restored snapshots carry the full immutable session
    // binding. Missing or rotated bindings are always ignored.
    const ignored = (reason) => ({ applied: false, reason, complete: false });
    const matchingBinding = this.matchesBinding(entry, snapshot);
    const matchingIdentity = Number.isSafeInteger(outcome?.projectId) && outcome.projectId > 0
      && typeof outcome?.sessionToken === 'string' && Boolean(outcome.sessionToken)
      && outcome.projectId === snapshot.projectId && outcome.sessionToken === snapshot.sessionToken
      && snapshot.documentId === entry.documentId;
    const matchingOperation = !snapshot.operationKey
      || (outcome?.operationKey === snapshot.operationKey
        && entry.reconciliationOperationKey === snapshot.operationKey);
    // Restart metadata deliberately stores the opaque operation key and edit
    // version, not its write revision. That key is revalidated by Rust; accept
    // only a non-regressing backend revision for that narrowly scoped path.
    const matchingRevision = snapshot?.restoredReconciliation
      ? outcome?.writeRevision >= entry.writeRevision
      : outcome?.writeRevision === snapshot?.revision;
    const currentRevision = snapshot.operationKey
      ? entry.writeRevision <= outcome?.writeRevision
      : entry.writeRevision < snapshot?.revision;
    const currentSnapshot = entry.stateEpoch === snapshot?.stateEpoch
      && entry.conflictEpoch === snapshot?.conflictEpoch
      && currentRevision;
    if (!outcome?.fileCommitted) return ignored('file-not-committed');
    if (!matchingBinding) return ignored('binding');
    if (!matchingIdentity) return ignored('identity');
    if (!matchingOperation) return ignored('operation');
    if (!currentSnapshot) return ignored('state');
    if (!matchingRevision) return ignored('revision');
    if (outcome.savedClientEditVersion !== snapshot.clientEditVersion) return ignored('edit-version');
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
      entry.stateEpoch += 1;
      entry.reconciliationSnapshot = {
        ...snapshot,
        stateEpoch: entry.stateEpoch,
        operationKey: outcome.operationKey,
        baselineHash: outcome.baselineHash,
        contentBlake3: outcome.contentBlake3,
      };
    } else {
      entry.reconciliationError = null;
      entry.reconciliationAttempts = 0;
      if (entry.reconciliationTimer !== null) this.cancel(entry.reconciliationTimer);
      entry.reconciliationTimer = null;
      entry.reconciliationSnapshot = null;
      entry.reconciliationOperationKey = null;
    }
    // A fully durable response is authoritative only for the immutable
    // snapshot it acknowledges. It clears errors which predate that snapshot,
    // but never a failure recorded while the request was in flight.
    const fullyDurable = canClearDirty
      && outcome.recoveryCheckpointDurable === true
      && outcome.selectorSessionStateDurable === true
      && !entry.reconciliationPending
      && !entry.reconciliationError
      && outcome.savedClientEditVersion === snapshot.clientEditVersion;
    if (fullyDurable) {
      const acknowledgedErrorEpoch = Number.isSafeInteger(snapshot.errorEpoch) ? snapshot.errorEpoch : 0;
      if (entry.recoveryError && entry.recoveryErrorEpoch <= acknowledgedErrorEpoch) entry.recoveryError = null;
      if (entry.saveError && entry.saveErrorEpoch <= acknowledgedErrorEpoch) entry.saveError = null;
    }
    this.manifestDirty = true;
    return { applied: true, reason: null, complete: !entry.dirty && !entry.reconciliationPending };
  }
  applySaveAsOutcome(entry, snapshot, outcome, document) {
    const ignored = (reason) => ({ applied: false, reason, complete: false });
    const nextKey = document?.relativePath;
    if (!this.contains(entry) || entry.documentId !== null || !Number.isSafeInteger(document?.documentId)
      || document.documentId < 1 || typeof nextKey !== 'string' || !nextKey
      || document.projectId !== entry.projectId
      || outcome?.documentId !== document.documentId || outcome?.relativePath !== nextKey
      || outcome?.baselineHash !== document.baselineHash || outcome?.contentBlake3 !== outcome.baselineHash
      || (outcome?.reconciliationPending && !outcome?.operationKey)
      || (this.entries.has(nextKey) && this.entries.get(nextKey) !== entry)) return ignored('save-as-rebind');
    // Preflight all map/identity work before applying the outcome. The remaining
    // rebind is local and infallible, so accepted Save As and its pending
    // reconciliation descriptor are one frontend transition.
    const applied = this.applySaveOutcome(entry, snapshot, outcome);
    if (!applied.applied) return applied;
    const oldKey = entry.key;
    entry.documentId = document.documentId;
    entry.relativePath = nextKey;
    entry.baselineHash = document.baselineHash || entry.baselineHash;
    this.entries.delete(oldKey);
    this.entries.set(nextKey, entry);
    entry.key = nextKey;
    if (this.activePath === null || this.activePath === oldKey) this.activePath = nextKey;
    if (entry.reconciliationPending) {
      // Replace, rather than patch, the retired untitled descriptor. This
      // records the same committed operation under its destination identity.
      entry.reconciliationSnapshot = {
        projectId: entry.projectId,
        projectRoot: entry.projectRoot,
        sessionToken: entry.sessionToken,
        generation: this.generation,
        entryKey: entry.key,
        documentId: entry.documentId,
        relativePath: entry.relativePath,
        revision: outcome.writeRevision,
        clientEditVersion: outcome.savedClientEditVersion,
        editSequence: entry.editSequence,
        stateEpoch: entry.stateEpoch,
        conflictEpoch: entry.conflictEpoch,
        errorEpoch: snapshot.errorEpoch,
        operationKey: entry.reconciliationOperationKey,
        content: snapshot.content,
        baselineHash: outcome.baselineHash,
        contentBlake3: outcome.contentBlake3,
      };
    }
    this.manifestDirty = true;
    return { ...applied, complete: !entry.dirty && !entry.reconciliationPending };
  }
  markConflict(entry, reason) {
    entry.conflict = String(reason || 'File changed on disk');
    entry.dirty = true;
    entry.conflictEpoch += 1;
    entry.stateEpoch += 1;
  }
  autosaveEligible() { return [...this.entries.values()].filter((entry) => (entry.dirty || entry.reconciliationPending) && entry.documentId !== null && !entry.conflict); }

  queueRecoverySnapshot(entry, binding = this.bindingFor(entry)) {
    if (!this.matchesBinding(entry, binding)) return null;
    const snapshot = {
      ...binding,
      recoveryGeneration: ++entry.recoveryGeneration,
      recoveryCancellationEpoch: entry.recoveryCancellationEpoch,
      relativePath: entry.relativePath, documentId: entry.documentId, writeRevision: entry.writeRevision,
      editSequence: entry.editSequence, clientEditVersion: entry.snapshotVersion,
      stateEpoch: entry.stateEpoch, errorEpoch: entry.errorEpoch,
      dirty: entry.dirty, content: entry.state.doc.toString(), baselineHash: entry.baselineHash,
      sourceScroll: { ...entry.sourceScroll }, selection: { ...entry.selection }, previewScroll: entry.previewScroll,
      previewing: entry.previewing, searchTerm: entry.searchTerm, chapterIdentity: entry.chapterIdentity,
      chapterScroll: entry.chapterScroll, collapsedChapterIdentities: [...entry.collapsedChapterIdentities], activity: entry.activity,
      reconciliationOperationKey: entry.reconciliationOperationKey,
      reconciliationClientEditVersion: entry.reconciliationSnapshot?.clientEditVersion || null,
    };
    entry.recoveryQueuedSnapshot = snapshot;
    entry.recoveryPending = true;
    return snapshot;
  }
  armRecoveryTimer(entry, persist, binding = this.bindingFor(entry)) {
    if (!this.matchesBinding(entry, binding) || !entry.recoveryQueuedSnapshot) return;
    if (entry.recoveryTimer !== null) this.cancel(entry.recoveryTimer);
    const now = Date.now();
    if (!entry.recoveryFirstChange) entry.recoveryFirstChange = now;
    const wait = Math.max(0, Math.min(this.recoveryDelay, this.recoveryMaxDelay - (now - entry.recoveryFirstChange)));
    entry.recoveryTimer = this.schedule(() => {
      entry.recoveryTimer = null;
      // The per-entry writer is shared. A timer never starts a parallel write
      // and consumes its rejection because the next debounce is already queued.
      void this.persistRecovery(entry, persist, binding).catch(() => {});
    }, wait);
  }
  scheduleRecovery(entry, persist) {
    if (!this.contains(entry) || this.persistenceSuppressed) return;
    // A clean post-rename document can still own a pending opaque operation.
    // Persist it under the destination binding so a restart never revives the
    // retired untitled descriptor or reopens a picker.
    if (!entry.dirty && !entry.reconciliationPending) { entry.recoveryState = 'durable'; return; }
    entry.snapshotVersion += 1;
    entry.recoveryState = 'pending';
    const binding = this.bindingFor(entry);
    if (!this.queueRecoverySnapshot(entry, binding)) return;
    this.armRecoveryTimer(entry, persist, binding);
  }
  startRecoveryWrite(entry, persist, binding) {
    if (!this.matchesBinding(entry, binding) || entry.recoveryWritePromise) return entry.recoveryWritePromise;
    const snapshot = entry.recoveryQueuedSnapshot || this.queueRecoverySnapshot(entry, binding);
    if (!snapshot) return null;
    if (entry.recoveryTimer !== null) { this.cancel(entry.recoveryTimer); entry.recoveryTimer = null; }
    entry.recoveryQueuedSnapshot = null;
    entry.recoveryPending = false;
    entry.recoveryWriting = true;
    entry.recoveryState = 'persisting';
    const work = (async () => {
      try {
        const result = await persist(entry, snapshot, binding);
        if (!this.matchesBinding(entry, binding) || entry.recoveryCancellationEpoch !== snapshot.recoveryCancellationEpoch) return;
        if (result?.clientEditVersion !== snapshot.clientEditVersion || result?.durable !== true) throw new Error('Recovery acknowledgement was stale or not durable');
        // This acknowledgement durably covers this immutable queue generation
        // even if newer local edits arrived while the write was in flight.
        entry.recoveryDurableGeneration = Math.max(entry.recoveryDurableGeneration, snapshot.recoveryGeneration);
        if (entry.stateEpoch !== snapshot.stateEpoch) {
          if (!entry.recoveryQueuedSnapshot) this.queueRecoverySnapshot(entry, binding);
          return;
        }
        entry.recoverySequence = result.recoverySequence;
        entry.lastDurableEditVersion = snapshot.clientEditVersion;
        entry.recoveryState = 'durable';
        entry.recoveryFirstChange = 0;
        if (!entry.recoveryError || entry.recoveryErrorEpoch <= snapshot.errorEpoch) entry.recoveryError = null;
      } catch (error) {
        if (this.matchesBinding(entry, binding) && entry.recoveryCancellationEpoch === snapshot.recoveryCancellationEpoch) {
          this.recordRecoveryError(entry, error, binding);
          if (!entry.recoveryQueuedSnapshot) this.queueRecoverySnapshot(entry, binding);
        }
        throw error;
      } finally {
        entry.recoveryWriting = false;
      }
    })();
    entry.recoveryWritePromise = work;
    this.recoveryWrites.add(work);
    const finish = () => {
      if (entry.recoveryWritePromise === work) entry.recoveryWritePromise = null;
      this.recoveryWrites.delete(work);
      if (this.matchesBinding(entry, binding) && entry.recoveryCancellationEpoch === snapshot.recoveryCancellationEpoch && entry.recoveryQueuedSnapshot) {
        this.armRecoveryTimer(entry, persist, binding);
      }
    };
    void work.then(finish, finish);
    return work;
  }
  async persistRecovery(entry, persist, binding = this.bindingFor(entry), barrierGeneration = null) {
    if (!this.matchesBinding(entry, binding)) return;
    const cancellationEpoch = entry.recoveryCancellationEpoch;
    const queued = entry.recoveryQueuedSnapshot || this.queueRecoverySnapshot(entry, binding);
    const target = barrierGeneration ?? queued?.recoveryGeneration;
    if (!target) return;
    // Never await from the writer itself: callers only await the separate
    // per-entry work promise, then drain enough immutable generations to pass
    // their captured barrier. Later edits remain queued for debounce.
    while (this.matchesBinding(entry, binding)
      && entry.recoveryCancellationEpoch === cancellationEpoch
      && entry.recoveryDurableGeneration < target) {
      const work = entry.recoveryWritePromise || this.startRecoveryWrite(entry, persist, binding);
      if (!work) return;
      await work;
    }
  }
  async flushRecovery(persist) {
    if (this.persistenceSuppressed) return;
    await Promise.all([...this.entries.values()].map(async (entry) => {
      if (entry.recoveryTimer !== null) { this.cancel(entry.recoveryTimer); entry.recoveryTimer = null; }
      // Clean entries keep only UI metadata (not manuscript text), which lets a
      // normal close restore positions without rewriting a large document.
      if (!entry.dirty && !entry.reconciliationPending) {
        if (entry.recoveryWritePromise) await entry.recoveryWritePromise;
        return;
      }
      const binding = this.bindingFor(entry);
      const snapshot = this.queueRecoverySnapshot(entry, binding);
      if (snapshot) await this.persistRecovery(entry, persist, binding, snapshot.recoveryGeneration);
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

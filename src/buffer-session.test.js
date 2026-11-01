import assert from 'node:assert/strict';
import test from 'node:test';
import { BufferSession, clampRestoredSelection, recoverySequenceFrom } from './buffer-session.js';

const state = (text) => ({ doc: { toString: () => text } });
const project = (projectId = 1, sessionToken = `session-${projectId}`) => ({ projectId, root: `/synthetic/${projectId}`, sessionToken });
const outcomeFor = (snapshot, fields) => ({ projectId: snapshot.projectId, sessionToken: snapshot.sessionToken, ...fields });
const saveAsDocument = (session, relativePath = 'saved.md', documentId = 22, baselineHash = 'b'.repeat(64)) => ({ projectId: session.project.projectId, relativePath, documentId, baselineHash });
const acknowledged = (snapshot) => ({ durable: true, recoverySequence: 99, clientEditVersion: snapshot.clientEditVersion });
const entrySnapshot = (entry) => {
  const { state, ...serializable } = entry;
  return JSON.parse(JSON.stringify(serializable));
};
// Metadata-only fixture from the diagnosed healthy production save. It omits
// manuscript text and exercises the exact durable response shape.
const diagnosedHealthySaveOutcome = Object.freeze({
  fileCommitted: true,
  recoveryCheckpointDurable: true,
  selectorSessionStateDurable: true,
  reconciliationPending: false,
  writeRevision: 7,
  savedClientEditVersion: 77,
  baselineHash: '0bde5bf24caf866eb7bef6dad84c5d5ec1be70dc3067b5dfa8fddc333f36c225',
  contentBlake3: '0bde5bf24caf866eb7bef6dad84c5d5ec1be70dc3067b5dfa8fddc333f36c225',
  operationKey: 'b51cb83b32952ee7300a090dfb298e9b2d8fff45a8a412b3b4c142a47c9a4692',
});

test('CodeMirror state is live-only while SQLite acknowledgement owns recovery sequence', async () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'a' }, state('edited'));
  session.markEdit(entry);
  let captured;
  await session.persistRecovery(entry, async (_entry, snapshot) => { captured = snapshot; return acknowledged(snapshot); });
  assert.equal(captured.content, 'edited');
  assert.equal(captured.recoverySequence, undefined);
  assert.equal(entry.recoverySequence, 99);
  assert.equal(entry.recoveryState, 'durable');
  assert.equal(entry.dirty, true, 'recovery durability never marks the manuscript clean');
});

test('coalesces delayed writes and rejects stale acknowledgements without clearing dirty state', async () => {
  const timers = []; let release;
  const session = new BufferSession({ project: project(7), schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: () => {} });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'A' }, state('A'));
  session.markEdit(entry); session.scheduleRecovery(entry, () => new Promise((resolve) => { release = resolve; }));
  timers[0](); await Promise.resolve(); session.markEdit(entry);
  release({ durable: true, recoverySequence: 1, clientEditVersion: 1 });
  await session.drainRecovery();
  assert.equal(entry.dirty, true); assert.ok(['error', 'pending'].includes(entry.recoveryState));
});

test('recovery writer drains a captured barrier serially, retries failures, and cancels stale generations', async () => {
  let text = 'A'; let timerId = 0; let active = 0; let maximumActive = 0;
  const timers = new Map(); const writes = []; const pending = [];
  const session = new BufferSession({
    project: project(8),
    schedule: (fn) => { const id = ++timerId; timers.set(id, fn); return id; },
    cancel: (id) => timers.delete(id),
  });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: text }, { doc: { toString: () => text } });
  const persist = (_entry, snapshot) => new Promise((resolve, reject) => {
    writes.push(snapshot); active += 1; maximumActive = Math.max(maximumActive, active);
    pending.push({ snapshot, resolve: () => { active -= 1; resolve(acknowledged(snapshot)); }, reject: (error) => { active -= 1; reject(error); } });
  });
  const fire = () => { const [id, callback] = timers.entries().next().value; timers.delete(id); callback(); };
  const settle = async () => { for (let index = 0; index < 8; index += 1) await Promise.resolve(); };

  session.markEdit(entry); session.scheduleRecovery(entry, persist); fire(); await settle();
  text = 'B'; session.markEdit(entry); session.scheduleRecovery(entry, persist);
  text = 'C'; session.markEdit(entry); session.scheduleRecovery(entry, persist);
  const barrier = session.flushRecovery(persist);
  assert.equal(writes.length, 1, 'the barrier shares the blocked writer');
  pending.shift().resolve(); await settle();
  assert.equal(writes.length, 2);
  assert.equal(writes[1].content, 'C', 'coalescing keeps only the newest queued content');
  assert.equal(maximumActive, 1, 'one buffer never invokes recovery persistence in parallel');
  pending.shift().resolve(); await barrier;

  text = 'D'; session.markEdit(entry); session.scheduleRecovery(entry, persist);
  const failedBarrier = session.flushRecovery(persist); await settle();
  pending.shift().reject(new Error('injected recovery failure'));
  await assert.rejects(failedBarrier, /injected recovery failure/);
  assert.match(entry.recoveryError, /injected recovery failure/);
  assert.ok(entry.recoveryQueuedSnapshot, 'a failed write retains a retryable latest snapshot');
  assert.equal(timers.size, 1, 'failure queues one bounded retry');
  fire(); await settle(); pending.shift().resolve(); await settle();
  assert.equal(entry.recoveryError, null, 'the retry acknowledgement clears its own recovery error');
  assert.equal(maximumActive, 1);

  text = 'E'; session.markEdit(entry); session.scheduleRecovery(entry, persist); fire(); await settle();
  session.cancelRecovery(); pending.shift().resolve(); await session.drainRecovery();
  assert.equal(timers.size, 0, 'teardown cancels queued work after an active write');
  assert.equal(entry.recoveryQueuedSnapshot, null);

  text = 'F'; session.markEdit(entry); session.scheduleRecovery(entry, persist); fire(); await settle();
  const writesBeforeSwitch = writes.length;
  session.setProject(project(9)); pending.shift().resolve(); await session.drainRecovery();
  assert.equal(session.entries.size, 0);
  assert.equal(timers.size, 0);
  assert.equal(writes.length, writesBeforeSwitch, 'a stale session cannot queue a new-project recovery write');
});

test('legacy sequence is read only for recovery-v1 input and UTF-16 selections clamp', () => {
  assert.equal(recoverySequenceFrom({ sequence: 11 }), 11);
  assert.equal(recoverySequenceFrom({ recoverySequence: 12 }), 12);
  assert.throws(() => recoverySequenceFrom({ recoverySequence: 1, sequence: 2 }), RangeError);
  assert.deepEqual(clampRestoredSelection({ anchor: 9, head: 12 }, 'a😀b'), { anchor: 4, head: 4 });
});

test('file and recovery lifecycles remain independently versioned', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'a' }, state('a'));
  session.markEdit(entry); const saved = session.writeSnapshot(entry);
  assert.equal(saved.clientEditVersion, entry.snapshotVersion);
  assert.equal(session.applyWriteAck(entry, saved, { writeRevision: saved.revision }), true);
  assert.equal(entry.lastDurableEditVersion, saved.clientEditVersion);
  session.markEdit(entry); assert.equal(entry.dirty, true);
  assert.equal(session.applyWriteAck(entry, saved, { writeRevision: saved.revision }), false);
});

test('a committed SaveOutcome after a newer edit is ignored without mutating the entry', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'a' }, state('a'));
  session.markEdit(entry);
  const saved = session.writeSnapshot(entry);
  session.markEdit(entry); // The user typed again while the first save was in flight.
  const before = entrySnapshot(entry);
  assert.equal(session.applySaveOutcome(entry, saved, outcomeFor(saved, {
    fileCommitted: true, recoveryCheckpointDurable: false, reconciliationPending: true,
    reconciliationErrorCode: 'RECOVERY_CHECKPOINT_PENDING', baselineHash: 'b'.repeat(64),
    operationKey: 'a'.repeat(64), writeRevision: saved.revision, savedClientEditVersion: saved.clientEditVersion,
  })).applied, false);
  assert.deepEqual(entrySnapshot(entry), before);
});

test('a pending opaque reconciliation key survives a frontend restart and never clears newer text', () => {
  const key = 'a'.repeat(64);
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, writeRevision: 4, content: 'newer' }, state('newer'), {
    dirty: true, clientEditVersion: 8, reconciliationPending: true,
    reconciliationOperationKey: key, reconciliationClientEditVersion: 7,
  });
  assert.equal(entry.reconciliationOperationKey, key);
  session.applySaveOutcome(entry, entry.reconciliationSnapshot, outcomeFor(entry.reconciliationSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, operationKey: key, writeRevision: 7, baselineHash: 'b'.repeat(64), savedClientEditVersion: 7,
  }));
  assert.equal(entry.dirty, true, 'a restarted buffer with newer recovered text remains dirty');
  assert.equal(entry.reconciliationPending, false);
  assert.equal(entry.reconciliationOperationKey, null);
});

test('activation and restore DTOs bind every entry to the current session and reject rotated-session outcomes', () => {
  const activation = { projectId: 41, root: '/synthetic/activation', sessionToken: 'a'.repeat(64) };
  const session = new BufferSession({ project: activation });
  const restored = session.add({ projectId: activation.projectId, relativePath: 'restored.md', documentId: 9, writeRevision: 4, content: 'draft' }, state('draft'), {
    dirty: true, clientEditVersion: 7, reconciliationPending: true,
    reconciliationOperationKey: 'b'.repeat(64), reconciliationClientEditVersion: 7,
  });
  assert.deepEqual({ projectId: restored.projectId, sessionToken: restored.sessionToken }, { projectId: activation.projectId, sessionToken: activation.sessionToken });
  const restoredOutcome = outcomeFor(restored.reconciliationSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, operationKey: 'b'.repeat(64), writeRevision: 8, savedClientEditVersion: 7,
  });
  assert.equal(session.applySaveOutcome(restored, restored.reconciliationSnapshot, restoredOutcome).applied, true);
  const oldSnapshot = session.writeSnapshot(restored);
  session.setProject({ projectId: 42, root: activation.root, sessionToken: 'c'.repeat(64) });
  const rebound = session.add({ projectId: 42, relativePath: 'restored.md', documentId: 9, content: 'draft' }, state('draft'));
  assert.equal(rebound.sessionToken, 'c'.repeat(64));
  assert.equal(session.applySaveOutcome(restored, oldSnapshot, outcomeFor(oldSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: oldSnapshot.revision, savedClientEditVersion: oldSnapshot.clientEditVersion,
  })).applied, false);
});

test('partial Save As rebinds its pending descriptor atomically and reconciliation converges on the new binding', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ content: 'draft' }, state('draft'));
  session.markEdit(entry);
  const initial = session.writeSnapshot(entry);
  const savedDocument = saveAsDocument(session);
  const partial = outcomeFor(initial, {
    fileCommitted: true, recoveryCheckpointDurable: false, selectorSessionStateDurable: false,
    reconciliationPending: true, reconciliationErrorCode: 'SELECTOR_PENDING', operationKey: 'd'.repeat(64),
    writeRevision: initial.revision, savedClientEditVersion: initial.clientEditVersion,
    documentId: savedDocument.documentId, relativePath: savedDocument.relativePath,
    baselineHash: savedDocument.baselineHash, contentBlake3: savedDocument.baselineHash, document: savedDocument,
  });
  const applied = session.applySaveAsOutcome(entry, initial, partial, savedDocument);
  assert.equal(applied.applied, true);
  assert.equal(session.get(), entry);
  assert.equal(session.entries.has('__untitled__'), false);
  assert.deepEqual({ key: entry.key, relativePath: entry.relativePath, documentId: entry.documentId }, { key: 'saved.md', relativePath: 'saved.md', documentId: 22 });
  assert.deepEqual({ entryKey: entry.reconciliationSnapshot.entryKey, documentId: entry.reconciliationSnapshot.documentId, operationKey: entry.reconciliationSnapshot.operationKey }, { entryKey: 'saved.md', documentId: 22, operationKey: 'd'.repeat(64) });
  assert.equal(entry.reconciliationSnapshot.stateEpoch, entry.stateEpoch);
  const retried = session.applySaveOutcome(entry, entry.reconciliationSnapshot, outcomeFor(entry.reconciliationSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, operationKey: 'd'.repeat(64), writeRevision: initial.revision,
    savedClientEditVersion: initial.clientEditVersion, baselineHash: savedDocument.baselineHash, document: savedDocument,
  }));
  assert.deepEqual(retried, { applied: true, reason: null, complete: true });
  assert.equal(entry.reconciliationPending, false);
});

test('DTO-less partial Save As retains the untitled operation and atomically rebinds on retry', () => {
  const original = new BufferSession({ project: project(3, 'dto-less-session') });
  const entry = original.add({ content: 'draft' }, state('draft'));
  original.markEdit(entry);
  const snapshot = original.writeSnapshot(entry);
  const destination = saveAsDocument(original, 'from-retry.md', 71, 'e'.repeat(64));
  const partial = outcomeFor(snapshot, {
    fileCommitted: true, recoveryCheckpointDurable: false, selectorSessionStateDurable: false,
    reconciliationPending: true, operationKey: 'f'.repeat(64), writeRevision: snapshot.revision,
    savedClientEditVersion: snapshot.clientEditVersion, documentId: destination.documentId,
    relativePath: destination.relativePath, baselineHash: destination.baselineHash, contentBlake3: destination.baselineHash,
  });
  assert.deepEqual(original.applySaveOutcome(entry, snapshot, partial), { applied: true, reason: null, complete: false });
  assert.deepEqual({ key: entry.key, documentId: entry.documentId, operationKey: entry.reconciliationOperationKey, snapshotKey: entry.reconciliationSnapshot.entryKey, snapshotDocumentId: entry.reconciliationSnapshot.documentId }, { key: '__untitled__', documentId: null, operationKey: 'f'.repeat(64), snapshotKey: '__untitled__', snapshotDocumentId: null });
  const durable = outcomeFor(entry.reconciliationSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, operationKey: 'f'.repeat(64), writeRevision: entry.writeRevision,
    savedClientEditVersion: entry.reconciliationSnapshot.clientEditVersion, documentId: destination.documentId,
    relativePath: destination.relativePath, baselineHash: destination.baselineHash, contentBlake3: destination.baselineHash, document: destination,
  });
  assert.deepEqual(original.applySaveAsOutcome(entry, entry.reconciliationSnapshot, durable, destination), { applied: true, reason: null, complete: true });
  assert.deepEqual({ key: entry.key, documentId: entry.documentId, pending: entry.reconciliationPending }, { key: 'from-retry.md', documentId: 71, pending: false });
});

test('a reconciliation scheduling failure preserves the pending operation and persistent error state', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ content: 'draft' }, state('draft'));
  session.markEdit(entry);
  const snapshot = session.writeSnapshot(entry);
  session.applySaveOutcome(entry, snapshot, outcomeFor(snapshot, {
    fileCommitted: true, recoveryCheckpointDurable: false, selectorSessionStateDurable: false,
    reconciliationPending: true, operationKey: 'f'.repeat(64), writeRevision: snapshot.revision,
    savedClientEditVersion: snapshot.clientEditVersion,
  }));
  const pending = entry.reconciliationSnapshot;
  assert.equal(session.recordReconciliationError(entry, 'timer unavailable', session.bindingFor(entry)), true);
  assert.equal(entry.reconciliationError, 'timer unavailable');
  assert.equal(entry.reconciliationOperationKey, 'f'.repeat(64));
  assert.equal(entry.reconciliationSnapshot, pending);
  assert.equal(session.manifest().entries[0].reconciliationOperationKey, 'f'.repeat(64));
});

test('newer edits and stale Save As outcomes cannot replace the re-bound pending descriptor', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ content: 'draft' }, state('draft'));
  session.markEdit(entry);
  const initial = session.writeSnapshot(entry);
  const document = saveAsDocument(session);
  const partial = outcomeFor(initial, {
    fileCommitted: true, recoveryCheckpointDurable: false, selectorSessionStateDurable: false,
    reconciliationPending: true, operationKey: 'd'.repeat(64), writeRevision: initial.revision,
    documentId: document.documentId, relativePath: document.relativePath,
    savedClientEditVersion: initial.clientEditVersion, baselineHash: document.baselineHash, contentBlake3: document.baselineHash, document,
  });
  assert.equal(session.applySaveAsOutcome(entry, initial, partial, document).applied, true);
  entry.state = state('newer draft');
  session.markEdit(entry);
  const reconciled = session.applySaveOutcome(entry, entry.reconciliationSnapshot, outcomeFor(entry.reconciliationSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, operationKey: 'd'.repeat(64), writeRevision: initial.revision,
    savedClientEditVersion: initial.clientEditVersion, document,
  }));
  assert.deepEqual(reconciled, { applied: true, reason: null, complete: false });
  assert.equal(entry.dirty, true, 'a post-Save As edit is not cleared by reconciliation');
  assert.equal(entry.reconciliationPending, false);

  const stale = session.add({ content: 'second' }, state('second'));
  session.markEdit(stale);
  const staleSnapshot = session.writeSnapshot(stale);
  session.markEdit(stale);
  const beforeStale = entrySnapshot(stale);
  assert.equal(session.applySaveAsOutcome(stale, staleSnapshot, outcomeFor(staleSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: false, selectorSessionStateDurable: false,
    reconciliationPending: true, operationKey: 'e'.repeat(64), writeRevision: staleSnapshot.revision,
    documentId: 23, relativePath: 'never.md', baselineHash: 'c'.repeat(64), contentBlake3: 'c'.repeat(64),
    savedClientEditVersion: staleSnapshot.clientEditVersion, document: { projectId: stale.projectId, relativePath: 'never.md', documentId: 23, baselineHash: 'c'.repeat(64) },
  }), { projectId: stale.projectId, relativePath: 'never.md', documentId: 23, baselineHash: 'c'.repeat(64) }).applied, false);
  assert.deepEqual(entrySnapshot(stale), beforeStale);
});

test('manual save, autosave, Save All members, and Save As consume the same exact SaveOutcome contract', () => {
  for (const kind of ['manual', 'autosave', 'save-all:first', 'save-all:later', 'save-as']) {
    const session = new BufferSession({ project: project() });
    const entry = session.add({ relativePath: `${kind}.md`, documentId: 2, content: 'old' }, state('edited'));
    session.markEdit(entry);
    session.recordRecoveryError(entry, 'older recovery failure');
    session.recordSaveError(entry, 'older file failure');
    const snapshot = session.writeSnapshot(entry);
    assert.equal(session.applySaveOutcome(entry, snapshot, outcomeFor(snapshot, {
      fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
      reconciliationPending: false, operationKey: 'a'.repeat(64), writeRevision: snapshot.revision,
      baselineHash: 'b'.repeat(64), savedClientEditVersion: snapshot.clientEditVersion,
    })).applied, true, kind);
    assert.equal(entry.dirty, false, `${kind} reaches a clean file state only after fileCommitted`);
    assert.equal(entry.reconciliationPending, false, `${kind} has no false pending status`);
    assert.equal(entry.recoveryError, null, `${kind} clears only the pre-request recovery error`);
    assert.equal(entry.saveError, null, `${kind} clears only the pre-request file error`);
  }
});

test('a diagnosed fully durable SaveOutcome clears only residual pre-request errors', () => {
  const session = new BufferSession({ project: { projectId: 1, root: '/synthetic', sessionToken: 'session' } });
  const entry = session.add({ relativePath: 'healthy.md', documentId: 13, writeRevision: 6, content: 'same' }, state('same'), { clientEditVersion: 77 });
  session.recordRecoveryError(entry, 'old recovery error');
  session.recordSaveError(entry, 'old save error');
  const snapshot = session.writeSnapshot(entry);
  assert.equal(snapshot.clientEditVersion, 77);
  assert.equal(session.applySaveOutcome(entry, snapshot, { ...diagnosedHealthySaveOutcome, projectId: snapshot.projectId, sessionToken: snapshot.sessionToken }).applied, true);
  assert.equal(entry.dirty, false);
  assert.equal(entry.recoveryError, null);
  assert.equal(entry.saveError, null);
  assert.equal(entry.reconciliationPending, false);
});

test('committed partial outcomes retain prior errors and require reconciliation', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'same' }, state('same'));
  session.recordRecoveryError(entry, 'old recovery error');
  const snapshot = session.writeSnapshot(entry);
  session.applySaveOutcome(entry, snapshot, outcomeFor(snapshot, {
    fileCommitted: true, recoveryCheckpointDurable: false, selectorSessionStateDurable: false,
    reconciliationPending: true, reconciliationErrorCode: 'RECOVERY_CHECKPOINT_PENDING',
    writeRevision: snapshot.revision, savedClientEditVersion: snapshot.clientEditVersion,
  }));
  assert.equal(entry.recoveryError, 'old recovery error');
  assert.equal(entry.reconciliationPending, true);
  assert.equal(entry.reconciliationError, 'RECOVERY_CHECKPOINT_PENDING');
});

test('a recovery failure after a save request is newer than and survives its late durable acknowledgement', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'same' }, state('same'));
  session.markEdit(entry);
  const snapshot = session.writeSnapshot(entry);
  session.recordRecoveryError(entry, 'late recovery failure');
  const before = entrySnapshot(entry);
  assert.equal(session.applySaveOutcome(entry, snapshot, outcomeFor(snapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: snapshot.revision,
    savedClientEditVersion: snapshot.clientEditVersion,
  })).applied, false);
  assert.deepEqual(entrySnapshot(entry), before);
});

test('a late acknowledgement after a conflict preserves every newer conflict and dirty field', () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'same' }, state('same'));
  session.markEdit(entry);
  const snapshot = session.writeSnapshot(entry);
  session.markConflict(entry, 'newer disk conflict');
  const before = entrySnapshot(entry);
  assert.equal(session.applySaveOutcome(entry, snapshot, outcomeFor(snapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: snapshot.revision,
    savedClientEditVersion: snapshot.clientEditVersion,
  })).applied, false);
  assert.deepEqual(entrySnapshot(entry), before);
});

test('rejected revision, identity, operation, and stale outcomes mutate no entry fields', () => {
  const session = new BufferSession({ project: project() });
  const first = session.add({ relativePath: 'first.md', documentId: 1, content: 'same' }, state('same'));
  const second = session.add({ relativePath: 'second.md', documentId: 2, content: 'same' }, state('same'));
  const firstSnapshot = session.writeSnapshot(first);
  session.recordRecoveryError(second, 'current second-buffer error');
  const beforeWrongBuffer = entrySnapshot(second);
  assert.equal(session.applySaveOutcome(second, firstSnapshot, outcomeFor(firstSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: firstSnapshot.revision,
    savedClientEditVersion: firstSnapshot.clientEditVersion,
  })).applied, false);
  assert.deepEqual(entrySnapshot(second), beforeWrongBuffer);
  const current = session.add({ relativePath: 'current.md', documentId: 3, content: 'same' }, state('same'));
  const snapshot = session.writeSnapshot(current);
  for (const outcome of [
    { fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true, reconciliationPending: false, writeRevision: snapshot.revision - 1, savedClientEditVersion: snapshot.clientEditVersion },
    { fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true, reconciliationPending: false, projectId: 999, writeRevision: snapshot.revision, savedClientEditVersion: snapshot.clientEditVersion },
    { fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true, reconciliationPending: false, writeRevision: snapshot.revision, savedClientEditVersion: snapshot.clientEditVersion, operationKey: 'wrong' },
  ]) {
    const candidate = outcome.operationKey ? { ...snapshot, operationKey: 'expected' } : snapshot;
    if (candidate.operationKey) current.reconciliationOperationKey = 'expected';
    const before = entrySnapshot(current);
    assert.equal(session.applySaveOutcome(current, candidate, outcomeFor(candidate, outcome)).applied, false);
    assert.deepEqual(entrySnapshot(current), before);
  }
  const beforeMissingIdentity = entrySnapshot(current);
  const missingIdentity = session.applySaveOutcome(current, snapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: snapshot.revision, savedClientEditVersion: snapshot.clientEditVersion,
  });
  assert.deepEqual(missingIdentity, { applied: false, reason: 'identity', complete: false });
  assert.deepEqual(entrySnapshot(current), beforeMissingIdentity);
  const newerRevision = session.add({ relativePath: 'newer.md', documentId: 4, content: 'same' }, state('same'));
  const newerSnapshot = session.writeSnapshot(newerRevision);
  newerRevision.writeRevision = newerSnapshot.revision;
  const beforeRevision = entrySnapshot(newerRevision);
  assert.equal(session.applySaveOutcome(newerRevision, newerSnapshot, outcomeFor(newerSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: newerSnapshot.revision,
    savedClientEditVersion: newerSnapshot.clientEditVersion,
  })).applied, false);
  assert.deepEqual(entrySnapshot(newerRevision), beforeRevision);
  session.markEdit(first);
  session.recordRecoveryError(first, 'newer first-buffer error');
  const beforeStale = entrySnapshot(first);
  session.applySaveOutcome(first, firstSnapshot, outcomeFor(firstSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, writeRevision: firstSnapshot.revision,
    savedClientEditVersion: firstSnapshot.clientEditVersion,
  }));
  assert.deepEqual(entrySnapshot(first), beforeStale);
});

test('a matching recovery acknowledgement clears its own current recovery error', async () => {
  const session = new BufferSession({ project: project() });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'draft' }, state('draft'));
  session.markEdit(entry);
  session.recordRecoveryError(entry, 'older recovery failure');
  await session.persistRecovery(entry, async (_entry, snapshot) => acknowledged(snapshot));
  assert.equal(entry.recoveryError, null);
});

test('every current post-commit SaveOutcome boundary remains pending', () => {
  for (const code of [
    'OPERATION_JOURNAL_PENDING', 'RECOVERY_CHECKPOINT_PENDING', 'SELECTOR_PENDING', 'RESPONSE_ACK_LOST',
  ]) {
    const session = new BufferSession({ project: project() });
    const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'newer' }, state('newer'));
    session.markEdit(entry);
    const snapshot = session.writeSnapshot(entry);
    session.applySaveOutcome(entry, snapshot, outcomeFor(snapshot, {
      fileCommitted: true, recoveryCheckpointDurable: code !== 'OPERATION_JOURNAL_PENDING' && code !== 'RECOVERY_CHECKPOINT_PENDING',
      selectorSessionStateDurable: code === 'RESPONSE_ACK_LOST', reconciliationPending: true,
      reconciliationErrorCode: code, operationKey: 'c'.repeat(64), writeRevision: snapshot.revision,
      baselineHash: 'b'.repeat(64), savedClientEditVersion: snapshot.clientEditVersion,
    }));
    assert.equal(entry.reconciliationPending, true, `${code} is not rendered as plain success`);
    assert.equal(entry.reconciliationOperationKey, 'c'.repeat(64));
  }
});

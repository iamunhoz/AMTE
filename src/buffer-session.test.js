import assert from 'node:assert/strict';
import test from 'node:test';
import { BufferSession, clampRestoredSelection, recoverySequenceFrom } from './buffer-session.js';

const state = (text) => ({ doc: { toString: () => text } });
const acknowledged = (snapshot) => ({ durable: true, recoverySequence: 99, clientEditVersion: snapshot.clientEditVersion });

test('CodeMirror state is live-only while SQLite acknowledgement owns recovery sequence', async () => {
  const session = new BufferSession({ project: { projectId: 1, root: '/synthetic' } });
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
  const session = new BufferSession({ project: { projectId: 7, root: '/synthetic' }, schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: () => {} });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'A' }, state('A'));
  session.markEdit(entry); session.scheduleRecovery(entry, () => new Promise((resolve) => { release = resolve; }));
  timers[0](); await Promise.resolve(); session.markEdit(entry);
  release({ durable: true, recoverySequence: 1, clientEditVersion: 1 });
  await session.drainRecovery();
  assert.equal(entry.dirty, true); assert.ok(['error', 'pending'].includes(entry.recoveryState));
});

test('legacy sequence is read only for recovery-v1 input and UTF-16 selections clamp', () => {
  assert.equal(recoverySequenceFrom({ sequence: 11 }), 11);
  assert.equal(recoverySequenceFrom({ recoverySequence: 12 }), 12);
  assert.throws(() => recoverySequenceFrom({ recoverySequence: 1, sequence: 2 }), RangeError);
  assert.deepEqual(clampRestoredSelection({ anchor: 9, head: 12 }, 'a😀b'), { anchor: 4, head: 4 });
});

test('file and recovery lifecycles remain independently versioned', () => {
  const session = new BufferSession({ project: { projectId: 1 } });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'a' }, state('a'));
  session.markEdit(entry); const saved = session.writeSnapshot(entry);
  assert.equal(saved.clientEditVersion, entry.snapshotVersion);
  assert.equal(session.applyWriteAck(entry, saved, { writeRevision: saved.revision }), true);
  assert.equal(entry.lastDurableEditVersion, saved.clientEditVersion);
  session.markEdit(entry); assert.equal(entry.dirty, true);
  assert.equal(session.applyWriteAck(entry, saved, { writeRevision: saved.revision }), false);
});

test('a committed SaveOutcome preserves a newer edit and exposes reconciliation work', () => {
  const session = new BufferSession({ project: { projectId: 1 } });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'a' }, state('a'));
  session.markEdit(entry);
  const saved = session.writeSnapshot(entry);
  session.markEdit(entry); // The user typed again while the first save was in flight.
  session.applySaveOutcome(entry, saved, {
    fileCommitted: true, recoveryCheckpointDurable: false, reconciliationPending: true,
    reconciliationErrorCode: 'RECOVERY_CHECKPOINT_PENDING', baselineHash: 'b'.repeat(64),
    writeRevision: saved.revision,
  });
  assert.equal(entry.dirty, true, 'an old committed version cannot clear a newer edit');
  assert.equal(entry.reconciliationPending, true);
  assert.equal(entry.reconciliationSnapshot.clientEditVersion, saved.clientEditVersion);
  assert.equal(entry.reconciliationOperationKey, null);
});

test('a pending opaque reconciliation key survives a frontend restart and never clears newer text', () => {
  const key = 'a'.repeat(64);
  const session = new BufferSession({ project: { projectId: 1 } });
  const entry = session.add({ relativePath: 'a.md', documentId: 2, writeRevision: 4, content: 'newer' }, state('newer'), {
    dirty: true, clientEditVersion: 8, reconciliationPending: true,
    reconciliationOperationKey: key, reconciliationClientEditVersion: 7,
  });
  assert.equal(entry.reconciliationOperationKey, key);
  session.applySaveOutcome(entry, entry.reconciliationSnapshot, {
    fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
    reconciliationPending: false, operationKey: key, writeRevision: 7, baselineHash: 'b'.repeat(64), savedClientEditVersion: 7,
  });
  assert.equal(entry.dirty, true, 'a restarted buffer with newer recovered text remains dirty');
  assert.equal(entry.reconciliationPending, false);
  assert.equal(entry.reconciliationOperationKey, null);
});

test('manual save, autosave, Save All members, and Save As consume the same exact SaveOutcome contract', () => {
  for (const kind of ['manual', 'autosave', 'save-all:first', 'save-all:later', 'save-as']) {
    const session = new BufferSession({ project: { projectId: 1 } });
    const entry = session.add({ relativePath: `${kind}.md`, documentId: 2, content: 'old' }, state('edited'));
    session.markEdit(entry);
    const snapshot = session.writeSnapshot(entry);
    assert.equal(session.applySaveOutcome(entry, snapshot, {
      fileCommitted: true, recoveryCheckpointDurable: true, selectorSessionStateDurable: true,
      reconciliationPending: false, operationKey: 'a'.repeat(64), writeRevision: snapshot.revision,
      baselineHash: 'b'.repeat(64), savedClientEditVersion: snapshot.clientEditVersion,
    }), true, kind);
    assert.equal(entry.dirty, false, `${kind} reaches a clean file state only after fileCommitted`);
    assert.equal(entry.reconciliationPending, false, `${kind} has no false pending status`);
  }
});

test('every post-commit SaveOutcome boundary remains pending and preserves newer edits', () => {
  for (const code of [
    'OPERATION_JOURNAL_PENDING', 'RECOVERY_CHECKPOINT_PENDING', 'SELECTOR_PENDING', 'RESPONSE_ACK_LOST',
  ]) {
    const session = new BufferSession({ project: { projectId: 1 } });
    const entry = session.add({ relativePath: 'a.md', documentId: 2, content: 'old' }, state('newer'));
    session.markEdit(entry);
    const snapshot = session.writeSnapshot(entry);
    session.markEdit(entry);
    session.applySaveOutcome(entry, snapshot, {
      fileCommitted: true, recoveryCheckpointDurable: code !== 'OPERATION_JOURNAL_PENDING' && code !== 'RECOVERY_CHECKPOINT_PENDING',
      selectorSessionStateDurable: code === 'RESPONSE_ACK_LOST', reconciliationPending: true,
      reconciliationErrorCode: code, operationKey: 'c'.repeat(64), writeRevision: snapshot.revision,
      baselineHash: 'b'.repeat(64), savedClientEditVersion: snapshot.clientEditVersion,
    });
    assert.equal(entry.dirty, true, `${code} cannot clear a newer editor version`);
    assert.equal(entry.reconciliationPending, true, `${code} is not rendered as plain success`);
    assert.equal(entry.reconciliationOperationKey, 'c'.repeat(64));
  }
});

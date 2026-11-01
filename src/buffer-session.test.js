import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { BufferSession, RECOVERY_SEQUENCE_MAX, clampRestoredSelection, recoverySequenceFrom } from './buffer-session.js';

const state = (text) => ({ doc: { toString: () => text } });
test('keeps independent states, write revisions, and stale acknowledgements', () => {
  const session = new BufferSession({ project: { projectId: 1 } });
  const a = session.add({ relativePath: 'a.md', documentId: 2, content: 'a' }, state('a'));
  const b = session.add({ relativePath: 'b.md', documentId: 3, content: 'b' }, state('b'));
  session.activate('a.md'); session.markEdit(a); const saved = session.writeSnapshot(a);
  session.activate('b.md'); session.markEdit(b);
  assert.equal(session.get('a.md').state, a.state);
  assert.equal(session.applyWriteAck(a, saved, { writeRevision: saved.revision }), true);
  session.markEdit(a); assert.equal(session.applyWriteAck(a, saved, { writeRevision: saved.revision }), false);
  assert.equal(b.dirty, true);
});

test('coalesces per-buffer recovery and retains UI metadata without serializing state/history', async () => {
  const timers = []; const cancelled = new Set();
  const session = new BufferSession({ project: { projectId: 7 }, schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: (id) => cancelled.add(id) });
  const a = session.add({ relativePath: 'a.md', documentId: 1, content: 'A' }, state('A'), { selection: { anchor: 1, head: 1 }, sourceScroll: { top: 12, left: 3 }, previewing: true, previewScroll: 4, searchTerm: 'A', chapterIdentity: 'x', chapterScroll: 8, activity: { day: '2026-10-25', added: 2, deleted: 1 } });
  const b = session.add({ relativePath: 'b.md', documentId: 2, content: 'B' }, state('B'));
  session.activate('a.md'); session.markEdit(a); session.scheduleRecovery(a, async () => {}); session.scheduleRecovery(a, async () => {});
  session.markEdit(b); session.scheduleRecovery(b, async () => {});
  assert.equal(timers.length, 3); assert.equal(cancelled.has(0), true);
  await timers[1](); await timers[2]();
  assert.equal(a.recoverySequence, 1); assert.equal(b.recoverySequence, 1);
  assert.deepEqual(session.manifest().entries[0].selection, { anchor: 1, head: 1 });
  assert.equal(a.state.doc.toString(), 'A');
});

test('uses the canonical Rust recoverySequence contract and reads the legacy sequence response shape', async () => {
  const fixture = JSON.parse(readFileSync(new URL('./recovery-contract.fixture.json', import.meta.url), 'utf8'));
  assert.equal(fixture.recoverySequence, 11);
  assert.equal(Object.hasOwn(fixture, 'sequence'), false, 'serialized Rust DTO fixture must retain the canonical property name');
  assert.equal(recoverySequenceFrom(fixture), 11);
  assert.equal(recoverySequenceFrom({ sequence: 11 }), 11, 'existing version-one recovery payloads remain readable');

  const writes = [];
  const timers = [];
  const createSession = (restored) => {
    const session = new BufferSession({ project: { projectId: 7, root: '/project' }, schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: () => {} });
    const entry = session.add({ relativePath: 'chapter.md', documentId: 42, content: 'disk text' }, state('edited text'), restored);
    session.markEdit(entry);
    session.scheduleRecovery(entry, async (_entry, sequence) => {
      assert.ok(sequence > (writes.at(-1) || 0), 'backend would reject a stale recovery sequence');
      writes.push(sequence);
    });
    return { session, entry };
  };
  const first = createSession(fixture);
  await timers.shift()();
  assert.equal(first.entry.recoverySequence, 12);
  const reloaded = createSession({ sequence: first.entry.recoverySequence });
  await timers.shift()();
  assert.equal(reloaded.entry.recoverySequence, 13);
  assert.deepEqual(writes, [12, 13]);
});

test('rejects malformed, ambiguous, and overflow recovery sequences fail-closed', async () => {
  for (const restored of [
    { recoverySequence: -1 }, { recoverySequence: 1.5 }, { recoverySequence: Number.MAX_SAFE_INTEGER + 1 },
    { recoverySequence: '11' }, { sequence: -1 }, { recoverySequence: 11, sequence: 12 },
  ]) assert.throws(() => recoverySequenceFrom(restored), RangeError);
  assert.equal(recoverySequenceFrom({ recoverySequence: 0 }), 0);
  assert.equal(recoverySequenceFrom({ recoverySequence: RECOVERY_SEQUENCE_MAX }), RECOVERY_SEQUENCE_MAX);
  const session = new BufferSession({ project: { projectId: 1 } });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'a' }, state('a'), { recoverySequence: RECOVERY_SEQUENCE_MAX });
  session.markEdit(entry);
  await assert.rejects(session.persistRecovery(entry, async () => {}), RangeError);
});

test('clamps restored CodeMirror positions as UTF-16 code units using the shared Rust fixture', () => {
  const fixture = JSON.parse(readFileSync(new URL('./recovery-contract.fixture.json', import.meta.url), 'utf8')).utf16PositionFixture;
  assert.equal(fixture.text.length, fixture.length, 'JavaScript positions match the shared UTF-16 fixture');
  assert.deepEqual(clampRestoredSelection(fixture.oversizedSelection, fixture.text), { anchor: 8, head: 8 });
  assert.deepEqual(clampRestoredSelection({ anchor: 6, head: 8 }, fixture.text), { anchor: 6, head: 8 }, 'astral pair counts as two code units and combining mark as one');
  assert.equal(clampRestoredSelection({ anchor: -1, head: 0 }, fixture.text).anchor, 0);
  assert.equal(clampRestoredSelection({ anchor: 1.5, head: 0 }, fixture.text), null);
});

test('a safely rebased clean restore becomes recoverable and autosavable after an edit while dirty conflicts stay blocked', async () => {
  const session = new BufferSession({ project: { projectId: 1, root: '/project' } });
  const rebased = session.add(
    { relativePath: 'clean.md', documentId: 1, writeRevision: 4, baselineHash: 'disk-baseline', content: 'current disk' },
    state('current disk'),
    { recoverySequence: 11, dirty: false, selection: { anchor: 12, head: 12 }, sourceScroll: { top: 100, left: 0 } },
  );
  assert.equal(rebased.dirty, false);
  assert.equal(rebased.conflict, null);
  assert.equal(rebased.baselineHash, 'disk-baseline');
  session.markEdit(rebased);
  assert.deepEqual(session.autosaveEligible(), [rebased]);
  const writes = [];
  await session.persistRecovery(rebased, async (_entry, sequence) => { writes.push(sequence); });
  assert.deepEqual(writes, [12]);

  const conflicted = session.add(
    { relativePath: 'dirty.md', documentId: 2, writeRevision: 4, baselineHash: 'old-baseline', content: 'recovered draft' },
    state('recovered draft'),
    { recoverySequence: 4, dirty: true, conflict: 'CONFLICT: File changed while AMTE was closed' },
  );
  assert.equal(conflicted.state.doc.toString(), 'recovered draft');
  assert.equal(conflicted.dirty, true);
  assert.deepEqual(session.autosaveEligible(), [rebased]);

  const untitled = session.add({ content: 'untitled recovery' }, state('untitled recovery'), { recoverySequence: 7, dirty: true });
  assert.equal(untitled.baselineHash, null);
  assert.equal(untitled.documentId, null);
  assert.deepEqual(session.autosaveEligible(), [rebased], 'untitled recovery is never eligible for file autosave');
});

test('keeps collapsed chapter identities isolated in each buffer recovery manifest', () => {
  const session = new BufferSession({ project: { projectId: 7 } });
  const a = session.add({ relativePath: 'a.md', documentId: 1, content: 'A' }, state('A'), { collapsedChapterIdentities: ['h2:0123456789abcdef:1'] });
  const b = session.add({ relativePath: 'b.md', documentId: 2, content: 'B' }, state('B'), { collapsedChapterIdentities: ['h3:fedcba9876543210:2', 'h3:fedcba9876543210:2'] });
  assert.deepEqual(a.collapsedChapterIdentities, ['h2:0123456789abcdef:1']);
  assert.deepEqual(b.collapsedChapterIdentities, ['h3:fedcba9876543210:2']);
  const entries = session.manifest().entries;
  assert.deepEqual(entries[0].collapsedChapterIdentities, a.collapsedChapterIdentities);
  assert.deepEqual(entries[1].collapsedChapterIdentities, b.collapsedChapterIdentities);
});

test('suppresses discarded recovery and binds timers/in-flight writes to their original project generation', async () => {
  const timers = []; const cancelled = []; let release;
  const blocked = new Promise((resolve) => { release = resolve; });
  const session = new BufferSession({ project: { projectId: 1 }, schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: (id) => cancelled.push(id) });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'draft' }, state('draft'));
  session.markEdit(entry); const writes = [];
  session.scheduleRecovery(entry, async (_entry, _sequence, binding) => { writes.push(binding); await blocked; });
  timers[0](); await Promise.resolve();
  session.setProject({ projectId: 2 });
  release(); await session.drainRecovery();
  assert.deepEqual(writes, [{ projectId: 1, projectRoot: undefined, generation: 0, entryKey: 'a.md' }]);
  assert.equal(entry.recoverySequence, 0, 'a stale completion cannot acknowledge the old snapshot');
  const replacement = session.add({ relativePath: 'b.md', documentId: 2, content: 'b' }, state('b'));
  session.scheduleRecovery(replacement, async () => { throw new Error('pending old-project timer must be cancelled'); });
  session.setProject({ projectId: 3 });
  assert.ok(cancelled.includes(1));
  const current = session.add({ relativePath: 'c.md', documentId: 3, content: 'c' }, state('c'));
  session.suppressPersistence();
  await session.flushRecovery(async () => { throw new Error('discard must not recreate recovery'); });
  session.scheduleRecovery(current, async () => { throw new Error('discard must not schedule recovery'); });
  assert.equal(timers.length, 2);
});

test('terminal discard waits for in-flight recovery before deletion and beforeunload cannot recreate it', async () => {
  const timers = []; let release; let started;
  const startedWrite = new Promise((resolve) => { started = resolve; });
  const blocked = new Promise((resolve) => { release = resolve; });
  const session = new BufferSession({ project: { projectId: 9, root: '/old' }, schedule: (fn) => { timers.push(fn); return timers.length - 1; }, cancel: () => {} });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'draft' }, state('draft'));
  session.markEdit(entry);
  const writes = [];
  session.scheduleRecovery(entry, async () => { writes.push('started'); started(); await blocked; writes.push('finished'); });
  timers[0](); await startedWrite;
  session.suppressPersistence();
  let drained = false; const drain = session.drainRecovery().then(() => { drained = true; });
  await Promise.resolve(); assert.equal(drained, false);
  release(); await drain;
  writes.push('deleted');
  await session.flushRecovery(async () => writes.push('recreated'));
  assert.deepEqual(writes, ['started', 'finished', 'deleted']);
});

test('a rejected recovery write reports the failure and retains dirty content', async () => {
  const session = new BufferSession({ project: { projectId: 3, root: '/project' } });
  const entry = session.add({ relativePath: 'a.md', documentId: 1, content: 'draft' }, state('draft'));
  session.markEdit(entry);
  await assert.rejects(session.persistRecovery(entry, async () => { throw new Error('Recovery aggregate limit exceeded; the latest snapshot was not recorded'); }));
  assert.equal(entry.dirty, true);
  assert.match(entry.recoveryError, /aggregate limit/);
  assert.equal(entry.recoverySequence, 0);
});

test('close failure after discard restores all dirty-buffer recovery and allows retry', async () => {
  const session = new BufferSession({ project: { projectId: 4, root: '/project' } });
  const a = session.add({ relativePath: 'a.md', documentId: 1, content: 'a' }, state('a'));
  const b = session.add({ relativePath: 'b.md', documentId: 2, content: 'b' }, state('b'));
  session.markEdit(a); session.markEdit(b); session.suppressPersistence();
  await session.drainRecovery(); // recovery was deleted while close was attempted
  session.resumePersistence();
  const restored = [];
  await session.flushRecovery(async (entry) => { restored.push(entry.relativePath); return {}; });
  assert.deepEqual(restored.sort(), ['a.md', 'b.md']);
  assert.equal(a.dirty && b.dirty, true);
  assert.equal(session.persistenceSuppressed, false);
});

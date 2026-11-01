import assert from 'node:assert/strict';
import test from 'node:test';
import { createSaveIndicatorController, SAVE_INDICATOR_DURATION_MS } from './save-indicator.js';

function fakeClock() {
  let now = 0;
  let nextId = 0;
  const tasks = new Map();
  return {
    setTimeout(callback, delay) { const id = ++nextId; tasks.set(id, { callback, due: now + delay }); return id; },
    clearTimeout(id) { tasks.delete(id); },
    advance(milliseconds) {
      const end = now + milliseconds;
      while (true) {
        const next = [...tasks.entries()].filter(([, task]) => task.due <= end).sort((left, right) => left[1].due - right[1].due)[0];
        if (!next) break;
        const [id, task] = next;
        now = task.due;
        tasks.delete(id);
        task.callback();
      }
      now = end;
    },
    get pending() { return tasks.size; },
  };
}

function statusElement() {
  const icon = { textContent: '·' }; const label = { textContent: 'No save activity' };
  return {
    hidden: true, dataset: {}, attributes: {}, icon, label,
    setAttribute(name, value) { this.attributes[name] = value; },
    querySelector(selector) { return selector === '[data-save-icon]' ? icon : (selector === '[data-save-label]' ? label : null); },
  };
}

test('save confirmation is initially hidden, lasts exactly five seconds, and resets from the latest success', () => {
  const clock = fakeClock();
  const element = { hidden: true };
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  assert.equal(element.hidden, true);

  indicator.show();
  clock.advance(SAVE_INDICATOR_DURATION_MS - 1);
  assert.equal(element.hidden, false);
  clock.advance(1);
  assert.equal(element.hidden, true);

  indicator.show();
  clock.advance(4000);
  indicator.show();
  clock.advance(4999);
  assert.equal(element.hidden, false);
  clock.advance(1);
  assert.equal(element.hidden, true);
});

test('a fully durable file acknowledgement is visible immediately through 4999ms and hides at 5000ms', () => {
  const clock = fakeClock();
  const element = { hidden: true, dataset: {}, attributes: {}, setAttribute(name, value) { this.attributes[name] = value; } };
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  assert.equal(indicator.update({ fileSaved: true }), 'fileSaved');
  assert.equal(element.hidden, false);
  assert.match(element.attributes['aria-label'], /arquivo salvo/i);
  clock.advance(4999);
  assert.equal(indicator.status, 'fileSaved');
  assert.equal(element.hidden, false);
  clock.advance(1);
  assert.equal(element.hidden, true);
});

test('only accepted write completions notify; failed, canceled, conflicted, stale, and recovery writes do not', () => {
  const clock = fakeClock();
  const element = { hidden: true };
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  const notifyAfterAcceptedProjectWrite = ({ succeeded, accepted, recovery = false }) => {
    if (succeeded && accepted && !recovery) indicator.show();
  };

  for (const write of [
    { succeeded: false, accepted: true }, // failure
    { succeeded: false, accepted: false }, // canceled Save As
    { succeeded: true, accepted: false }, // conflict/stale completion
    { succeeded: true, accepted: true, recovery: true },
  ]) notifyAfterAcceptedProjectWrite(write);
  assert.equal(element.hidden, true);

  // Manual save, first Save As, inactive-buffer autosave, and Save All all
  // share the accepted project-write notifier.
  for (const write of [
    { succeeded: true, accepted: true, kind: 'manual' },
    { succeeded: true, accepted: true, kind: 'save-as' },
    { succeeded: true, accepted: true, kind: 'inactive-autosave' },
    { succeeded: true, accepted: true, kind: 'save-all' },
  ]) notifyAfterAcceptedProjectWrite(write);
  assert.equal(element.hidden, false);
  assert.equal(clock.pending, 1, 'a batch leaves one final timer');
});

test('teardown clears the pending save timer and hides the status', () => {
  const clock = fakeClock();
  const element = { hidden: true };
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  indicator.show();
  indicator.destroy();
  assert.equal(element.hidden, true);
  assert.equal(clock.pending, 0);
  clock.advance(SAVE_INDICATOR_DURATION_MS);
  assert.equal(element.hidden, true);
});

test('a global floppy pulse preserves persistent active-buffer status through expiry', () => {
  const clock = fakeClock(); const element = statusElement();
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  indicator.update({ recoveryDurable: true });
  indicator.show();
  assert.equal(indicator.status, 'recoveryDurable');
  assert.match(element.attributes['aria-label'], /rascunho protegido/i);
  assert.equal(element.icon.textContent, '✓');
  clock.advance(SAVE_INDICATOR_DURATION_MS);
  assert.equal(indicator.status, 'recoveryDurable');
  assert.equal(element.icon.textContent, '◉');
});

test('inactive autosave, Save All, and Save As pulses never replace active conflict text or AX state', () => {
  const clock = fakeClock(); const element = statusElement();
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  indicator.update({ conflict: true });
  const activeLabel = element.attributes['aria-label'];
  for (const kind of ['inactive-autosave', 'inactive-save-all', 'inactive-save-as']) {
    indicator.show();
    assert.equal(indicator.status, 'conflict', kind);
    assert.equal(element.attributes['aria-label'], activeLabel, kind);
    assert.equal(element.label.textContent, activeLabel, kind);
    assert.equal(element.icon.textContent, '✓', kind);
  }
  clock.advance(4999);
  assert.equal(element.icon.textContent, '✓');
  clock.advance(1);
  assert.equal(indicator.status, 'conflict');
  assert.equal(element.attributes['aria-label'], activeLabel);
  assert.equal(element.icon.textContent, '!');
});

test('committed-file reconciliation is persistent and never rendered as plain success', () => {
  const clock = fakeClock(); const element = { hidden: true, dataset: {} };
  const indicator = createSaveIndicatorController({ element, setTimeoutFn: clock.setTimeout.bind(clock), clearTimeoutFn: clock.clearTimeout.bind(clock) });
  indicator.update({ fileSaved: true, reconciliationPending: true });
  assert.equal(indicator.status, 'reconciliationPending');
  clock.advance(SAVE_INDICATOR_DURATION_MS);
  assert.equal(indicator.status, 'reconciliationPending');
  indicator.update({ reconciliationError: true });
  assert.equal(indicator.status, 'reconciliationError');
});

test('committed reconciliation states outrank residual channel failures and remain specific', () => {
  const element = { hidden: true, dataset: {}, attributes: {}, setAttribute(name, value) { this.attributes[name] = value; } };
  const indicator = createSaveIndicatorController({ element });
  indicator.update({ fileSaveError: true, recoveryError: true, reconciliationPending: true });
  assert.equal(indicator.status, 'reconciliationPending');
  assert.match(element.attributes['aria-label'], /arquivo salvo; recuperação pendente/i);
  indicator.update({ fileSaveError: true, recoveryError: true, reconciliationError: true });
  assert.equal(indicator.status, 'reconciliationError');
  assert.match(element.attributes['aria-label'], /arquivo salvo; recuperação com erro/i);
});

test('accessible labels distinguish file-save failure, recovery failure, and conflict', () => {
  const element = { hidden: true, dataset: {}, attributes: {}, setAttribute(name, value) { this.attributes[name] = value; } };
  const indicator = createSaveIndicatorController({ element });
  indicator.update({ fileSaveError: true });
  assert.equal(indicator.status, 'fileSaveError');
  assert.match(element.attributes['aria-label'], /falha ao salvar arquivo/i);
  indicator.update({ recoveryError: true });
  assert.equal(indicator.status, 'recoveryError');
  assert.match(element.attributes['aria-label'], /falha na recuperação/i);
  indicator.update({ conflict: true });
  assert.equal(indicator.status, 'conflict');
  assert.match(element.attributes['aria-label'], /conflito no arquivo/i);
});

test('reconciliation status remains exposed through an accessible label', () => {
  const element = { hidden: true, dataset: {}, attributes: {}, setAttribute(name, value) { this.attributes[name] = value; } };
  const indicator = createSaveIndicatorController({ element });
  indicator.update({ reconciliationPending: true });
  assert.match(element.attributes['aria-label'], /recuperação pendente/i);
  indicator.update({ reconciliationError: true });
  assert.match(element.attributes['aria-label'], /recuperação com erro/i);
});

import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  applySavedBaseline,
  decideLoadApply,
  decideSaveApply,
  resolveRefreshBaseline,
  settleLoad,
  settleSave,
  type CurrentEditor,
  type SaveIdentity,
} from './saveApply.ts';
import type { DocumentView } from './types.ts';

function delay<T>(ms: number, value: T): Promise<T> {
  return new Promise((resolve) => setTimeout(() => resolve(value), ms));
}

const savedA: SaveIdentity = { docId: 'settings.user', txn: 3, draftRev: 1 };

test('same document and unchanged draft reloads the editor', () => {
  assert.equal(
    decideSaveApply(savedA, { docId: 'settings.user', txn: 3, draftRev: 1 }),
    'reload'
  );
});

test('switching documents ignores the editor even if draft matches', () => {
  assert.equal(
    decideSaveApply(savedA, { docId: 'mcp.user', txn: 4, draftRev: 1 }),
    'ignore-editor'
  );
});

test('returning to the saved document after a switch still ignores (new load txn)', () => {
  assert.equal(
    decideSaveApply(savedA, { docId: 'settings.user', txn: 5, draftRev: 0 }),
    'ignore-editor'
  );
});

test('further edits on the same document keep the live draft', () => {
  assert.equal(
    decideSaveApply(savedA, { docId: 'settings.user', txn: 3, draftRev: 2 }),
    'keep-edits'
  );
});

test('delayed save samples identity at completion, not at start', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };

  const pending = settleSave({
    saved: savedA,
    run: () => delay(40, { version: 'v2' }),
    current: () => ({ ...current }),
  });

  current.docId = 'mcp.user';
  current.txn = 4;
  current.draftRev = 0;

  const settled = await pending;
  assert.equal(settled.action, 'ignore-editor');
  assert.deepEqual(settled.result, { version: 'v2' });
});

test('delayed save on the same document keeps later typing', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };

  const pending = settleSave({
    saved: savedA,
    run: () => delay(40, { version: 'v2' }),
    current: () => ({ ...current }),
  });

  current.draftRev = 4;

  const settled = await pending;
  assert.equal(settled.action, 'keep-edits');
});

test('a late save that does not abort still cannot reload after a newer save reload', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };

  const older = settleSave({
    saved: { docId: 'settings.user', txn: 3, draftRev: 1 },
    run: () => delay(40, { version: 'old' }),
    current: () => ({ ...current }),
  });
  const newer = settleSave({
    saved: { docId: 'settings.user', txn: 3, draftRev: 1 },
    run: () => delay(5, { version: 'new' }),
    current: () => ({ ...current }),
  });

  const first = await newer;
  assert.equal(first.action, 'reload');
  // Successful apply reloads the document and therefore bumps the load txn.
  current.txn = 4;
  current.draftRev = 0;

  const late = await older;
  assert.equal(late.action, 'ignore-editor');
});

function stubView(id: string, version: string, content: string): DocumentView {
  return {
    document: {
      id,
      module: 'settings',
      scope: 'global',
      title: 'settings',
      path: 'settings.json',
      project_root: null,
      exists: true,
      format: 'json',
      capabilities: { write: true, create: true, form: true, source: true, restore: true },
      sensitive: false,
      effect: 'new-session',
      effect_note: '',
      managed_by: 'one',
      read_only_reason: null,
      precedence: 0,
      override_note: null,
    },
    version,
    content,
    masked: false,
    unlocked: false,
    parsed: JSON.parse(content),
    diagnostics: [],
    form: null,
    read_error: null,
  };
}

test('refresh GET with unchanged draft replaces the editor', () => {
  assert.equal(
    decideLoadApply({ seq: 4, draftRev: 1, refresh: true }, { seq: 4, draftRev: 1 }),
    'replace'
  );
});

test('refresh GET after typing keeps the live draft', () => {
  assert.equal(
    decideLoadApply({ seq: 4, draftRev: 1, refresh: true }, { seq: 4, draftRev: 2 }),
    'baseline-only'
  );
});

test('a newer document load ignores the in-flight refresh', () => {
  assert.equal(
    decideLoadApply({ seq: 4, draftRev: 1, refresh: true }, { seq: 5, draftRev: 0 }),
    'ignore'
  );
});

test('intentional loads still replace even if the user typed during GET', () => {
  assert.equal(
    decideLoadApply({ seq: 4, draftRev: 1, refresh: false }, { seq: 4, draftRev: 9 }),
    'replace'
  );
});

test('POST success then GET pending then typing keeps the live draft', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };
  let loadSeq = 3;

  const { action: saveAction } = await settleSave({
    saved: savedA,
    run: () => delay(5, { version: 'v2' }),
    current: () => ({ ...current }),
  });
  assert.equal(saveAction, 'reload');

  const started = { seq: ++loadSeq, draftRev: current.draftRev, refresh: true };
  const pendingGet = settleLoad({
    started,
    run: () => delay(40, stubView('settings.user', 'v2', '{"saved":true}')),
    current: () => ({ seq: loadSeq, draftRev: current.draftRev }),
  });

  current.draftRev = 6;

  const loaded = await pendingGet;
  assert.equal(loaded.action, 'baseline-only');
  assert.equal(loaded.result.version, 'v2');
});

test('POST success then GET with no further typing replaces the editor', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };
  let loadSeq = 3;

  const { action: saveAction } = await settleSave({
    saved: savedA,
    run: () => delay(5, { version: 'v2' }),
    current: () => ({ ...current }),
  });
  assert.equal(saveAction, 'reload');

  const loaded = await settleLoad({
    started: { seq: ++loadSeq, draftRev: current.draftRev, refresh: true },
    run: () => delay(10, stubView('settings.user', 'v2', '{"saved":true}')),
    current: () => ({ seq: loadSeq, draftRev: current.draftRev }),
  });
  assert.equal(loaded.action, 'replace');
});

test('POST success then GET pending then switching documents ignores the refresh', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };
  let loadSeq = 3;

  const { action: saveAction } = await settleSave({
    saved: savedA,
    run: () => delay(5, { version: 'v2' }),
    current: () => ({ ...current }),
  });
  assert.equal(saveAction, 'reload');

  const started = { seq: ++loadSeq, draftRev: current.draftRev, refresh: true };
  const pendingGet = settleLoad({
    started,
    run: () => delay(40, stubView('settings.user', 'v2', '{"saved":true}')),
    current: () => ({ seq: loadSeq, draftRev: current.draftRev }),
  });

  current.docId = 'mcp.user';
  loadSeq += 1;
  current.txn = loadSeq;
  current.draftRev = 0;

  const loaded = await pendingGet;
  assert.equal(loaded.action, 'ignore');
});

test('POST v2 GET v3 with local typing keeps v2 as the next-save origin', async () => {
  const current: CurrentEditor = { docId: 'settings.user', txn: 3, draftRev: 1 };
  let loadSeq = 3;
  const origin = {
    version: 'v2',
    draft: '{"provider":"openai","model":"m1"}',
  };

  const { action: saveAction, result: saved } = await settleSave({
    saved: savedA,
    run: () => delay(5, { version: 'v2' }),
    current: () => ({ ...current }),
  });
  assert.equal(saveAction, 'reload');
  assert.equal(saved.version, origin.version);

  const pendingGet = settleLoad({
    started: { seq: ++loadSeq, draftRev: current.draftRev, refresh: true },
    run: () =>
      delay(
        40,
        stubView('settings.user', 'v3', '{"provider":"anthropic","model":"m1"}')
      ),
    current: () => ({ seq: loadSeq, draftRev: current.draftRev }),
  });

  current.draftRev = 6;

  const loaded = await pendingGet;
  assert.equal(loaded.action, 'baseline-only');
  assert.equal(loaded.result.version, 'v3');

  const prev = stubView('settings.user', 'v1', '{"provider":"openai","model":"m1"}');
  const resolved = resolveRefreshBaseline({
    prev,
    fetched: loaded.result,
    origin,
  });
  assert.equal(resolved.conflict, true);
  assert.equal(resolved.view?.version, 'v2');
  assert.equal(resolved.view?.content, origin.draft);
  assert.notEqual(resolved.view?.version, loaded.result.version);
});

test('refresh baseline matches POST when GET is still that write', () => {
  const origin = { version: 'v2', draft: '{"provider":"openai","model":"m1"}' };
  const prev = stubView('settings.user', 'v1', '{"provider":"openai","model":"m0"}');
  const fetched = stubView('settings.user', 'v2', '{"provider":"openai","model":"m1"}');
  const resolved = resolveRefreshBaseline({ prev, fetched, origin });
  assert.equal(resolved.conflict, false);
  assert.equal(resolved.view?.version, 'v2');
  assert.equal(resolved.view?.content, origin.draft);
});

test('keep-edits advances version without replacing a different document', () => {
  const view = stubView('settings.user', 'v1', '{"a":1}');
  const next = applySavedBaseline(view, 'settings.user', 'v2', '{"a":1,"b":2}');
  assert.equal(next?.version, 'v2');
  assert.equal(next?.content, '{"a":1,"b":2}');
  assert.deepEqual(next?.parsed, { a: 1, b: 2 });

  const other = applySavedBaseline(view, 'mcp.user', 'v9', '{}');
  assert.equal(other, view);
});

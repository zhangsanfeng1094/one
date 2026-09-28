import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createRequestGate, runValidateRequest } from './validateGate.ts';

function delay<T>(ms: number, value: T, signal?: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => resolve(value), ms);
    signal?.addEventListener('abort', () => {
      clearTimeout(timer);
      const err = new Error('The operation was aborted.');
      err.name = 'AbortError';
      reject(err);
    });
  });
}

test('out-of-order responses keep the newer draft', async () => {
  const gate = createRequestGate();
  const applied: string[] = [];
  let validating = false;

  const old = runValidateRequest({
    gate,
    fingerprint: 'old-draft',
    run: (signal) => delay(40, { ok: 'old' }, signal),
    onSuccess: (_result, fingerprint) => applied.push(fingerprint),
    onError: () => applied.push('old-error'),
    onFinally: () => {
      validating = false;
    },
  });
  validating = true;

  const newer = runValidateRequest({
    gate,
    fingerprint: 'new-draft',
    run: (signal) => delay(5, { ok: 'new' }, signal),
    onSuccess: (_result, fingerprint) => applied.push(fingerprint),
    onError: () => applied.push('new-error'),
    onFinally: () => {
      validating = false;
    },
  });

  await Promise.allSettled([old, newer]);
  assert.deepEqual(applied, ['new-draft']);
  assert.equal(validating, false);
});

test('switching documents drops the in-flight result', async () => {
  const gate = createRequestGate();
  const applied: string[] = [];
  let validating = true;

  const pending = runValidateRequest({
    gate,
    fingerprint: 'doc-a',
    run: (signal) => delay(30, { ok: 'a' }, signal),
    onSuccess: (_result, fingerprint) => applied.push(fingerprint),
    onError: () => applied.push('error'),
    onFinally: () => {
      validating = false;
    },
  });

  gate.cancel();
  await pending;
  assert.deepEqual(applied, []);
  assert.equal(validating, true, 'stale finally must not clear validating');
});

test('stale error does not replace a newer success', async () => {
  const gate = createRequestGate();
  const applied: string[] = [];

  const failing = runValidateRequest({
    gate,
    fingerprint: 'old-draft',
    run: async (signal) => {
      await delay(40, null, signal);
      throw new Error('old failed');
    },
    onSuccess: () => applied.push('old-success'),
    onError: () => applied.push('old-error'),
    onFinally: () => applied.push('old-finally'),
  });

  const newer = runValidateRequest({
    gate,
    fingerprint: 'new-draft',
    run: (signal) => delay(5, { ok: 'new' }, signal),
    onSuccess: (_result, fingerprint) => applied.push(fingerprint),
    onError: () => applied.push('new-error'),
    onFinally: () => applied.push('new-finally'),
  });

  await Promise.allSettled([failing, newer]);
  assert.deepEqual(applied, ['new-draft', 'new-finally']);
});

/**
 * Serializes in-flight validate requests so a slower older response cannot
 * overwrite a newer one. Auto-debounce and the manual button must share one gate.
 */

export type RequestGate = {
  begin: () => { seq: number; signal: AbortSignal; isCurrent: () => boolean };
  cancel: () => void;
};

/** Abort + monotonic sequence. `isCurrent` is what stale `.then` callbacks check. */
export function createRequestGate(): RequestGate {
  let seq = 0;
  let controller: AbortController | null = null;

  return {
    begin() {
      controller?.abort();
      controller = new AbortController();
      const requestSeq = ++seq;
      const signal = controller.signal;
      return {
        seq: requestSeq,
        signal,
        isCurrent: () => requestSeq === seq,
      };
    },
    cancel() {
      controller?.abort();
      controller = null;
      seq += 1;
    },
  };
}

function isAbortError(err: unknown): boolean {
  return (err instanceof DOMException || err instanceof Error) && err.name === 'AbortError';
}

/**
 * Run one validate call through `gate`. Success, error, and finally only fire
 * when this invocation is still the live request.
 */
export async function runValidateRequest<T>(opts: {
  gate: RequestGate;
  run: (signal: AbortSignal) => Promise<T>;
  fingerprint: string;
  onSuccess: (result: T, fingerprint: string) => void;
  onError: (err: unknown) => void;
  onFinally: () => void;
}): Promise<void> {
  const { signal, isCurrent } = opts.gate.begin();
  try {
    const result = await opts.run(signal);
    if (!isCurrent()) return;
    opts.onSuccess(result, opts.fingerprint);
  } catch (err) {
    if (!isCurrent() || isAbortError(err)) return;
    opts.onError(err);
  } finally {
    if (isCurrent()) opts.onFinally();
  }
}

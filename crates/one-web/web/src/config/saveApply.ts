/**
 * Decide whether a completed save may refresh the editor.
 *
 * The HTTP save itself is not aborted when the user navigates: the write must
 * finish. Only the follow-up `loadDocument` is gated, so a slower save of A
 * cannot clobber B's editor (or later edits of A).
 */

import type { DocumentView } from './types';

export type SaveIdentity = {
  docId: string;
  /** `loadSeq` at confirm: document switches bump this. */
  txn: number;
  /** Bumped on each local draft edit after the document was loaded. */
  draftRev: number;
};

export type CurrentEditor = {
  docId: string | null;
  txn: number;
  draftRev: number;
};

export type SaveApplyAction = 'reload' | 'keep-edits' | 'ignore-editor';

export function decideSaveApply(saved: SaveIdentity, current: CurrentEditor): SaveApplyAction {
  if (current.txn !== saved.txn || current.docId !== saved.docId) {
    return 'ignore-editor';
  }
  if (current.draftRev !== saved.draftRev) {
    return 'keep-edits';
  }
  return 'reload';
}

/** Sample identity at response time so a delayed save cannot use a stale closure. */
export async function settleSave<T>(opts: {
  saved: SaveIdentity;
  run: () => Promise<T>;
  current: () => CurrentEditor;
}): Promise<{ result: T; action: SaveApplyAction }> {
  const result = await opts.run();
  return { result, action: decideSaveApply(opts.saved, opts.current()) };
}

/** The write this refresh is following. Baseline may not advance past this. */
export type RefreshOrigin = {
  version: string;
  draft: string;
};

export type LoadStart = {
  seq: number;
  draftRev: number;
  /** Post-write refresh: keep live typing if `draftRev` moved during GET. */
  refresh: boolean;
};

export type LoadApplyAction = 'replace' | 'baseline-only' | 'ignore';

export function decideLoadApply(
  started: LoadStart,
  current: { seq: number; draftRev: number }
): LoadApplyAction {
  if (current.seq !== started.seq) return 'ignore';
  if (started.refresh && current.draftRev !== started.draftRev) return 'baseline-only';
  return 'replace';
}

/** Sample load identity at GET completion so a hung refresh cannot wipe later typing. */
export async function settleLoad<T>(opts: {
  started: LoadStart;
  run: () => Promise<T>;
  current: () => { seq: number; draftRev: number };
}): Promise<{ result: T; action: LoadApplyAction }> {
  const result = await opts.run();
  return { result, action: decideLoadApply(opts.started, opts.current()) };
}

/**
 * After a save that raced with further typing, keep the live draft and only
 * advance the OCC baseline so the next save is not a 409 against the old version.
 */
export function applySavedBaseline(
  view: DocumentView | null,
  docId: string,
  version: string,
  savedDraft: string
): DocumentView | null {
  if (!view || view.document.id !== docId) return view;
  let parsed = view.parsed;
  try {
    parsed = JSON.parse(savedDraft);
  } catch {
    // Source drafts may not be JSON.
  }
  return { ...view, version, content: savedDraft, parsed };
}

/**
 * After a post-save GET, never adopt `fetched.version` onto an unmerged draft.
 * The OCC baseline is the write we just performed. A later on-disk version is a
 * conflict, not a new origin for the next save.
 */
export function resolveRefreshBaseline(opts: {
  prev: DocumentView | null;
  fetched: DocumentView;
  origin: RefreshOrigin;
}): { view: DocumentView | null; conflict: boolean } {
  if (!opts.prev || opts.prev.document.id !== opts.fetched.document.id) {
    return { view: opts.prev, conflict: false };
  }
  return {
    view: applySavedBaseline(
      opts.prev,
      opts.fetched.document.id,
      opts.origin.version,
      opts.origin.draft
    ),
    conflict: opts.fetched.version !== opts.origin.version,
  };
}

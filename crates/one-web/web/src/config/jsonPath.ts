/**
 * Dot-path helpers for editing a JSON document immutably.
 *
 * The server describes editable fields as dot paths (`compaction.ratio`,
 * `mcpServers`), so form controls read and write values by path rather than
 * holding their own copies. Edits produce a new tree so React sees a change, and
 * every untouched key — including ones the studio does not model — is preserved.
 */

export type Json = unknown;

export function isPlainObject(value: Json): value is Record<string, Json> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Read a dot path, returning `undefined` when any segment is missing. */
export function getPath(root: Json, path: string): Json {
  if (!path) return root;
  let cursor: Json = root;
  for (const segment of path.split('.')) {
    if (!isPlainObject(cursor) && !Array.isArray(cursor)) return undefined;
    cursor = (cursor as Record<string, Json>)[segment];
    if (cursor === undefined) return undefined;
  }
  return cursor;
}

function cloneContainer(value: Json): Json {
  if (Array.isArray(value)) return [...value];
  if (isPlainObject(value)) return { ...value };
  return {};
}

/**
 * Write a dot path immutably.
 *
 * Passing `undefined` deletes the key, which is how "unset this field" is
 * expressed — including clearing a stored secret.
 */
export function setPath(root: Json, path: string, value: Json): Json {
  const segments = path.split('.');
  const next = cloneContainer(root);
  let cursor: Json = next;

  for (let i = 0; i < segments.length - 1; i += 1) {
    const segment = segments[i];
    const child = (cursor as Record<string, Json>)[segment];
    const cloned = cloneContainer(child);
    (cursor as Record<string, Json>)[segment] = cloned;
    cursor = cloned;
  }

  const last = segments[segments.length - 1];
  if (value === undefined) {
    delete (cursor as Record<string, Json>)[last];
  } else {
    (cursor as Record<string, Json>)[last] = value;
  }
  return next;
}

/** Stable stringify used for dirty checks and display. */
export function prettyJson(value: Json): string {
  return `${JSON.stringify(value ?? null, null, 2)}\n`;
}

/** Render any scalar for readonly display. */
export function displayValue(value: Json): string {
  if (value === undefined) return '(未设置)';
  if (value === null) return 'null';
  if (typeof value === 'string') return value;
  return JSON.stringify(value);
}

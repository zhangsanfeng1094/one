/**
 * Typed client for `/api/config/*`.
 *
 * Access model: the server prints a URL containing a one-time token. We read it
 * from the query string, move it into `sessionStorage`, and immediately rewrite
 * the visible URL so the token does not linger in the address bar, in browser
 * history entries the user copies, or in a `Referer` header. `sessionStorage`
 * (not `localStorage`) means it disappears with the tab rather than persisting
 * on disk across browser restarts.
 */

import type {
  BackupRecord,
  BackupsResponse,
  CatalogResponse,
  DocumentView,
  DraftView,
  EffectiveReport,
  ModuleId,
  OverrideResponse,
  SaveResponse,
  ValidateResponse,
} from './types';

const TOKEN_STORAGE_KEY = 'one-config-token';
const PREFIX = '/api/config';

let token = '';

/** Read the token from the URL (once) or from this tab's session storage. */
export function initToken(): boolean {
  const params = new URLSearchParams(window.location.search);
  const fromUrl = params.get('token');
  if (fromUrl) {
    token = fromUrl;
    try {
      sessionStorage.setItem(TOKEN_STORAGE_KEY, fromUrl);
    } catch {
      // Storage may be unavailable (private mode); the in-memory token still works.
    }
    params.delete('token');
    const query = params.toString();
    window.history.replaceState(
      {},
      '',
      window.location.pathname + (query ? `?${query}` : '')
    );
    return true;
  }
  try {
    token = sessionStorage.getItem(TOKEN_STORAGE_KEY) ?? '';
  } catch {
    token = '';
  }
  return token !== '';
}

/** Error carrying the HTTP status and any structured diagnostics. */
export class ApiError extends Error {
  readonly status: number;
  readonly diagnostics: ValidateResponse['diagnostics'];

  constructor(status: number, message: string, diagnostics: ValidateResponse['diagnostics'] = []) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.diagnostics = diagnostics;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (token) {
    headers.set('X-One-Config-Token', token);
  }
  if (init?.body !== undefined) {
    headers.set('Content-Type', 'application/json');
  }

  const response = await fetch(`${PREFIX}${path}${path.includes('?') ? '&' : '?'}studio_global=1`, { ...init, headers });
  if (init?.signal?.aborted) {
    throw new DOMException('The operation was aborted.', 'AbortError');
  }
  const text = await response.text();

  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      payload = null;
    }
  }

  if (!response.ok) {
    const record = (payload ?? {}) as { error?: string; diagnostics?: ValidateResponse['diagnostics'] };
    const message =
      typeof record.error === 'string'
        ? record.error
        : `${response.status} ${response.statusText}`;
    throw new ApiError(response.status, message, record.diagnostics ?? []);
  }

  return payload as T;
}

function post<T>(path: string, body: unknown, signal?: AbortSignal): Promise<T> {
  return request<T>(path, { method: 'POST', body: JSON.stringify(body), signal });
}

/** True when a fetch was cancelled by AbortController. */
export function isAbortError(err: unknown): boolean {
  return (err instanceof DOMException || err instanceof Error) && err.name === 'AbortError';
}

/** Fetch the catalog: context, modules, and every known document. */
export const fetchCatalog = () => request<CatalogResponse>('/catalog');

/** Fetch one document. `unlock` reveals secrets and must be an explicit choice. */
export const fetchDocument = (id: string, unlock: boolean, signal?: AbortSignal) =>
  request<DocumentView>(`/documents/${encodeURIComponent(id)}${unlock ? '?unlock=1' : ''}`, {
    signal,
  });

/** Validate a draft and obtain a masked change preview. */
export const validateDraft = (
  id: string,
  draft: string,
  view: DraftView,
  version: string,
  signal?: AbortSignal
) =>
  post<ValidateResponse>(
    `/documents/${encodeURIComponent(id)}/validate`,
    { draft, view, version },
    signal
  );

/**
 * Validate a draft and obtain a masked change preview.
 *
 * `unlocked` must be true when saving a sensitive document from the source view;
 * form saves rely on server-side sentinel restoration instead.
 */
export const saveDocument = (
  id: string,
  draft: string,
  version: string,
  view: DraftView,
  unlocked: boolean
) =>
  post<SaveResponse>(`/documents/${encodeURIComponent(id)}/save`, {
    draft,
    version,
    view,
    confirm: true,
    unlocked,
  });

/** List backups for a document. */
export const fetchBackups = (id: string) =>
  request<BackupsResponse>(`/documents/${encodeURIComponent(id)}/backups`);

/** Restore a document from a backup through the normal validation path. */
export const restoreBackup = (id: string, backup: string, version: string) =>
  post<SaveResponse>(`/documents/${encodeURIComponent(id)}/restore`, { backup, version });

/** Resolve the effective configuration for a module. */
export const fetchEffective = (module: ModuleId) =>
  request<EffectiveReport>(`/effective?module=${encodeURIComponent(module)}`);

/** Copy the winning MCP entry for `server` into a project document. */
export const createMcpOverride = (server: string, targetDoc: string) =>
  post<OverrideResponse>('/mcp/override', { server, target_doc: targetDoc });

export interface PromptPreviewResponse {
  preset: string;
  provider: string;
  model: string;
  quirks: string[];
  compiled_prompt?: string;
  error?: string;
  matched_rules?: unknown[];
  slots?: { slot: string; component: string; operations: number; emitted: boolean; text?: string; trace: { source: string; operation: string; rule: number | null }[] }[];
  enhancers?: ResolvedEnhancer[];
  matches?: EnhancerMatch[];
  context?: { note: string; tools: string[]; cwd: string; mode: string };
}

export const fetchPromptPreview = (preset: string, provider: string, model: string, document?: string) =>
  request<PromptPreviewResponse>(
    `/prompts/preview?preset=${encodeURIComponent(preset)}&provider=${encodeURIComponent(provider)}&model=${encodeURIComponent(model)}${document ? `&document=${encodeURIComponent(document)}` : ''}`
  );

export type { BackupRecord };

export interface EnhancerOverride { enabled?: boolean; prompt?: string }
export interface BoundModel { provider: string; model: string }
export interface EnhancerCatalogItem {
  id: string; name: string; title: string; description: string; kind: 'builtin' | 'custom' | string;
  enabled: boolean; prompt: string; default_prompt: string;
  models: BoundModel[]; agents: string[]; all_agents: boolean;
}
export interface EnhancerMatch {
  id: string; name: string; title: string; kind: string; active: boolean; reasons: string[];
}
export interface ResolvedEnhancer {
  id: string; name: string; description: string; hook: string;
  default_prompt: string; prompt: string; enabled: boolean;
  prompt_source: string; binding_source: string; kind?: string;
}
export interface EnhancerView {
  models: { provider: string; id: string; name: string; quirks?: string[] }[];
  presets: string[];
  agents?: { name: string; preset: string }[];
  enhancers: ResolvedEnhancer[];
  catalog?: EnhancerCatalogItem[];
  matches?: EnhancerMatch[];
  config: { version: number; definitions?: { id: string; name: string; description?: string; prompt: string }[]; bindings: { provider: string; model: string; preset?: string; enhancers: Record<string, EnhancerOverride> }[] };
  version: string; path: string; effect_note: string;
}
export const fetchEnhancers = (scope: string, provider: string, model: string, preset: string) =>
  request<EnhancerView>(`/prompts/enhancers?${new URLSearchParams({ scope, provider, model, preset })}`);
export const saveEnhancer = (edit: {
  scope: string; provider: string; model: string; preset?: string;
  id: string; version: string; enabled?: boolean; prompt?: string; reset?: boolean;
}) => post<SaveResponse>('/prompts/enhancers', edit);
export const applyEnhancer = (edit: {
  version: string; id: string; delete?: boolean; name?: string; description?: string;
  prompt?: string; enabled?: boolean; models?: BoundModel[]; presets?: string[] | null;
}) => post<SaveResponse>('/prompts/enhancers/apply', edit);

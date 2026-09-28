/**
 * Wire types for the One Config Studio API.
 *
 * These mirror `crates/one-cli/src/config_studio/document.rs` and `api.rs`.
 * Keeping them hand-written (rather than generated) is deliberate: the structs
 * are small, and a mismatch shows up immediately as a runtime `undefined` in a
 * place where the UI renders "unknown" instead of silently doing the wrong thing.
 */

export type Scope = 'global' | 'project' | 'builtin' | 'foreign';
export type DocFormat = 'json' | 'toml' | 'markdown' | 'jsonl' | 'text';
export type ModuleId =
  | 'providers'
  | 'settings'
  | 'mcp'
  | 'agents'
  | 'prompts'
  | 'extensions';
/** When a saved change actually applies. */
export type EffectTiming = 'new-session' | 'immediate' | 'restart';
export type Severity = 'error' | 'warning' | 'info';

export type FieldKind =
  | 'text'
  | 'textarea'
  | 'number'
  | 'boolean'
  | 'enum'
  | 'string-list'
  | 'string-map'
  | 'secret'
  | 'reference';

/** Placeholder written in place of a secret; the server restores it on save. */
export const REDACTED = '***REDACTED***';

export interface Capabilities {
  write: boolean;
  create: boolean;
  form: boolean;
  source: boolean;
  restore: boolean;
}

export interface ConfigDocument {
  id: string;
  module: ModuleId;
  scope: Scope;
  title: string;
  path: string;
  project_root: string | null;
  exists: boolean;
  format: DocFormat;
  capabilities: Capabilities;
  sensitive: boolean;
  effect: EffectTiming;
  effect_note: string;
  /** Runtime loader(s) that read this file — the provenance evidence. */
  managed_by: string;
  read_only_reason: string | null;
  /** Precedence within the module; lower wins. */
  precedence: number;
  override_note: string | null;
}

export interface Diagnostic {
  severity: Severity;
  message: string;
  line?: number;
  column?: number;
  field?: string;
}

export interface FieldSpec {
  path: string;
  label: string;
  kind: FieldKind;
  help?: string;
  options: string[];
  default?: unknown;
  min?: number;
  max?: number;
  advanced: boolean;
}

export interface CollectionSpec {
  path: string;
  label: string;
  key_label: string;
  entry_fields: FieldSpec[];
  nested?: CollectionSpec;
  key_immutable: boolean;
}

export interface FormModel {
  fields: FieldSpec[];
  collections: CollectionSpec[];
  value: unknown;
  masked_fields: string[];
}

export interface EffectiveEntry {
  name: string;
  source: string;
  value: string;
  overridden: boolean;
}

export interface OverrideInfo {
  name: string;
  value: string | null;
  affects: string;
}

export interface EffectiveReport {
  module: ModuleId;
  note: string;
  entries: EffectiveEntry[];
  sources: string[];
  overrides: OverrideInfo[];
}

export interface StudioContext {
  version: string;
  cwd: string;
  agent_dir: string;
  home_dir: string;
  project_roots: string[];
  agent_dir_overridden: boolean;
}

export interface ModuleInfo {
  id: ModuleId;
  label: string;
  summary: string;
  document_count: number;
  writable: boolean;
}

export interface CatalogResponse {
  context: StudioContext;
  modules: ModuleInfo[];
  documents: ConfigDocument[];
  backup_root: string;
}

export interface DocumentView {
  document: ConfigDocument;
  version: string;
  content: string;
  masked: boolean;
  unlocked: boolean;
  parsed: unknown;
  diagnostics: Diagnostic[];
  form: FormModel | null;
  read_error: string | null;
}

export interface ValidateResponse {
  version: string;
  diagnostics: Diagnostic[];
  valid: boolean;
  diff: string;
  added: number;
  removed: number;
  approximate: boolean;
  target: string;
  effect_note: string;
  /** Masked preview of what a save would write. Display-only. */
  preview_text: string;
}

export interface BackupRecord {
  id: string;
  created_at: string;
  size: number;
  version: string;
  reason: string;
}

export interface SaveResponse {
  version: string;
  created: boolean;
  bytes: number;
  backup: BackupRecord | null;
  effect_note: string;
  target: string;
}

export interface BackupsResponse {
  backups: BackupRecord[];
  directory: string;
}

export interface OverrideResponse {
  target_doc: string;
  version: string;
  entry: unknown;
  source: string;
  note: string;
  diff: string;
}

/** How the client produced a draft; decides server-side secret merging. */
export type DraftView = 'form' | 'source';

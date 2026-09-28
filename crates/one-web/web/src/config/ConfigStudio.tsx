import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  AlertTriangle,
  Bot,
  Boxes,
  Check,
  ChevronRight,
  Cpu,
  FileCog,
  Lock,
  LockOpen,
  MessageSquareCode,
  Puzzle,
  RefreshCw,
  Save,
  Settings2,
  X,
  type LucideIcon,
} from 'lucide-react';
import clsx from 'clsx';
import {
  ApiError,
  fetchCatalog,
  fetchDocument,
  fetchEnhancers,
  initToken,
  isAbortError,
  saveDocument,
  validateDraft,
  type EnhancerCatalogItem,
} from './api';
import { createRequestGate, runValidateRequest } from './validateGate';
import {
  applySavedBaseline,
  resolveRefreshBaseline,
  settleLoad,
  settleSave,
  type RefreshOrigin,
  type SaveIdentity,
} from './saveApply';
import type {
  CatalogResponse,
  ConfigDocument,
  DocumentView,
  DraftView,
  ModuleId,
  Scope,
  ValidateResponse,
} from './types';
import { CollectionEditor } from './components/CollectionEditor';
import { FieldControl } from './components/FieldControl';
import { Diagnostics } from './components/Diagnostics';
import { DiffView } from './components/DiffView';
import { EffectivePanel } from './components/EffectivePanel';
import { BackupsPanel } from './components/BackupsPanel';
import { BEHAVIORS, ModelEnhancers } from './components/ModelEnhancers';
import { AgentPreview } from './components/AgentPreview';
import { getPath, prettyJson, setPath, type Json } from './jsonPath';

type Tab = 'preview' | 'form' | 'source' | 'diff' | 'effective' | 'backups';

type EditScope = 'global';
type PromptPage = 'work' | 'enhancers' | 'effective';

function promptTitle(doc: ConfigDocument) {
  if (doc.module !== 'prompts') return doc.title;
  const name = doc.title.split(' · ').slice(1).join(' · ');
  return name ? `${name.charAt(0).toUpperCase()}${name.slice(1)}` : doc.title;
}

const TABS: Tab[] = ['preview', 'form', 'source', 'diff', 'effective', 'backups'];
const MODULE_IDS: ModuleId[] = ['providers', 'settings', 'mcp', 'agents', 'prompts', 'extensions'];

const MODULE_ICONS: Record<ModuleId, LucideIcon> = {
  settings: Settings2,
  providers: Cpu,
  mcp: Boxes,
  agents: Bot,
  prompts: MessageSquareCode,
  extensions: Puzzle,
};

/**
 * Initial selection from `?module=…&doc=…&scope=…&tab=…`.
 *
 * Deep links are what make a view shareable ("look at this MCP entry") and let a
 * reload keep your place instead of snapping back to the first document.
 */
function initialSelection(): {
  module: ModuleId | null;
  doc: string | null;
  scope: EditScope;
  tab: Tab | null;
} {
  const params = new URLSearchParams(window.location.search);
  const module = params.get('module');
  const doc = params.get('doc');
  const tab = params.get('tab');
  return {
    module: MODULE_IDS.includes(module as ModuleId) ? (module as ModuleId) : null,
    doc: doc || null,
    scope: 'global',
    tab: TABS.includes(tab as Tab) ? (tab as Tab) : null,
  };
}

/** Keep the address bar in step with the selection so reload/back behave. */
function syncUrl(module: ModuleId, doc: string | null, scope: EditScope, tab: Tab) {
  const params = new URLSearchParams(window.location.search);
  params.set('module', module);
  params.set('scope', scope);
  params.delete('enhancerScope');
  params.delete('enhancerModel');
  if (doc) params.set('doc', doc);
  else params.delete('doc');
  params.set('tab', tab);
  const query = params.toString();
  window.history.replaceState(
    {},
    '',
    window.location.pathname + (query ? `?${query}` : '')
  );
}

/** Modules whose effective configuration can be resolved today. */
const EFFECTIVE_MODULES: ModuleId[] = ['settings', 'providers', 'mcp', 'prompts'];

const SCOPE_LABELS: Record<Scope, string> = {
  global: '全局',
  project: '项目',
  builtin: '内置',
  foreign: '外部工具',
};

/**
 * One Config Studio.
 *
 * The layout keeps the spec's central distinction visible at all times: the left
 * rail selects the module, the second column lists the **编辑目标** (real files in
 * real layers), and the `生效配置` tab shows the merged **最终生效配置** as a
 * separate, read-only result with per-entry source attribution.
 */
export const ConfigStudio: React.FC = () => {
  const [tokenReady] = useState(() => initToken());
  const [initial] = useState(initialSelection);
  const [catalog, setCatalog] = useState<CatalogResponse | null>(null);
  const [moduleId, setModuleId] = useState<ModuleId>(initial.module ?? 'prompts');
  const [promptPage, setPromptPage] = useState<PromptPage>(() => {
    const page = new URLSearchParams(window.location.search).get('promptPage');
    return page === 'work' || page === 'effective' || page === 'enhancers' ? page : initial.tab === 'effective' ? 'effective' : initial.doc ? 'work' : 'enhancers';
  });
  const editScope: EditScope = 'global';
  const [enhancerId, setEnhancerId] = useState(() => new URLSearchParams(window.location.search).get('enhancer') ?? '');
  const [docId, setDocId] = useState<string | null>(initial.doc);
  const [pinnedTab, setPinnedTab] = useState<Tab | null>(initial.tab);
  const [view, setView] = useState<DocumentView | null>(null);
  const [tab, setTab] = useState<Tab>('preview');
  const [editMode, setEditMode] = useState<DraftView>('form');
  const [source, setSource] = useState('');
  const [form, setForm] = useState<Json>(null);
  const [validation, setValidation] = useState<ValidateResponse | null>(null);
  const [validatedDraft, setValidatedDraft] = useState<string | null>(null);
  const [validating, setValidating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [unlocked, setUnlocked] = useState(false);
  const [pendingSave, setPendingSave] = useState<{
    draft: string;
    editMode: DraftView;
    version: string;
    validation: ValidateResponse;
  } | null>(null);
  const loadSeq = useRef(0);
  const loadAbort = useRef<AbortController | null>(null);
  const validateGate = useRef(createRequestGate());
  const validateDebounce = useRef<number | null>(null);
  const draftRev = useRef(0);
  const docIdRef = useRef<string | null>(docId);
  docIdRef.current = docId;
  const tabRef = useRef<Tab>('preview');
  tabRef.current = tab;

  const cancelValidation = useCallback(() => {
    if (validateDebounce.current !== null) {
      window.clearTimeout(validateDebounce.current);
      validateDebounce.current = null;
    }
    validateGate.current.cancel();
  }, []);
  const [message, setMessage] = useState<{ kind: 'ok' | 'error'; text: string } | null>(null);
  const [fatal, setFatal] = useState<string | null>(null);
  const [enhancerDraft, setEnhancerDraft] = useState({ dirty: false, busy: false });
  const [enhancerCatalog, setEnhancerCatalog] = useState<EnhancerCatalogItem[]>([]);
  const onEnhancerDraft = useCallback((dirty: boolean, busy: boolean) => setEnhancerDraft({ dirty, busy }), []);

  const reloadCatalog = useCallback(() => {
    fetchCatalog()
      .then(setCatalog)
      .catch((err: unknown) =>
        setFatal(err instanceof ApiError ? err.message : String(err))
      );
    fetchEnhancers('global', '', '', 'code')
      .then((view) => setEnhancerCatalog(view.catalog ?? []))
      .catch(() => setEnhancerCatalog([]));
  }, []);

  useEffect(() => {
    if (!tokenReady) return;
    reloadCatalog();
  }, [tokenReady, reloadCatalog]);

  const loadDocument = useCallback((id: string, unlock: boolean, origin?: RefreshOrigin) => {
    loadAbort.current?.abort();
    const ac = new AbortController();
    loadAbort.current = ac;
    const seq = ++loadSeq.current;
    const startedRev = draftRev.current;
    const refresh = origin !== undefined;
    cancelValidation();
    if (!refresh) {
      setMessage(null);
      setValidation(null);
      setValidatedDraft(null);
      setPendingSave(null);
    }
    setValidating(false);
    void settleLoad({
      started: { seq, draftRev: startedRev, refresh },
      run: () => fetchDocument(id, unlock, ac.signal),
      current: () => ({ seq: loadSeq.current, draftRev: draftRev.current }),
    })
      .then(({ result: next, action }) => {
        if (action === 'ignore' || docIdRef.current !== id) return;
        if (action === 'baseline-only') {
          if (!origin) return;
          const resolvedConflict = next.version !== origin.version;
          setView((prev) => resolveRefreshBaseline({ prev, fetched: next, origin }).view);
          if (resolvedConflict) {
            setMessage({
              kind: 'error',
              text: '文件在保存后又被写入。已保留你的未保存修改，版本基准仍为刚才保存的内容；请重新载入后再合并，直接保存会因版本冲突被拒绝。',
            });
          }
          return;
        }
        setView(next);
        draftRev.current = 0;
        setSource(next.content);
        setForm(next.parsed ?? (next.document.format === 'json' ? {} : null));
        setUnlocked(unlock);
        const preferred = pinnedTab ?? tabRef.current;
        const usable = (candidate: Tab) =>
          (candidate !== 'form' || Boolean(next.form)) &&
          (candidate !== 'source' || next.document.capabilities.source) &&
          (candidate !== 'effective' || (next.document.module !== 'prompts' && EFFECTIVE_MODULES.includes(next.document.module))) &&
          (candidate !== 'backups' || next.document.capabilities.restore) &&
          candidate !== 'diff';
        const nextTab = usable(preferred) ? preferred : 'preview';
        setTab(nextTab);
        setEditMode(nextTab === 'source' || !next.form ? 'source' : 'form');
        setPinnedTab(null);
      })
      .catch((err: unknown) => {
        if (seq !== loadSeq.current || isAbortError(err)) return;
        setMessage({ kind: 'error', text: err instanceof ApiError ? err.message : String(err) });
      });
  }, [pinnedTab, cancelValidation]);

  useEffect(() => {
    if (docId) loadDocument(docId, false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [docId]);

  useEffect(() => {
    if (view) syncUrl(moduleId, view.document.id, editScope, tab);
    if (moduleId === 'prompts') {
      const params = new URLSearchParams(window.location.search);
      params.set('promptPage', promptPage);
      if (enhancerId) params.set('enhancer', enhancerId); else params.delete('enhancer');
      window.history.replaceState({}, '', `${window.location.pathname}?${params}`);
    }
  }, [moduleId, view, editScope, tab, promptPage, enhancerId]);

  const moduleInfo = catalog?.modules.find((m) => m.id === moduleId) ?? null;
  const moduleDocs = useMemo(
    () => (catalog?.documents ?? []).filter((doc) => doc.module === moduleId && (doc.scope !== 'project' || doc.id.startsWith('prompts.ref.'))),
    [catalog, moduleId]
  );

  const visibleDocs = moduleDocs;

  const layerEmpty = Boolean(moduleInfo?.writable) && visibleDocs.length === 0;

  useEffect(() => {
    if (!moduleInfo?.writable) return;
    if (visibleDocs.length === 0) {
      setDocId(null);
      setView(null);
      return;
    }
    if (docId && visibleDocs.some((doc) => doc.id === docId)) return;
    setDocId(visibleDocs[0].id);
  }, [moduleInfo, visibleDocs, docId]);

  const draft = editMode === 'source' ? source : JSON.stringify(form ?? {});
  const baseline =
    editMode === 'source'
      ? view?.content ?? ''
      : JSON.stringify(view?.parsed ?? {});
  const dirty = view !== null && draft !== baseline;
  const writable = view?.document.capabilities.write ?? false;
  const sourceLocked = Boolean(view?.document.sensitive) && !unlocked;
  const draftFingerprint = view
    ? `${view.document.id}:${view.version}:${editMode}:${draft}`
    : '';

  const applyForm = useCallback((next: Json) => {
    draftRev.current += 1;
    setForm(next);
    setSource(prettyJson(next));
    setEditMode('form');
  }, []);

  const applySource = useCallback((text: string) => {
    draftRev.current += 1;
    setSource(text);
    setEditMode('source');
    try {
      setForm(JSON.parse(text) as Json);
    } catch {
      // Keep the last good form tree; save still uses source while editMode is source.
    }
  }, []);

  const selectTab = useCallback((next: Tab) => {
    if (next === 'form') {
      if (!view?.form) return;
      if (editMode === 'source') {
        try {
          applyForm(JSON.parse(source) as Json);
        } catch {
          setMessage({ kind: 'error', text: '源码无法解析为 JSON，请先修复后再切回表单。' });
          return;
        }
      }
    } else if (next === 'source') {
      if (editMode === 'form' && dirty) {
        setSource(prettyJson(form));
      }
      setEditMode('source');
    }
    setTab(next);
  }, [applyForm, dirty, editMode, form, source, view?.form]);

  const runValidate = useCallback(() => {
    if (!view || !dirty || !writable) return;
    const fingerprint = draftFingerprint;
    setValidating(true);
    void runValidateRequest({
      gate: validateGate.current,
      fingerprint,
      run: (signal) =>
        validateDraft(view.document.id, draft, editMode, view.version, signal),
      onSuccess: (result, matched) => {
        setValidation(result);
        setValidatedDraft(matched);
      },
      onError: (err) => {
        setValidation(null);
        setValidatedDraft(null);
        setMessage({
          kind: 'error',
          text: err instanceof ApiError ? err.message : String(err),
        });
      },
      onFinally: () => setValidating(false),
    });
  }, [view, dirty, writable, draft, editMode, draftFingerprint]);

  const runValidateNow = useCallback(() => {
    if (validateDebounce.current !== null) {
      window.clearTimeout(validateDebounce.current);
      validateDebounce.current = null;
    }
    runValidate();
  }, [runValidate]);

  useEffect(() => {
    setValidation(null);
    setValidatedDraft(null);
    if (!view || !dirty || !writable) {
      cancelValidation();
      setValidating(false);
      return;
    }
    validateDebounce.current = window.setTimeout(() => {
      validateDebounce.current = null;
      runValidate();
    }, 450);
    return () => {
      if (validateDebounce.current !== null) {
        window.clearTimeout(validateDebounce.current);
        validateDebounce.current = null;
      }
      validateGate.current.cancel();
    };
  }, [draft, dirty, writable, view, editMode, draftFingerprint, runValidate, cancelValidation]);

  const commitSave = () => {
    if (!view || !pendingSave) return;
    const snapshot = pendingSave;
    const saved: SaveIdentity = {
      docId: view.document.id,
      txn: loadSeq.current,
      draftRev: draftRev.current,
    };
    const unlock = unlocked;
    setPendingSave(null);
    setBusy(true);
    void settleSave({
      saved,
      run: () =>
        saveDocument(saved.docId, snapshot.draft, snapshot.version, snapshot.editMode, unlock),
      current: () => ({
        docId: docIdRef.current,
        txn: loadSeq.current,
        draftRev: draftRev.current,
      }),
    })
      .then(({ result, action }) => {
        const backupNote = result.backup ? `，原内容已备份为 ${result.backup.id}` : '';
        setMessage({
          kind: 'ok',
          text: `${result.created ? '已创建' : '已保存'} ${result.target}（版本 ${result.version.slice(0, 8)}）${backupNote}。${result.effect_note}`,
        });
        reloadCatalog();
        if (action === 'reload') {
          loadDocument(saved.docId, unlock, {
            version: result.version,
            draft: snapshot.draft,
          });
        } else if (action === 'keep-edits') {
          setView((prev) => applySavedBaseline(prev, saved.docId, result.version, snapshot.draft));
        }
      })
      .catch((err: unknown) =>
        setMessage({ kind: 'error', text: err instanceof ApiError ? err.message : String(err) })
      )
      .finally(() => setBusy(false));
  };

  if (!tokenReady) {
    return (
      <div className="cfg-fullscreen">
        <div className="cfg-brand-icon-box" style={{ width: 44, height: 44 }}>
          <FileCog size={24} />
        </div>
        <h2>缺少访问凭证</h2>
        <p>
          配置中心仅允许持有临时 Token 的本地请求访问。请在终端执行：
          <br />
          <code>one config</code>
        </p>
      </div>
    );
  }

  if (fatal) {
    return (
      <div className="cfg-fullscreen">
        <AlertTriangle size={32} color="#fbbf24" />
        <h2>无法载入配置目录</h2>
        <p>{fatal}</p>
      </div>
    );
  }

  const canSave = Boolean(
    writable &&
      dirty &&
      !busy &&
      !validating &&
      validation?.valid &&
      validatedDraft === draftFingerprint
  );

  return (
    <div className="cfg-app">
      <header className="cfg-topbar">
        <div className="cfg-brand">
          <div className="cfg-brand-icon-box">
            <FileCog size={17} />
          </div>
          <strong>One Config Studio</strong>
          {catalog && <span className="cfg-brand-version">v{catalog.context.version}</span>}
        </div>

        <span className="cfg-source-badge">全局配置</span>

      </header>

      <div className="cfg-body" onClickCapture={event => {
        if (!(event.target instanceof Element) || !event.target.closest('.cfg-modules button, .cfg-docs button')) return;
        if (enhancerDraft.busy || (enhancerDraft.dirty && !window.confirm('放弃尚未保存的行为设置？'))) {
          event.preventDefault(); event.stopPropagation();
        }
      }}>
        <nav className="cfg-modules">
          <div className="cfg-nav-heading">配置模块</div>
          {(catalog?.modules ?? []).map((module) => {
            const Icon = MODULE_ICONS[module.id] ?? Settings2;
            return (
              <button
                key={module.id}
                type="button"
                className={clsx('cfg-module', module.id === moduleId && 'is-active')}
                onClick={() => {
                  setModuleId(module.id);
                  if (module.id === 'prompts') {
                    setPromptPage('enhancers');
                    setTab('preview');
                  }
                  const nextDocs = (catalog?.documents ?? []).filter(
                    (d) => d.module === module.id
                  );
                  const nextVisible = nextDocs.filter(d => d.scope !== 'project' || d.id.startsWith('prompts.ref.'));
                  setDocId(nextVisible[0]?.id ?? null);
                  setTab('preview');
                }}
                title={module.id === 'prompts' ? 'Agent 定义、模型行为增强与最终配置' : undefined}
              >
                <span className="cfg-module-icon">
                  <Icon size={16} />
                </span>
                <div className="cfg-module-info">
                  <span className="cfg-module-label">{module.id === 'prompts' ? 'Agent 与行为增强' : module.id === 'agents' ? 'Agent 规格' : module.label}</span>
                </div>
                <div className="cfg-module-meta">
                  <span>{module.document_count}</span>
                  {module.writable && <b className="cfg-pill cfg-pill-write">可写</b>}
                </div>
              </button>
            );
          })}
        </nav>

        <aside className="cfg-docs">
          <div className="cfg-docs-header">
            <h2>{moduleId === 'prompts' ? 'Agent 与行为增强' : moduleInfo?.label ?? '配置清单'}</h2>
            {moduleId !== 'prompts' && <span className="cfg-docs-count">{visibleDocs.length}</span>}
          </div>
          {moduleId === 'prompts' && <nav aria-label="提示词导航" className="cfg-prompt-nav">
            <section aria-label="工作提示词">
              <h3>Agents</h3><p className="cfg-muted">定义 Agent 做什么</p>
              <div className="cfg-work-documents">{[...moduleDocs.filter(doc => doc.id.startsWith('prompts.ref.') || doc.id.startsWith('prompts.builtin.preset.'))].sort((a, b) => {
                const rank = (title: string) => title.toLowerCase().startsWith('research') ? 0 : title.toLowerCase().startsWith('code') ? 1 : title.toLowerCase().startsWith('general') ? 2 : 3;
                return rank(promptTitle(a)) - rank(promptTitle(b)) || promptTitle(a).localeCompare(promptTitle(b));
              }).map(doc => <DocumentListItem key={doc.id} doc={doc} active={promptPage === 'work' && doc.id === docId} onSelect={() => { setDocId(doc.id); setPromptPage('work'); setTab('preview'); }} />)}</div>
              <details><summary>高级 / 其他来源</summary>
                {moduleDocs.filter(doc => !doc.id.startsWith('prompts.ref.') && !doc.id.startsWith('prompts.builtin.preset.')).map(doc => <DocumentListItem key={doc.id} doc={doc} active={promptPage === 'work' && doc.id === docId} onSelect={() => { setDocId(doc.id); setPromptPage('work'); setTab('source'); }} />)}
              </details>
            </section>
            <section><h3>Model Enhancers</h3><p className="cfg-muted">定义模型怎么表现</p>
              <div className="cfg-enhancer-nav-items">
              <button type="button" className={clsx('cfg-doc', promptPage === 'enhancers' && !enhancerId && 'is-active')} onClick={() => { setPromptPage('enhancers'); setEnhancerId(''); }}>全部行为增强<span className="cfg-doc-path">状态与适用范围</span></button>
              {(enhancerCatalog.length ? enhancerCatalog : Object.entries(BEHAVIORS).map(([id, info]) => ({ id, title: info.title, name: info.name, kind: 'builtin' }))).map((row) => (
                <button key={row.id} type="button" className={clsx('cfg-doc', promptPage === 'enhancers' && enhancerId === row.id && 'is-active')} onClick={() => { setPromptPage('enhancers'); setEnhancerId(row.id); }}>
                  {row.title}<span className="cfg-doc-path">{row.name}{row.kind === 'custom' ? ' · Custom' : ''}</span>
                </button>
              ))}
              <button type="button" className={clsx('cfg-doc', promptPage === 'enhancers' && enhancerId === 'new' && 'is-active')} onClick={() => { setPromptPage('enhancers'); setEnhancerId('new'); }}>＋ 新建行为增强</button>
              </div>
            </section>
            <section><h3>Effective Config</h3><p className="cfg-muted">展示最终实际生效结果</p>
              <button type="button" className={clsx('cfg-doc', promptPage === 'effective' && 'is-active')} onClick={() => setPromptPage('effective')}>最终配置</button>
            </section>
          </nav>}
          {layerEmpty && <EmptyLayer />}
          {moduleId !== 'prompts' && visibleDocs.map((doc) => (
            <DocumentListItem
              key={doc.id}
              doc={doc}
              active={doc.id === docId}
              onSelect={() => { setDocId(doc.id); }}
            />
          ))}
        </aside>

        <main className="cfg-detail">
          <div className="cfg-detail-container">
            {moduleId === 'prompts' && promptPage !== 'work' && <ModelEnhancers key={promptPage} workPrompts={moduleDocs.filter(doc => doc.id.startsWith('prompts.ref.')).map(doc => ({ id: doc.id, title: promptTitle(doc), scope: SCOPE_LABELS[doc.scope] }))} previewOnly={promptPage === 'effective'} enhancerId={enhancerId} onSelectEnhancer={(id) => { setPromptPage('enhancers'); setEnhancerId(id); }} onOpenEffective={() => setPromptPage('effective')} onSaved={reloadCatalog} onDraftChange={onEnhancerDraft} />}
            {!view && layerEmpty && moduleId !== 'prompts' && (
              <EmptyLayer />
            )}
            {!view && !layerEmpty && moduleId !== 'prompts' && (
              <div style={{ padding: '40px 0', textAlign: 'center' }}>
                <p className="cfg-muted">从左侧文档列表中选择一项进行查看或编辑。</p>
              </div>
            )}
            {view && (moduleId !== 'prompts' || promptPage === 'work') && (
              <>
                <DocumentHeader doc={view.document} version={view.version} />

              {message && (
                <div className={clsx('cfg-banner', message.kind === 'ok' ? 'cfg-banner-ok' : 'cfg-banner-error')}>
                  <span>{message.text}</span>
                  <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => setMessage(null)}>
                    <X size={13} />
                  </button>
                </div>
              )}

              {view.read_error && (
                <Diagnostics diagnostics={[{ severity: 'error', message: view.read_error }]} />
              )}

              <div className="cfg-toolbar">
                <div className="cfg-tabs-group" role="tablist">
                  {(['preview', 'form', 'source'] as Tab[]).map((candidate) =>
                    candidate === 'form' && (!view.form || view.document.module === 'prompts') ? null : (
                      <button
                        key={candidate}
                        type="button"
                        className={clsx('cfg-tab', tab === candidate && 'is-active')}
                        onClick={() => selectTab(candidate)}
                      >
                        {candidate === 'preview' ? '可读预览' : candidate === 'form' ? '编辑设置' : '高级 / 原始配置'}
                      </button>
                    )
                  )}
                  {validation && validation.diff.trim() !== '' && (
                    <button
                      type="button"
                      className={clsx('cfg-tab', tab === 'diff' && 'is-active')}
                      onClick={() => selectTab('diff')}
                    >
                      差异预览
                      <span className="cfg-diff-added">+{validation.added}</span>
                      <span className="cfg-diff-removed">−{validation.removed}</span>
                    </button>
                  )}
                  {moduleId !== 'prompts' && EFFECTIVE_MODULES.includes(moduleId) && (
                    <button
                      type="button"
                      className={clsx('cfg-tab', tab === 'effective' && 'is-active')}
                      onClick={() => selectTab('effective')}
                    >
                      生效配置
                    </button>
                  )}
                  {view.document.capabilities.restore && (
                    <button
                      type="button"
                      className={clsx('cfg-tab', tab === 'backups' && 'is-active')}
                      onClick={() => selectTab('backups')}
                    >
                      快照备份
                    </button>
                  )}
                </div>

                <div className="cfg-toolbar-spacer" />

                <div className="cfg-actions-group">
                  {view.document.sensitive && (
                    <button
                      type="button"
                      className="cfg-btn"
                      onClick={() => {
                        if (unlocked) {
                          loadDocument(view.document.id, false);
                          return;
                        }
                        if (
                          window.confirm(
                            '显示完整源码会在此浏览器页面中呈现明文凭据。\n\n' +
                              '内容不会写入磁盘或日志，页面刷新后需要重新解锁。确认继续？'
                          )
                        ) {
                          loadDocument(view.document.id, true);
                        }
                      }}
                    >
                      {unlocked ? <Lock size={13} /> : <LockOpen size={13} />}
                      {unlocked ? '重新脱敏' : '解锁查看源码'}
                    </button>
                  )}

                  <button
                    type="button"
                    className="cfg-btn"
                    disabled={!dirty || !writable || busy}
                    onClick={() => {
                      if (!view) return;
                      runValidateNow();
                    }}
                  >
                    <RefreshCw size={13} className={validating ? 'spin' : undefined} />
                    {validating ? '校验中…' : '校验'}
                  </button>

                  {writable && (
                    <button
                      type="button"
                      className="cfg-btn cfg-btn-primary"
                      disabled={!canSave}
                      onClick={() => {
                        if (!view || !validation) return;
                        setPendingSave({
                          draft,
                          editMode,
                          version: view.version,
                          validation,
                        });
                      }}
                    >
                      <Save size={14} /> 保存配置
                    </button>
                  )}
                </div>
              </div>

              {tab === 'preview' && <AgentPreview view={view} onOpenEffective={() => setPromptPage('effective')} />}

              {tab === 'form' && view.form && (
                <div className="cfg-form">
                  {view.form.fields.filter((f) => !f.advanced).map((field) => (
                    <FieldControl
                      key={field.path}
                      spec={field}
                      masked={view.masked}
                      value={getPath(form, field.path)}
                      onChange={(next) => applyForm(setPath(form, field.path, next))}
                    />
                  ))}

                  {view.form.collections.map((collection) => (
                    <CollectionEditor
                      key={collection.path}
                      spec={collection}
                      masked={view.masked}
                      value={getPath(form, collection.path)}
                      onChange={(next) => applyForm(setPath(form, collection.path, next))}
                    />
                  ))}

                  {view.form.fields.some((f) => f.advanced) && (
                    <details className="cfg-advanced">
                      <summary>高级配置选项（{view.form.fields.filter((f) => f.advanced).length}）</summary>
                      <div style={{ display: 'flex', flexDirection: 'column', gap: 16, marginTop: 12 }}>
                        {view.form.fields.filter((f) => f.advanced).map((field) => (
                          <FieldControl
                            key={field.path}
                            spec={field}
                            masked={view.masked}
                            value={getPath(form, field.path)}
                            onChange={(next) => applyForm(setPath(form, field.path, next))}
                          />
                        ))}
                      </div>
                    </details>
                  )}

                  {view.form.masked_fields.length > 0 && view.masked && (
                    <p className="cfg-note">
                      已脱敏字段：<code>{view.form.masked_fields.join('、')}</code>。
                      表单保存会自动保留未变动的敏感凭据；若需在源码视图查看明文，请使用上方「解锁」按钮。
                    </p>
                  )}
                </div>
              )}

              {tab === 'source' && (
                <div className="cfg-source">
                  {sourceLocked && (
                    <p className="cfg-warn">
                      该文档包含敏感凭据，源码视图默认脱敏且不可编辑。请点击上方「解锁查看源码」
                      后再行编辑，或直接使用表单视图（表单会安全保留未改动的凭据）。
                    </p>
                  )}
                  <div className="cfg-source-box">
                    <div className="cfg-source-topbar">
                      <span>格式：<code>{view.document.format.toUpperCase()}</code></span>
                      <span>
                        {writable ? (sourceLocked ? '🔒 脱敏保护（只读）' : '✏️ 可编辑') : '只读文档'}
                      </span>
                    </div>
                    <textarea
                      className="cfg-source-editor"
                      spellCheck={false}
                      readOnly={sourceLocked || !writable}
                      value={source}
                      onChange={(e) => applySource(e.target.value)}
                    />
                  </div>
                </div>
              )}

              {tab === 'diff' && validation && <DiffView result={validation} />}

              {tab === 'effective' && (
                <EffectivePanel
                  module={moduleId}

                />
              )}

              {tab === 'backups' && view.document.capabilities.restore && (
                <BackupsPanel
                  docId={view.document.id}
                  version={view.version}
                  onRestored={(text) => {
                    setMessage({ kind: 'ok', text });
                    loadDocument(view.document.id, unlocked);
                  }}
                />
              )}

              {tab !== 'preview' && tab !== 'effective' && tab !== 'backups' && (
                <section className="cfg-validation" style={{ marginTop: 24 }}>
                  <h3>校验与诊断结果</h3>
                  <Diagnostics
                    diagnostics={
                      validation ? validation.diagnostics : view.diagnostics
                    }
                    emptyMessage={
                      dirty
                        ? undefined
                        : '当前配置已通过语法与结构校验，无错误报告。'
                    }
                  />
                    {!dirty && view.diagnostics.length === 0 && (
                      <p className="cfg-muted">修改字段后将自动进行实时语法校验并生成脱敏差异。</p>
                    )}
                  </section>
                )}
              </>
            )}
          </div>
        </main>
      </div>

      {pendingSave && (
        <div className="cfg-modal-backdrop" role="dialog" aria-modal="true">
          <div className="cfg-modal">
            <header>
              <h3>确认写入配置</h3>
              <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => setPendingSave(null)}>
                <X size={15} />
              </button>
            </header>
            <p className="cfg-note">
              目标文件：<code>{pendingSave.validation.target}</code>
              <br />
              生效说明：{pendingSave.validation.effect_note}
            </p>
            <div className="cfg-modal-body">
              <DiffView result={pendingSave.validation} />
            </div>
            <footer>
              <button type="button" className="cfg-btn" onClick={() => setPendingSave(null)}>
                取消
              </button>
              <button type="button" className="cfg-btn cfg-btn-primary" onClick={commitSave} disabled={busy}>
                <Check size={14} /> {busy ? '正在写入…' : '确认并原子写入'}
              </button>
            </footer>
          </div>
        </div>
      )}
    </div>
  );
};

/**
 * Shown when the selected layer has nothing to edit.
 *
 * This is the spec's 「全局专属」 state: the module genuinely has no project
 * layer, so the studio says so instead of offering an override that the runtime
 * would ignore.
 */
const EmptyLayer: React.FC = () => <div className="cfg-empty-layer"><h3>暂无全局配置</h3><p>此模块还没有可显示的配置。</p></div>;

const DocumentListItem: React.FC<{
  doc: ConfigDocument;
  active: boolean;
  onSelect: () => void;
}> = ({ doc, active, onSelect }) => (
  <button
    type="button"
    className={clsx('cfg-doc', active && 'is-active')}
    onClick={onSelect}
    title={doc.path}
  >
    <div className="cfg-doc-title-row">
      <span className="cfg-doc-title">{promptTitle(doc)}</span>
      <ChevronRight size={13} className="cfg-muted" style={{ opacity: active ? 0.9 : 0.4 }} />
    </div>
    {doc.module !== 'prompts' && <span className="cfg-doc-path">{doc.path}</span>}
    {doc.module !== 'prompts' && <div className="cfg-doc-badges">
      <span className={`cfg-pill cfg-scope-${doc.scope}`}>{SCOPE_LABELS[doc.scope]}</span>
      {!doc.exists && <span className="cfg-pill cfg-pill-missing">未创建</span>}
      {doc.sensitive && <span className="cfg-pill cfg-pill-secret">含凭据</span>}
      {!doc.capabilities.write && !doc.capabilities.create && (
        <span className="cfg-pill cfg-pill-ro">只读</span>
      )}
    </div>}
  </button>
);

const DocumentHeader: React.FC<{ doc: ConfigDocument; version: string }> = ({ doc, version }) => doc.module === 'prompts' ? (
  <header className="cfg-doc-header">
    <h1>{promptTitle(doc)}</h1>
    <p className="cfg-note">这个 Agent 负责什么、能做什么。</p>
    <div className="cfg-doc-badges">{!doc.capabilities.write && <span className="cfg-pill cfg-pill-ro">只读</span>}</div>
    <details className="cfg-advanced"><summary>高级 / 来源</summary>
      <p><code>{doc.path}</code></p><p>{doc.managed_by}</p><p>{doc.effect_note}</p>
      <p>{doc.read_only_reason}</p><p>版本：{version.slice(0, 8)}</p>
    </details>
  </header>
) : (
  <header className="cfg-doc-header">
    <div className="cfg-doc-header-main">
      <h1>{doc.title}</h1>
      <div className="cfg-doc-header-path">
        <code>{doc.path}</code>
        <span className="cfg-muted">
          版本：{version === 'absent' ? '未落地' : version.slice(0, 8)}
        </span>
      </div>
    </div>
    <p className="cfg-note">全局配置 · 内置默认 → 全局自定义 → 最终生效</p>
    <details><summary>高级 / 来源</summary><p>{doc.managed_by}</p><p>{doc.effect_note}</p></details>
  </header>
);

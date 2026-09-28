import { useEffect, useMemo, useState } from 'react';
import {
  applyEnhancer,
  fetchEnhancers,
  fetchPromptPreview,
  type EnhancerCatalogItem,
  type EnhancerView,
  type PromptPreviewResponse,
} from '../api';

export const BEHAVIORS: Record<string, { title: string; name: string; description: string }> = {
  over_planning: { title: '规划过多', name: 'OverPlanning', description: '模型容易花大量时间规划，而不是直接执行。' },
  stops_early: { title: '过早结束', name: 'StopsEarly', description: '任务还没完成就停止。让它继续执行并检查，直到要求得到满足。' },
  reluctant_to_use_tools: { title: '不愿调用工具', name: 'ReluctantToUseTools', description: '该用工具时只解释不执行。让它依据实际工具结果回答。' },
  weak_verification: { title: '验证不足', name: 'WeakVerification', description: '修改后缺少检查就声称完成。让它先验证结果，再汇报完成情况。' },
};

const PARALLEL_GUIDANCE = `Prefer parallel tool calls when operations are independent.
Do not serialize independent reads, searches, or inspections unnecessarily.
Keep dependent operations sequential when later actions require earlier results.
Parallelism must preserve correctness and avoid duplicate or conflicting side effects.`;

type Model = EnhancerView['models'][number];
const modelKey = (model: Pick<Model, 'provider' | 'id'> | { provider: string; model: string }) =>
  'id' in model ? JSON.stringify([model.provider, model.id]) : JSON.stringify([model.provider, model.model]);
export const agentName = (name: string) => name.charAt(0).toUpperCase() + name.slice(1);

export function suggestEnhancerId(name: string) {
  const ascii = name.replace(/[^A-Za-z0-9]+/g, ' ').trim();
  if (!ascii) return 'CustomEnhancer';
  return ascii
    .split(/\s+/)
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join('');
}

function displayTitle(item: EnhancerCatalogItem) {
  return BEHAVIORS[item.id]?.title ?? item.title ?? item.name;
}
function displayName(item: EnhancerCatalogItem) {
  return BEHAVIORS[item.id]?.name ?? item.name;
}
function displayDescription(item: EnhancerCatalogItem) {
  return BEHAVIORS[item.id]?.description ?? item.description;
}
function agentLabel(item: EnhancerCatalogItem, agents: { name: string; preset: string }[]) {
  if (item.all_agents) return '所有 Agent';
  const names = item.agents.map((preset) => {
    const extra = agents.filter((a) => a.preset === preset).map((a) => agentName(a.name));
    return [agentName(preset), ...extra].join(' / ');
  });
  return names.join('、') || '指定 Agent';
}

/** Render configuration as text, never as executable HTML. */
export function ReadablePreview({ text }: { text: string }) {
  return (
    <div className="cfg-readable" data-testid="readable-preview">
      {text
        .replace(/<[^>]+>/g, '')
        .replace(/```[^\n]*\n/g, '')
        .replace(/```/g, '')
        .split('\n')
        .map((line, i) => {
          const clean = line.replace(/^\s*#{1,6}\s+/, '').replace(/\*\*([^*]+)\*\*/g, '$1').replace(/`([^`]+)`/g, '$1');
          if (!clean.trim()) return null;
          return /^\s*#/.test(line) ? <h4 key={i}>{clean}</h4> : <p key={i}>{clean}</p>;
        })}
    </div>
  );
}

function ModelPicker({
  models,
  selected,
  onChange,
  multiple = true,
  disabled,
}: {
  models: Model[];
  selected: string[];
  onChange: (keys: string[]) => void;
  multiple?: boolean;
  disabled: boolean;
}) {
  const [search, setSearch] = useState('');
  const matches = search.trim()
    ? models.filter((m) => `${m.provider} ${m.name} ${m.id}`.toLowerCase().includes(search.trim().toLowerCase()))
    : [];
  return (
    <div className="cfg-model-picker">
      <label>
        搜索 provider 或模型名称
        <input
          type="search"
          aria-label="搜索模型"
          placeholder="搜索 provider 或模型名称……"
          value={search}
          disabled={disabled}
          onChange={(e) => setSearch(e.target.value)}
        />
      </label>
      <p className="cfg-muted">{multiple ? '可搜索后多选。不会一次列出全部模型。' : '搜索并选择一个模型。'}</p>
      <div className="cfg-model-chips" aria-label="已选模型">
        {selected.map((key) => {
          const model = models.find((m) => modelKey(m) === key);
          return (
            <button
              key={key}
              type="button"
              className="cfg-chip"
              disabled={disabled}
              onClick={() => onChange(selected.filter((k) => k !== key))}
            >
              {model ? model.id : key} ×
            </button>
          );
        })}
      </div>
      {search.trim() && (
        <div className="cfg-model-results" aria-label="模型搜索结果">
          {matches.slice(0, 40).map((model) => {
            const key = modelKey(model);
            return (
              <label key={key}>
                <input
                  type={multiple ? 'checkbox' : 'radio'}
                  name="model-result"
                  checked={selected.includes(key)}
                  disabled={disabled}
                  onChange={() => {
                    onChange(
                      multiple
                        ? selected.includes(key)
                          ? selected.filter((k) => k !== key)
                          : [...selected, key]
                        : [key],
                    );
                    if (!multiple) setSearch('');
                  }}
                />
                <span>
                  {model.provider} / {model.name}
                  <small>{model.id}</small>
                </span>
              </label>
            );
          })}
          {!matches.length && <p>没有匹配的模型，请更换关键词。</p>}
          {matches.length > 40 && <p>还有 {matches.length - 40} 个结果，请输入更具体的名称。</p>}
        </div>
      )}
    </div>
  );
}

export function ModelEnhancers({
  previewOnly = false,
  workPrompts = [],
  enhancerId = '',
  onSelectEnhancer,
  onOpenEffective,
  onSaved,
  onDraftChange,
}: {
  workPrompts?: { id: string; title: string; scope: string }[];
  previewOnly?: boolean;
  enhancerId?: string;
  onSelectEnhancer?: (id: string) => void;
  onOpenEffective?: () => void;
  onSaved?: () => void;
  onDraftChange?: (dirty: boolean, busy: boolean) => void;
}) {
  const [data, setData] = useState<EnhancerView | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [agent, setAgent] = useState('code');
  const [allAgents, setAllAgents] = useState(true);
  const [agentPresets, setAgentPresets] = useState<string[]>([]);
  const [enabled, setEnabled] = useState(true);
  const [prompt, setPrompt] = useState('');
  const [name, setName] = useState('');
  const [customId, setCustomId] = useState('');
  const [idEdited, setIdEdited] = useState(false);
  const [description, setDescription] = useState('');
  const [preview, setPreview] = useState<PromptPreviewResponse | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [busy, setBusy] = useState(false);
  const [revision, setRevision] = useState(0);
  const [loading, setLoading] = useState(true);
  const [dirty, setDirty] = useState(false);
  const creating = !previewOnly && enhancerId === 'new';
  const catalog = data?.catalog ?? [];
  const item = catalog.find((row) => row.id === enhancerId);
  const builtin = item?.kind === 'builtin' || Boolean(BEHAVIORS[enhancerId]);

  useEffect(() => {
    onDraftChange?.(dirty, busy);
    const warn = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = '';
    };
    if (dirty || busy) window.addEventListener('beforeunload', warn);
    return () => {
      window.removeEventListener('beforeunload', warn);
      onDraftChange?.(false, false);
    };
  }, [dirty, busy, onDraftChange]);

  useEffect(() => {
    let active = true;
    setLoading(true);
    fetchEnhancers('global', '', '', 'code')
      .then((result) => {
        if (active) setData(result);
      })
      .catch((e) => {
        if (active) setError(String(e));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [revision]);

  useEffect(() => {
    if (!creating) return;
    setError('');
    setDirty(false);
    setIdEdited(false);
    setName('');
    setCustomId('');
    setDescription('');
    setPrompt(PARALLEL_GUIDANCE);
    setSelected([]);
    setAllAgents(true);
    setAgentPresets([]);
    setEnabled(true);
  }, [creating]);
  useEffect(() => {
    if (creating || previewOnly) return;
    setError('');
    setDirty(false);
    if (!item) return;
    setName(item.title || item.name);
    setCustomId(item.id);
    setDescription(item.description);
    setPrompt(item.prompt || item.default_prompt);
    setEnabled(item.enabled);
    setAllAgents(item.all_agents);
    setAgentPresets(item.agents);
    setSelected(item.models.map((m) => JSON.stringify([m.provider, m.model])));
  }, [creating, previewOnly, enhancerId, data?.version]);

  useEffect(() => {
    let active = true;
    if (!previewOnly) return;
    setPreview(null);
    setError('');
    if (!selected.length) return;
    const [provider, model] = JSON.parse(selected[0]) as string[];
    fetchPromptPreview(
      agent.startsWith('prompts.ref.') ? 'code' : agent,
      provider,
      model,
      agent.startsWith('prompts.ref.') ? agent : undefined,
    )
      .then((result) => {
        if (active) setPreview(result);
      })
      .catch((e) => {
        if (active) setError(String(e));
      });
    return () => {
      active = false;
    };
  }, [previewOnly, selected, agent, revision]);

  const selectedModels = useMemo(() => {
    if (selected.length) {
      return selected.map((key) => {
        const [provider, model] = JSON.parse(key) as string[];
        return { provider, model };
      });
    }
    return item?.models ?? [];
  }, [selected, item]);

  async function persist(nextEnabled = enabled, remove = false) {
    if (!data) return;
    const id = creating ? customId.trim() : enhancerId;
    if (!id) {
      setError('请填写 ID。');
      return;
    }
    setBusy(true);
    setError('');
    setNotice('');
    try {
      await applyEnhancer({
        version: data.version,
        id,
        delete: remove,
        name: creating || !builtin ? name : undefined,
        description: creating || !builtin ? description : undefined,
        prompt,
        enabled: nextEnabled,
        models: selectedModels,
        presets: allAgents ? null : agentPresets,
      });
      setNotice(remove ? '已删除该自定义增强。' : nextEnabled ? (creating ? '已创建并启用增强。' : '已保存修改。') : '已关闭增强。');
      setEnabled(nextEnabled);
      setDirty(false);
      onSaved?.();
      if (remove) onSelectEnhancer?.('');
      else if (creating) onSelectEnhancer?.(id);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
      setRevision((r) => r + 1);
    }
  }

  const heading = previewOnly
    ? '最终配置'
    : creating
      ? '新建行为增强'
      : item
        ? `${displayTitle(item)}`
        : '模型行为增强';

  return (
    <section className="cfg-panel cfg-enhancers" aria-label={previewOnly ? 'Effective Config' : 'Model Enhancers'}>
      <header className="cfg-panel-head">
        <div>
          <h2>{heading}</h2>
          {!previewOnly && item && <p className="cfg-id-line">{displayName(item)}</p>}
        </div>
        <button className="cfg-btn cfg-btn-ghost" disabled={busy} onClick={() => setRevision((r) => r + 1)}>
          重新读取
        </button>
      </header>
      <p className="cfg-note">
        {previewOnly
          ? '选择 Agent 和模型，查看实际生效结果。这里不能编辑。'
          : creating
            ? '创建一个模型行为特性。创建后可以随时编辑、开关或删除。'
            : item
              ? displayDescription(item)
              : '每个增强修正一类模型行为。模型只是匹配条件，不是单独的配置对象。'}
      </p>
      {error && (
        <p role="alert" className="cfg-warn">
          {error}
        </p>
      )}
      {notice && (
        <p role="status" className="cfg-ok">
          {notice}
        </p>
      )}
      {loading && <p role="status">正在读取全局配置…</p>}

      {!previewOnly && !creating && !enhancerId && data && (
        <div className="cfg-enhancer-grid">
          {catalog.map((row) => (
            <button key={row.id} type="button" className="cfg-enhancer-card" onClick={() => onSelectEnhancer?.(row.id)}>
              <div className="cfg-enhancer-card-head">
                <strong>{displayTitle(row)}</strong>
                <span className="cfg-id-line">{displayName(row)}</span>
              </div>
              <p>{displayDescription(row)}</p>
              <div className="cfg-enhancer-meta">
                <span className={row.enabled ? 'cfg-pill cfg-pill-on' : 'cfg-pill'}>{row.enabled ? '已启用' : '已关闭'}</span>
                <span className="cfg-pill">{row.kind === 'custom' ? 'Custom' : 'Built-in'}</span>
                <span>适用模型：{row.models.length}</span>
                <span>Agent：{agentLabel(row, data.agents ?? [])}</span>
              </div>
            </button>
          ))}
          <button type="button" className="cfg-enhancer-card cfg-enhancer-create" onClick={() => onSelectEnhancer?.('new')}>
            <strong>＋ 新建行为增强</strong>
            <p>添加一个你自己的模型行为特性，例如并行工具调用。</p>
          </button>
        </div>
      )}

      {!previewOnly && (creating || item) && data && (
        <form
          className="cfg-enhancer-form"
          onSubmit={(e) => {
            e.preventDefault();
            void persist(enabled);
          }}
        >
          <section>
            <h3>① 这个增强解决什么问题</h3>
            {creating || !builtin ? (
              <>
                <label>
                  名称
                  <input
                    value={name}
                    disabled={busy}
                    placeholder="例如：并行工具调用"
                    onChange={(e) => {
                      setName(e.target.value);
                      setDirty(true);
                      if (!idEdited) setCustomId(suggestEnhancerId(e.target.value));
                    }}
                  />
                </label>
                <label>
                  ID
                  <input
                    value={customId}
                    disabled={busy || (!creating && builtin)}
                    placeholder="例如：ParallelToolCalls"
                    onChange={(e) => {
                      setCustomId(e.target.value);
                      setIdEdited(true);
                      setDirty(true);
                    }}
                  />
                </label>
                <p className="cfg-muted">默认根据名称生成，必要时可以改成更稳定的英文 ID。</p>
                <label>
                  这个增强解决什么问题？
                  <textarea
                    value={description}
                    disabled={busy}
                    placeholder="例如：多个工具调用彼此独立时，优先并行执行，减少无意义等待。"
                    onChange={(e) => {
                      setDescription(e.target.value);
                      setDirty(true);
                    }}
                  />
                </label>
              </>
            ) : (
              <p>{displayDescription(item!)}</p>
            )}
          </section>

          <section>
            <h3>② 当前状态</h3>
            <label className="cfg-enhancer-toggle">
              <input
                type="checkbox"
                checked={enabled}
                disabled={busy}
                onChange={(e) => {
                  setEnabled(e.target.checked);
                  setDirty(true);
                }}
              />
              {enabled ? '开启' : '关闭'}
            </label>
            {creating && <p className="cfg-muted">{enabled ? '创建后启用' : '创建后关闭'}</p>}
          </section>

          <section>
            <h3>③ 行为指导</h3>
            <p className="cfg-muted">这是实际给模型的说明。用对方能执行的句子写。</p>
            <textarea
              aria-label="行为指导"
              className="cfg-enhancer-text"
              value={prompt}
              disabled={busy}
              onChange={(e) => {
                setPrompt(e.target.value);
                setDirty(true);
              }}
            />
          </section>

          <section>
            <h3>④ 适用于哪些模型</h3>
            <ModelPicker
              models={data.models}
              selected={selected}
              onChange={(keys) => {
                setSelected(keys);
                setDirty(true);
              }}
              disabled={busy || loading}
            />
          </section>

          <section>
            <h3>⑤ 对哪些 Agent 生效</h3>
            <label className="cfg-enhancer-toggle">
              <input
                type="radio"
                name="agent-scope"
                checked={allAgents}
                disabled={busy}
                onChange={() => {
                  setAllAgents(true);
                  setDirty(true);
                }}
              />
              所有 Agent
            </label>
            <label className="cfg-enhancer-toggle">
              <input
                type="radio"
                name="agent-scope"
                checked={!allAgents}
                disabled={busy}
                onChange={() => {
                  setAllAgents(false);
                  if (!agentPresets.length) setAgentPresets(data.presets.slice(0, 1));
                  setDirty(true);
                }}
              />
              指定 Agent
            </label>
            {!allAgents && (
              <div className="cfg-agent-picks">
                {data.presets.map((preset) => (
                  <label key={preset}>
                    <input
                      type="checkbox"
                      checked={agentPresets.includes(preset)}
                      disabled={busy}
                      onChange={() => {
                        setAgentPresets((current) =>
                          current.includes(preset) ? current.filter((p) => p !== preset) : [...current, preset],
                        );
                        setDirty(true);
                      }}
                    />
                    {agentName(preset)}
                    {(data.agents ?? [])
                      .filter((a) => a.preset === preset)
                      .map((a) => ` / ${agentName(a.name)}`)}
                  </label>
                ))}
              </div>
            )}
            <p className="cfg-muted">只有这些 Agent 使用上述所选模型时，该增强才会生效。</p>
          </section>

          <details className="cfg-advanced">
            <summary>⑥ 高级设置</summary>
            <p>来源：{builtin ? 'Built-in Default' : 'Global Custom'} → Global Override → Effective</p>
            <p>原始 ID：{creating ? customId || '（未定）' : enhancerId}</p>
            <p>保存位置：{data.path}</p>
            <p>实际 prompt</p>
            <pre className="cfg-enhancer-prompt">{prompt}</pre>
            <pre className="cfg-enhancer-prompt">{JSON.stringify(data.config, null, 2)}</pre>
          </details>

          <div className="cfg-enhancer-actions">
            {creating ? (
              <button className="cfg-btn cfg-btn-primary" disabled={busy || !name.trim() || !customId.trim() || !prompt.trim()}>
                {enabled ? '启用增强' : '保存修改'}
              </button>
            ) : (
              <>
                <button
                  type="button"
                  className="cfg-btn cfg-btn-primary"
                  disabled={busy || !prompt.trim()}
                  onClick={() => void persist(true)}
                >
                  {enabled ? '保存修改' : '启用增强'}
                </button>
                {enabled && (
                  <button type="button" className="cfg-btn" disabled={busy} onClick={() => void persist(false)}>
                    关闭增强
                  </button>
                )}
                {!builtin && (
                  <button
                    type="button"
                    className="cfg-btn cfg-btn-danger"
                    disabled={busy}
                    onClick={() => {
                      if (window.confirm('删除这个自定义增强？内置增强不会出现此操作。')) void persist(enabled, true);
                    }}
                  >
                    删除
                  </button>
                )}
              </>
            )}
            <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => onOpenEffective?.()}>
              查看最终配置 →
            </button>
          </div>
        </form>
      )}

      {previewOnly && data && (
        <>
          <div className="cfg-effective-picks">
            <label>
              Agent
              <select aria-label="Agent" value={agent} onChange={(e) => setAgent(e.target.value)}>
                {data.presets.map((p) => (
                  <option key={p} value={p}>
                    {agentName(p)}
                  </option>
                ))}
                {workPrompts.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.title}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <ModelPicker models={data.models} selected={selected} onChange={setSelected} multiple={false} disabled={busy} />
          {!selected.length && <p className="cfg-note">选择模型后，展示这个 Agent + 模型实际生效的配置。</p>}
          {selected.length > 0 && !preview && !error && <p role="status">正在生成最终配置…</p>}
          {preview && (
            <EffectiveResult
              preview={preview}
              agentLabel={agent.startsWith('prompts.ref.') ? workPrompts.find((p) => p.id === agent)?.title ?? agent : agentName(agent)}
              onOpenEnhancer={onSelectEnhancer}
            />
          )}
        </>
      )}
    </section>
  );
}

function EffectiveResult({
  preview,
  agentLabel,
  onOpenEnhancer,
}: {
  preview: PromptPreviewResponse;
  agentLabel: string;
  onOpenEnhancer?: (id: string) => void;
}) {
  const hits = (preview.matches ?? preview.enhancers?.filter((e) => e.enabled).map((e) => ({
    id: e.id,
    title: BEHAVIORS[e.id]?.title ?? e.name,
    name: e.name,
    kind: e.kind ?? 'builtin',
    active: true,
    reasons: [] as string[],
  }))) ?? [];
  const active = hits.filter((row) => row.active);
  const role = preview.slots?.find((s) => s.slot === 'role' && s.emitted)?.text ?? '';
  const guidance = preview.enhancers?.filter((e) => e.enabled).map((e) => e.prompt).join('\n\n') ?? '';
  return (
    <section className="cfg-effective" aria-label="最终生效配置">
      <div className="cfg-effective-formula">
        <span>{agentLabel}</span>
        <span>+</span>
        <span>{preview.model}</span>
        {active.length ? (
          <>
            <span>+</span>
            <span>{active.map((row) => row.title).join('、')}</span>
          </>
        ) : null}
        <span>↓</span>
        <strong>最终生效配置</strong>
      </div>
      <section>
        <h3>Agent 定义摘要</h3>
        {role ? <ReadablePreview text={role} /> : <p className="cfg-muted">没有单独的角色段落。</p>}
        <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => onOpenEnhancer?.('')}>
          去 Agent 页面
        </button>
      </section>
      <section>
        <h3>所选模型</h3>
        <p>
          {preview.provider} / {preview.model}
        </p>
      </section>
      <section>
        <h3>实际命中的 Enhancers</h3>
        {(preview.matches ?? []).map((row) => (
          <article key={row.id} className={row.active ? 'cfg-match is-on' : 'cfg-match'}>
            <header>
              <strong>{row.title}</strong>
              <span>{row.active ? '已生效' : '未生效'}</span>
              <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => onOpenEnhancer?.(row.id)}>
                去修改
              </button>
            </header>
            <p>原因：</p>
            <ul>
              {row.reasons.map((reason) => (
                <li key={reason}>{reason}</li>
              ))}
            </ul>
          </article>
        ))}
        {!preview.matches?.length && <p>没有可解释的增强项。</p>}
      </section>
      <section>
        <h3>最终行为指导</h3>
        {guidance ? <ReadablePreview text={guidance} /> : <p className="cfg-muted">当前没有生效的行为增强。</p>}
      </section>
      <section>
        <h3>可读预览</h3>
        {preview.error ? <p role="alert">{preview.error}</p> : <StructuredPrompt preview={preview} />}
      </section>
      <details className="cfg-advanced">
        <summary>原始文本</summary>
        <pre className="cfg-enhancer-prompt">{preview.compiled_prompt}</pre>
      </details>
    </section>
  );
}

const SLOT_LABELS: Record<string, string> = {
  role: '角色',
  safety: '安全 / 权限约束',
  tools: '工具能力',
  planning: '执行原则',
  style: '执行原则',
  output: '执行原则',
  subagent: '其他核心行为',
  memory_write: '其他核心行为',
  background: '其他核心行为',
  formatting: '其他核心行为',
  user_guide: '其他核心行为',
  project: '其他核心行为',
};

export function StructuredPrompt({ preview }: { preview: PromptPreviewResponse }) {
  const groups: { label: string; text: string }[] = [];
  const order = ['角色', '执行原则', '工具能力', '安全 / 权限约束', '其他核心行为'];
  const bucket = new Map<string, string[]>();
  for (const slot of preview.slots ?? []) {
    if (!slot.emitted || !slot.text?.trim() || slot.slot === 'behavior_hooks' || slot.slot === 'extra') continue;
    const label = SLOT_LABELS[slot.slot] ?? '其他核心行为';
    bucket.set(label, [...(bucket.get(label) ?? []), slot.text]);
  }
  if (preview.context?.tools?.length) {
    bucket.set('工具能力', [...(bucket.get('工具能力') ?? []), preview.context.tools.map((t) => `- ${t}`).join('\n')]);
  }
  for (const label of order) {
    const text = (bucket.get(label) ?? []).join('\n').trim();
    if (text) groups.push({ label, text });
  }
  if (!groups.length && preview.compiled_prompt) {
    return <ReadablePreview text={preview.compiled_prompt} />;
  }
  return (
    <div className="cfg-structured">
      {groups.map((group) => (
        <section key={group.label}>
          <h4>{group.label}</h4>
          <ReadablePreview text={group.text} />
        </section>
      ))}
    </div>
  );
}

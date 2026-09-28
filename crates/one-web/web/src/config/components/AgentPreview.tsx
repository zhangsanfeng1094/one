import { useEffect, useState } from 'react';
import { fetchPromptPreview, type PromptPreviewResponse } from '../api';
import type { DocumentView } from '../types';
import { getPath } from '../jsonPath';
import { StructuredPrompt } from './ModelEnhancers';

export function AgentPreview({
  view,
  onOpenEffective,
}: {
  view: DocumentView;
  onOpenEffective?: () => void;
}) {
  const [preview, setPreview] = useState<PromptPreviewResponse | null>(null);
  const [error, setError] = useState('');
  const id = view.document.id;
  const isPrompt = id.startsWith('prompts.ref.') || id.startsWith('prompts.builtin.preset.');
  useEffect(() => {
    let active = true;
    setPreview(null);
    setError('');
    if (isPrompt) {
      fetchPromptPreview(id.split('.').pop() ?? 'code', '', '', id.startsWith('prompts.ref.') ? id : undefined)
        .then((result) => {
          if (active) {
            setPreview(result);
            setError(result.error ?? '');
          }
        })
        .catch((e) => {
          if (active) setError(String(e));
        });
    }
    return () => {
      active = false;
    };
  }, [id, isPrompt, view.version]);
  if (isPrompt) {
    return (
      <section className="cfg-agent-preview" aria-label="Agent 可读预览">
        <h3>这个 Agent 是谁</h3>
        <p className="cfg-muted">只说明职责和能力。模型行为增强由全局 Model Enhancers 决定。</p>
        {error ? <p role="alert">{error}</p> : preview ? <StructuredPrompt preview={preview} /> : <p>正在生成可读预览…</p>}
        <button type="button" className="cfg-btn cfg-btn-primary" onClick={() => onOpenEffective?.()}>
          查看最终配置 →
        </button>
      </section>
    );
  }
  return (
    <section aria-label="配置可读预览">
      <h3>配置概览</h3>
      <p>{view.document.exists ? '已载入保存的全局配置。' : '尚未自定义，将使用内置默认值。'}</p>
      {view.form?.fields
        .filter((field) => !field.advanced)
        .map((field) => {
          const value = getPath(view.parsed, field.path);
          return (
            <p key={field.path}>
              <b>{field.label}</b>：
              {typeof value === 'boolean' ? (value ? '已启用' : '已停用') : value == null ? '默认' : typeof value === 'object' ? '已配置' : String(value)}
            </p>
          );
        })}
      {view.form?.collections.map((collection) => (
        <p key={collection.path}>
          <b>{collection.label}</b>：在“编辑设置”中查看和管理。
        </p>
      ))}
      {!view.form && <p>此定义的详细内容可在“高级 / 原始配置”中查看。</p>}
    </section>
  );
}

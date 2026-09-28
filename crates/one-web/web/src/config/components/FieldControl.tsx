import React, { useState } from 'react';
import { Eye, EyeOff, Key, Plus, Trash2 } from 'lucide-react';
import { REDACTED, type FieldSpec } from '../types';
import { displayValue, isPlainObject, type Json } from '../jsonPath';

interface Props {
  spec: FieldSpec;
  value: Json;
  onChange: (value: Json) => void;
  /** True when the document is rendered with secrets replaced by the sentinel. */
  masked: boolean;
}

const hint = (spec: FieldSpec): string | null => {
  const parts: string[] = [];
  if (spec.help) parts.push(spec.help);
  if (spec.default !== undefined) parts.push(`默认 ${displayValue(spec.default)}`);
  if (spec.min !== undefined) parts.push(`最小值 ${spec.min}`);
  return parts.length ? parts.join(' · ') : null;
};

/** Renders one declarative field. The server decides which kind applies. */
export const FieldControl: React.FC<Props> = ({ spec, value, onChange, masked }) => {
  const label = (
    <label className="cfg-field-label" htmlFor={`f-${spec.path}`}>
      <span>{spec.label}</span>
      {spec.default !== undefined && (
        <span className="cfg-default-dot" title="该配置项有默认值">•</span>
      )}
    </label>
  );
  const help = hint(spec);
  const body = () => {
    switch (spec.kind) {
      case 'boolean':
        return (
          <div className="cfg-switch-row">
            <label className="cfg-switch">
              <input
                id={`f-${spec.path}`}
                type="checkbox"
                checked={value === true}
                onChange={(e) => onChange(e.target.checked)}
              />
              <span className="cfg-switch-slider" />
            </label>
            <span className="cfg-switch-status">
              {value === undefined ? '未设置（沿用默认）' : value === true ? '已开启' : '已关闭'}
            </span>
            {value !== undefined && (
              <button
                type="button"
                className="cfg-link"
                onClick={() => onChange(undefined)}
                title="恢复为默认（删除该字段）"
              >
                恢复默认
              </button>
            )}
          </div>
        );

      case 'enum':
        return (
          <select
            id={`f-${spec.path}`}
            value={typeof value === 'string' ? value : ''}
            onChange={(e) => onChange(e.target.value === '' ? undefined : e.target.value)}
          >
            <option value="">未设置（默认）</option>
            {spec.options.map((option) => (
              <option key={option} value={option}>
                {option}
              </option>
            ))}
          </select>
        );

      case 'number': {
        const numeric = typeof value === 'number' ? value : '';
        return (
          <input
            id={`f-${spec.path}`}
            type="number"
            min={spec.min}
            max={spec.max}
            value={numeric}
            onChange={(e) => {
              const raw = e.target.value;
              onChange(raw === '' ? undefined : Number(raw));
            }}
          />
        );
      }

      case 'string-list': {
        const list = Array.isArray(value) ? (value as string[]) : [];
        return (
          <textarea
            id={`f-${spec.path}`}
            rows={Math.min(Math.max(list.length, 3), 8)}
            value={list.join('\n')}
            placeholder="每行一项"
            onChange={(e) => {
              const items = e.target.value
                .split('\n')
                .map((line) => line.trim())
                .filter((line) => line.length > 0);
              onChange(items.length ? items : undefined);
            }}
          />
        );
      }

      case 'string-map':
        return <StringMapControl value={value} onChange={onChange} />;

      case 'secret':
        return <SecretControl value={value} onChange={onChange} masked={masked} />;

      default:
        return (
          <input
            id={`f-${spec.path}`}
            type="text"
            value={typeof value === 'string' ? value : ''}
            onChange={(e) => onChange(e.target.value === '' ? undefined : e.target.value)}
          />
        );
    }
  };

  return (
    <div className="cfg-field">
      {label}
      {body()}
      {help && <p className="cfg-help">{help}</p>}
    </div>
  );
};

/**
 * Credential editor with explicit keep / replace / clear semantics.
 *
 * A masked document shows the sentinel rather than the value. Leaving it alone
 * keeps the stored secret (the server restores it), `替换` writes a new value,
 * and `清除` deletes the key.
 */
const SecretControl: React.FC<{
  value: Json;
  onChange: (value: Json) => void;
  masked: boolean;
}> = ({ value, onChange, masked }) => {
  const isSentinel = value === REDACTED;
  const [replacing, setReplacing] = useState(false);
  const [revealed, setRevealed] = useState(false);

  if (isSentinel && !replacing) {
    return (
      <div className="cfg-secret">
        <span className="cfg-secret-value">
          <Key size={12} style={{ display: 'inline', marginRight: 4, verticalAlign: -1 }} />
          {REDACTED}
        </span>
        <span className="cfg-muted">保持当前安全凭据</span>
        <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => setReplacing(true)}>
          替换
        </button>
        <button
          type="button"
          className="cfg-btn cfg-btn-ghost"
          onClick={() => onChange(undefined)}
          title="删除该字段，写入后不再保存任何凭据"
        >
          清除
        </button>
      </div>
    );
  }

  const isReference = typeof value === 'string' && /^\$(\{)?[A-Za-z0-9_]+(\})?$/.test(value);

  return (
    <div className="cfg-secret">
      <input
        type={revealed || isReference ? 'text' : 'password'}
        value={typeof value === 'string' ? value : ''}
        placeholder="留空表示保持默认；建议写成 ${ENV_VAR}"
        onChange={(e) => onChange(e.target.value === '' ? undefined : e.target.value)}
      />
      {!isReference && (
        <button type="button" className="cfg-btn cfg-btn-ghost" onClick={() => setRevealed(!revealed)}>
          {revealed ? <EyeOff size={13} /> : <Eye size={13} />}
          {revealed ? '隐藏' : '显示'}
        </button>
      )}
      {isSentinel && (
        <button
          type="button"
          className="cfg-btn cfg-btn-ghost"
          onClick={() => setReplacing(false)}
        >
          取消替换
        </button>
      )}
      <button
        type="button"
        className="cfg-btn cfg-btn-ghost"
        onClick={() => onChange(undefined)}
      >
        <Trash2 size={13} /> 清除
      </button>
      {masked && (
        <span className="cfg-help cfg-help-inline">
          文档处于脱敏保护：保持原值时提交安全无泄露；填入新值将执行替换。
        </span>
      )}
    </div>
  );
};

/** String→string map editor (MCP `env`, HTTP `headers`). */
const StringMapControl: React.FC<{
  value: Json;
  onChange: (value: Json) => void;
}> = ({ value, onChange }) => {
  const map = isPlainObject(value) ? value : {};
  const entries = Object.entries(map);

  const update = (next: Record<string, Json>) => {
    onChange(Object.keys(next).length ? next : undefined);
  };

  const renameKey = (from: string, to: string) => {
    if (from === to) return;
    const next: Record<string, Json> = {};
    for (const [key, item] of entries) {
      next[key === from ? to : key] = item;
    }
    update(next);
  };

  return (
    <div className="cfg-map">
      {entries.length === 0 && <p className="cfg-muted">暂无自定义项</p>}
      {entries.map(([key, item]) => (
        <div className="cfg-map-row" key={key}>
          <input
            className="cfg-map-key"
            defaultValue={key}
            onBlur={(e) => renameKey(key, e.target.value.trim())}
            placeholder="键名（Key）"
          />
          <input
            className="cfg-map-value"
            value={typeof item === 'string' ? item : displayValue(item)}
            onChange={(e) => update({ ...map, [key]: e.target.value })}
            placeholder="键值（Value，支持 ${ENV_VAR}）"
          />
          <button
            type="button"
            className="cfg-btn cfg-btn-danger"
            onClick={() => {
              const next = { ...map };
              delete next[key];
              update(next);
            }}
          >
            <Trash2 size={13} />
          </button>
        </div>
      ))}
      <div>
        <button
          type="button"
          className="cfg-btn cfg-btn-ghost"
          onClick={() => update({ ...map, '': '' })}
        >
          <Plus size={13} /> 添加一项
        </button>
      </div>
    </div>
  );
};

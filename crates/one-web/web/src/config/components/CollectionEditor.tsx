import React, { useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import type { CollectionSpec } from '../types';
import { FieldControl } from './FieldControl';
import { getPath, isPlainObject, setPath, type Json } from '../jsonPath';

interface Props {
  spec: CollectionSpec;
  /** The collection itself (an object map or an array). */
  value: Json;
  onChange: (value: Json) => void;
  masked: boolean;
  /** Nesting level, used only for indentation. */
  depth?: number;
}

/**
 * Editor for a repeating group of entries.
 *
 * A collection is either a keyed map (`providers`, `mcpServers`) or a list
 * (a provider's `models`), and an entry may itself contain another collection.
 * Entries are edited in place so unknown sub-fields survive untouched.
 */
export const CollectionEditor: React.FC<Props> = ({ spec, value, onChange, masked, depth = 0 }) => {
  const [pendingKey, setPendingKey] = useState('');

  const asArray = Array.isArray(value);
  const map = isPlainObject(value) ? value : {};
  const list = asArray ? (value as Json[]) : [];
  const keys = Object.keys(map);

  const writeMap = (next: Record<string, Json>) => onChange(Object.keys(next).length ? next : undefined);
  const writeList = (next: Json[]) => onChange(next.length ? next : undefined);

  const renderEntry = (
    entryValue: Json,
    entryKey: string | number,
    onEntryChange: (next: Json) => void,
    onRemove: () => void,
    keyEditor: React.ReactNode
  ) => (
    <div
      className="cfg-entry"
      key={String(entryKey)}
      style={{ marginLeft: depth > 0 ? depth * 12 : undefined }}
    >
      <div className="cfg-entry-head">
        {keyEditor}
        <button
          type="button"
          className="cfg-btn cfg-btn-danger"
          onClick={onRemove}
          title="移除此项"
        >
          <Trash2 size={13} /> 删除
        </button>
      </div>

      <div className="cfg-entry-fields">
        {spec.entry_fields.map((field) => (
          <FieldControl
            key={field.path}
            spec={field}
            value={getPath(entryValue, field.path)}
            masked={masked}
            onChange={(next) => onEntryChange(setPath(entryValue, field.path, next))}
          />
        ))}
      </div>

      {spec.nested && (
        <div className="cfg-nested">
          <CollectionEditor
            spec={spec.nested}
            value={getPath(entryValue, spec.nested.path)}
            masked={masked}
            depth={depth + 1}
            onChange={(next) => onEntryChange(setPath(entryValue, spec.nested!.path, next))}
          />
        </div>
      )}
    </div>
  );

  return (
    <section className="cfg-collection">
      <header className="cfg-collection-head">
        <h4>{spec.label}</h4>
        <span className="cfg-muted">
          {asArray ? `${list.length} 个配置项` : `${keys.length} 个实例`} · 标识：{spec.key_label}
        </span>
      </header>

      {asArray
        ? list.map((entry, index) =>
            renderEntry(
              entry,
              index,
              (next) => {
                const nextList = [...list];
                nextList[index] = next;
                writeList(nextList);
              },
              () => writeList(list.filter((_, i) => i !== index)),
              <span className="cfg-entry-key">#{index + 1}</span>
            )
          )
        : keys.map((key) =>
            renderEntry(
              map[key],
              key,
              (next) => writeMap({ ...map, [key]: next }),
              () => {
                const next = { ...map };
                delete next[key];
                writeMap(next);
              },
              spec.key_immutable || masked ? (
                <span
                  className="cfg-entry-key"
                  title={
                    masked && !spec.key_immutable
                      ? '脱敏状态下不能改名，否则无法还原凭据。请先解锁后再改。'
                      : undefined
                  }
                >
                  {key}
                </span>
              ) : (
                <input
                  className="cfg-entry-key-input"
                  defaultValue={key}
                  onBlur={(e) => {
                    const renamed = e.target.value.trim();
                    if (!renamed || renamed === key) {
                      e.target.value = key;
                      return;
                    }
                    const next: Record<string, Json> = {};
                    for (const [existing, item] of Object.entries(map)) {
                      next[existing === key ? renamed : existing] = item;
                    }
                    writeMap(next);
                  }}
                />
              )
            )
          )}

      {asArray ? (
        <div>
          <button
            type="button"
            className="cfg-btn cfg-btn-ghost"
            onClick={() => writeList([...list, {}])}
          >
            <Plus size={13} /> 添加一项
          </button>
        </div>
      ) : (
        <div className="cfg-inline" style={{ marginTop: 4 }}>
          <input
            type="text"
            value={pendingKey}
            placeholder={`新增 ${spec.key_label} 标识（ID）`}
            onChange={(e) => setPendingKey(e.target.value)}
          />
          <button
            type="button"
            className="cfg-btn cfg-btn-ghost"
            disabled={!pendingKey.trim() || keys.includes(pendingKey.trim())}
            onClick={() => {
              const key = pendingKey.trim();
              writeMap({ ...map, [key]: {} });
              setPendingKey('');
            }}
          >
            <Plus size={13} /> 添加条目
          </button>
        </div>
      )}
    </section>
  );
};

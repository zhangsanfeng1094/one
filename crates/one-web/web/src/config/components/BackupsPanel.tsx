import React, { useCallback, useEffect, useState } from 'react';
import { History, RotateCcw } from 'lucide-react';
import { ApiError, fetchBackups, restoreBackup } from '../api';
import type { BackupRecord } from '../types';
import { Diagnostics } from './Diagnostics';

interface Props {
  docId: string;
  /** Current on-disk version, re-checked by the server before restoring. */
  version: string;
  /** Called after a successful restore so the caller can reload the document. */
  onRestored: (message: string) => void;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  return `${(bytes / 1024).toFixed(1)} KB`;
}

/**
 * Backup list and restore.
 *
 * Restores go through the same validation and version check as a normal save:
 * putting back a snapshot that the current rules reject is refused rather than
 * silently writing a broken config.
 */
export const BackupsPanel: React.FC<Props> = ({ docId, version, onRestored }) => {
  const [backups, setBackups] = useState<BackupRecord[] | null>(null);
  const [directory, setDirectory] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(() => {
    setError(null);
    fetchBackups(docId)
      .then((response) => {
        setBackups(response.backups);
        setDirectory(response.directory);
      })
      .catch((err: unknown) => setError(err instanceof ApiError ? err.message : String(err)));
  }, [docId]);

  useEffect(load, [load]);

  const handleRestore = (backup: BackupRecord) => {
    const confirmed = window.confirm(
      `恢复历史备份 ${backup.id}？\n\n` +
        `创建时间：${backup.created_at}\n` +
        `内容版本：${backup.version.slice(0, 8)}\n\n` +
        '当前内容会先被自动备份，因此这次恢复操作本身也可以撤销。'
    );
    if (!confirmed) return;

    setBusy(backup.id);
    setError(null);
    restoreBackup(docId, backup.id, version)
      .then((result) => {
        onRestored(
          `已恢复备份 ${backup.id}（新版本 ${result.version.slice(0, 8)}）` +
            (result.backup ? `，恢复前的内容也已自动备份为 ${result.backup.id}` : '')
        );
      })
      .catch((err: unknown) => {
        if (err instanceof ApiError) {
          setError(err.message);
        } else {
          setError(String(err));
        }
      })
      .finally(() => setBusy(null));
  };

  return (
    <div className="cfg-panel">
      <header className="cfg-panel-head">
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <History size={16} color="#818cf8" />
          <h3>配置快照与历史备份</h3>
        </div>
        <span className="cfg-muted">自动滚动保留最近 10 份历史快照</span>
      </header>

      <p className="cfg-note">
        存储目录：<code>{directory || '—'}</code>
        <br />
        备份存放在该隔离目录下，不参与任何运行时加载。每次保存或恢复前会自动记录快照，保障配置安全。
      </p>

      {error && <Diagnostics diagnostics={[{ severity: 'error', message: error }]} />}

      {backups === null && <p className="cfg-muted">正在载入备份快照…</p>}
      {backups !== null && backups.length === 0 && (
        <p className="cfg-muted">暂无历史备份。首次保存或修改配置时将自动生成。</p>
      )}

      {backups !== null && backups.length > 0 && (
        <table className="cfg-table">
          <thead>
            <tr>
              <th>快照生成时间</th>
              <th>版本 Hash</th>
              <th>体积</th>
              <th>触发原因</th>
              <th style={{ textAlign: 'right' }}>操作</th>
            </tr>
          </thead>
          <tbody>
            {backups.map((backup) => (
              <tr key={backup.id}>
                <td>{backup.created_at}</td>
                <td>
                  <code>{backup.version.slice(0, 8)}</code>
                </td>
                <td>{formatBytes(backup.size)}</td>
                <td>
                  <span className="cfg-pill">
                    {backup.reason === 'restore' ? '恢复前快照' : '保存前快照'}
                  </span>
                </td>
                <td style={{ textAlign: 'right' }}>
                  <button
                    type="button"
                    className="cfg-btn cfg-btn-ghost"
                    disabled={busy !== null}
                    onClick={() => handleRestore(backup)}
                  >
                    <RotateCcw size={13} /> {busy === backup.id ? '恢复中…' : '恢复此版'}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
};

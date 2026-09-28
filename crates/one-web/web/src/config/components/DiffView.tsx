import React from 'react';
import type { ValidateResponse } from '../types';

/**
 * Unified diff preview for a pending save.
 *
 * The text comes from the server already masked, so a credential can never be
 * shown here even though the real file contains it. Hunk headers are rendered
 * verbatim so the preview matches what a normal `diff` would print.
 */
export const DiffView: React.FC<{ result: ValidateResponse }> = ({ result }) => {
  if (!result.diff.trim()) {
    return <p className="cfg-ok">与磁盘上的内容一致，没有改动。</p>;
  }

  return (
    <div className="cfg-diff-wrap">
      <p className="cfg-diff-summary">
        <span className="cfg-diff-added">+{result.added}</span>
        <span className="cfg-diff-removed">−{result.removed}</span>
        <span className="cfg-muted">目标：{result.target}</span>
        {result.approximate && (
          <span className="cfg-warn">
            改动范围过大，已退化为整块替换预览（仍会完整写入）
          </span>
        )}
      </p>
      <pre className="cfg-diff">
        {result.diff.split('\n').map((line, index) => {
          if (line === '') return null;
          let className = 'cfg-diff-line';
          if (line.startsWith('@@')) className += ' cfg-diff-hunk';
          else if (line.startsWith('+')) className += ' cfg-diff-add';
          else if (line.startsWith('-')) className += ' cfg-diff-del';
          return (
            // Diff lines have no stable identity; position is the identity here.
            <div className={className} key={index}>
              {line}
            </div>
          );
        })}
      </pre>
    </div>
  );
};

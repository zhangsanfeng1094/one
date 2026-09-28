import React from 'react';
import { AlertCircle, AlertTriangle, Info } from 'lucide-react';
import type { Diagnostic } from '../types';

const ICONS = {
  error: AlertCircle,
  warning: AlertTriangle,
  info: Info,
} as const;

const LABELS = {
  error: '错误',
  warning: '警告',
  info: '提示',
} as const;

/**
 * Renders validation findings.
 *
 * Findings carry the same severity the runtime would apply: `error` blocks a
 * save, everything else is advisory. Line/column and field path are shown when
 * known so the user can jump straight to the problem.
 */
export const Diagnostics: React.FC<{
  diagnostics: Diagnostic[];
  emptyMessage?: string;
}> = ({ diagnostics, emptyMessage }) => {
  if (diagnostics.length === 0) {
    return emptyMessage ? <p className="cfg-ok">{emptyMessage}</p> : null;
  }

  return (
    <ul className="cfg-diagnostics">
      {diagnostics.map((item, index) => {
        const Icon = ICONS[item.severity];
        const location = [
          item.field ? `字段 ${item.field}` : null,
          item.line ? `第 ${item.line} 行${item.column ? `:${item.column}` : ''}` : null,
        ]
          .filter(Boolean)
          .join(' · ');

        return (
          <li key={`${index}-${item.message}`} className={`cfg-diagnostic cfg-diag-${item.severity}`}>
            <Icon size={14} aria-hidden />
            <div>
              <span className="cfg-diag-label">{LABELS[item.severity]}</span>
              <span className="cfg-diag-message">{item.message}</span>
              {location && <span className="cfg-diag-location">{location}</span>}
            </div>
          </li>
        );
      })}
    </ul>
  );
};

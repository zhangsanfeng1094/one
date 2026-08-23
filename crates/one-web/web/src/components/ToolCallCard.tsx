import React, { useState } from 'react';
import { ToolCallItem } from '../types/acp';
import { Terminal, FileCode, Check, X, Loader2, ChevronDown, ChevronRight, Copy } from 'lucide-react';

interface ToolCallCardProps {
  tool: ToolCallItem;
}

export const ToolCallCard: React.FC<ToolCallCardProps> = ({ tool }) => {
  const [isExpanded, setIsExpanded] = useState(true);
  const [isCopied, setIsCopied] = useState(false);

  const getStatusBadge = () => {
    switch (tool.status) {
      case 'running':
      case 'pending':
        return (
          <span className="status-pill running">
            <Loader2 size={10} className="animate-spin" /> running
          </span>
        );
      case 'completed':
        return (
          <span className="status-pill completed">
            <Check size={10} /> completed
          </span>
        );
      case 'failed':
      case 'cancelled':
        return (
          <span className="status-pill failed">
            <X size={10} /> {tool.status}
          </span>
        );
      default:
        return <span className="status-pill">{tool.status}</span>;
    }
  };

  const formattedContent =
    (tool.content || []).join('\n') || (tool.rawInput ? JSON.stringify(tool.rawInput, null, 2) : '');

  const handleCopy = (e: React.MouseEvent) => {
    e.stopPropagation();
    if (formattedContent) {
      navigator.clipboard.writeText(formattedContent);
      setIsCopied(true);
      setTimeout(() => setIsCopied(false), 2000);
    }
  };

  return (
    <div className="tool-card">
      <div className="tool-header" onClick={() => setIsExpanded(!isExpanded)}>
        <div className="tool-title">
          {tool.kind === 'terminal' ? (
            <Terminal size={13} style={{ opacity: 0.8 }} />
          ) : (
            <FileCode size={13} style={{ opacity: 0.8 }} />
          )}
          <span>{tool.title || tool.id}</span>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          {getStatusBadge()}
          {formattedContent && (
            <button
              className="btn-copy-code"
              onClick={handleCopy}
              title="Copy output"
              style={{ marginLeft: 4 }}
            >
              {isCopied ? <Check size={10} /> : <Copy size={10} />}
              <span>{isCopied ? 'Copied' : 'Copy'}</span>
            </button>
          )}
          {isExpanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        </div>
      </div>
      {isExpanded && formattedContent && <pre className="tool-output">{formattedContent}</pre>}
    </div>
  );
};

import React, { useEffect, useRef } from 'react';
import { ChatMessage, ApprovalRequest } from '../types/acp';
import { ThoughtBlock } from './ThoughtBlock';
import { ToolCallCard } from './ToolCallCard';
import { ApprovalCard } from './ApprovalCard';
import { marked } from 'marked';
import hljs from 'highlight.js';
import { Terminal, FileCode, Search, Compass, Loader2 } from 'lucide-react';

interface ChatAreaProps {
  messages: ChatMessage[];
  pendingApprovals: ApprovalRequest[];
  onRespondApproval: (rpcId: number | string, optionId: string) => void;
  onCancelApproval: (rpcId: number | string) => void;
  onSelectQuickPrompt: (text: string) => void;
}

// Configure marked with highlight.js
const renderer = new marked.Renderer();
renderer.code = function (code: string, infostring: string | undefined): string {
  const language = (infostring || '').trim();
  let highlighted = '';
  if (language && hljs.getLanguage(language)) {
    try {
      highlighted = hljs.highlight(code, { language }).value;
    } catch {
      highlighted = code;
    }
  } else {
    try {
      highlighted = hljs.highlightAuto(code).value;
    } catch {
      highlighted = code;
    }
  }
  const langLabel = language || 'text';
  const encodedCode = encodeURIComponent(code);
  return `<div class="code-block-wrapper">
    <div class="code-header">
      <span>${langLabel}</span>
      <button class="btn-copy-code" data-code="${encodedCode}" onclick="(function(btn){
        navigator.clipboard.writeText(decodeURIComponent(btn.getAttribute('data-code')));
        btn.innerHTML = '<span>Copied</span>';
        setTimeout(() => { btn.innerHTML = '<span>Copy</span>'; }, 2000);
      })(this)"><span>Copy</span></button>
    </div>
    <pre><code class="hljs ${language}">${highlighted}</code></pre>
  </div>`;
};

marked.setOptions({
  renderer,
  gfm: true,
  breaks: true,
});

export const ChatArea: React.FC<ChatAreaProps> = ({
  messages,
  pendingApprovals,
  onRespondApproval,
  onCancelApproval,
  onSelectQuickPrompt,
}) => {
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (containerRef.current) {
      containerRef.current.scrollTop = containerRef.current.scrollHeight;
    }
  }, [messages, pendingApprovals]);

  const renderMarkdown = (text: string) => {
    if (!text) return '';
    try {
      return marked.parse(text) as string;
    } catch {
      return text;
    }
  };

  return (
    <div className="chat-container" ref={containerRef}>
      {messages.length === 0 ? (
        <div className="welcome-container">
          <div className="welcome-header">
            <div className="welcome-logo">
              <Terminal size={22} />
            </div>
            <h1 className="welcome-title">One Agent</h1>
            <p className="welcome-subtitle">
              Autonomous engineering agent operating over the Agent Client Protocol.
            </p>
          </div>

          <div className="action-grid">
            <button
              className="action-card"
              onClick={() => onSelectQuickPrompt('Explore this codebase and give me an architectural overview.')}
            >
              <div className="action-card-header">
                <Search size={14} style={{ color: '#3b82f6' }} />
                <span>Explore Codebase</span>
              </div>
              <div className="action-card-desc">
                Map project structure, major modules, and core dependencies.
              </div>
            </button>

            <button
              className="action-card"
              onClick={() => onSelectQuickPrompt('Run cargo test and report any failures.')}
            >
              <div className="action-card-header">
                <Terminal size={14} style={{ color: '#10b981' }} />
                <span>Run Workspace Tests</span>
              </div>
              <div className="action-card-desc">
                Execute test suites and diagnose any regressions.
              </div>
            </button>

            <button
              className="action-card"
              onClick={() => onSelectQuickPrompt('Review recent git status and diff changes.')}
            >
              <div className="action-card-header">
                <FileCode size={14} style={{ color: '#8b5cf6' }} />
                <span>Review Changes</span>
              </div>
              <div className="action-card-desc">
                Inspect modified files and review diffs for quality.
              </div>
            </button>

            <button
              className="action-card"
              onClick={() => onSelectQuickPrompt('/plan Draft a refactoring strategy for the workspace.')}
            >
              <div className="action-card-header">
                <Compass size={14} style={{ color: '#f59e0b' }} />
                <span>Plan Architecture</span>
              </div>
              <div className="action-card-desc">
                Enter read-only plan mode to design implementation steps.
              </div>
            </button>
          </div>
        </div>
      ) : (
        messages.map((msg) => (
          <div key={msg.id} className={`msg-row ${msg.role}`}>
            {msg.role === 'assistant' && (
              <div className="msg-avatar-icon">
                <Terminal size={13} />
              </div>
            )}
            <div className="msg-content">
              {msg.role === 'user' ? (
                <div className="msg-bubble-user">{msg.text}</div>
              ) : (
                <div className="msg-assistant-body">
                  {msg.thought && <ThoughtBlock thought={msg.thought} isStreaming={msg.isStreaming} />}

                  {msg.toolCalls && msg.toolCalls.length > 0 && (
                    <div style={{ margin: '6px 0' }}>
                      {msg.toolCalls.map((tc) => (
                        <ToolCallCard key={tc.id} tool={tc} />
                      ))}
                    </div>
                  )}

                  {msg.text ? (
                    <div
                      className="markdown-body"
                      dangerouslySetInnerHTML={{ __html: renderMarkdown(msg.text) }}
                    />
                  ) : msg.isStreaming && !msg.thought && (!msg.toolCalls || msg.toolCalls.length === 0) ? (
                    <div style={{ display: 'flex', alignItems: 'center', gap: 6, color: '#71717a', fontSize: 12 }}>
                      <Loader2 size={12} className="animate-spin" />
                      <span>Thinking...</span>
                    </div>
                  ) : null}
                </div>
              )}
            </div>
            {msg.role === 'user' && (
              <div className="msg-avatar-icon" style={{ background: '#27272a', color: '#f4f4f5' }}>
                U
              </div>
            )}
          </div>
        ))
      )}

      {/* Pending Approvals */}
      {pendingApprovals.map((req) => (
        <div key={req.rpcId} className="msg-row assistant">
          <div className="msg-avatar-icon">
            <Terminal size={13} />
          </div>
          <div className="msg-content">
            <ApprovalCard
              request={req}
              onRespond={onRespondApproval}
              onCancel={onCancelApproval}
            />
          </div>
        </div>
      ))}
    </div>
  );
};

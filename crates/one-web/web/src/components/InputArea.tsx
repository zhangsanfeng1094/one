import React, { useState, useRef, useEffect } from 'react';
import { AgentMode } from '../types/acp';
import { CornerDownLeft, Square, Zap, Compass, Terminal, Shield, HelpCircle, Layers } from 'lucide-react';

interface InputAreaProps {
  mode: AgentMode;
  isStreaming: boolean;
  onSend: (text: string) => void;
  onCancel: () => void;
  onModeChange: (mode: AgentMode) => void;
}

const SLASH_COMMANDS = [
  { cmd: '/help', desc: 'Show available commands', icon: HelpCircle },
  { cmd: '/clear', desc: 'Clear conversation view', icon: Layers },
  { cmd: '/plan', desc: 'Switch to Plan mode (read-only)', icon: Compass },
  { cmd: '/act', desc: 'Switch to Act mode (execute)', icon: Zap },
  { cmd: '/status', desc: 'Check agent and server status', icon: Terminal },
  { cmd: '/mcp', desc: 'List connected MCP servers', icon: Shield },
];

export const InputArea: React.FC<InputAreaProps> = ({
  mode,
  isStreaming,
  onSend,
  onCancel,
  onModeChange,
}) => {
  const [text, setText] = useState('');
  const [showSlash, setShowSlash] = useState(false);
  const [slashIndex, setSlashIndex] = useState(0);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (textareaRef.current) {
      textareaRef.current.style.height = 'auto';
      textareaRef.current.style.height = `${Math.min(textareaRef.current.scrollHeight, 200)}px`;
    }
  }, [text]);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (showSlash) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSlashIndex((prev) => (prev + 1) % SLASH_COMMANDS.length);
        return;
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSlashIndex((prev) => (prev - 1 + SLASH_COMMANDS.length) % SLASH_COMMANDS.length);
        return;
      } else if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        applySlash(SLASH_COMMANDS[slashIndex].cmd);
        return;
      } else if (e.key === 'Escape') {
        setShowSlash(false);
        return;
      }
    }

    // Toggle mode with Tab when text is empty
    if (e.key === 'Tab' && !text.trim()) {
      e.preventDefault();
      onModeChange(mode === 'act' ? 'plan' : 'act');
      return;
    }

    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSubmit();
    }
  };

  const handleChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    setText(val);
    if (val.startsWith('/') && !val.includes(' ')) {
      setShowSlash(true);
    } else {
      setShowSlash(false);
    }
  };

  const applySlash = (cmd: string) => {
    setText(cmd + ' ');
    setShowSlash(false);
    textareaRef.current?.focus();
  };

  const handleSubmit = () => {
    if (!text.trim() || isStreaming) return;
    onSend(text.trim());
    setText('');
    setShowSlash(false);
  };

  return (
    <div className="input-dock">
      {showSlash && (
        <div className="slash-popover">
          {SLASH_COMMANDS.map((item, idx) => {
            const Icon = item.icon;
            return (
              <div
                key={item.cmd}
                className={`slash-option ${idx === slashIndex ? 'selected' : ''}`}
                onClick={() => applySlash(item.cmd)}
              >
                <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                  <Icon size={13} style={{ opacity: 0.7 }} />
                  <span className="slash-cmd">{item.cmd}</span>
                </div>
                <span className="slash-desc">{item.desc}</span>
              </div>
            );
          })}
        </div>
      )}

      <div className="input-box">
        <textarea
          ref={textareaRef}
          className="prompt-textarea"
          rows={1}
          placeholder={
            mode === 'plan'
              ? 'Describe architectural goals or ask questions (Plan mode: read-only)...'
              : 'Ask One to build, edit files, or execute tests...'
          }
          value={text}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
        />

        <div className="input-toolbar">
          <div className="input-toolbar-left">
            <span className="btn-tag-pill" title="Press Tab to toggle mode">
              {mode === 'act' ? <Zap size={10} fill="currentColor" /> : <Compass size={10} />}
              <span>{mode.toUpperCase()}</span>
            </span>
            <span style={{ fontSize: 11, color: '#71717a' }}>Type / for commands</span>
          </div>

          <div>
            {isStreaming ? (
              <button className="btn-stop" onClick={onCancel} title="Stop generation">
                <Square size={11} fill="currentColor" />
                <span>Stop</span>
              </button>
            ) : (
              <button
                className="btn-send"
                disabled={!text.trim()}
                onClick={handleSubmit}
                title="Send message (Enter)"
              >
                <span>Send</span>
                <CornerDownLeft size={11} />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
};

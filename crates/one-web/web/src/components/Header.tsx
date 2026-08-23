import React from 'react';
import { AgentMode, ThinkingLevel } from '../types/acp';
import { Brain, Zap, Compass, Menu } from 'lucide-react';

interface HeaderProps {
  currentSessionTitle: string;
  mode: AgentMode;
  thinkingLevel: ThinkingLevel;
  onModeChange: (mode: AgentMode) => void;
  onThinkingChange: (level: ThinkingLevel) => void;
  onToggleSidebar: () => void;
}

export const Header: React.FC<HeaderProps> = ({
  currentSessionTitle,
  mode,
  thinkingLevel,
  onModeChange,
  onThinkingChange,
  onToggleSidebar,
}) => {
  return (
    <header className="top-bar">
      <div className="top-bar-left">
        <button
          className="btn-mobile-menu"
          onClick={onToggleSidebar}
          aria-label="Toggle sessions menu"
        >
          <Menu size={16} />
        </button>

        <span className="session-title" title={currentSessionTitle}>
          {currentSessionTitle}
        </span>

        <div className="mode-segmented">
          <button
            className={`mode-segment-btn ${mode === 'act' ? 'active' : ''}`}
            onClick={() => onModeChange('act')}
            title="Act Mode: execute edits and commands"
          >
            <Zap size={11} fill={mode === 'act' ? 'currentColor' : 'none'} />
            <span>Act</span>
          </button>
          <button
            className={`mode-segment-btn ${mode === 'plan' ? 'active' : ''}`}
            onClick={() => onModeChange('plan')}
            title="Plan Mode: read-only architecture and design"
          >
            <Compass size={11} />
            <span>Plan</span>
          </button>
        </div>
      </div>

      <div className="top-bar-right">
        <div className="config-selector">
          <Brain size={12} style={{ opacity: 0.8 }} />
          <span className="config-selector-label">Thinking:</span>
          <select
            className="select-clean"
            value={thinkingLevel}
            onChange={(e) => onThinkingChange(e.target.value as ThinkingLevel)}
          >
            <option value="off">Off</option>
            <option value="low">Low</option>
            <option value="medium">Med</option>
            <option value="high">High</option>
          </select>
        </div>
      </div>
    </header>
  );
};

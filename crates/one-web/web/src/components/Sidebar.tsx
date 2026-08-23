import React from 'react';
import { SessionInfo, ServerInfo } from '../types/acp';
import { Plus, MessageSquare, Folder, Terminal, X } from 'lucide-react';

interface SidebarProps {
  sessions: SessionInfo[];
  currentSessionId: string | null;
  serverInfo: ServerInfo | null;
  isConnected: boolean;
  isOpen: boolean;
  onClose: () => void;
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
}

export const Sidebar: React.FC<SidebarProps> = ({
  sessions,
  currentSessionId,
  serverInfo,
  isConnected,
  isOpen,
  onClose,
  onSelectSession,
  onNewSession,
}) => {
  return (
    <aside className={`sidebar ${isOpen ? 'open' : ''}`}>
      <div className="sidebar-header">
        <div className="brand">
          <div className="brand-icon">
            <Terminal size={14} />
          </div>
          <span className="brand-name">One</span>
          <span className="brand-badge">ACP</span>
        </div>
        <button
          className="btn-sidebar-close"
          onClick={onClose}
          aria-label="Close sidebar"
        >
          <X size={16} />
        </button>
      </div>

      <div className="sidebar-action-bar">
        <button
          className="btn-new-session"
          onClick={() => {
            onNewSession();
            onClose();
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <Plus size={14} />
            <span>New Session</span>
          </div>
          <span className="kbd-shortcut">⌘N</span>
        </button>
      </div>

      <div className="sidebar-section-label">Sessions</div>
      <div className="session-list">
        {sessions.length === 0 ? (
          <div style={{ padding: '20px 12px', color: '#71717a', fontSize: 12, textAlign: 'center' }}>
            No active sessions
          </div>
        ) : (
          sessions.map((s) => {
            const label = s.title || s.sessionId;
            const folderName = s.cwd ? s.cwd.split('/').filter(Boolean).pop() : '';
            const isActive = s.sessionId === currentSessionId;
            return (
              <div
                key={s.sessionId}
                className={`session-item ${isActive ? 'active' : ''}`}
                onClick={() => {
                  onSelectSession(s.sessionId);
                  onClose();
                }}
              >
                <MessageSquare size={13} style={{ flexShrink: 0, opacity: isActive ? 1 : 0.6 }} />
                <div className="session-item-content">
                  <div className="session-name">{label}</div>
                  {folderName && (
                    <div className="session-meta">
                      <Folder size={10} />
                      <span>{folderName}</span>
                    </div>
                  )}
                </div>
              </div>
            );
          })
        )}
      </div>

      <div className="sidebar-footer">
        <div className="connection-indicator">
          <div className={`status-dot ${isConnected ? 'online' : ''}`} />
          <span style={{ fontWeight: 500, color: isConnected ? '#f4f4f5' : '#71717a', fontSize: 12 }}>
            {isConnected ? 'Connected' : 'Connecting...'}
          </span>
        </div>
        {serverInfo?.cwd && (
          <div className="workspace-cwd" title={serverInfo.cwd}>
            <Folder size={11} style={{ flexShrink: 0 }} />
            <span>{serverInfo.cwd}</span>
          </div>
        )}
      </div>
    </aside>
  );
};

import React, { useState, useEffect, useRef } from 'react';
import { Sidebar } from './components/Sidebar';
import { Header } from './components/Header';
import { ChatArea } from './components/ChatArea';
import { InputArea } from './components/InputArea';
import {
  ChatMessage,
  SessionInfo,
  ServerInfo,
  AgentMode,
  ThinkingLevel,
  ApprovalRequest,
  ToolCallItem,
} from './types/acp';
import { acpClient } from './services/acpClient';

export const App: React.FC = () => {
  const [isConnected, setIsConnected] = useState(false);
  const [serverInfo, setServerInfo] = useState<ServerInfo | null>(null);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [currentSessionId, setCurrentSessionId] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [pendingApprovals, setPendingApprovals] = useState<ApprovalRequest[]>([]);
  const [mode, setMode] = useState<AgentMode>('act');
  const [thinkingLevel, setThinkingLevel] = useState<ThinkingLevel>('medium');
  const [isStreaming, setIsStreaming] = useState(false);
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);

  const activeAssistantMsgIdRef = useRef<string | null>(null);
  const currentSessionIdRef = useRef<string | null>(null);
  currentSessionIdRef.current = currentSessionId;

  // 1. Fetch server info
  useEffect(() => {
    fetch('/api/info')
      .then((res) => (res.ok ? res.json() : null))
      .then((data) => {
        if (data) setServerInfo(data);
      })
      .catch((err) => console.log('Server info fetch fallback:', err));
  }, []);

  // 2. Global keyboard shortcuts
  useEffect(() => {
    const handleGlobalKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'n') {
        e.preventDefault();
        handleNewSession();
      }
    };
    window.addEventListener('keydown', handleGlobalKeyDown);
    return () => window.removeEventListener('keydown', handleGlobalKeyDown);
  }, []);

  // 3. Connect ACP WebSocket & handle events
  useEffect(() => {
    const unsubConn = acpClient.onConnectionChange((connected) => {
      setIsConnected(connected);
      if (connected) {
        loadSessionList();
      }
    });

    const unsubMsg = acpClient.onMessage((method, params) => {
      if (method === 'session/update') {
        handleSessionUpdate(params);
      }
    });

    const unsubAppr = acpClient.onApproval((req) => {
      setPendingApprovals((prev) => [...prev, req]);
    });

    acpClient.connect();

    return () => {
      unsubConn();
      unsubMsg();
      unsubAppr();
    };
  }, []);

  const loadSessionList = async () => {
    try {
      const resp = await acpClient.listSessions();
      if (resp && resp.sessions) {
        const list: SessionInfo[] = resp.sessions.map((s: any) => ({
          sessionId: s.sessionId || s.id || '',
          cwd: s.cwd || '',
          title: s.title || s.sessionId || 'Untitled Session',
          updatedAt: s.updatedAt,
        }));
        setSessions(list);
        if (!currentSessionIdRef.current) {
          if (list.length > 0) {
            selectSession(list[0].sessionId);
          } else {
            handleNewSession();
          }
        }
      }
    } catch (e) {
      console.error('Failed to list sessions:', e);
    }
  };

  const selectSession = async (sessionId: string) => {
    setCurrentSessionId(sessionId);
    setMessages([]);
    setPendingApprovals([]);
    setIsStreaming(false);
    activeAssistantMsgIdRef.current = null;
    setIsSidebarOpen(false);

    try {
      await acpClient.loadSession(sessionId);
    } catch (e) {
      console.error('Failed to load session:', e);
    }
  };

  const handleNewSession = async () => {
    try {
      const resp = await acpClient.newSession();
      if (resp && resp.sessionId) {
        await loadSessionList();
        selectSession(resp.sessionId);
      }
    } catch (e) {
      console.error('Failed to create session:', e);
    }
  };

  const ensureAssistantMessage = (): string => {
    if (activeAssistantMsgIdRef.current) {
      return activeAssistantMsgIdRef.current;
    }
    const newId = 'agent-' + Date.now();
    activeAssistantMsgIdRef.current = newId;
    setMessages((prev) => [
      ...prev,
      {
        id: newId,
        role: 'assistant',
        text: '',
        thought: '',
        toolCalls: [],
        timestamp: Date.now(),
        isStreaming: true,
      },
    ]);
    return newId;
  };

  const handleSessionUpdate = (params: any) => {
    const update = params.update || {};
    const updateType = update.sessionUpdate || update.session_update;

    // 1. Text chunk
    if (updateType === 'agent_message_chunk' || updateType === 'agentMessageChunk' || update.agent_message_chunk || update.agentMessageChunk) {
      const content = update.content || update.agent_message_chunk?.content || update.agentMessageChunk?.content;
      const text = typeof content === 'string' ? content : (content?.text || '');
      if (text) {
        const msgId = ensureAssistantMessage();
        setMessages((prev) =>
          prev.map((m) => (m.id === msgId ? { ...m, text: m.text + text, isStreaming: true } : m))
        );
      }
    }

    // 2. User message chunk (from history replay)
    if (updateType === 'user_message_chunk' || updateType === 'userMessageChunk' || update.user_message_chunk || update.userMessageChunk) {
      const content = update.content || update.user_message_chunk?.content || update.userMessageChunk?.content;
      const text = typeof content === 'string' ? content : (content?.text || '');
      if (text) {
        setMessages((prev) => [
          ...prev,
          {
            id: 'user-' + Date.now() + '-' + Math.random().toString(36).slice(2, 6),
            role: 'user',
            text,
            timestamp: Date.now(),
          },
        ]);
      }
    }

    // 3. Thought chunk
    if (updateType === 'agent_thought_chunk' || updateType === 'agentThoughtChunk' || update.agent_thought_chunk || update.agentThoughtChunk) {
      const content = update.content || update.agent_thought_chunk?.content || update.agentThoughtChunk?.content;
      const text = typeof content === 'string' ? content : (content?.text || '');
      if (text) {
        const msgId = ensureAssistantMessage();
        setMessages((prev) =>
          prev.map((m) => (m.id === msgId ? { ...m, thought: (m.thought || '') + text, isStreaming: true } : m))
        );
      }
    }

    // 4. Tool call start
    if (updateType === 'tool_call' || updateType === 'toolCall' || update.tool_call || update.toolCall) {
      const call = update;
      const callId = call.toolCallId || call.tool_call_id || call.id || 'tc-' + Date.now();
      const toolItem: ToolCallItem = {
        id: callId,
        title: call.title || call.name || 'tool',
        kind: call.kind,
        status: (call.status as any) || 'running',
        rawInput: call.rawInput || call.raw_input,
        content: [],
      };
      const msgId = ensureAssistantMessage();
      setMessages((prev) =>
        prev.map((m) =>
          m.id === msgId ? { ...m, toolCalls: [...(m.toolCalls || []), toolItem], isStreaming: true } : m
        )
      );
    }

    // 5. Tool call update
    if (updateType === 'tool_call_update' || updateType === 'toolCallUpdate' || update.tool_call_update || update.toolCallUpdate) {
      const callUpdate = update;
      const callId = callUpdate.toolCallId || callUpdate.tool_call_id || callUpdate.id;
      const fields = callUpdate.fields || callUpdate;
      const contentLines: string[] = [];

      if (fields.content && Array.isArray(fields.content)) {
        fields.content.forEach((c: any) => {
          if (typeof c === 'string') {
            contentLines.push(c);
          } else if (c.type === 'text' && c.text) {
            contentLines.push(c.text);
          } else if (c.type === 'content' && c.content?.text) {
            contentLines.push(c.content.text);
          } else if (c.text) {
            contentLines.push(c.text);
          }
        });
      }

      setMessages((prev) =>
        prev.map((m) => {
          if (!m.toolCalls) return m;
          const updated = m.toolCalls.map((tc) => {
            if (tc.id === callId) {
              return {
                ...tc,
                status: (fields.status?.toLowerCase() as any) || tc.status,
                content: contentLines.length > 0 ? contentLines : tc.content,
              };
            }
            return tc;
          });
          return { ...m, toolCalls: updated };
        })
      );
    }

    // 6. Mode update
    if (updateType === 'current_mode_update' || updateType === 'currentModeUpdate' || update.current_mode_update || update.currentModeUpdate || update.modeId) {
      const mId = update.currentModeId || update.current_mode_id || update.current_mode_update?.mode_id || update.currentModeUpdate?.modeId || update.modeId;
      if (mId === 'plan' || mId === 'act') {
        setMode(mId);
      }
    }
  };

  const handleSendPrompt = async (text: string) => {
    let sid = currentSessionId;
    if (!sid) {
      const res = await acpClient.newSession();
      sid = res.sessionId;
      setCurrentSessionId(sid);
      await loadSessionList();
    }

    // Add user message
    const userMsg: ChatMessage = {
      id: 'user-' + Date.now(),
      role: 'user',
      text,
      timestamp: Date.now(),
    };
    setMessages((prev) => [...prev, userMsg]);
    setIsStreaming(true);
    activeAssistantMsgIdRef.current = null;

    try {
      await acpClient.sendPrompt(sid, text);
    } catch (err: any) {
      console.error('Prompt error:', err);
      const msgId = ensureAssistantMessage();
      setMessages((prev) =>
        prev.map((m) =>
          m.id === msgId ? { ...m, text: m.text + `\n\n*Error: ${err.message || err}*`, isStreaming: false } : m
        )
      );
    } finally {
      setIsStreaming(false);
      if (activeAssistantMsgIdRef.current) {
        const finishedId = activeAssistantMsgIdRef.current;
        setMessages((prev) =>
          prev.map((m) => (m.id === finishedId ? { ...m, isStreaming: false } : m))
        );
        activeAssistantMsgIdRef.current = null;
      }
    }
  };

  const handleCancel = () => {
    if (currentSessionId) {
      acpClient.cancel(currentSessionId);
    }
    setIsStreaming(false);
  };

  const handleModeChange = async (newMode: AgentMode) => {
    setMode(newMode);
    if (currentSessionId) {
      try {
        await acpClient.setMode(currentSessionId, newMode);
      } catch (err) {
        console.error('Failed to change mode:', err);
      }
    }
  };

  const handleThinkingChange = async (level: ThinkingLevel) => {
    setThinkingLevel(level);
    if (currentSessionId) {
      try {
        await acpClient.setConfigOption(currentSessionId, 'thinking', level);
      } catch (err) {
        console.error('Failed to change thinking level:', err);
      }
    }
  };

  const handleRespondApproval = (rpcId: number | string, optionId: string) => {
    acpClient.respondPermission(rpcId, optionId);
    setPendingApprovals((prev) => prev.filter((p) => p.rpcId !== rpcId));
  };

  const handleCancelApproval = (rpcId: number | string) => {
    acpClient.cancelPermission(rpcId);
    setPendingApprovals((prev) => prev.filter((p) => p.rpcId !== rpcId));
  };

  const activeSessionObj = sessions.find((s) => s.sessionId === currentSessionId);
  const currentTitle = activeSessionObj?.title || currentSessionId || 'New Session';

  return (
    <div className="app-container">
      {/* Mobile Drawer Backdrop */}
      <div
        className={`sidebar-backdrop ${isSidebarOpen ? 'visible' : ''}`}
        onClick={() => setIsSidebarOpen(false)}
        aria-hidden="true"
      />

      <Sidebar
        sessions={sessions}
        currentSessionId={currentSessionId}
        serverInfo={serverInfo}
        isConnected={isConnected}
        isOpen={isSidebarOpen}
        onClose={() => setIsSidebarOpen(false)}
        onSelectSession={selectSession}
        onNewSession={handleNewSession}
      />

      <div className="main-area">
        <Header
          currentSessionTitle={currentTitle}
          mode={mode}
          thinkingLevel={thinkingLevel}
          onModeChange={handleModeChange}
          onThinkingChange={handleThinkingChange}
          onToggleSidebar={() => setIsSidebarOpen((prev) => !prev)}
        />

        <ChatArea
          messages={messages}
          pendingApprovals={pendingApprovals}
          onRespondApproval={handleRespondApproval}
          onCancelApproval={handleCancelApproval}
          onSelectQuickPrompt={handleSendPrompt}
        />

        <InputArea
          mode={mode}
          isStreaming={isStreaming}
          onSend={handleSendPrompt}
          onCancel={handleCancel}
          onModeChange={handleModeChange}
        />
      </div>
    </div>
  );
};

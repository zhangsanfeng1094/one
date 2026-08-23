// Agent Client Protocol (ACP) & UI Types

export interface JsonRpcRequest {
  jsonrpc: '2.0';
  id?: number | string;
  method: string;
  params?: any;
}

export interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number | string;
  result?: any;
  error?: {
    code: number;
    message: string;
    data?: any;
  };
}

export interface ServerInfo {
  name: string;
  version: string;
  protocol: string;
  cwd: string;
}

export interface SessionInfo {
  sessionId: string;
  cwd: string;
  title?: string;
  updatedAt?: string;
}

export type ThinkingLevel = 'off' | 'low' | 'medium' | 'high';
export type AgentMode = 'act' | 'plan';

export interface ToolCallItem {
  id: string;
  title: string;
  kind?: string;
  status: 'pending' | 'running' | 'completed' | 'failed' | 'cancelled';
  rawInput?: any;
  content?: string[];
  locations?: Array<{ path: string; line?: number }>;
}

export interface ApprovalOption {
  optionId: string;
  name: string;
  kind?: string;
}

export interface ApprovalRequest {
  rpcId: number | string;
  sessionId: string;
  toolCallId: string;
  title: string;
  rawInput: any;
  options: ApprovalOption[];
}

export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'system';
  text: string;
  thought?: string;
  toolCalls?: ToolCallItem[];
  timestamp: number;
  isStreaming?: boolean;
}

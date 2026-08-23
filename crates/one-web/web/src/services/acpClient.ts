import { JsonRpcRequest, JsonRpcResponse, ApprovalRequest } from '../types/acp';

export type MessageHandler = (method: string, params: any) => void;
export type ApprovalHandler = (req: ApprovalRequest) => void;
export type ConnectionChangeHandler = (connected: boolean) => void;

export class AcpClient {
  private ws: WebSocket | null = null;
  private reqId = 1;
  private pendingRequests = new Map<number | string, { resolve: (val: any) => void; reject: (err: any) => void }>();
  private messageListeners = new Set<MessageHandler>();
  private approvalListeners = new Set<ApprovalHandler>();
  private connectionListeners = new Set<ConnectionChangeHandler>();
  private isConnecting = false;
  private reconnectTimer: any = null;

  constructor() {}

  public get connecting(): boolean {
    return this.isConnecting;
  }

  public connect(url?: string): void {
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      return;
    }

    if (!url) {
      const isHttps = window.location.protocol === 'https:';
      const wsProto = isHttps ? 'wss:' : 'ws:';
      url = `${wsProto}//${window.location.host}/ws`;
    }

    this.isConnecting = true;
    try {
      this.ws = new WebSocket(url);
    } catch (e) {
      console.error('WebSocket connection error:', e);
      this.scheduleReconnect();
      return;
    }

    this.ws.onopen = async () => {
      this.isConnecting = false;
      try {
        await this.initialize();
        this.notifyConnection(true);
      } catch (err) {
        console.error('ACP initialize error:', err);
        this.notifyConnection(false);
      }
    };

    this.ws.onclose = () => {
      this.isConnecting = false;
      this.notifyConnection(false);
      this.scheduleReconnect();
    };

    this.ws.onerror = (err) => {
      console.error('WebSocket error:', err);
      this.ws?.close();
    };

    this.ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        this.handleIncoming(msg);
      } catch (err) {
        console.error('Failed to parse incoming WebSocket frame:', err, event.data);
      }
    };
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, 2500);
  }

  private notifyConnection(connected: boolean): void {
    this.connectionListeners.forEach((cb) => cb(connected));
  }

  public onConnectionChange(cb: ConnectionChangeHandler): () => void {
    this.connectionListeners.add(cb);
    return () => this.connectionListeners.delete(cb);
  }

  public onMessage(cb: MessageHandler): () => void {
    this.messageListeners.add(cb);
    return () => this.messageListeners.delete(cb);
  }

  public onApproval(cb: ApprovalHandler): () => void {
    this.approvalListeners.add(cb);
    return () => this.approvalListeners.delete(cb);
  }

  public rpcCall(method: string, params: any = {}): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
        return reject(new Error('WebSocket is not connected'));
      }
      const id = this.reqId++;
      this.pendingRequests.set(id, { resolve, reject });
      const payload: JsonRpcRequest = {
        jsonrpc: '2.0',
        id,
        method,
        params,
      };
      this.ws.send(JSON.stringify(payload));
    });
  }

  public rpcNotify(method: string, params: any = {}): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const payload: JsonRpcRequest = {
      jsonrpc: '2.0',
      method,
      params,
    };
    this.ws.send(JSON.stringify(payload));
  }

  public rpcRespond(id: number | string, result: any): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const payload: JsonRpcResponse = {
      jsonrpc: '2.0',
      id,
      result,
    };
    this.ws.send(JSON.stringify(payload));
  }

  private handleIncoming(msg: any): void {
    // 1. RPC Response to our request
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const handler = this.pendingRequests.get(msg.id);
      if (handler) {
        this.pendingRequests.delete(msg.id);
        if (msg.error) {
          handler.reject(new Error(msg.error.message || JSON.stringify(msg.error)));
        } else {
          handler.resolve(msg.result);
        }
      }
      return;
    }

    // 2. Server Request (e.g. session/requestPermission)
    if (msg.id !== undefined && msg.method) {
      if (msg.method === 'session/requestPermission') {
        const params = msg.params || {};
        const approvalReq: ApprovalRequest = {
          rpcId: msg.id,
          sessionId: params.sessionId,
          toolCallId: params.toolCall?.toolCallId || params.toolCallId || '',
          title: params.toolCall?.title || params.title || 'Permission Request',
          rawInput: params.toolCall?.rawInput || params.rawInput || {},
          options: (params.options || []).map((o: any) => ({
            optionId: o.optionId || o.id || '',
            name: o.name || o.title || 'Allow',
            kind: o.kind,
          })),
        };
        this.approvalListeners.forEach((cb) => cb(approvalReq));
        return;
      }

      // Default empty response to unhandled server requests
      this.rpcRespond(msg.id, {});
      return;
    }

    // 3. Server Notification (e.g. session/update)
    if (msg.method) {
      this.messageListeners.forEach((cb) => cb(msg.method, msg.params));
    }
  }

  public async initialize(): Promise<any> {
    return this.rpcCall('initialize', {
      protocolVersion: 1,
      clientCapabilities: {
        fs: {
          readTextFile: true,
          writeTextFile: true,
        },
        terminal: true,
      },
    });
  }

  public async newSession(cwd: string = ''): Promise<{ sessionId: string }> {
    return this.rpcCall('session/new', { cwd, mcpServers: [] });
  }

  public async listSessions(cwd: string = ''): Promise<{ sessions: any[] }> {
    return this.rpcCall('session/list', { cwd });
  }

  public async loadSession(sessionId: string, cwd: string = ''): Promise<any> {
    return this.rpcCall('session/load', { sessionId, cwd, mcpServers: [] });
  }

  public async sendPrompt(sessionId: string, text: string): Promise<any> {
    return this.rpcCall('session/prompt', {
      sessionId,
      prompt: [
        {
          type: 'text',
          text,
        },
      ],
    });
  }

  public cancel(sessionId: string): void {
    this.rpcNotify('session/cancel', { sessionId });
  }

  public async setMode(sessionId: string, modeId: string): Promise<any> {
    return this.rpcCall('session/setMode', { sessionId, modeId });
  }

  public async setConfigOption(sessionId: string, configId: string, value: string): Promise<any> {
    return this.rpcCall('session/setConfigOption', { sessionId, configId, value });
  }

  public respondPermission(rpcId: number | string, optionId: string): void {
    this.rpcRespond(rpcId, {
      outcome: {
        outcome: 'selected',
        optionId,
      },
    });
  }

  public cancelPermission(rpcId: number | string): void {
    this.rpcRespond(rpcId, {
      outcome: {
        outcome: 'cancelled',
      },
    });
  }
}

export const acpClient = new AcpClient();

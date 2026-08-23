import React from 'react';
import { ApprovalRequest } from '../types/acp';
import { ShieldAlert, Check, X } from 'lucide-react';

interface ApprovalCardProps {
  request: ApprovalRequest;
  onRespond: (rpcId: number | string, optionId: string) => void;
  onCancel: (rpcId: number | string) => void;
}

export const ApprovalCard: React.FC<ApprovalCardProps> = ({ request, onRespond, onCancel }) => {
  const inputStr = typeof request.rawInput === 'string' ? request.rawInput : JSON.stringify(request.rawInput, null, 2);

  return (
    <div className="approval-card">
      <div className="approval-title">
        <ShieldAlert size={15} />
        <span>Permission Request: {request.title}</span>
      </div>
      {inputStr && <pre className="approval-content">{inputStr}</pre>}
      <div className="approval-actions">
        {request.options.length > 0 ? (
          request.options.map((opt) => (
            <button
              key={opt.optionId}
              className={opt.kind === 'allow' || opt.kind === 'once' ? 'btn-allow' : 'btn-deny'}
              onClick={() => onRespond(request.rpcId, opt.optionId)}
            >
              {opt.name}
            </button>
          ))
        ) : (
          <>
            <button
              className="btn-allow"
              style={{ display: 'flex', alignItems: 'center', gap: 4 }}
              onClick={() => onRespond(request.rpcId, 'allow')}
            >
              <Check size={13} /> Allow
            </button>
            <button
              className="btn-deny"
              style={{ display: 'flex', alignItems: 'center', gap: 4 }}
              onClick={() => onCancel(request.rpcId)}
            >
              <X size={13} /> Reject
            </button>
          </>
        )}
      </div>
    </div>
  );
};

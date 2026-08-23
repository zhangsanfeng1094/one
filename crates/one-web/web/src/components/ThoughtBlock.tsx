import React, { useState } from 'react';
import { ChevronDown, ChevronRight, Brain, Loader2 } from 'lucide-react';

interface ThoughtBlockProps {
  thought: string;
  isStreaming?: boolean;
}

export const ThoughtBlock: React.FC<ThoughtBlockProps> = ({ thought, isStreaming }) => {
  const [isOpen, setIsOpen] = useState(true);

  if (!thought) return null;

  return (
    <div className="thought-container">
      <div className="thought-header" onClick={() => setIsOpen(!isOpen)}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <Brain size={13} style={{ opacity: 0.8 }} />
          <span style={{ fontWeight: 500 }}>Thinking</span>
          {isStreaming && (
            <span
              style={{
                display: 'inline-flex',
                alignItems: 'center',
                gap: 4,
                fontSize: 11,
                color: '#a1a1aa',
                marginLeft: 4,
              }}
            >
              <Loader2 size={10} className="animate-spin" /> In progress
            </span>
          )}
        </div>
        {isOpen ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
      </div>
      {isOpen && <div className="thought-body">{thought}</div>}
    </div>
  );
};

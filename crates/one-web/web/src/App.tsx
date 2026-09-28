import React, { useEffect, useState } from 'react';
import { ChatApp } from './ChatApp';
import { ConfigStudio } from './config/ConfigStudio';
import './config/styles.css';

type Frontend = 'chat' | 'config' | 'resolving';

/**
 * Chooses which frontend to mount.
 *
 * The same SPA shell is served by two very different servers: the chat server
 * (`/`, ACP over WebSocket) and the config studio (`/config`, tokenized HTTP
 * API). The URL decides first so no request is wasted, and `/api/info` is the
 * tie-breaker — that is what lets the studio work when it is mounted on a root
 * path instead of `/config`.
 */
export const App: React.FC = () => {
  const [frontend, setFrontend] = useState<Frontend>(() =>
    window.location.pathname.startsWith('/config') ? 'config' : 'resolving'
  );

  useEffect(() => {
    if (frontend !== 'resolving') return;

    let cancelled = false;
    fetch('/api/info')
      .then((res) => (res.ok ? res.json() : null))
      .then((info: { mode?: string } | null) => {
        if (cancelled) return;
        setFrontend(info?.mode === 'config' ? 'config' : 'chat');
      })
      .catch(() => {
        if (!cancelled) setFrontend('chat');
      });

    return () => {
      cancelled = true;
    };
  }, [frontend]);

  if (frontend === 'config') return <ConfigStudio />;
  if (frontend === 'chat') return <ChatApp />;
  return <div className="cfg-fullscreen">正在载入…</div>;
};

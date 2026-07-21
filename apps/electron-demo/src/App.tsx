import { useEffect, useState } from "react";

import type { KomoConnectResponse } from "./global";
import { ConnectionContext, NavContext, type View } from "./app-context";
import { ChatView } from "./chat/ChatView";
import { Dashboard } from "./dashboard/Dashboard";
import { newSessionId } from "./lib/ipc";

export function App() {
  const [conn, setConn] = useState<KomoConnectResponse>({ connected: false });
  const [view, setView] = useState<View>("chat");
  const [session, setSession] = useState<string>(() => newSessionId());

  // Connection lifecycle: probe on mount, then every 3s — attach when the
  // gateway starts, show offline when it stops.
  useEffect(() => {
    let alive = true;
    const tick = async () => {
      const r = await window.komo.connect();
      if (alive) setConn(r);
    };
    void tick();
    const id = setInterval(tick, 3000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  return (
    <ConnectionContext.Provider value={conn}>
      <NavContext.Provider value={{ view, setView, session, setSession }}>
        <div className="app">
          <nav className="navbar">
            <span className="brand">komo</span>
            <div className="tabs">
              <button
                className={view === "chat" ? "tab active" : "tab"}
                onClick={() => setView("chat")}
              >
                聊天
              </button>
              <button
                className={view === "dashboard" ? "tab active" : "tab"}
                onClick={() => setView("dashboard")}
              >
                仪表盘
              </button>
            </div>
            <span
              className={conn.connected ? "dot online" : "dot offline"}
              title={conn.connected ? "已连接" : "未连接"}
            />
          </nav>

          {!conn.connected && (
            <div className="banner">{conn.error ?? "正在连接 komo gateway…"}</div>
          )}

          <main className="content">
            {view === "chat" ? <ChatView key={session} /> : <Dashboard />}
          </main>
        </div>
      </NavContext.Provider>
    </ConnectionContext.Provider>
  );
}

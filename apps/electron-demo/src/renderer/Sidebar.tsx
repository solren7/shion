import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import { useApp, useConnection } from "./app-context";
import { apiField, fmtTs, newSessionId } from "./lib/ipc";
import type { SessionSummary } from "./types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

/** A short, human-readable label for a session id (fallback when untitled). */
function sessionLabel(id: string): string {
  const bare = id.replace(/^api:/, "");
  // gui-electron-<uuid> → last chunk of the uuid, so entries stay distinct.
  const m = bare.match(/gui-electron-.*?([0-9a-f]{4,})$/i);
  if (m) return `会话 ${m[1].slice(-6)}`;
  return bare.length > 22 ? `${bare.slice(0, 20)}…` : bare;
}

const itemBase = "group w-full flex flex-col gap-0.5 px-2.5 py-2 rounded-[10px] transition-colors";

export function Sidebar({ onOpenSettings }: { onOpenSettings: () => void }) {
  const { connected } = useConnection();
  const { session, setSession } = useApp();
  const qc = useQueryClient();
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [showArchived, setShowArchived] = useState(false);

  const q = useQuery({
    queryKey: ["sessions"],
    queryFn: () => apiField<SessionSummary[]>("/api/sessions", "sessions"),
    refetchInterval: 6000,
    enabled: connected,
  });

  const sessions = q.data ?? [];

  const refresh = () => void qc.invalidateQueries({ queryKey: ["sessions"] });

  const commitRename = async (id: string) => {
    const title = draft.trim();
    setEditingId(null);
    await window.komo.api({
      path: `/api/sessions/${encodeURIComponent(id)}/title`,
      method: "POST",
      body: { title },
    });
    refresh();
  };

  const setStatus = async (id: string, status: string) => {
    await window.komo.api({
      path: `/api/sessions/${encodeURIComponent(id)}/status`,
      method: "POST",
      body: { status },
    });
    // Leaving the open session (deleted/archived) → drop into a fresh one.
    if (id === session && status !== "active") setSession(newSessionId());
    refresh();
  };

  const remove = async (id: string) => {
    if (!window.confirm("删除该会话？（软删除，从列表移除）")) return;
    await setStatus(id, "deleted");
  };

  const activeSessions = sessions.filter((s) => s.status !== "archive");
  const archivedSessions = sessions.filter((s) => s.status === "archive");

  const renderRow = (s: SessionSummary) => {
    const active = s.id === session;
    const archived = s.status === "archive";
    const label = s.title?.trim() ? s.title : sessionLabel(s.id);
    const rowTint = active
      ? "bg-(--mc-accent-soft) ring-1 ring-(--mc-accent-ring) text-(--mc-fg)"
      : "text-(--mc-fg) hover:bg-(--mc-surface-2)";

    if (editingId === s.id) {
      return (
        <div key={s.id} className={`${itemBase} ${rowTint}`}>
          <Input
            autoFocus
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void commitRename(s.id);
              else if (e.key === "Escape") setEditingId(null);
            }}
            onBlur={() => void commitRename(s.id)}
            className="h-7 text-[13px]"
          />
        </div>
      );
    }

    return (
      <div key={s.id} className={`${itemBase} ${rowTint}`}>
        <div className="flex items-center gap-1">
          <button
            className="flex-1 min-w-0 text-left cursor-pointer"
            onClick={() => setSession(s.id)}
            title={s.id}
          >
            <span className="text-[13px] truncate block">{label}</span>
            <span className="text-[11px] text-(--mc-fg-faint)">
              {s.user_turns} 轮 · {fmtTs(s.created_at)}
            </span>
          </button>
          <div className="shrink-0 flex gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity">
            <IconButton
              title="重命名"
              onClick={() => {
                setDraft(s.title ?? "");
                setEditingId(s.id);
              }}
            >
              <PencilIcon />
            </IconButton>
            {archived ? (
              <IconButton title="取消归档" onClick={() => void setStatus(s.id, "active")}>
                <UnarchiveIcon />
              </IconButton>
            ) : (
              <IconButton title="归档" onClick={() => void setStatus(s.id, "archive")}>
                <ArchiveIcon />
              </IconButton>
            )}
            <IconButton title="删除" danger onClick={() => void remove(s.id)}>
              <TrashIcon />
            </IconButton>
          </div>
        </div>
      </div>
    );
  };

  return (
    <aside className="w-[264px] shrink-0 flex flex-col min-h-0 border-r border-(--mc-border) bg-(--mc-surface) backdrop-blur-xl">
      <div className="h-12 shrink-0 px-4 flex items-center gap-2.5">
        <span
          className="w-6 h-6 rounded-[7px] shrink-0 shadow-(--mc-shadow-glow)"
          style={{ background: "var(--mc-accent-grad)" }}
        />
        <span className="font-bold tracking-wide text-(--mc-fg)">komo</span>
        <span className="flex-1" />
        <span
          className={`w-2.5 h-2.5 rounded-full ${connected ? "bg-(--mc-ok)" : "bg-(--mc-danger)"}`}
          title={connected ? "已连接" : "未连接"}
        />
      </div>

      <div className="px-3 pb-2">
        {/* New session only switches the active id — it does NOT add a row.
            The session appears in the list after the first message creates it. */}
        <Button variant="gradient" className="w-full" onClick={() => setSession(newSessionId())}>
          <PlusIcon />
          <span>新建会话</span>
        </Button>
      </div>

      <div className="flex-1 overflow-y-auto min-h-0 px-2 pb-2 flex flex-col gap-0.5">
        {!connected ? (
          <div className="px-3 py-3 text-[13px] text-(--mc-fg-faint)">未连接</div>
        ) : q.isPending ? (
          <div className="px-3 py-3 text-[13px] text-(--mc-fg-faint)">加载中…</div>
        ) : sessions.length === 0 ? (
          <div className="px-3 py-3 text-[13px] text-(--mc-fg-faint)">还没有会话</div>
        ) : (
          <>
            {activeSessions.map(renderRow)}
            {archivedSessions.length > 0 && (
              <div className="mt-1 flex flex-col gap-0.5">
                <button
                  className="px-2.5 py-1.5 text-left text-[11px] text-(--mc-fg-faint) hover:text-(--mc-fg) cursor-pointer"
                  onClick={() => setShowArchived((v) => !v)}
                >
                  {showArchived ? "▾" : "▸"} 已归档 ({archivedSessions.length})
                </button>
                {showArchived && archivedSessions.map(renderRow)}
              </div>
            )}
          </>
        )}
      </div>

      <div className="border-t border-(--mc-border) p-2">
        <Button
          variant="ghost"
          className="w-full justify-start text-(--mc-fg-muted) hover:text-(--mc-fg)"
          onClick={onOpenSettings}
        >
          <GearIcon />
          <span>设置</span>
        </Button>
      </div>
    </aside>
  );
}

function IconButton({
  title,
  danger,
  onClick,
  children,
}: {
  title: string;
  danger?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      title={title}
      onClick={onClick}
      className={`w-6 h-6 grid place-items-center rounded-md cursor-pointer hover:bg-(--mc-surface-strong) ${
        danger ? "text-(--mc-fg-muted) hover:text-(--mc-danger)" : "text-(--mc-fg-muted) hover:text-(--mc-fg)"
      }`}
    >
      {children}
    </button>
  );
}

function PlusIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
      <path d="M12 5v14M5 12h14" />
    </svg>
  );
}

function PencilIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z" />
    </svg>
  );
}

function ArchiveIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect x="3" y="4" width="18" height="4" rx="1" />
      <path d="M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8M10 12h4" />
    </svg>
  );
}

function UnarchiveIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect x="3" y="4" width="18" height="4" rx="1" />
      <path d="M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8M12 18v-6M9.5 14.5 12 12l2.5 2.5" />
    </svg>
  );
}

function TrashIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M3 6h18M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
    </svg>
  );
}

function GearIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
    </svg>
  );
}

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { useConnection, useNav } from "../app-context";
import { apiField, apiGet, apiPost, fmtTs } from "../lib/ipc";
import type { Memory, Run, SessionSummary, StatusSnapshot, Task } from "../types";

type Tab = "status" | "tasks" | "memories" | "runs" | "sessions";
const TABS: [Tab, string][] = [
  ["status", "状态"],
  ["tasks", "任务"],
  ["memories", "记忆"],
  ["runs", "运行"],
  ["sessions", "会话"],
];

const POLL_MS = 6000;

export function Dashboard() {
  const [tab, setTab] = useState<Tab>("status");
  const { connected } = useConnection();

  return (
    <div className="dashboard">
      <div className="dash-tabs">
        {TABS.map(([t, label]) => (
          <button
            key={t}
            className={tab === t ? "dash-tab active" : "dash-tab"}
            onClick={() => setTab(t)}
          >
            {label}
          </button>
        ))}
      </div>
      <div className="dash-body">
        {!connected ? (
          <div className="empty">未连接到 gateway。</div>
        ) : tab === "status" ? (
          <StatusTab />
        ) : tab === "tasks" ? (
          <TasksTab />
        ) : tab === "memories" ? (
          <MemoriesTab />
        ) : tab === "runs" ? (
          <RunsTab />
        ) : (
          <SessionsTab />
        )}
      </div>
    </div>
  );
}

function useLoad<T>(key: string[], fn: () => Promise<T>) {
  return useQuery({ queryKey: key, queryFn: fn, refetchInterval: POLL_MS });
}

function Loading() {
  return <div className="loading">加载中…</div>;
}
function ErrLine({ error }: { error: unknown }) {
  return <div className="err">{error instanceof Error ? error.message : String(error)}</div>;
}

function StatusTab() {
  const q = useLoad(["status"], () => apiGet<StatusSnapshot>("/api/status"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const s = q.data!;
  return (
    <div className="panel">
      <div className="status-grid">
        <div className="stat-card">
          <div className="stat-value">{s.version}</div>
          <div className="stat-label">版本</div>
        </div>
        <div className="stat-card">
          <div className="stat-value">{s.open_tasks}</div>
          <div className="stat-label">开放任务</div>
        </div>
        <div className="stat-card">
          <div className="stat-value">{s.sessions}</div>
          <div className="stat-label">会话数</div>
        </div>
        <div className="stat-card">
          <div className="stat-value">{s.home_chat ?? "—"}</div>
          <div className="stat-label">Home</div>
        </div>
      </div>
      <div className="channels">
        <span className="muted">渠道：</span>
        {s.channels.length === 0 ? (
          <span>无</span>
        ) : (
          s.channels.map((c) => (
            <span className="chip" key={c}>
              {c}
            </span>
          ))
        )}
      </div>
    </div>
  );
}

function TasksTab() {
  const q = useLoad(["tasks"], () => apiField<Task[]>("/api/tasks", "tasks"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const tasks = q.data!;
  if (tasks.length === 0) return <div className="empty">没有开放任务。</div>;
  return (
    <div className="panel">
      {tasks.map((t) => (
        <div className="row" key={t.id}>
          <span className="tag">{t.status}</span>
          <span className="row-main">{t.title}</span>
          {t.board && <span className="chip">#{t.board}</span>}
          {t.due_at != null && <span className="muted">截止 {fmtTs(t.due_at)}</span>}
        </div>
      ))}
    </div>
  );
}

function MemoriesTab() {
  const [filter, setFilter] = useState("");
  const qc = useQueryClient();
  const q = useLoad(["memories", filter], () =>
    apiField<Memory[]>(filter ? `/api/memories?status=${filter}` : "/api/memories", "memories"),
  );
  const act = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) =>
      apiPost(`/api/memories/${id}/${action}`, {}),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["memories"] }),
  });

  return (
    <div className="panel">
      <div className="dash-toolbar">
        <select className="small" value={filter} onChange={(e) => setFilter(e.target.value)}>
          <option value="">全部</option>
          <option value="candidate">候选</option>
          <option value="active">活跃</option>
          <option value="archived">归档</option>
          <option value="rejected">拒绝</option>
        </select>
      </div>
      {q.isPending ? (
        <Loading />
      ) : q.error ? (
        <ErrLine error={q.error} />
      ) : q.data!.length === 0 ? (
        <div className="empty">没有记忆。</div>
      ) : (
        q.data!.map((m) => (
          <div className="row mem-row" key={m.id}>
            <div className="mem-head">
              <span className="tag">{m.status}</span>
              <span className="chip">{m.kind}</span>
              {m.pinned && <span className="chip warn">📌</span>}
              <span className="muted">{m.confidence}</span>
            </div>
            <div className="mem-content">{m.content}</div>
            <div className="mem-actions">
              <button className="btn ok" onClick={() => act.mutate({ id: m.id, action: "promote" })}>
                promote
              </button>
              <button className="btn" onClick={() => act.mutate({ id: m.id, action: "pin" })}>
                pin
              </button>
              <button className="btn deny" onClick={() => act.mutate({ id: m.id, action: "reject" })}>
                reject
              </button>
            </div>
          </div>
        ))
      )}
    </div>
  );
}

function RunsTab() {
  const [open, setOpen] = useState<string | null>(null);
  const q = useLoad(["runs"], () => apiField<Run[]>("/api/runs?limit=50", "runs"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const runs = q.data!;
  if (runs.length === 0) return <div className="empty">还没有运行记录。</div>;
  return (
    <div className="panel">
      {runs.map((r) => (
        <div key={r.id}>
          <div
            className="row clickable"
            onClick={() => setOpen(open === r.id ? null : r.id)}
          >
            <span className="tag">{r.status}</span>
            <span className="row-main">{r.input}</span>
            {r.recoverable && (
              <span className="chip warn" title="可恢复">
                ⟲
              </span>
            )}
            <span className="muted">{fmtTs(r.started_at)}</span>
          </div>
          {open === r.id && <RunDetail id={r.id} />}
        </div>
      ))}
    </div>
  );
}

function RunDetail({ id }: { id: string }) {
  const q = useQuery({
    queryKey: ["run", id],
    queryFn: () => apiGet<{ run: Run; steps: RunStepLite[] }>(`/api/runs/${id}`),
  });
  if (q.isPending) return <div className="run-detail loading">加载步骤…</div>;
  if (q.error) return <div className="run-detail err">{String(q.error)}</div>;
  const { run, steps } = q.data!;
  return (
    <div className="run-detail">
      {run.final_output && <div className="run-output">{run.final_output}</div>}
      {run.error && <div className="err">{run.error}</div>}
      {steps.map((s) => (
        <div className="step" key={s.seq}>
          <span className={s.ok ? "tag ok" : "tag deny"}>
            {s.seq}. {s.tool_name}
          </span>
          <span className="step-args">{s.args}</span>
        </div>
      ))}
    </div>
  );
}
interface RunStepLite {
  seq: number;
  tool_name: string;
  args: string;
  ok: boolean;
}

function SessionsTab() {
  const { setSession, setView } = useNav();
  const q = useLoad(["sessions"], () => apiField<SessionSummary[]>("/api/sessions", "sessions"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const sessions = q.data!;
  if (sessions.length === 0) return <div className="empty">没有会话。</div>;
  return (
    <div className="panel">
      {sessions.map((s) => (
        <div className="row" key={s.id}>
          <span className="row-main mono">{s.id}</span>
          <span className="muted">
            {s.user_turns} 轮 · {s.messages} 条 · {fmtTs(s.created_at)}
          </span>
          <button
            className="small"
            onClick={() => {
              setSession(s.id);
              setView("chat");
            }}
          >
            在聊天中继续
          </button>
        </div>
      ))}
    </div>
  );
}

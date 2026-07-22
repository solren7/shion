import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { apiField, apiGet, apiPost, fmtTs } from "../lib/ipc";
import type { Memory, Run, StatusSnapshot, Task } from "../types";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const MEM_FILTERS: [string, string][] = [
  ["all", "全部"],
  ["candidate", "候选"],
  ["active", "活跃"],
  ["archived", "归档"],
  ["rejected", "拒绝"],
];

const POLL_MS = 6000;

// Layout-only class strings (the token look); anything button- or chip-shaped
// now goes through the shadcn <Button> / <Badge> components.
const ROW =
  "flex items-center gap-2 px-2.5 py-2 rounded-[10px] border border-(--mc-border) bg-(--mc-surface)";
const MUTED = "text-xs text-(--mc-fg-muted) whitespace-nowrap";
const PANEL = "flex flex-col gap-1.5";

function useLoad<T>(key: string[], fn: () => Promise<T>) {
  return useQuery({ queryKey: key, queryFn: fn, refetchInterval: POLL_MS });
}

function Loading() {
  return <div className="flex items-center justify-center py-6 text-(--mc-fg-faint)">加载中…</div>;
}
function ErrLine({ error }: { error: unknown }) {
  return (
    <div className="py-2 text-(--mc-danger) text-[13px]">
      {error instanceof Error ? error.message : String(error)}
    </div>
  );
}
function EmptyLine({ children }: { children: React.ReactNode }) {
  return <div className="flex items-center justify-center py-8 text-(--mc-fg-faint)">{children}</div>;
}

export function StatusTab() {
  const q = useLoad(["status"], () => apiGet<StatusSnapshot>("/api/status"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const s = q.data!;
  const cards: [string | number, string][] = [
    [s.version, "版本"],
    [s.open_tasks, "开放任务"],
    [s.sessions, "会话数"],
    [s.home_chat ?? "—", "Home"],
  ];
  return (
    <div className="flex flex-col gap-3">
      <div className="grid gap-2.5 [grid-template-columns:repeat(auto-fill,minmax(120px,1fr))]">
        {cards.map(([value, label]) => (
          <div
            key={label}
            className="p-3.5 text-center rounded-[12px] border border-(--mc-border) bg-(--mc-surface)"
          >
            <div className="text-[22px] font-bold text-(--mc-fg) truncate">{value}</div>
            <div className="text-xs text-(--mc-fg-muted) mt-1">{label}</div>
          </div>
        ))}
      </div>
      <div className="flex items-center gap-1.5 flex-wrap">
        <span className={MUTED}>渠道：</span>
        {s.channels.length === 0 ? (
          <span className="text-[13px]">无</span>
        ) : (
          s.channels.map((c) => <Badge key={c}>{c}</Badge>)
        )}
      </div>
    </div>
  );
}

export function TasksTab() {
  const q = useLoad(["tasks"], () => apiField<Task[]>("/api/tasks", "tasks"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const tasks = q.data!;
  if (tasks.length === 0) return <EmptyLine>没有开放任务。</EmptyLine>;
  return (
    <div className={PANEL}>
      {tasks.map((t) => (
        <div className={ROW} key={t.id}>
          <Badge variant="pill">{t.status}</Badge>
          <span className="flex-1 truncate">{t.title}</span>
          {t.board && <Badge>#{t.board}</Badge>}
          {t.due_at != null && <span className={MUTED}>截止 {fmtTs(t.due_at)}</span>}
        </div>
      ))}
    </div>
  );
}

export function MemoriesTab() {
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
    <div className={PANEL}>
      <div className="mb-1.5">
        <Select
          value={filter || "all"}
          onValueChange={(v) => setFilter(!v || v === "all" ? "" : String(v))}
        >
          <SelectTrigger size="sm" className="w-28">
            <SelectValue>
              {(value) => MEM_FILTERS.find(([v]) => v === value)?.[1] ?? "全部"}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            {MEM_FILTERS.map(([v, label]) => (
              <SelectItem key={v} value={v}>
                {label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      {q.isPending ? (
        <Loading />
      ) : q.error ? (
        <ErrLine error={q.error} />
      ) : q.data!.length === 0 ? (
        <EmptyLine>没有记忆。</EmptyLine>
      ) : (
        q.data!.map((m) => (
          <div className={`${ROW} !flex-col !items-stretch`} key={m.id}>
            <div className="flex items-center gap-1.5">
              <Badge variant="pill">{m.status}</Badge>
              <Badge>{m.kind}</Badge>
              {m.pinned && <Badge variant="warn">📌</Badge>}
              <span className={MUTED}>{m.confidence}</span>
            </div>
            <div className="my-1 whitespace-pre-wrap break-words text-[13px]">{m.content}</div>
            <div className="flex gap-1.5">
              <Button size="sm" onClick={() => act.mutate({ id: m.id, action: "promote" })}>
                promote
              </Button>
              <Button
                variant="secondary"
                size="sm"
                onClick={() => act.mutate({ id: m.id, action: "pin" })}
              >
                pin
              </Button>
              <Button
                variant="destructive"
                size="sm"
                onClick={() => act.mutate({ id: m.id, action: "reject" })}
              >
                reject
              </Button>
            </div>
          </div>
        ))
      )}
    </div>
  );
}

export function RunsTab() {
  const [open, setOpen] = useState<string | null>(null);
  const q = useLoad(["runs"], () => apiField<Run[]>("/api/runs?limit=50", "runs"));
  if (q.isPending) return <Loading />;
  if (q.error) return <ErrLine error={q.error} />;
  const runs = q.data!;
  if (runs.length === 0) return <EmptyLine>还没有运行记录。</EmptyLine>;
  return (
    <div className={PANEL}>
      {runs.map((r) => (
        <div key={r.id}>
          <div
            className={`${ROW} cursor-pointer hover:border-(--mc-accent)`}
            onClick={() => setOpen(open === r.id ? null : r.id)}
          >
            <Badge variant="pill">{r.status}</Badge>
            <span className="flex-1 truncate">{r.input}</span>
            {r.recoverable && (
              <Badge variant="warn" title="可恢复">
                ⟲
              </Badge>
            )}
            <span className={MUTED}>{fmtTs(r.started_at)}</span>
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
  if (q.isPending)
    return (
      <div className="ml-4 mt-1 pl-3 border-l-2 border-(--mc-accent) text-(--mc-fg-faint) text-[13px] py-1">
        加载步骤…
      </div>
    );
  if (q.error)
    return (
      <div className="ml-4 mt-1 pl-3 border-l-2 border-(--mc-danger) text-(--mc-danger) text-[13px] py-1">
        {String(q.error)}
      </div>
    );
  const { run, steps } = q.data!;
  return (
    <div className="ml-4 mt-1 pl-3 border-l-2 border-(--mc-accent) flex flex-col gap-1 py-1">
      {run.final_output && <div className="whitespace-pre-wrap text-[13px]">{run.final_output}</div>}
      {run.error && <div className="text-(--mc-danger) text-[13px]">{run.error}</div>}
      {steps.map((s) => (
        <div className="flex gap-2 items-baseline text-[13px]" key={s.seq}>
          <Badge variant={s.ok ? "ok" : "danger"} className="rounded-full">
            {s.seq}. {s.tool_name}
          </Badge>
          <span className="text-(--mc-fg-muted) font-mono text-[11px] truncate">{s.args}</span>
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

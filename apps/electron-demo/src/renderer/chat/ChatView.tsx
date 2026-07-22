import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AssistantRuntimeProvider,
  ComposerPrimitive,
  MessagePrimitive,
  ThreadPrimitive,
  unstable_useComposerInputHistory,
  useLocalRuntime,
  type ChatModelAdapter,
  type ThreadMessageLike,
  type ToolCallMessagePartProps,
} from "@assistant-ui/react";

import { useApp, useConnection } from "../app-context";
import type { Interactions, PendingApproval, Run, RunDetail, RunStep, SessionMessage } from "../types";
import type { TurnEvent } from "../global";
import { apiField, apiGet, headerFor } from "../lib/ipc";
import { MarkdownText } from "@/components/assistant-ui/markdown-text";
import { Button, buttonVariants } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

function UserMessage() {
  return (
    <MessagePrimitive.Root className="flex justify-end">
      <div
        className="max-w-[80%] px-3.5 py-2 rounded-2xl rounded-br-md leading-relaxed whitespace-pre-wrap break-words text-white shadow-(--mc-shadow-card)"
        style={{ background: "var(--mc-accent-grad)" }}
      >
        <MessagePrimitive.Parts />
      </div>
    </MessagePrimitive.Root>
  );
}

function ToolCallView({ toolName, args, argsText, result, isError }: ToolCallMessagePartProps) {
  const skillName = toolName === "skill" && typeof args?.name === "string" ? args.name : null;
  const action = toolName === "skill" && typeof args?.action === "string" ? args.action : null;
  const title = skillName ? `Skill · ${skillName}` : toolName;
  const detail = argsText || (args ? JSON.stringify(args, null, 2) : "");
  const output = typeof result === "string" ? result : result == null ? "" : JSON.stringify(result, null, 2);

  return (
    <details className="my-1 rounded-[10px] border border-(--mc-border) bg-(--mc-surface-2) overflow-hidden">
      <summary className="cursor-pointer select-none px-3 py-2 flex items-center gap-2 text-[13px]">
        <span className={isError ? "text-(--mc-danger)" : "text-(--mc-ok)"}>{isError ? "✗" : "✓"}</span>
        <span className="font-mono font-semibold text-(--mc-fg)">{title}</span>
        {action && <span className="text-[11px] text-(--mc-fg-faint)">{action}</span>}
      </summary>
      {(detail || output) && (
        <div className="border-t border-(--mc-border) px-3 py-2 grid gap-2 text-[12px]">
          {detail && <pre className="whitespace-pre-wrap break-all text-(--mc-fg-muted)">{detail}</pre>}
          {output && <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all text-(--mc-fg-muted)">{output}</pre>}
        </div>
      )}
    </details>
  );
}

function AssistantMessage() {
  return (
    <MessagePrimitive.Root className="flex justify-start">
      <div className="max-w-[80%] px-3.5 py-2 rounded-2xl rounded-bl-md leading-relaxed break-words bg-(--mc-surface-strong) border border-(--mc-border) text-(--mc-fg)">
        <MessagePrimitive.Parts components={{ Text: MarkdownText, tools: { Override: ToolCallView } }} />
      </div>
    </MessagePrimitive.Root>
  );
}

/** The composer with terminal-style input history (ArrowUp on an empty draft
 *  recalls previously sent messages). Must be rendered inside the runtime
 *  provider — the hook reads the composer runtime. */
function Composer() {
  const history = unstable_useComposerInputHistory();
  return (
    <ComposerPrimitive.Root className="flex gap-2 items-end px-4 py-3 border-t border-(--mc-border)">
      <ComposerPrimitive.Input
        {...history}
        className="flex-1 resize-none min-h-[44px] max-h-[160px] px-3.5 py-3 rounded-[14px] border border-(--mc-border) bg-(--mc-surface-strong) text-(--mc-fg) outline-none focus:border-(--mc-accent) focus:shadow-(--mc-shadow-glow) transition-shadow font-[inherit]"
        placeholder="给 komo 发消息…（↑ 召回历史输入）"
      />
      <ComposerPrimitive.Send className={cn(buttonVariants({ variant: "gradient", size: "lg" }))}>
        发送
      </ComposerPrimitive.Send>
    </ComposerPrimitive.Root>
  );
}

function ApprovalModal({
  req,
  onDecide,
}: {
  req: PendingApproval;
  onDecide: (decision: "once" | "session" | "deny") => void;
}) {
  const dangerous = req.risk === "dangerous";
  return (
    <Dialog open onOpenChange={() => {}}>
      <DialogContent
        showCloseButton={false}
        className={cn("sm:max-w-[480px]", dangerous && "ring-destructive/50")}
      >
        <DialogHeader>
          <DialogTitle>{dangerous ? "🛑 需要审批（危险操作）" : "⚠️ 需要审批"}</DialogTitle>
          <DialogDescription className="break-words text-(--mc-fg)">{req.summary}</DialogDescription>
        </DialogHeader>
        {req.detail && (
          <div className="text-[13px] text-(--mc-fg-muted) whitespace-pre-wrap">{req.detail}</div>
        )}
        <DialogFooter>
          <Button variant="gradient" size="sm" onClick={() => onDecide("once")}>
            批准本次
          </Button>
          <Button variant="secondary" size="sm" onClick={() => onDecide("session")}>
            批准本会话
          </Button>
          <Button variant="destructive" size="sm" onClick={() => onDecide("deny")}>
            拒绝
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface ToolActivity {
  seq: number;
  name: string;
  args: string;
  done: boolean;
  ok?: boolean;
  summary?: string;
}

/** Live feed of the turn's tool calls (from the SSE `event: tool` frames). */
function ToolActivityStrip({ tools }: { tools: ToolActivity[] }) {
  if (tools.length === 0) return null;
  return (
    <div className="mx-4 mb-2 px-3 py-2 rounded-[12px] border border-(--mc-border) bg-(--mc-surface-strong) flex flex-col gap-1.5">
      <div className="text-[11px] uppercase tracking-wide text-(--mc-fg-faint)">工具调用</div>
      {tools.map((t) => (
        <div key={t.seq} className="flex items-center gap-2 text-[13px]">
          <span className="w-4 text-center">
            {!t.done ? "⏳" : t.ok ? "✓" : "✗"}
          </span>
          <span className="font-mono font-semibold text-(--mc-fg)">{t.name}</span>
          <span className="flex-1 truncate text-(--mc-fg-muted)">
            {t.done ? (t.summary ?? "") : t.args}
          </span>
        </div>
      ))}
    </div>
  );
}

function ClarifyBar({ question, onAnswer }: { question: string; onAnswer: (text: string) => void }) {
  const [text, setText] = useState("");
  const submit = () => {
    const t = text.trim();
    if (t) onAnswer(t);
  };
  return (
    <div className="mx-4 mb-2 px-3.5 py-2.5 rounded-[12px] border border-(--mc-accent-ring) bg-(--mc-accent-soft)">
      <div className="font-semibold mb-1.5 text-(--mc-fg)">❓ {question}</div>
      <div className="flex gap-2">
        <Input
          className="flex-1"
          value={text}
          placeholder="输入你的回答…"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
        />
        <Button variant="gradient" size="sm" onClick={submit}>
          回答
        </Button>
      </div>
    </div>
  );
}

type JsonValue = string | number | boolean | null | JsonValue[] | { [k: string]: JsonValue };

function parseArgs(raw: string): Record<string, JsonValue> | undefined {
  try {
    const value = JSON.parse(raw);
    return value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, JsonValue>)
      : undefined;
  } catch {
    return undefined;
  }
}

/** A RunStep → an assistant tool-call message part (rendered by `ToolCallView`).
 *  `argsText` is always set (the raw JSON); `args` is the parsed object when it
 *  parses, for skill-name detection in the view. */
function toolPart(step: Pick<RunStep, "seq" | "tool_name" | "args" | "result" | "error" | "ok">) {
  return {
    type: "tool-call" as const,
    toolCallId: `tool-${step.seq}`,
    toolName: step.tool_name,
    args: parseArgs(step.args) ?? {},
    argsText: step.args,
    result: step.ok ? step.result : step.error,
    isError: !step.ok,
  };
}

function buildInitialMessages(messages: SessionMessage[], details: RunDetail[]): ThreadMessageLike[] {
  const runs = [...details].sort((a, b) => a.run.started_at - b.run.started_at);
  let runIndex = 0;
  let pending: RunDetail | undefined;
  const result: ThreadMessageLike[] = [];
  for (const message of messages) {
    if (message.role === "user") {
      pending = runs[runIndex++];
      result.push({ role: "user", content: message.content, createdAt: new Date(message.timestamp * 1000) });
    } else if (message.role === "assistant") {
      result.push({
        role: "assistant",
        content: [
          ...(pending?.steps ?? []).map(toolPart),
          { type: "text" as const, text: message.content },
        ],
        createdAt: new Date(message.timestamp * 1000),
      });
      pending = undefined;
    }
  }
  return result;
}

async function loadSession(session: string): Promise<ThreadMessageLike[]> {
  const id = encodeURIComponent(session);
  const [messages, runs] = await Promise.all([
    apiField<SessionMessage[]>(`/api/sessions/${id}/messages`, "messages"),
    apiField<Run[]>("/api/runs?limit=500", "runs"),
  ]);
  const matching = runs
    .filter((run) => run.session_id === session)
    .sort((a, b) => a.started_at - b.started_at);
  const details = await Promise.all(
    matching.map((run) => apiGet<RunDetail>(`/api/runs/${encodeURIComponent(run.id)}`)),
  );
  return buildInitialMessages(messages, details);
}

export function ChatView() {
  const { connected } = useConnection();
  const { session } = useApp();
  const history = useQuery({
    queryKey: ["session-history", session],
    queryFn: () => loadSession(session),
    enabled: connected,
  });

  if (history.isPending && connected) {
    return <div className="flex-1 grid place-items-center text-[13px] text-(--mc-fg-faint)">加载历史…</div>;
  }
  if (history.isError) {
    return <div className="flex-1 grid place-items-center text-[13px] text-(--mc-danger)">历史加载失败：{history.error.message}</div>;
  }
  return <ChatRuntime initialMessages={history.data ?? []} />;
}

function ChatRuntime({ initialMessages }: { initialMessages: ThreadMessageLike[] }) {
  const { session, mode } = useApp();
  const qc = useQueryClient();
  const [approval, setApproval] = useState<PendingApproval | null>(null);
  const [question, setQuestion] = useState<string | null>(null);
  const [tools, setTools] = useState<ToolActivity[]>([]);
  const toolsRef = useRef<ToolActivity[]>([]);

  // Live tool-call feed: fold each streamed TurnEvent into the activity list.
  useEffect(() => {
    return window.komo.onToolEvent(({ session: eventSession, event }: { session: string; event: TurnEvent }) => {
      if (eventSession !== headerFor(session)) return;
      setTools((prev) => {
        if (event.type === "tool_started") {
          const next = prev.filter((t) => t.seq !== event.seq);
          next.push({ seq: event.seq, name: event.name, args: event.args, done: false });
          toolsRef.current = next;
          return next;
        }
        const next = prev.map((t) =>
          t.seq === event.seq
            ? { ...t, done: true, ok: event.ok, summary: event.summary }
            : t,
        );
        toolsRef.current = next;
        return next;
      });
    });
  }, [session]);

  // One turn = one chat request. While it's in flight, poll the interactions
  // endpoint so an approval raises the modal and a clarify raises the answer
  // bar; both resolve out-of-band and the same request eventually returns the
  // final reply. Capture `mode` through a getter so it stays live.
  const modeGetter = useMemo(() => ({ current: mode }), [mode]);

  const adapter = useMemo<ChatModelAdapter>(
    () => ({
      async run({ messages }) {
        let text = "";
        for (let i = messages.length - 1; i >= 0; i--) {
          const m = messages[i];
          if (m.role === "user") {
            text = m.content.map((p) => (p.type === "text" ? p.text : "")).join("");
            break;
          }
        }
        // Fresh tool feed for this turn.
        toolsRef.current = [];
        setTools([]);
        const path = `/api/interactions/${encodeURIComponent(session)}`;
        let stop = false;
        const poll = (async () => {
          while (!stop) {
            const r = await window.komo.api<Interactions>({ path });
            if (r.ok && r.data) {
              setApproval(r.data.approval ?? null);
              setQuestion(r.data.question ?? null);
            }
            await new Promise((res) => setTimeout(res, 1000));
          }
        })();
        try {
          const res = await window.komo.chat({
            header: headerFor(session),
            message: text,
            mode: modeGetter.current,
          });
          if (!res.ok) throw new Error(res.error || "请求失败");
          // A brand-new session now exists server-side — surface it in the list.
          void qc.invalidateQueries({ queryKey: ["sessions"] });
          const calls = toolsRef.current.map((tool) => toolPart({
            seq: tool.seq,
            tool_name: tool.name,
            args: tool.args,
            result: tool.ok ? (tool.summary ?? "") : "",
            error: tool.ok ? "" : (tool.summary ?? "调用失败"),
            ok: tool.ok ?? false,
          }));
          setTools([]);
          return { content: [...calls, { type: "text" as const, text: res.reply ?? "" }] };
        } finally {
          stop = true;
          await poll.catch(() => {});
          setApproval(null);
          setQuestion(null);
        }
      },
    }),
    [session, modeGetter],
  );

  const runtime = useLocalRuntime(adapter, { initialMessages });

  const decide = (decision: "once" | "session" | "deny") => {
    setApproval(null);
    void window.komo.api({
      path: `/api/interactions/${encodeURIComponent(session)}/approval`,
      method: "POST",
      body: { decision },
    });
  };

  const answer = (text: string) => {
    setQuestion(null);
    void window.komo.api({
      path: `/api/interactions/${encodeURIComponent(session)}/answer`,
      method: "POST",
      body: { text },
    });
  };

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <div className="flex-1 flex flex-col min-h-0">
        <ThreadPrimitive.Root className="flex-1 flex flex-col min-h-0">
          <ThreadPrimitive.Viewport className="flex-1 overflow-y-auto min-h-0 px-4 py-5 flex flex-col gap-3">
            <ThreadPrimitive.Empty>
              <div className="flex-1 flex items-center justify-center text-(--mc-fg-faint) py-10">
                开始和 komo 对话…
              </div>
            </ThreadPrimitive.Empty>
            <ThreadPrimitive.Messages components={{ UserMessage, AssistantMessage }} />
            <ThreadPrimitive.If running>
              <ToolActivityStrip tools={tools} />
              <div className="text-[13px] italic text-(--mc-fg-faint) px-1">komo 正在思考…</div>
            </ThreadPrimitive.If>
          </ThreadPrimitive.Viewport>

          {question && <ClarifyBar question={question} onAnswer={answer} />}

          <Composer />
        </ThreadPrimitive.Root>

        {approval && <ApprovalModal req={approval} onDecide={decide} />}
      </div>
    </AssistantRuntimeProvider>
  );
}

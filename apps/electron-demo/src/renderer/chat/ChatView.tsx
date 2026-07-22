import { useMemo, useState } from "react";
import {
  AssistantRuntimeProvider,
  ComposerPrimitive,
  MessagePrimitive,
  ThreadPrimitive,
  useLocalRuntime,
  type ChatModelAdapter,
} from "@assistant-ui/react";

import { useApp } from "../app-context";
import type { Interactions, PendingApproval } from "../types";
import { headerFor } from "../lib/ipc";
import { MarkdownText } from "@/components/assistant-ui/markdown-text";
import { Button, buttonVariants } from "@/components/ui/button";
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

function AssistantMessage() {
  return (
    <MessagePrimitive.Root className="flex justify-start">
      <div className="max-w-[80%] px-3.5 py-2 rounded-2xl rounded-bl-md leading-relaxed break-words bg-(--mc-surface-strong) border border-(--mc-border) text-(--mc-fg)">
        <MessagePrimitive.Parts components={{ Text: MarkdownText }} />
      </div>
    </MessagePrimitive.Root>
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
    <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/45 backdrop-blur-sm">
      <div
        className={`w-[min(480px,90vw)] p-5 rounded-2xl bg-(--mc-bg-elev) shadow-(--mc-shadow-card) border ${
          dangerous ? "border-(--mc-danger)" : "border-(--mc-border-strong)"
        }`}
      >
        <div className="font-bold mb-2.5 text-(--mc-fg)">
          {dangerous ? "🛑 需要审批（危险操作）" : "⚠️ 需要审批"}
        </div>
        <div className="mb-2 break-words text-(--mc-fg)">{req.summary}</div>
        {req.detail && (
          <div className="text-[13px] text-(--mc-fg-muted) whitespace-pre-wrap mb-2">
            {req.detail}
          </div>
        )}
        <div className="flex gap-2 justify-end mt-3.5">
          <Button variant="gradient" size="sm" onClick={() => onDecide("once")}>
            批准本次
          </Button>
          <Button variant="secondary" size="sm" onClick={() => onDecide("session")}>
            批准本会话
          </Button>
          <Button variant="destructive" size="sm" onClick={() => onDecide("deny")}>
            拒绝
          </Button>
        </div>
      </div>
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
        <input
          className="flex-1 px-2.5 py-1.5 rounded-[10px] border border-(--mc-border) bg-(--mc-bg) text-(--mc-fg) outline-none focus:border-(--mc-accent)"
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

export function ChatView() {
  const { session, mode } = useApp();
  const [approval, setApproval] = useState<PendingApproval | null>(null);
  const [question, setQuestion] = useState<string | null>(null);

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
          return { content: [{ type: "text" as const, text: res.reply ?? "" }] };
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

  const runtime = useLocalRuntime(adapter);

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
              <div className="text-[13px] italic text-(--mc-fg-faint) px-1">komo 正在思考…</div>
            </ThreadPrimitive.If>
          </ThreadPrimitive.Viewport>

          {question && <ClarifyBar question={question} onAnswer={answer} />}

          <ComposerPrimitive.Root className="flex gap-2 items-end px-4 py-3 border-t border-(--mc-border)">
            <ComposerPrimitive.Input
              className="flex-1 resize-none min-h-[44px] max-h-[160px] px-3.5 py-3 rounded-[14px] border border-(--mc-border) bg-(--mc-surface-strong) text-(--mc-fg) outline-none focus:border-(--mc-accent) focus:shadow-(--mc-shadow-glow) transition-shadow font-[inherit]"
              placeholder="给 komo 发消息…"
            />
            <ComposerPrimitive.Send
              className={cn(buttonVariants({ variant: "gradient", size: "lg" }))}
            >
              发送
            </ComposerPrimitive.Send>
          </ComposerPrimitive.Root>
        </ThreadPrimitive.Root>

        {approval && <ApprovalModal req={approval} onDecide={decide} />}
      </div>
    </AssistantRuntimeProvider>
  );
}

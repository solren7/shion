import { useMemo, useState } from "react";
import {
  AssistantRuntimeProvider,
  ComposerPrimitive,
  MessagePrimitive,
  ThreadPrimitive,
  useLocalRuntime,
  useMessagePartText,
  type ChatModelAdapter,
} from "@assistant-ui/react";

import { useNav } from "../app-context";
import type { Interactions, PendingApproval } from "../types";
import { headerFor, newSessionId } from "../lib/ipc";
import { renderMarkdown } from "../lib/markdown";

/** Assistant message text rendered as sanitized markdown. */
function MarkdownText() {
  const part = useMessagePartText();
  return (
    <div className="md" dangerouslySetInnerHTML={{ __html: renderMarkdown(part.text ?? "") }} />
  );
}

function UserMessage() {
  return (
    <MessagePrimitive.Root className="msg user">
      <div className="bubble user">
        <MessagePrimitive.Parts />
      </div>
    </MessagePrimitive.Root>
  );
}

function AssistantMessage() {
  return (
    <MessagePrimitive.Root className="msg assistant">
      <div className="bubble assistant">
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
    <div className="modal-backdrop">
      <div className={dangerous ? "modal danger" : "modal"}>
        <div className="modal-title">{dangerous ? "🛑 需要审批（危险操作）" : "⚠️ 需要审批"}</div>
        <div className="modal-summary">{req.summary}</div>
        {req.detail && <div className="modal-detail">{req.detail}</div>}
        <div className="modal-actions">
          <button className="btn ok" onClick={() => onDecide("once")}>
            批准本次
          </button>
          <button className="btn" onClick={() => onDecide("session")}>
            批准本会话
          </button>
          <button className="btn deny" onClick={() => onDecide("deny")}>
            拒绝
          </button>
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
    <div className="clarify">
      <div className="clarify-q">❓ {question}</div>
      <div className="clarify-row">
        <input
          className="clarify-input"
          value={text}
          placeholder="输入你的回答…"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
        />
        <button className="btn ok" onClick={submit}>
          回答
        </button>
      </div>
    </div>
  );
}

export function ChatView() {
  const { session, setSession } = useNav();
  const [mode, setMode] = useState<"interactive" | "trusted">("interactive");
  const [approval, setApproval] = useState<PendingApproval | null>(null);
  const [question, setQuestion] = useState<string | null>(null);

  // One turn = one chat request. While it's in flight, poll the interactions
  // endpoint so an approval raises the modal and a clarify raises the answer
  // bar; both resolve out-of-band and the same request eventually returns the
  // final reply. `mode` is read live via a ref-free closure over state is fine
  // here because the adapter is recreated when `session` changes (the view is
  // keyed by session in App) — but capture mode through a getter to stay live.
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
      <div className="chat">
        <div className="chat-toolbar">
          <button className="small" onClick={() => setSession(newSessionId())}>
            新会话
          </button>
          <label className="mode">
            <input
              type="checkbox"
              checked={mode === "trusted"}
              onChange={(e) => setMode(e.target.checked ? "trusted" : "interactive")}
            />
            <span title="开启后副作用工具自动批准（等同 komo chat）；关闭则弹出审批">
              信任模式（自动批准）
            </span>
          </label>
        </div>

        <ThreadPrimitive.Root className="thread">
          <ThreadPrimitive.Viewport className="messages">
            <ThreadPrimitive.Empty>
              <div className="empty">开始和 komo 对话…</div>
            </ThreadPrimitive.Empty>
            <ThreadPrimitive.Messages
              components={{ UserMessage, AssistantMessage }}
            />
            <ThreadPrimitive.If running>
              <div className="typing">komo 正在思考…</div>
            </ThreadPrimitive.If>
          </ThreadPrimitive.Viewport>

          {question && <ClarifyBar question={question} onAnswer={answer} />}

          <ComposerPrimitive.Root className="composer">
            <ComposerPrimitive.Input className="composer-input" placeholder="给 komo 发消息…" />
            <ComposerPrimitive.Send className="send">发送</ComposerPrimitive.Send>
          </ComposerPrimitive.Root>
        </ThreadPrimitive.Root>

        {approval && <ApprovalModal req={approval} onDecide={decide} />}
      </div>
    </AssistantRuntimeProvider>
  );
}

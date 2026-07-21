// Mirror DTOs for the gateway's `/api/*` responses. Kept loose (enum fields as
// plain strings) — the GUI only displays them, so exact variant typing isn't
// worth coupling to the Rust definitions.

export interface StatusSnapshot {
  ok: boolean;
  version: string;
  channels: string[];
  home_chat: string | null;
  open_tasks: number;
  sessions: number;
}

export interface Task {
  id: string;
  title: string;
  note: string;
  status: string;
  board: string;
  due_at: number | null;
  created_at: number;
}

export interface Memory {
  id: string;
  kind: string;
  content: string;
  status: string;
  confidence: string;
  pinned: boolean;
}

export interface Run {
  id: string;
  session_id: string;
  input: string;
  plan: string;
  status: string;
  recoverable: boolean;
  started_at: number;
  final_output: string;
  error: string;
}

export interface RunStep {
  seq: number;
  tool_name: string;
  args: string;
  result: string;
  error: string;
  ok: boolean;
}

export interface SessionSummary {
  id: string;
  created_at: number;
  messages: number;
  user_turns: number;
}

export interface PendingApproval {
  summary: string;
  detail: string | null;
  risk: string;
}

export interface Interactions {
  approval: PendingApproval | null;
  question: string | null;
}

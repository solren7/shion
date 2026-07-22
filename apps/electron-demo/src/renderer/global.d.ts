// The IPC surface the preload exposes on `window.komo`.

export interface KomoApiRequest {
  path: string;
  method?: "GET" | "POST";
  body?: unknown;
}
export interface KomoApiResponse<T = unknown> {
  ok: boolean;
  status: number;
  data?: T;
  error?: string;
}
export interface KomoChatRequest {
  header: string;
  message: string;
  mode: "interactive" | "trusted";
}
export interface KomoChatResponse {
  ok: boolean;
  reply?: string;
  error?: string;
}
export interface KomoConnectResponse {
  connected: boolean;
  base?: string;
  error?: string;
}

/** A live tool-call event streamed during a turn (mirrors komo's `TurnEvent`). */
export type TurnEvent =
  | { type: "tool_started"; seq: number; name: string; args: string }
  | { type: "tool_finished"; seq: number; name: string; ok: boolean; summary: string };

export interface ToolEventPayload {
  session: string;
  event: TurnEvent;
}

declare global {
  interface Window {
    komo: {
      connect(): Promise<KomoConnectResponse>;
      api<T = unknown>(req: KomoApiRequest): Promise<KomoApiResponse<T>>;
      chat(req: KomoChatRequest): Promise<KomoChatResponse>;
      onToolEvent(cb: (payload: ToolEventPayload) => void): () => void;
    };
  }
}

export {};

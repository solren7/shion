// Thin typed helpers over the `window.komo` IPC bridge.

export async function apiGet<T>(path: string): Promise<T> {
  const r = await window.komo.api<T>({ path, method: "GET" });
  if (!r.ok) throw new Error(r.error || `HTTP ${r.status}`);
  return r.data as T;
}

/** GET a `{ "<key>": T }` envelope and return the inner value. */
export async function apiField<T>(path: string, key: string): Promise<T> {
  const obj = await apiGet<Record<string, unknown>>(path);
  return (obj?.[key] ?? []) as T;
}

export async function apiPost<T = unknown>(path: string, body?: unknown): Promise<T> {
  const r = await window.komo.api<T>({ path, method: "POST", body });
  if (!r.ok) throw new Error(r.error || `HTTP ${r.status}`);
  return r.data as T;
}

/** Full session id → the `X-Komo-Session-Id` header (server re-prepends `api:`). */
export function headerFor(fullSession: string): string {
  return fullSession.startsWith("api:") ? fullSession.slice("api:".length) : fullSession;
}

export function newSessionId(): string {
  return `api:gui-electron-${crypto.randomUUID()}`;
}

/** UTC `MM-DD HH:MM` from unix seconds. */
export function fmtTs(ts: number): string {
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())}`;
}

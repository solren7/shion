// Electron main process for the komo desktop demo.
//
// The renderer is fully sandboxed (contextIsolation + no node integration), so
// all I/O runs here: gateway discovery (reading ~/.komo/gateway.json) and every
// HTTP call to the gateway. The bearer key never enters the renderer — the
// renderer only sends {path, method, body} over IPC and gets JSON back. This is
// the "REST-over-IPC" pattern hermes-agent's desktop uses, minus the WebSocket
// gateway (komo has no token streaming yet, so chat is one request/response).

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { app, BrowserWindow, ipcMain, shell } from "electron";

const PROBE_TIMEOUT_MS = 2000;
// Longer than a plain HTTP request: an interactive turn can block server-side on
// a human approving a tool (up to the gateway's 5-min approval timeout) after
// the model has already done work.
const REQUEST_TIMEOUT_MS = 600_000;

interface Gateway {
  base: string;
  key: string;
}
interface ApiRequest {
  path: string;
  method?: string;
  body?: unknown;
}
interface ChatRequest {
  header: string;
  message: string;
  mode?: "interactive" | "trusted";
}

/** Resolve ~/.komo, honoring KOMO_HOME / SHION_HOME and the .shion legacy dir. */
function komoHome(): string {
  const env = process.env.KOMO_HOME || process.env.SHION_HOME;
  if (env && env.length > 0) return env;
  const home = os.homedir();
  const current = path.join(home, ".komo");
  const legacy = path.join(home, ".shion");
  if (!fs.existsSync(current) && fs.existsSync(legacy)) return legacy;
  return current;
}

/** Read the gateway rendezvous file, or null if absent/unparseable. */
function readGateway(): Gateway | null {
  try {
    const raw = fs.readFileSync(path.join(komoHome(), "gateway.json"), "utf8");
    const info = JSON.parse(raw);
    const host = info.bind === "0.0.0.0" ? "127.0.0.1" : info.bind;
    return { base: `http://${host}:${info.port}`, key: String(info.key) };
  } catch {
    return null;
  }
}

// The connection the renderer is currently bound to. Refreshed on every
// `komo:connect`, so a gateway restart (new port/key) is picked up.
let gateway: Gateway | null = null;

async function fetchWithTimeout(
  url: string,
  options: RequestInit,
  timeoutMs: number,
): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(url, { ...options, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

async function healthOk(base: string): Promise<boolean> {
  try {
    const res = await fetchWithTimeout(`${base}/health`, {}, PROBE_TIMEOUT_MS);
    return res.ok;
  } catch {
    return false;
  }
}

function registerIpc(): void {
  // Discover + probe the gateway. Returns connection status (never the key).
  ipcMain.handle("komo:connect", async () => {
    const found = readGateway();
    if (!found) {
      gateway = null;
      return {
        connected: false,
        error: "未发现运行中的 komo gateway（启动 `komo gateway` 后自动连接）",
      };
    }
    if (!(await healthOk(found.base))) {
      gateway = null;
      return { connected: false, error: "gateway 无响应（rendezvous 可能过期）" };
    }
    gateway = found;
    return { connected: true, base: found.base };
  });

  // Generic authenticated request against a /api/* or /v1/* path.
  ipcMain.handle("komo:api", async (_evt, req: ApiRequest) => {
    if (!gateway) return { ok: false, status: 0, error: "未连接" };
    const { path: p, method = "GET", body } = req ?? {};
    try {
      const res = await fetchWithTimeout(
        `${gateway.base}${p}`,
        {
          method,
          headers: {
            Authorization: `Bearer ${gateway.key}`,
            ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
          },
          body: body !== undefined ? JSON.stringify(body) : undefined,
        },
        REQUEST_TIMEOUT_MS,
      );
      const text = await res.text();
      const data = text ? JSON.parse(text) : null;
      if (!res.ok) {
        const msg = (data && data.error) || `HTTP ${res.status}`;
        return { ok: false, status: res.status, error: msg, data };
      }
      return { ok: true, status: res.status, data };
    } catch (err) {
      return { ok: false, status: 0, error: errMsg(err) };
    }
  });

  // One chat turn over the SSE stream. `mode` picks the loopback session context:
  // interactive (approval/clarify suspend the turn, resolved out-of-band) or
  // trusted (side-effecting tools auto-approve, like `komo chat`). Tool-call
  // events (`event: tool`) are forwarded live to the renderer via
  // `komo:tool-event`; the final assistant text is accumulated and returned.
  ipcMain.handle("komo:chat", async (evt, req: ChatRequest) => {
    if (!gateway) return { ok: false, error: "未连接" };
    const { header, message, mode } = req ?? {};
    const headers: Record<string, string> = {
      Authorization: `Bearer ${gateway.key}`,
      "Content-Type": "application/json",
      "X-Komo-Session-Id": header,
    };
    if (mode === "trusted") headers["X-Komo-Trusted"] = "1";
    else headers["X-Komo-Interactive"] = "1";
    try {
      const res = await fetchWithTimeout(
        `${gateway.base}/v1/chat/completions`,
        {
          method: "POST",
          headers,
          body: JSON.stringify({
            model: "komo",
            stream: true,
            messages: [{ role: "user", content: message }],
          }),
        },
        REQUEST_TIMEOUT_MS,
      );
      if (!res.ok || !res.body) {
        const text = await res.text().catch(() => "");
        let msg = `HTTP ${res.status}`;
        try {
          const j = JSON.parse(text);
          if (j?.error) msg = j.error;
        } catch {
          /* keep default */
        }
        return { ok: false, error: msg };
      }

      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = "";
      let reply = "";
      const flushFrame = (frame: string) => {
        let event = "message";
        const dataLines: string[] = [];
        for (const line of frame.split("\n")) {
          if (line.startsWith("event:")) event = line.slice(6).trim();
          else if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""));
        }
        const data = dataLines.join("\n");
        if (!data || data === "[DONE]") return;
        if (event === "tool") {
          try {
            evt.sender.send("komo:tool-event", { session: header, event: JSON.parse(data) });
          } catch {
            /* ignore malformed frame */
          }
        } else {
          try {
            const chunk = JSON.parse(data);
            const piece = chunk?.choices?.[0]?.delta?.content;
            if (piece) reply += piece;
          } catch {
            /* ignore malformed frame */
          }
        }
      };

      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx: number;
        while ((idx = buf.indexOf("\n\n")) >= 0) {
          flushFrame(buf.slice(0, idx));
          buf = buf.slice(idx + 2);
        }
      }
      if (buf.trim()) flushFrame(buf);
      return { ok: true, reply };
    } catch (err) {
      return { ok: false, error: errMsg(err) };
    }
  });
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1100,
    height: 780,
    minWidth: 720,
    minHeight: 520,
    title: "komo",
    backgroundColor: "#07070d",
    webPreferences: {
      preload: path.join(__dirname, "../preload/index.cjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });

  // Open external links in the OS browser, never in-app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });

  // electron-vite injects the renderer dev URL; production loads the bundle.
  const devUrl = process.env.ELECTRON_RENDERER_URL;
  if (devUrl) {
    void win.loadURL(devUrl);
  } else {
    void win.loadFile(path.join(__dirname, "../renderer/index.html"));
  }
}

app.whenReady().then(() => {
  registerIpc();
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

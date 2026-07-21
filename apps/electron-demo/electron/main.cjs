// Electron main process for the komo desktop demo.
//
// The renderer is fully sandboxed (contextIsolation + no node integration), so
// all I/O runs here: gateway discovery (reading ~/.komo/gateway.json) and every
// HTTP call to the gateway. The bearer key never enters the renderer — the
// renderer only sends {path, method, body} over IPC and gets JSON back. This is
// the "REST-over-IPC" pattern hermes-agent's desktop uses, minus the WebSocket
// gateway (komo has no token streaming yet, so chat is one request/response).

const { app, BrowserWindow, ipcMain, shell } = require("electron");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const PROBE_TIMEOUT_MS = 2000;
// Longer than a plain HTTP request: an interactive turn can block server-side on
// a human approving a tool (up to the gateway's 5-min approval timeout) after
// the model has already done work.
const REQUEST_TIMEOUT_MS = 600_000;

/** Resolve ~/.komo, honoring KOMO_HOME / SHION_HOME and the .shion legacy dir. */
function komoHome() {
  const env = process.env.KOMO_HOME || process.env.SHION_HOME;
  if (env && env.length > 0) return env;
  const home = os.homedir();
  const current = path.join(home, ".komo");
  const legacy = path.join(home, ".shion");
  if (!fs.existsSync(current) && fs.existsSync(legacy)) return legacy;
  return current;
}

/** Read the gateway rendezvous file, or null if absent/unparseable. */
function readGateway() {
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
let gateway = null;

async function fetchWithTimeout(url, options, timeoutMs) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(url, { ...options, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

async function healthOk(base) {
  try {
    const res = await fetchWithTimeout(`${base}/health`, {}, PROBE_TIMEOUT_MS);
    return res.ok;
  } catch {
    return false;
  }
}

function registerIpc() {
  // Discover + probe the gateway. Returns connection status (never the key).
  ipcMain.handle("komo:connect", async () => {
    const found = readGateway();
    if (!found) {
      gateway = null;
      return { connected: false, error: "未发现运行中的 komo gateway（启动 `komo gateway` 后自动连接）" };
    }
    if (!(await healthOk(found.base))) {
      gateway = null;
      return { connected: false, error: "gateway 无响应（rendezvous 可能过期）" };
    }
    gateway = found;
    return { connected: true, base: found.base };
  });

  // Generic authenticated request against a /api/* or /v1/* path.
  ipcMain.handle("komo:api", async (_evt, req) => {
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
      return { ok: false, status: 0, error: String(err && err.message ? err.message : err) };
    }
  });

  // One chat turn. `mode` picks the loopback session context: interactive
  // (approval/clarify suspend the turn, resolved out-of-band) or trusted
  // (side-effecting tools auto-approve, like `komo chat`).
  ipcMain.handle("komo:chat", async (_evt, req) => {
    if (!gateway) return { ok: false, error: "未连接" };
    const { header, message, mode } = req ?? {};
    const headers = {
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
            stream: false,
            messages: [{ role: "user", content: message }],
          }),
        },
        REQUEST_TIMEOUT_MS,
      );
      const data = await res.json();
      if (!res.ok) return { ok: false, error: (data && data.error) || `HTTP ${res.status}` };
      const reply = data?.choices?.[0]?.message?.content ?? "";
      return { ok: true, reply };
    } catch (err) {
      return { ok: false, error: String(err && err.message ? err.message : err) };
    }
  });
}

function createWindow() {
  const win = new BrowserWindow({
    width: 1100,
    height: 780,
    minWidth: 720,
    minHeight: 520,
    title: "komo",
    backgroundColor: "#16181c",
    webPreferences: {
      preload: path.join(__dirname, "preload.cjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });

  // Open external links in the OS browser, never in-app.
  win.webContents.setWindowOpenHandler(({ url }) => {
    shell.openExternal(url);
    return { action: "deny" };
  });

  const devUrl = process.env.KOMO_ELECTRON_DEV;
  if (devUrl) {
    win.loadURL(devUrl);
  } else {
    win.loadFile(path.join(__dirname, "..", "dist", "index.html"));
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

# komo — Electron desktop demo

A second desktop client for komo, using the **Electron + Vite + React + TypeScript
+ @assistant-ui/react + @tanstack/react-query** stack (the same shape as
hermes-agent's desktop shell). It's the JS sibling of the Rust
[`crates/komo-gui`](../../crates/komo-gui) Dioxus client — both are pure HTTP
front ends over the gateway's api channel.

## What it does

- **Auto-discovers** a running gateway via `~/.komo/gateway.json` (read in the
  Electron main process; the bearer key never enters the renderer).
- **Chat** built on `@assistant-ui/react` primitives (`useLocalRuntime` +
  `ChatModelAdapter`), replies rendered as sanitized markdown (marked +
  DOMPurify — injected HTML is neutralized).
- **Interactive tool approval + clarify**: while a turn is in flight the app
  polls `/api/interactions/{session}`; an approval raises a modal (approve
  once / this session / deny), a clarify question raises an inline answer bar.
  Both resolve out-of-band over the loopback POST endpoints. A **trusted-mode**
  toggle switches to auto-approve (like `komo chat`).
- **Dashboard** with TanStack Query: status, tasks, memories (with
  promote/pin/reject), runs (expand for steps), sessions ("continue in chat").
  Only the active tab polls.

## Architecture

- **Main process** (`electron/main.cjs`): gateway discovery + all HTTP calls,
  exposed to the renderer as `komo:connect` / `komo:api` / `komo:chat` over IPC.
  The renderer is sandboxed (`contextIsolation`, no node integration).
- **Preload** (`electron/preload.cjs`): the only bridge — `window.komo`.
- **Renderer** (`src/`): React 19 + Vite. `App.tsx` runs the connection
  lifecycle and view switch; `chat/ChatView.tsx` is the assistant-ui thread;
  `dashboard/Dashboard.tsx` is the TanStack-driven panels.

Single request/response per turn — komo has no token streaming yet, so there's
no WebSocket layer (unlike hermes). A turn suspends server-side for
approval/clarify and the same HTTP request returns the final reply.

## Run

Start a komo gateway first (so `~/.komo/gateway.json` exists), then:

```bash
cd apps/electron-demo
npm install
npm run dev      # Vite renderer + Electron with hot reload
# or
npm run build && npm start   # production build, then launch
```

## Known limitations (demo scope)

- No token streaming (spinner + whole reply), mirroring the backend.
- "Continue in chat" resumes the **server-side** session context (history
  threads correctly) but the visible transcript starts empty — past messages
  aren't re-hydrated into the assistant-ui thread.
- Not packaged (`electron-builder`); `npm run dev` / `start` only.

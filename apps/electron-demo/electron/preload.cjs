// Preload: the only bridge between the sandboxed renderer and the main process.
// Exposes a tiny typed surface on `window.komo`; the bearer key stays in main.

const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("komo", {
  connect: () => ipcRenderer.invoke("komo:connect"),
  api: (req) => ipcRenderer.invoke("komo:api", req),
  chat: (req) => ipcRenderer.invoke("komo:chat", req),
});

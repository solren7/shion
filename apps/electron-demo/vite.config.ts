import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `base: "./"` so the built index.html loads its assets over `file://` when
// Electron opens `dist/index.html` in production.
export default defineConfig({
  base: "./",
  plugins: [react()],
  server: { host: "127.0.0.1", port: 5273, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true },
});

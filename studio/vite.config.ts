import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The app `ampup-memory serve --local` serves at /studio (built into dist/). Its links are hashes
// (`/studio/#/f/...`), so the server answers one page and the pages route themselves.
//
// `npm run dev` serves it on :5174 and forwards the memory's routes to a local server (AMPUP_MEMORY_STUDIO_SERVER,
// http://127.0.0.1:8080 by default: `ampup-memory serve --local`), as if from the same origin.
const server = process.env.AMPUP_MEMORY_STUDIO_SERVER ?? "http://127.0.0.1:8080";
const memory = {
  target: server,
  changeOrigin: true,
  // The server takes browser writes from its own origin only: forwarded, they come from it.
  headers: { origin: new URL(server).origin },
};
export default defineConfig({
  root: __dirname,
  base: "/studio/",
  plugins: [react()],
  build: { outDir: "dist", emptyOutDir: true },
  server: {
    port: 5174,
    proxy: Object.fromEntries(
      ["/browse", "/search", "/me", "/lessons", "/mcp", "/agent/run", "/admin", "/learn", "/skills"].map((p) => [
        p,
        memory,
      ]),
    ),
  },
});

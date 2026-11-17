import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The pages as a library, for a host that renders them in its own app (lib/: ES modules and one stylesheet,
// style.css; React is the host's).
export default defineConfig({
  root: __dirname,
  plugins: [react()],
  build: {
    outDir: "lib",
    emptyOutDir: true,
    copyPublicDir: false,
    lib: {
      entry: { index: "src/index.ts", helpers: "src/helpers.ts" },
      formats: ["es"],
      cssFileName: "style",
    },
    rollupOptions: {
      external: ["react", "react-dom", "react/jsx-runtime", "react-dom/client"],
    },
  },
});

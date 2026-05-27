import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [solid()],

  // Scan only the app entry. The repo vendors research projects under
  // projects-to-research/ whose own index.html files would otherwise be pulled
  // into Vite's dependency scan (their deps aren't installed here), breaking
  // `vite`/`tauri dev`.
  optimizeDeps: {
    entries: ["index.html"],
    // Force pre-bundling of CJS deps in the markdown chain so their default
    // exports interop correctly under Vite's native ESM dev server.
    include: ["debug", "extend", "solid-markdown", "remark-gfm"],
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));

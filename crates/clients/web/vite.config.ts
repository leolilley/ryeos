import path from "node:path";
import { fileURLToPath } from "node:url";

import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";
import { browserDevelopmentPlugin, developmentUpstream } from "./dev-server.mjs";

const packageRoot = path.dirname(fileURLToPath(import.meta.url));
const defaultOutDir = path.resolve(packageRoot, "../../../target/ui-browser-stage");

export default defineConfig(({ command }) => ({
  base: "/ui/assets/",
  plugins: [svelte(), browserDevelopmentPlugin(packageRoot)],
  // Never expose operator-session proxying on an external interface. WASM
  // comes from the running daemon, not an independently stale checkout copy.
  server: command === "serve" ? {
    host: "127.0.0.1",
    port: 7411,
    strictPort: true,
    cors: false,
    fs: { strict: true, allow: [packageRoot] },
    proxy: {
      "^/ui/(api(?:/|$)|launch(?:/|$))": {
        target: developmentUpstream(process.env.RYEOS_UI_DEV_UPSTREAM),
        changeOrigin: false,
      },
      "^/ui/assets/ryeos_web(?:_bg\\.wasm|\\.js)(?:\\?.*)?$": {
        target: developmentUpstream(process.env.RYEOS_UI_DEV_UPSTREAM),
        changeOrigin: false,
      },
    },
  } : undefined,
  build: {
    assetsDir: "",
    emptyOutDir: true,
    manifest: false,
    minify: false,
    outDir: process.env.RYEOS_UI_ASSET_STAGE || defaultOutDir,
    rollupOptions: {
      input: path.resolve(packageRoot, "browser/entry.ts"),
      preserveEntrySignatures: "strict",
      output: {
        manualChunks(id) {
          return id.includes("/node_modules/three/") ? "ryeos_three" : undefined;
        },
        entryFileNames: "ryeos_ui.js",
        chunkFileNames: "[name].js",
        assetFileNames: (asset) => asset.names.some((name) => name.endsWith(".css"))
          ? "ryeos_ui.css"
          : "[name][extname]",
      },
    },
    sourcemap: false,
  },
}));

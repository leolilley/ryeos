import path from "node:path";
import { fileURLToPath } from "node:url";

import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

const packageRoot = path.dirname(fileURLToPath(import.meta.url));
const defaultOutDir = path.resolve(packageRoot, "../../../target/ui-browser-stage");

export default defineConfig({
  base: "/ui/assets/",
  plugins: [svelte()],
  build: {
    assetsDir: "",
    emptyOutDir: true,
    manifest: false,
    minify: false,
    outDir: process.env.RYEOS_UI_ASSET_STAGE || defaultOutDir,
    rollupOptions: {
      input: path.resolve(packageRoot, "browser/main.ts"),
      preserveEntrySignatures: "strict",
      output: {
        entryFileNames: "ryeos_ui.js",
        chunkFileNames: "[name].js",
        assetFileNames: (asset) => asset.names.some((name) => name.endsWith(".css"))
          ? "ryeos_ui.css"
          : "[name][extname]",
      },
    },
    sourcemap: false,
  },
});

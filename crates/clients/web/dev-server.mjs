import { readFile } from "node:fs/promises";
import path from "node:path";

export function developmentUpstream(value = "http://127.0.0.1:7400") {
  const url = new URL(value);
  if (url.protocol !== "http:" || url.hostname !== "127.0.0.1"
    || url.username || url.password || url.pathname !== "/" || url.search || url.hash) {
    throw new Error("RYEOS_UI_DEV_UPSTREAM must be an HTTP loopback node origin (127.0.0.1)");
  }
  return url.origin;
}

export function developmentDocument(html) {
  if (!html.includes('src="/ui/assets/ryeos_ui.js"')) {
    throw new Error("browser document has no expected renderer entry");
  }
  return html
    .replace('<link rel="stylesheet" href="/ui/assets/ryeos_ui.css">', "")
    // transformIndexHtml applies Vite's configured base to source URLs.
    .replace('src="/ui/assets/ryeos_ui.js"', 'src="/browser/entry.ts"')
    .replace("Loading signed interface", "Loading local development renderer");
}

/** Development presentation only; all node/session authority stays upstream. */
export function browserDevelopmentPlugin(packageRoot) {
  return {
    name: "ryeos-browser-development",
    apply: "serve",
    configResolved(config) {
      if (config.server.host !== "127.0.0.1" || !config.server.strictPort
        || config.server.cors !== false || config.server.https) {
        throw new Error("RyeOS browser development requires strict HTTP loopback serving without CORS");
      }
    },
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
        if (!["/", "/ui", "/ui/"].includes(pathname)) return next();
        if (request.method !== "GET" && request.method !== "HEAD") return next();
        try {
          const source = await readFile(path.join(packageRoot, "pkg/index.html"), "utf8");
          const html = await server.transformIndexHtml("/ui", developmentDocument(source));
          const port = server.config.server.port;
          response.setHeader("Content-Type", "text/html; charset=utf-8");
          response.setHeader("Cache-Control", "no-store");
          // Vite injects styles and uses a websocket; these allowances apply
          // only to this local development document, never installed assets.
          response.setHeader("Content-Security-Policy", [
            "default-src 'self'", "script-src 'self' 'wasm-unsafe-eval'",
            "style-src 'self' 'unsafe-inline'", "img-src 'self' data:",
            `connect-src 'self' ws://127.0.0.1:${port}`,
            "object-src 'none'", "base-uri 'none'", "frame-ancestors 'none'",
          ].join("; "));
          response.end(request.method === "HEAD" ? undefined : html);
        } catch (error) {
          next(error);
        }
      });
    },
  };
}

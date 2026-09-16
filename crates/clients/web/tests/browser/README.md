# Real-browser component checks

`run.mjs` starts a loopback-only static server for the checked-in browser asset
tree, launches Playwright Chromium, and exercises the modules through the same
`/ui/assets/` namespace used by an installed RyeOS node. It does not start or
modify a RyeOS daemon.

Run from `crates/clients/web`:

```text
npm run test:browser
```

The runner uses an already installed `playwright` package and browser. It does
not download dependencies. Resolution order is:

1. `RYEOS_PLAYWRIGHT_PACKAGE`, as a package name or absolute package path;
2. the crate's normal Node module resolution;
3. the system's existing global npm module root.

`RYEOS_PLAYWRIGHT_BROWSER` may select `chromium`, `firefox`, or `webkit`; the
default is `chromium`. Missing packages or browser executables fail with an
actionable message rather than silently skipping the browser gate.

Pure transformation and layout tests remain in `tests/*.test.js`. Put focus,
native events, accessibility-tree, lifecycle, and computed-layout checks here,
because fake DOM objects cannot qualify those browser contracts.

# Real-browser component checks

`run.mjs` starts a loopback-only static server for the checked-in browser asset
tree, launches Playwright Chromium, and exercises the modules through the same
`/ui/assets/` namespace used by an installed RyeOS node. It does not start or
modify a RyeOS daemon.

Run from `crates/clients/web`:

```text
npm run test:browser
```

The runner uses the exact `playwright` package selected by this crate's
`package-lock.json` and the browser revision retained by that package. It does
not download dependencies while running and never searches global npm state.
Set `PLAYWRIGHT_BROWSERS_PATH` to the exact retained browser realization.

`RYEOS_PLAYWRIGHT_BROWSER` may select `chromium`, `firefox`, or `webkit`; the
default is `chromium`. Missing packages or browser executables fail with an
actionable message rather than silently skipping the browser gate.

Pure transformation and layout tests remain in `tests/*.test.js`. Put focus,
native events, accessibility-tree, lifecycle, and computed-layout checks here,
because fake DOM objects cannot qualify those browser contracts.

## Shared-model visual preview

`ryeos-client-base/examples/view_set_visual_fixture.rs` emits production
`RyeOsCore` envelopes for a synthetic surface. The browser runner can render
these with the real DOM adapter and CSS, without a daemon, seat attachment or
execution dispatcher. This is different from the standalone design study.

From the repository root, generate the fixture with:

```sh
cargo run -p ryeos-client-base --example view_set_visual_fixture --offline -j1 --quiet > /tmp/ryeos-ui-visual-fixture.json
```

Then run the browser checks with `RYEOS_UI_VISUAL_FIXTURE` set to that file.
`RYEOS_UI_SCREENSHOT_DIR` optionally selects the generated screenshot directory
(default `/tmp/ryeos-ui-production-preview`). The runner captures the work,
overview and launcher arrangements at 1600×1000, plus work at 1024×768 and
390×844. It checks narrow focus reachability, composer/content separation and
reduced motion. External HTTPS requests are blocked for this preview.

All projects, transcripts and evidence are synthetic. The preview intentionally
has no executable input route; a disabled send control is not a live failure.
It proves component composition, not signed bundle publication, installed boot,
independent conversation targeting or a successful worker execution.

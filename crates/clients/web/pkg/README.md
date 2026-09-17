# Browser asset ownership

This directory is the browser client's served asset root. `ryeos-ui` embeds
these files for installed operation, while the development asset proxy and the
browser test harness serve the same `/ui/assets/` URL namespace.

The following files are generated from `src/wasm.rs` and the shared Rust UI
core:

- `ryeos_web.js`
- `ryeos_web_bg.wasm`

Regenerate and verify them with `scripts/check-ui-wasm-fresh.sh`. Do not edit
them by hand.

`ryeos_three.js` is the exact `three@0.128.0` `build/three.module.js` artifact
selected by `package-lock.json`; its retained MIT notice is
`../third_party/three/LICENSE`. Regenerate it from the locked offline npm input,
never from a CDN or an ambient host installation.

All remaining JavaScript, CSS and HTML files in this directory are authored
source. They are committed beside generated artifacts because this directory is
also the exact static asset tree embedded by `ryeos-ui`. Browser adapter code
must remain transport, DOM and platform glue over the shared semantic view
model; it must not acquire product routing or authorization logic.

Imports beginning with `/ui/assets/` are the installed serving contract. Tests
must serve that namespace rather than rewriting those imports to suit Node.

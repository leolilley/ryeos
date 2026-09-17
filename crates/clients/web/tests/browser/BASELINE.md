# Pre-Svelte browser renderer baseline

This record freezes the accepted browser presentation before the renderer is
migrated to Svelte. It is comparison evidence, not a second UI contract: Rust
still owns the semantic view model, layout state, intents, effects and
authority decisions.

## Source and exact inputs

- Source commit: `87710628f` (`Establish the RyeOS workspace visual system`).
- Node.js: `24.21.0` (LTS), Linux x64 archive SHA-256
  `fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6`.
- npm: `11.19.0`, bundled with that Node.js archive.
- npm lockfile SHA-256:
  `e93746f0349e9401562b0c49c1a8114fd126260c8d4dad02dc9565480cc79ebd`.
- Playwright: `1.63.0`, resolved by the exact lockfile.
- Playwright Chromium: revision `1243` (Chrome for Testing
  `153.0.8010.12`); headless shell revision `1243`; ffmpeg revision `1011`.
- Three.js: `0.128.0`; packaged `pkg/ryeos_three.js` SHA-256
  `af527c374b56b8688737a42d7fcea7cb8aaeb57a4e3c6da98b4dffd55bcc3514`.
- Retained Three.js license SHA-256:
  `7dddf7c5b8fd10ee654db8857d75d104b5557889aa5a91fc4ca545ea7c07062f`.
- Rust-generated fixture SHA-256:
  `e538e3e434737e4beb24f00ced666550939430ea5d852f816a33257f49098e2d`.
- Packaged `pkg/` asset-tree digest: SHA-256
  `cfa2e78effbf3a76c22784ccd53211f5bc9fcb937273fde4c2f41c515203e4f3`.
  This is the digest of the sorted `sha256sum` output for every regular file
  under `pkg/`, with paths relative to this package directory.

The fixture is produced by the checked-in
`ryeos-client-base` `workspace_visual_fixture` example. The renderer receives
a fixed ambient random source through its browser-service adapter and the
browser context enforces reduced motion. No semantic fixture is handwritten in
JavaScript.

## Screenshot evidence

The following SHA-256 values were reproduced byte-for-byte by two consecutive
runs using the exact inputs above:

| Capture | SHA-256 |
| --- | --- |
| `work.png` | `f6caf97b1d399d2706e650574fea83cafa59f664a2ba2ff8a956011be97764ef` |
| `overview.png` | `3782fbe4fd0481527f358678a61856955ced7a0f3de5110b4bed83399df0b120` |
| `launcher.png` | `26e5587f9e4ff824ddc122de0282b2a9d881976c42725d411f8852976ee9aca4` |
| `work-1024.png` | `aaa51b6e4a53ecfbb1c12127db933cc34146a591b4c6b08ae1f14720dfbc0578` |
| `work-390.png` | `5402bd5b50df26fc29dd972b217114eb8e6e970f0706ba11e0a2ff8e547a96f2` |

The qualification also checks native focus, exact binding-wire payloads,
semantic HTML, authored code/scene projections, narrow layout reachability,
non-overlapping input composition and reduced-motion behavior.

## Qualification boundary

These exact inputs were realized locally under `/tmp` to close and freeze the
source baseline. Artix requires Playwright's Ubuntu 24.04 fallback browser
build. This record therefore does **not** claim a host-independent or durable
RyeOS browser-build environment. That closure must be admitted and retained as
an ordinary RyeOS environment product before release qualification relies on
it.

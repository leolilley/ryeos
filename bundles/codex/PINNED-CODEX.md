# Pinned Codex contract

- Codex CLI/App Server: `0.147.0`
- target: `x86_64-unknown-linux-musl`
- official standalone-package SHA-256: `bd758d53d56e41dc65e045f4589df79a038ed197a011adcb52a258e6ad64cfda`
- executable SHA-256: `cb0a15567e9a60a5820d54b0f6ae86d504dc3805c1eab21a47f70e3eb7b73a40`
- code-mode host SHA-256: `00ecf5d040865b97884c488883abd342581c2a432debe7a54e4646bceee3d2d6`
- packaged Bubblewrap SHA-256: `77360cb751ccedc5971391444ac86a8a33c15b04d6b4a6fe45f5d25496e62c4c`
- packaged Zsh SHA-256: `67faaaa89242c4a332e16e508a1977cffc24bf7fca31d4411cdfd101f3831ef3`
- packaged rg SHA-256: `e62198eb19b136b88c330af83647b5a962cb99b6b1f066758568f12de1974849`
- generated stable-schema set: 285 files
- ordered schema-file checksum digest: `1f680f3a84e80b770fe163b7c031fb53d8d03c9692a27027537780cba4b9baf4`

The schema set is reproduced with:

```sh
codex app-server generate-json-schema --out schema/app-server-0.147.0
```

Do not pass `--experimental`. The structured-session bridge initializes without the
`experimentalApi` capability and rejects methods outside its closed stable
allowlist. The upstream protocol reference is the official
[Codex App Server documentation](https://developers.openai.com/codex/app-server/).

The standalone package's `codex-resources/bwrap`, bundled shell, and bundled
`rg` are part of the pinned Codex workload realization. Bubblewrap here is
Codex's own model-command sandbox resource, not RyeOS's optional isolation
backend and not a prerequisite imposed on the RyeOS host installation.

Codex 0.147 rejects a request-level granular `approvalPolicy` without the
experimental capability, and its exec boundary rejects an explicit sandbox
escalation when the immutable policy itself uses the granular variant. RyeOS
therefore pins `on-request` in immutable CLI configuration and omits the field
from `thread/start`, `thread/resume`, and `turn/start`. The admitted response
predicates require that exact policy, `ryeos-workspace-only`, user review, and
disabled command networking. Every mapped request remains deny-only at the
RyeOS boundary.

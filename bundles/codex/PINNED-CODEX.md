# Pinned Codex contract

- Codex CLI/App Server: `0.147.0`
- target: `x86_64-unknown-linux-musl`
- official archive SHA-256: `0246e2e773834e07f0fb5249ed6ebad12e4591e608f8c7bb97dd6a9690544c36`
- executable SHA-256: `cb0a15567e9a60a5820d54b0f6ae86d504dc3805c1eab21a47f70e3eb7b73a40`
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

Codex 0.147 rejects a request-level granular `approvalPolicy` without the
experimental capability. RyeOS therefore supplies the granular policy only in
the immutable CLI configuration and omits that field from `thread/start`,
`thread/resume`, and `turn/start`. The admitted response predicates still
require `ryeos-workspace-only`, user review, and disabled command networking.

# Render controller deployment

`controller.render.yaml.example` is the Render packaging for one RyeOS
controller. It pins the hosted-workflow image from the successful v0.5.102
release to `sha256:76845035deeefd312011f0792adbe85b0d23aac72307524284a5acb7586dd952`.
It is not a worker-runtime qualification or a grant of external execution
authority. Validate the Blueprint with
`render blueprints validate` and confirm the intended Render workspace. The
controller needs one persistent `/data` disk: its installed generation,
operator identity, keys, journal, and retained state must survive redeploys.
Do not first boot it as an ephemeral service.

The Render CLI currently validates Blueprints but does not apply them or attach
a persistent disk with `render services create`. Applying this Blueprint
requires the Render Dashboard or an authorized Render API operation. Record
the resulting service and disk IDs before proceeding. Deployment readiness at
`/_ryeos/ready` proves only controller admission readiness.

The first Blueprint sync on 2026-09-29 created service
`srv-datiea7avr4c73dm14fg` and 10 GB `/data` disk
`dsk-datiea7avr4c73dm14og` in Singapore. Deploy
`dep-datieb7avr4c73dm199g` finished `live` using the digest pinned above.
The image's amd64 manifest was
`sha256:918e198cee8386d57dacf68811179d893d939539f7cc1691ae13b67a91ae583e`.
First boot installed seven bundles and retained operator fingerprint
`73d6f82da3e34f061ee1f9a50aed461d5fe9f5aee0f89d7bff9fa3baf90c6a73`
and node fingerprint
`685322487706f6744d6f9d8aaac2aeb002519d8513aa9d7cd2741fc96f351822`.
The daemon logged startup ready and the public readiness endpoint returned
HTTP 200. No Codex turn or Render guest was launched by this deployment.

## Current qualification boundary

The signed `render-sandbox` provider specification still declares
`activate_supervisor` and `reconcile_supervisor_activation` as
`unsupported_pending`. This is deliberate: a controller image, Sandbox
allocation, or a successful command is not an authenticated guest-owner
startup. The installed path still needs the exact owner runtime product,
qualified snapshot and restored-content evidence, source-derived binding,
one-shot activation, authenticated supervisor `Ready`, frozen output with
writer exclusion, and independent candidate evaluation. See the adapter
README for the exact trust joins and refusal cases. Do not enable activation
by changing the spec alone.

On 2026-09-29 a disposable, deny-all-network Oregon Starter Sandbox
`sbx-18p4gdathrqpsrm7s738p3j2g` provided a narrow process-lifetime probe.
The background `sleep 120` at PID 312 survived the end of its launching
`render ea sandboxes exec` command. A foreground `sleep 120` at PID 315 was
still present after the local CLI session was interrupted with Ctrl-C and a
new exec connected. The Sandbox was then explicitly terminated. This is
evidence only about that CLI/proxy behavior, not a guarantee across arbitrary
network failure, a qualified snapshot, or an authenticated owner `Ready`.

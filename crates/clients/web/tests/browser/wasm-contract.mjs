import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const directory = path.dirname(fileURLToPath(import.meta.url));
const pkg = path.resolve(directory, "../../pkg");
const wasm = await import(path.join(pkg, "ryeos_web.js"));
await wasm.default({ module_or_path: await readFile(path.join(pkg, "ryeos_web_bg.wasm")) });

const beyondSafeInteger = 9_007_199_254_740_993n;
const envelope = wasm.ryeos_start({
  ui_binding_contract_revision: "ryeos.ui.binding.v4",
  session_id: "wasm-contract",
  surface_ref: "surface:ryeos/ryeos/base",
  user_principal_id: null,
  effective_surface: {
    name: "wasm-contract",
    slots: {},
    views: {},
  },
  project_path: null,
  binding_digest: "11".repeat(32),
  binding_request_bounds: {
    max_request_bytes: 65_536n,
    max_input_bytes: 16_384n,
  },
  posture: "observation_only",
  events_url: null,
}, {
  width: 1_280,
  height: 720,
  device_pixel_ratio: 1,
}, beyondSafeInteger);

assert.equal(typeof envelope.generation, "bigint", "u64 generations must cross WASM as BigInt");
assert.equal(typeof envelope.view_model.now_ms, "bigint", "u64 timestamps must cross WASM as BigInt");
assert.equal(envelope.view_model.now_ms, beyondSafeInteger, "WASM must retain integers above Number.MAX_SAFE_INTEGER exactly");
assert.ok(Array.isArray(envelope.effects), "vector fields must cross WASM as arrays");
for (const effect of envelope.effects) {
  assert.equal(typeof effect.id, "bigint", "effect identities must cross WASM as BigInt");
  assert.equal(typeof effect.kind?.type, "string", "tagged effect enums must retain their serde tag");
}
assert.ok(!Object.hasOwn(envelope.view_model, "tail_thread_id") || envelope.view_model.tail_thread_id === undefined,
  "omitted optional fields must not acquire browser defaults");

console.log(JSON.stringify({
  generationType: typeof envelope.generation,
  nowMsType: typeof envelope.view_model.now_ms,
  exactLargeInteger: envelope.view_model.now_ms === beyondSafeInteger,
  effects: envelope.effects.length,
}));

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const directory = path.dirname(fileURLToPath(import.meta.url));
const pkg = path.resolve(process.env.RYEOS_WEB_WASM_PKG || path.resolve(directory, "../../pkg"));
const wasm = await import(path.join(pkg, "ryeos_web.js"));
await wasm.default({ module_or_path: await readFile(path.join(pkg, "ryeos_web_bg.wasm")) });

const beyondSafeInteger = 9_007_199_254_740_993n;
const envelope = wasm.ryeos_start({
  ui_binding_contract_revision: "ryeos.ui.binding.v6",
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

function assertPlainData(value, path = "root") {
  assert.notEqual(value, undefined, `${path} must preserve JSON null instead of undefined`);
  if (value === null || typeof value !== "object") return;
  assert.ok(!(value instanceof Map), `${path} must not cross WASM as Map`);
  if (Array.isArray(value)) {
    value.forEach((item, index) => assertPlainData(item, `${path}[${index}]`));
    return;
  }
  assert.ok(Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null,
    `${path} must be a plain record`);
  for (const [key, item] of Object.entries(value)) assertPlainData(item, `${path}.${key}`);
}

assertPlainData(envelope);

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

const replaySeq = 9_007_199_254_740_999n;
const replayValue = {
  exact: 18_446_744_073_709_551_615n,
  fractional: 0.000001,
  nested: [null, { ["__proto__"]: "inert", emoji: "😀" }],
};
const replayEnvelope = wasm.ryeos_replay_seat_events([{
  event_type: "seat.facet",
  chain_seq: replaySeq,
  payload: {
    seq: replaySeq,
    payload: { key: "transport.populated", value: replayValue },
  },
}]);
assertPlainData(replayEnvelope, "replayEnvelope");
const replayed = wasm.ryeos_seat_events();
const populated = replayed.find((event) => event.payload?.key === "transport.populated");
assert.ok(populated, "populated replay facet must survive JS→WASM");
assert.equal(populated.seq, replaySeq, "replay sequence must remain an exact BigInt");
assert.equal(populated.payload.value.exact, replayValue.exact, "nested u64 must remain exact");
assert.equal(populated.payload.value.fractional, replayValue.fractional, "f64 must remain a number");
assert.equal(populated.payload.value.nested[0], null, "nested null must remain null");
assert.equal(populated.payload.value.nested[1].__proto__, "inert", "prototype-sensitive keys must be data");

console.log(JSON.stringify({
  generationType: typeof envelope.generation,
  nowMsType: typeof envelope.view_model.now_ms,
  exactLargeInteger: envelope.view_model.now_ms === beyondSafeInteger,
  plainObjectAbi: true,
  effects: envelope.effects.length,
  populatedReplay: true,
}));

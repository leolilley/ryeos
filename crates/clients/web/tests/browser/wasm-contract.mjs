import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const directory = path.dirname(fileURLToPath(import.meta.url));
const pkg = path.resolve(process.env.RYEOS_WEB_WASM_PKG || path.resolve(directory, "../../pkg"));
const wasm = await import(path.join(pkg, "ryeos_web.js"));
await wasm.default({ module_or_path: await readFile(path.join(pkg, "ryeos_web_bg.wasm")) });

const beyondSafeInteger = 9_007_199_254_740_993n;
const presentationId = "11111111-1111-4111-8111-111111111111";
const rootBinding = {
  binding_attachment_id: "wasm-shared-root",
  binding_generation: 1n,
  binding_digest: "11".repeat(32),
};
const session = {
  ui_binding_contract_revision: "ryeos.ui.binding.v10",
  session_id: "wasm-contract",
  user_principal_id: null,
  root_attachment_id: rootBinding.binding_attachment_id,
  binding_attachments: [{
    ...rootBinding,
    root_ref: "config:test/wasm-contract",
    root_generation: "22".repeat(32),
    presentation: {
      definition: { kind: "shared_presentation", config: {
        category: "ui", shared_presentation: {
          schema: 1, title: "WASM contract", launcher: "launch",
          dialogs: [{ id: "launch", title: "Launch", view: "view:test/launch" }],
          capabilities: { sources: [], affordances: [] },
        },
      } },
      views: { "view:test/launch": { widget: "rows" } },
    },
    project_path: null,
    subject: { kind: "projectless" },
    posture: "observation_only",
    binding_request_bounds: {
      max_request_bytes: 65_536n,
      max_input_bytes: 16_384n,
      max_seat_append_bytes: 65_536n,
    },
  }],
  presentations: [{
    presentation_id: presentationId,
    root_binding: rootBinding,
    thread_id: "wasm-seat",
    chain_root_id: "wasm-chain",
    next_mount_id: 1n,
    initialization_digest: "33".repeat(32),
    surfaces: [],
  }],
  events_url: null,
};
const envelope = wasm.ryeos_start(session, presentationId, {
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

// Mutable replay has no alternate ingress: use completed candidate staging
// or the exact canonical acknowledgement API, never a loose row vector.
assert.equal(wasm.ryeos_replay_seat_events, undefined);

const appendIdentityInput = {
  presentation_id: "22222222-2222-4222-8222-222222222222",
  chain_root_id: "retained-root", thread_id: "retained-seat", producer_incarnation: "ab".repeat(32),
  first_engine_seq: beyondSafeInteger, last_engine_seq: beyondSafeInteger + 1n, event_count: 2n,
  payload_digest: "cd".repeat(32),
};
const appendIdentity = wasm.ryeos_seat_append_identity(appendIdentityInput);
assert.match(appendIdentity, /^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
assert.equal(wasm.ryeos_seat_append_identity(appendIdentityInput), appendIdentity, "exact retry retains its shared Rust UUID");
assert.notEqual(wasm.ryeos_seat_append_identity({...appendIdentityInput, payload_digest:"ef".repeat(32)}), appendIdentity);
assert.throws(() => wasm.ryeos_seat_append_identity({...appendIdentityInput, event_count:3n}));

const controlIntent = {
  action:"claim", schema:2, operation_id:"", presentation_id:"22222222-2222-4222-8222-222222222222",
  thread_id:"retained-seat", chain_root_id:"retained-root",
  predecessor:{kind:"initialization",event_hash:"ab".repeat(32)},
  intent_nonce:"11111111-1111-4111-8111-111111111111",claim:{kind:"fresh"},
};
const controlId = wasm.ryeos_seat_control_identity(controlIntent);
assert.match(controlId, /^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
assert.equal(wasm.ryeos_seat_control_identity({...controlIntent,operation_id:controlId}),controlId);
assert.notEqual(wasm.ryeos_seat_control_identity({...controlIntent,intent_nonce:"33333333-3333-4333-8333-333333333333"}),controlId);
assert.throws(() => wasm.ryeos_seat_control_identity({...controlIntent,predecessor:{kind:"control",event_hash:"ab".repeat(32)}}));

console.log(JSON.stringify({
  generationType: typeof envelope.generation,
  nowMsType: typeof envelope.view_model.now_ms,
  exactLargeInteger: envelope.view_model.now_ms === beyondSafeInteger,
  plainObjectAbi: true,
  effects: envelope.effects.length,
  mutableReplayExportAbsent: true,
}));

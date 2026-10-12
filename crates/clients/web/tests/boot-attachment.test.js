import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

async function load(source) {
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  return import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
}

const transport = await load(await readFile(new URL("../browser/runtime/transport.ts", import.meta.url), "utf8"));
// Boot dependencies mount the renderer; this test exercises only the pure
// attachment decoder, with the real exact-integer transport implementation.
const boot = await load((await readFile(new URL("../browser/runtime/boot.ts", import.meta.url), "utf8"))
  .replace(/^import .*;\n/gm, "") + await readFile(new URL("../browser/runtime/attachment-subject.ts", import.meta.url), "utf8") + await readFile(new URL("../browser/runtime/presentation-document.js",import.meta.url),"utf8"));

test("boot source fences initialization and partial replacement before readiness", async () => {
  // Structural regression, not an executed WASM/browser bootstrap test.
  const source = await readFile(new URL("../browser/runtime/boot.ts", import.meta.url), "utf8");
  const install = source.slice(source.indexOf("function installCore("), source.indexOf("async function recoverLive("));
  assert.ok(install.includes("const completion = activeSession.currentReplayCompletion()"));
  assert.ok(install.includes("if (!completion) throw new Error"));
  assert.ok(install.indexOf("const initial = wasm.ryeos_commit_seat_replay(completion)") < install.indexOf("activeSession.completeEngineBootstrap()"));
  assert.ok(install.indexOf("runtime.enqueueEnvelope(initial)") < install.indexOf("activeSession.completeEngineBootstrap()"));
  assert.ok(!install.includes("ryeos_initial_effects"), "cold replay and initial fetches must use one startup envelope");
  assert.ok(install.indexOf("activeSession.completeEngineBootstrap()") < install.indexOf('type: "presentation_producer_ready"'));
  assert.ok(install.indexOf('type: "presentation_producer_ready"') < install.indexOf("wasm.ryeos_require_presentation_producer_ready("));
  assert.ok(install.indexOf("wasm.ryeos_require_presentation_producer_ready(") < install.indexOf("releaseEffects(true)"));
  assert.ok(install.includes("activeSession.currentEngineFrontier()"));
  assert.ok(install.includes("next_engine_seq: nextEngineSeq"));
  assert.ok(install.indexOf("activeSession.completeEngineBootstrap()") < install.indexOf("history.replaceState("));
  assert.ok(install.indexOf("runtime.enqueueEnvelope(initial)") < install.indexOf("history.replaceState("));
  assert.ok(install.indexOf("detachBrowserEvents = attachBrowserEvents(") < install.indexOf("history.replaceState("));
  assert.ok(source.includes("deriveSeatAppendIdentity: (identity) => wasm.ryeos_seat_append_identity(identity)"));
  assert.ok(source.includes("deriveSeatControlIdentity: (request) => wasm.ryeos_seat_control_identity(request)"));
  const recovery = source.slice(source.indexOf("async function recoverLive("), source.indexOf("sessionRuntime = createSessionRuntime("));
  const partial = recovery.slice(recovery.indexOf("// A partially installed Core"));
  assert.ok(partial.indexOf("await disposeInstalledCore()") >= 0 && partial.indexOf("await disposeInstalledCore()") < partial.indexOf("await awaitBootRecovery("));
  const disposal = source.slice(source.indexOf("async function disposeInstalledCore("), source.indexOf("async function recoverLive("));
  for (const cleanup of ["runtime?.close()", "releaseEffects(false)", "await renderer?.destroy()", "coreStarted = false"]) {
    assert.ok(disposal.includes(cleanup), `partial installation cleanup includes ${cleanup}`);
  }
});

test("installed presentation retains its hash but removes unrelated URL selectors", () => {
  for (const query of ["?source=arc", "?presentation_id=old&source=arc", "?presentation_id=old&source=arc&presentation_id=other"]) {
    const original = new URL(`http://127.0.0.1:7411/ui${query}#solve`);
    const next = boot.retainedPresentationUrl(original, "11111111-1111-4111-8111-111111111111");
    assert.deepEqual(next.searchParams.getAll("presentation_id"), ["11111111-1111-4111-8111-111111111111"]);
    assert.equal(next.searchParams.has("source"), false);
    assert.equal(next.hash, "#solve");
    assert.equal(original.search, query);
  }
});

function session(generation) {
  return transport.decodeJsonBody(`{"root_attachment_id":"surface","binding_attachments":[{"binding_attachment_id":"surface","binding_generation":${generation},"binding_digest":"digest","project_path":null,"subject":{"kind":"projectless"}}]}`);
}

test("boot preserves exact root attachment generations from HTTP", () => {
  for (const generation of ["1", "9007199254740993", "18446744073709551615"]) {
    assert.equal(boot.sessionRootAttachment(session(generation)).binding_generation, BigInt(generation));
  }
});

test("boot rejects invalid root attachment generations", () => {
  for (const generation of ["0", "-1", "1.5", '"1"']) {
    assert.throws(() => boot.sessionRootAttachment(session(generation)), /generation is invalid/);
  }
});

test("boot requires exactly one matching surface attachment", () => {
  const value = session("1");
  value.binding_attachments.push(value.binding_attachments[0]);
  assert.throws(() => boot.sessionRootAttachment(value), /exactly one root attachment/);
});

test("prepared link selects only an exact authenticated membership and binding", () => {
  const value = session("9007199254740993");
  value.presentations = [{
    presentation_id: "prepared", thread_id: "thread", chain_root_id: "chain",
    root_binding: { ...value.binding_attachments[0] }, surfaces: [],
  }];
  const selected = boot.preparedPresentationSelection(value, new URL("http://127.0.0.1:7411/ui?presentation_id=prepared"));
  assert.equal(selected.rootAttachment.binding_generation, 9007199254740993n);
  assert.deepEqual(selected.presentation, { presentation_id: "prepared", thread_id: "thread", chain_root_id: "chain" });
  value.presentations[0].root_binding.binding_generation = 1n;
  assert.throws(() => boot.preparedPresentationSelection(value, new URL("http://127.0.0.1:7411/ui?presentation_id=prepared")), /root binding is stale/);
});

test("prepared coordinates never fall back to another session membership", () => {
  const value = session("1");
  value.presentations = [];
  for (const query of ["?presentation_id=foreign", "?presentation_id=", "?presentation_id=one&presentation_id=two"]) {
    assert.throws(() => boot.preparedPresentationSelection(value, new URL(`http://127.0.0.1:7411/ui${query}`)));
  }
  assert.equal(boot.preparedPresentationSelection(value, new URL("http://127.0.0.1:7411/ui")), null);
});
test("boot requires selected retained root's positive signed append route bound", () => {
  const value = session("1");
  const attachment = value.binding_attachments[0];
  attachment.binding_request_bounds = {max_seat_append_bytes: 2000n};
  assert.equal(boot.sessionSeatAppendBound(value, "surface"), 2000n);
  for (const bound of [undefined, 0n, -1n, 9007199254740992n, 2000]) {
    attachment.binding_request_bounds.max_seat_append_bytes = bound;
    assert.throws(() => boot.sessionSeatAppendBound(value, "surface"), /byte bound is invalid/);
  }
  assert.throws(() => boot.sessionSeatAppendBound(value, "foreign"), /exactly one root attachment/);
});

test("boot requires closed subject metadata consistent with the admitted path", () => {
  const value = session("1"), attachment = value.binding_attachments[0];
  for (const subject of [undefined, null, {kind:"unknown"}, {kind:"projectless",local_id:"extra"}, {kind:"registered_project",local_id:"project"}]) {
    attachment.subject = subject;
    assert.throws(() => boot.sessionRootAttachment(value), /subject/);
  }
  attachment.project_path = "/retained/display/path";
  for (const subject of [{kind:"unregistered_project"},{kind:"registered_project",local_id:"arc"}]) {
    attachment.subject = subject;
    assert.equal(boot.sessionRootAttachment(value).binding_attachment_id, "surface");
  }
  for (const local_id of ["", " ", "arc\n", 1]) {
    attachment.subject = {kind:"registered_project",local_id};
    assert.throws(() => boot.sessionRootAttachment(value), /subject/);
  }
});

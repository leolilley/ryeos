import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const effects = await readFile(new URL("../browser/runtime/effects.ts", import.meta.url), "utf8");
const prepare = effects.slice(effects.indexOf('case "prepare_transfer":'), effects.indexOf('case "fetch_source":'));
const contact = effects.slice(effects.indexOf('case "contact_transfer":'), effects.indexOf('case "prepare_transfer":'));
const observation = effects.slice(effects.indexOf('case "observe_transfer_destination":'), effects.indexOf('case "contact_transfer":'));

test("external URL result reports dispatch only, not an observed opening", () => {
  const opening = effects.slice(effects.indexOf('case "open_url":'), effects.indexOf('case "replace_session":'));
  assert.match(opening, /window\.open\(kind\.url, "_blank", "noopener,noreferrer"\)/);
  assert.match(opening, /action: "open_url", dispatch: "returned", opened: "unknown"/);
  assert.doesNotMatch(opening, /opened:\s*(true|"success")|if\s*\(!.*open/);
});

test("destination observation sends only the exact seat locator, never a captured export", () => {
  assert.match(observation, /producer\.producer_incarnation !== kind\.source_producer\.producer_incarnation/);
  assert.match(observation, /action: "observe"/);
  assert.match(observation, /presentation_id: kind\.destination_presentation_id/);
  assert.match(observation, /thread_id: kind\.destination_thread_id/);
  assert.match(observation, /chain_root_id: kind\.destination_chain_root_id/);
  assert.match(observation, /operation_id: crypto\.randomUUID\(\), observation: unwrapResult\(response\)/);
  assert.doesNotMatch(observation, /source_export|export_digest|revision\s*:/);
  assert.ok(observation.indexOf("max_request_bytes") < observation.indexOf("await postEncodedJson"));
});

test("Phase contact uses the phase's exact actor and never constructs evidence in the renderer", () => {
  assert.match(contact, /request\.action === "commit" \|\| request\.action === "abort" \|\| request\.action === "cancel_freeze"/);
  assert.match(contact, /request\.transfer\.source : request\.transfer\.destination/);
  assert.match(contact, /producer\.presentation_id !== actor\.presentation_id/);
  assert.match(contact, /producer\.thread_id !== actor\.thread_id/);
  assert.doesNotMatch(contact, /producer\.producer_incarnation !== actor\.producer_incarnation/);
  assert.match(contact, /encodeJsonBody\(request\)/);
  assert.ok(contact.indexOf("max_request_bytes") < contact.indexOf("await postEncodedJson"));
  assert.doesNotMatch(contact, /dispatch\(|\.activate\(|\.release\(|event_hash\s*:|chain_seq\s*:/);
});

test("Prepare contact preserves the exact Core identity and bounds before sending", () => {
  assert.match(prepare, /producer\.presentation_id !== kind\.transfer\.source\.presentation_id/);
  assert.match(prepare, /producer\.thread_id !== kind\.transfer\.source\.thread_id/);
  assert.match(prepare, /producer\.producer_incarnation !== kind\.transfer\.source\.producer_incarnation/);
  assert.match(prepare, /export: kind\.export, freeze: kind\.freeze/);
  assert.match(prepare, /new TextEncoder\(\)\.encode\(encoded\)\.byteLength/);
  assert.ok(prepare.indexOf("max_request_bytes") < prepare.indexOf("await postEncodedJson"));
});

test("Prepare response is observation only, not a renderer-owned transfer transition", () => {
  assert.match(prepare, /success\(effect, "binding_invoked", unwrapResult\(response\)\)/);
  assert.doesNotMatch(prepare, /dispatch\(|\.activate\(|\.release\(|localStorage|sessionStorage/);
});

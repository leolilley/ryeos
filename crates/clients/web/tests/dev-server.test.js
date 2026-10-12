import assert from "node:assert/strict";
import test from "node:test";
import { developmentDocument, developmentUpstream } from "../dev-server.mjs";

test("development proxy only accepts an explicit loopback node origin", () => {
  assert.equal(developmentUpstream(), "http://127.0.0.1:7400");
  assert.equal(developmentUpstream("http://127.0.0.1:7445"), "http://127.0.0.1:7445");
  for (const value of ["https://example.com", "http://0.0.0.0:7400", "http://localhost:7400",
    "http://user:secret@127.0.0.1:7400", "http://127.0.0.1:7400/private", "http://127.0.0.1:7400/?token=x"]) {
    assert.throws(() => developmentUpstream(value), /loopback node origin/);
  }
});

test("development HTML selects source entry without loading installed renderer twice", () => {
  const html = developmentDocument('<link rel="stylesheet" href="/ui/assets/ryeos_ui.css"><div id="app"></div><script type="module" src="/ui/assets/ryeos_ui.js"></script>');
  assert.ok(html.includes('src="/browser/entry.ts"'));
  assert.ok(!html.includes("ryeos_ui.js"));
  assert.ok(!html.includes("ryeos_ui.css"));
  assert.throws(() => developmentDocument("unexpected document"), /expected renderer entry/);
});

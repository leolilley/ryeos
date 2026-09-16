import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("web seat opening carries no renderer-authored authority selectors", async () => {
  const shell = await readFile(new URL("../pkg/ryeos_shell.js", import.meta.url), "utf8");
  const openCall = shell.match(/invokeSeatService\("open",\s*(\{[^)]*\})\)/s);
  assert.ok(openCall, "seat open call must remain explicit");
  assert.equal(openCall[1].replace(/\s/g, ""), "{}");
});

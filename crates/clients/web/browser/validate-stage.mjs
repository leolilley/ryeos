import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const stage = path.resolve(process.env.RYEOS_UI_ASSET_STAGE || "../../../target/ui-browser-stage");
const files = (await readdir(stage)).sort();
const allowed = new Set(["ryeos_ui.css", "ryeos_ui.js"]);
for (const file of files) {
  assert.ok(allowed.has(file), `undeclared browser build output: ${file}`);
  assert.ok(!file.endsWith(".map"), `production source map is forbidden: ${file}`);
}
assert.ok(files.includes("ryeos_ui.js"), "browser build did not emit ryeos_ui.js");

for (const file of files.filter((name) => name.endsWith(".js"))) {
  const source = await readFile(path.join(stage, file), "utf8");
  assert.ok(!/^\s*(?:import|export)\s+(?:[^"']*?\s+from\s+)?["'](?![./]|\/ui\/assets\/)/m.test(source),
    `${file} contains an unresolved bare import`);
  assert.ok(!/\b(?:import|export)\s+(?:[^"']*?\s+from\s+)?["']https?:\/\//.test(source),
    `${file} contains an external runtime import`);
  assert.ok(!/import\s*\(\s*["'](?!\/ui\/assets\/ryeos_three\.js["'])/.test(source),
    `${file} contains an undeclared dynamic import`);
}

console.log(`validated browser stage: ${files.join(", ")}`);

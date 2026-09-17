import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const cases = [
  ["RyeOsEffectKind", "../../base/src/ui/effect.rs"],
  ["RyeOsEvent", "../../base/src/ui/event.rs"],
  ["RyeOsUiIntent", "../../base/src/ui/event.rs"],
  ["RyeOsUiEvent", "../../base/src/ui/event.rs"],
  ["RyeOsInputAction", "../../base/src/ui/event.rs"],
  ["RyeOsLayoutNodeVm", "../../base/src/ui/view_model.rs"],
  ["RyeOsViewVm", "../../base/src/ui/view_model.rs"],
];

const here = new URL(".", import.meta.url);
const ledger = await readFile(new URL("../docs/web-parity.md", here), "utf8");
const statuses = new Set(["native", "shared", "presentation", "neutral", "gap"]);

function rustVariants(source, enumName) {
  const marker = `pub enum ${enumName}`;
  const declaration = source.indexOf(marker);
  assert.notEqual(declaration, -1, `missing Rust enum ${enumName}`);
  const open = source.indexOf("{", declaration + marker.length);
  let depth = 1;
  let close = open + 1;
  for (; close < source.length && depth > 0; close += 1) {
    const ch = source[close];
    if (ch === "{") depth += 1;
    else if (ch === "}") depth -= 1;
  }

  const variants = [];
  depth = 0;
  for (const rawLine of source.slice(open + 1, close - 1).split("\n")) {
    const line = rawLine.replace(/\/\/.*$/, "");
    if (depth === 0) {
      const match = /^\s*([A-Z][A-Za-z0-9_]*)\s*(?:\{|,)/.exec(line);
      if (match) variants.push(match[1]);
    }
    for (const ch of line) {
      if (ch === "{") depth += 1;
      else if (ch === "}") depth -= 1;
    }
  }
  return variants;
}

function ledgerRows(enumName) {
  const start = `<!-- parity:${enumName}:start -->`;
  const end = `<!-- parity:${enumName}:end -->`;
  const first = ledger.indexOf(start);
  const last = ledger.indexOf(end);
  assert.notEqual(first, -1, `missing ledger start for ${enumName}`);
  assert.ok(last > first, `missing ledger end for ${enumName}`);
  const block = ledger.slice(first + start.length, last);
  assert.equal(block.includes("[ ]"), false, `${enumName} has an unchecked row`);
  const rows = [...block.matchAll(/^\| \[x\] `([A-Za-z0-9_]+)` \| ([^|]+) \| ([^|]+) \|/gm)];
  for (const row of rows) {
    assert.ok(statuses.has(row[2].trim()), `${enumName}.${row[1]} has invalid web status`);
    assert.ok(statuses.has(row[3].trim()), `${enumName}.${row[1]} has invalid TUI status`);
  }
  return rows.map((row) => row[1]);
}

for (const [enumName, relativeSource] of cases) {
  test(`${enumName} has an exact checked web/TUI inventory`, async () => {
    const source = await readFile(new URL(relativeSource, here), "utf8");
    const actual = rustVariants(source, enumName);
    const recorded = ledgerRows(enumName);
    assert.equal(new Set(recorded).size, recorded.length, `${enumName} has duplicate rows`);
    assert.deepEqual(recorded.sort(), actual.sort());
  });
}

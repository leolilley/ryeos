import assert from "node:assert/strict";
import { test } from "node:test";
import { indexedGridTranscriptPage } from "../browser/visuals/ryeos_grid_canvas.js";

test("indexed grid transcript pages every admitted cell without a 64-cell cutoff", () => {
  const preview = {
    label: "Observed board",
    grid: {
      width: 64,
      height: 64,
      cells: Array.from({ length: 4096 }, (_, offset) => offset % 2),
      palette: [
        { index: 0, glyph: ".", label: "empty" },
        { index: 1, glyph: "#", label: "wall" },
      ],
      changed: [4095],
    },
  };
  const first = indexedGridTranscriptPage(preview);
  assert.equal(first.rows.length, 16);
  assert.equal(first.rows[0].cells.length, 64);
  assert.equal(first.rows[0].cells[0], "column 1: empty");
  assert.equal(first.rows[0].cells[1], "column 2: wall");
  const last = indexedGridTranscriptPage(preview, 48);
  assert.equal(last.end, 64);
  assert.equal(last.rows.at(-1).number, 64);
  assert.equal(last.rows.at(-1).cells.at(-1), "column 64: wall (changed)");
  const heardCoordinates = new Set();
  for (let start = 0; start < 64; start += 16) {
    const page = indexedGridTranscriptPage(preview, start);
    for (const row of page.rows) {
      for (const [column, cell] of row.cells.entries()) {
        assert.match(cell, new RegExp(`^column ${column + 1}: `));
        heardCoordinates.add(`${row.number}:${column + 1}`);
      }
    }
  }
  assert.equal(heardCoordinates.size, 4096);
});

test("transcript refuses inconsistent grids and bounds each requested page", () => {
  assert.equal(indexedGridTranscriptPage({ grid: { width: 2, height: 2, cells: [0] } }), null);
  assert.equal(indexedGridTranscriptPage({ grid: { width: 513, height: 1, cells: Array(513).fill(0) } }), null);
  const preview = { grid: { width: 1, height: 40, cells: Array(40).fill(3), palette: [], changed: [] } };
  assert.equal(indexedGridTranscriptPage(preview, -50).start, 0);
  assert.equal(indexedGridTranscriptPage(preview, 0, 999).rows.length, 16);
  assert.equal(indexedGridTranscriptPage(preview, 39).rows[0].cells[0], "column 1: 3");
});

export function drawIndexedGrid(canvas, preview, options = {}) {
  const grid = preview?.grid;
  const context = canvas?.getContext?.("2d");
  if (!grid || !context || !grid.width || !grid.height) return false;
  const scale = Math.max(1, Number(options.scale || 10));
  canvas.width = grid.width * scale;
  canvas.height = grid.height * scale;
  const palette = new Map((grid.palette || []).map((entry) => [entry.index, entry]));
  const changed = new Set(grid.changed || []);
  for (let index = 0; index < grid.cells.length; index += 1) {
    const entry = palette.get(grid.cells[index]);
    if (!entry) continue;
    const x = (index % grid.width) * scale;
    const y = Math.floor(index / grid.width) * scale;
    context.fillStyle = entry.color || "#a89984";
    context.fillRect(x, y, scale, scale);
    if (entry.glyph && scale >= 7) {
      context.fillStyle = contrastingTextColor(entry.color);
      context.font = `${Math.max(6, Math.floor(scale * 0.72))}px monospace`;
      context.textAlign = "center";
      context.textBaseline = "middle";
      context.fillText(entry.glyph, x + scale / 2, y + scale / 2, scale);
    }
    if (changed.has(index)) {
      context.strokeStyle = options.changedColor || "#fb4934";
      context.lineWidth = Math.max(1, scale / 5);
      context.strokeRect(x + 1, y + 1, scale - 2, scale - 2);
    }
  }
  return true;
}

export function indexedGridAccessibilityLabel(preview, label = preview?.label || preview?.id || "Grid") {
  const grid = preview?.grid;
  if (!grid) return label;
  const glyphs = new Map((grid.palette || []).map((entry) => [entry.index, entry.glyph]));
  const cells = (grid.cells || []).slice(0, 64).map(
    (index) => glyphs.get(index) || String(index),
  );
  const suffix = (grid.cells || []).length > cells.length
    ? "; remaining cells available in the grid transcript" : "";
  return `${label}; ${grid.width} by ${grid.height} indexed grid; cells ${cells.join(" ")}${suffix}`;
}

// The canvas label is deliberately short. The complete admitted grid remains
// inspectable a bounded row page at a time, without a second data source.
export function indexedGridTranscriptPage(preview, requestedRow = 0, pageSize = 16) {
  const grid = preview?.grid;
  if (!grid || !Number.isInteger(grid.width) || !Number.isInteger(grid.height)
    || grid.width < 1 || grid.height < 1 || grid.width > 512 || grid.height > 512
    || !Array.isArray(grid.cells) || grid.cells.length > 262144
    || grid.cells.length !== grid.width * grid.height) return null;
  const size = Number.isInteger(pageSize) && pageSize > 0 ? Math.min(pageSize, 16) : 16;
  const start = Number.isInteger(requestedRow)
    ? Math.max(0, Math.min(requestedRow, grid.height - 1)) : 0;
  const end = Math.min(start + size, grid.height);
  const palette = new Map((grid.palette || []).map((entry) => [entry.index, entry]));
  const changed = new Set(grid.changed || []);
  const rows = [];
  for (let row = start; row < end; row += 1) {
    const cells = [];
    for (let column = 0; column < grid.width; column += 1) {
      const offset = row * grid.width + column;
      const index = grid.cells[offset];
      const entry = palette.get(index);
      const name = entry?.label || entry?.glyph || String(index);
      cells.push(`column ${column + 1}: ${name}${changed.has(offset) ? " (changed)" : ""}`);
    }
    rows.push({ number: row + 1, cells });
  }
  return { rows, start, end, height: grid.height };
}

function contrastingTextColor(color) {
  const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(color || "");
  if (!match) return "CanvasText";
  const luminance = (Number.parseInt(match[1], 16) * 299
    + Number.parseInt(match[2], 16) * 587
    + Number.parseInt(match[3], 16) * 114) / 1000;
  return luminance >= 140 ? "#111111" : "#ffffff";
}
export function comparableGrids(left, right) {
  if (!left?.comparison_key || left.comparison_key !== right?.comparison_key) return false;
  if (left.kind !== right?.kind) return false;
  const a = left.grid;
  const b = right.grid;
  if (!a || !b || a.width !== b.width || a.height !== b.height) return false;
  return paletteMeaning(a.palette).join("\0") === paletteMeaning(b.palette).join("\0");
}

function paletteMeaning(palette) {
  return (palette || []).map((entry) => `${entry.index}:${entry.glyph}:${entry.label || ""}`).sort();
}

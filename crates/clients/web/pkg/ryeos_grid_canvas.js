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
    if (changed.has(index)) {
      context.strokeStyle = options.changedColor || "#fb4934";
      context.lineWidth = Math.max(1, scale / 5);
      context.strokeRect(x + 1, y + 1, scale - 2, scale - 2);
    }
  }
  return true;
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

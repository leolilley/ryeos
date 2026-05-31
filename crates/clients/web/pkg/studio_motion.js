const MOTION_MS = 220;

export function captureWorkspaceMotion(root) {
  const appRect = root?.getBoundingClientRect?.();
  const tiles = new Map();
  if (!root || !appRect) return { appRect: null, tiles };
  root.querySelectorAll(".studio-workspace .studio-tile[data-tile-id]").forEach((tile) => {
    const tileId = tile.dataset.tileId;
    if (!tileId) return;
    const rect = tile.getBoundingClientRect();
    tiles.set(tileId, {
      rect,
      clone: tile.cloneNode(true),
    });
  });
  return { appRect, tiles };
}

export function applyWorkspaceMotion(root, snapshot, currentTileIds, motionEvents = []) {
  if (!root || !snapshot?.appRect) return;
  animateRetainedExits(root, snapshot, currentTileIds || new Set());
  animateSplitBeams(root, snapshot, motionEvents);
  animatePersistentTiles(root, snapshot);
}

function animateSplitBeams(root, snapshot, motionEvents) {
  const splitEvents = (motionEvents || []).filter((event) => event.type === "tile_split");
  const targets = splitEvents.length > 0
    ? splitEvents.map((event) => ({ tileId: event.new_tile_id, axis: event.axis }))
    : [...root.querySelectorAll(".studio-workspace .studio-tile[data-tile-id]")]
      .filter((tile) => !snapshot.tiles.has(tile.dataset.tileId || ""))
      .map((tile) => ({ tileId: tile.dataset.tileId || "", axis: null }));
  if (targets.length === 0 || snapshot.tiles.size === 0) return;

  const layer = document.createElement("div");
  layer.className = "studio-motion-layer beam-layer";
  layer.setAttribute("aria-hidden", "true");
  root.append(layer);

  for (const target of targets) {
    const tile = root.querySelector(`.studio-workspace .studio-tile[data-tile-id="${cssEscape(target.tileId)}"]`);
    if (!tile) continue;
    const rect = tile.getBoundingClientRect();
    const beam = document.createElement("i");
    const vertical = target.axis ? target.axis === "horizontal" : rect.height >= rect.width * 0.62;
    beam.className = `studio-split-beam ${vertical ? "vertical" : "horizontal"}`;
    if (vertical) {
      beam.style.left = `${rect.left - snapshot.appRect.left}px`;
      beam.style.top = `${rect.top - snapshot.appRect.top}px`;
      beam.style.height = `${rect.height}px`;
    } else {
      beam.style.left = `${rect.left - snapshot.appRect.left}px`;
      beam.style.top = `${rect.top - snapshot.appRect.top}px`;
      beam.style.width = `${rect.width}px`;
    }
    layer.append(beam);
  }

  window.setTimeout(() => layer.remove(), MOTION_MS + 80);
}

function cssEscape(value) {
  return window.CSS?.escape ? window.CSS.escape(value) : String(value).replace(/"/g, '\\"');
}

function animateRetainedExits(root, snapshot, currentTileIds) {
  const exits = [...snapshot.tiles.entries()].filter(([tileId]) => !currentTileIds.has(tileId));
  if (exits.length === 0) return;

  const layer = document.createElement("div");
  layer.className = "studio-motion-layer";
  layer.setAttribute("aria-hidden", "true");
  root.append(layer);

  for (const [tileId, old] of exits) {
    const clone = old.clone;
    const rect = old.rect;
    clone.dataset.tileId = tileId;
    clone.dataset.motion = "exit";
    clone.style.left = `${rect.left - snapshot.appRect.left}px`;
    clone.style.top = `${rect.top - snapshot.appRect.top}px`;
    clone.style.width = `${rect.width}px`;
    clone.style.height = `${rect.height}px`;
    layer.append(clone);
  }

  window.setTimeout(() => layer.remove(), MOTION_MS + 40);
}

function animatePersistentTiles(root, snapshot) {
  root.querySelectorAll(".studio-workspace .studio-tile[data-tile-id]").forEach((tile) => {
    const old = snapshot.tiles.get(tile.dataset.tileId || "");
    if (!old || tile.dataset.motion === "enter" || tile.dataset.motion === "split-enter") return;

    const next = tile.getBoundingClientRect();
    if (!next.width || !next.height || !old.rect.width || !old.rect.height) return;

    const dx = old.rect.left - next.left;
    const dy = old.rect.top - next.top;
    const sx = old.rect.width / next.width;
    const sy = old.rect.height / next.height;
    if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5 && Math.abs(sx - 1) < 0.01 && Math.abs(sy - 1) < 0.01) return;

    tile.animate(
      [
        { transform: `translate(${dx}px, ${dy}px) scale(${sx}, ${sy})` },
        { transform: "translate(0, 0) scale(1, 1)" },
      ],
      {
        duration: MOTION_MS,
        easing: "cubic-bezier(.2, .8, .1, 1)",
      },
    );
  });
}

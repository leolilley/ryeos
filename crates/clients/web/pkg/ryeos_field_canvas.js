import {
  StaleFieldLayoutError,
  fieldLayoutIsLarge,
  fieldLayoutMembershipKey,
  hitTest,
  hitTestGroup,
  layoutField,
  layoutFieldChunked,
  rebindFieldLayout,
  settleLayout,
} from "./ryeos_field_layout.js";

const TONES = {
  good: "#8ec07c",
  warn: "#fabd2f",
  danger: "#fb4934",
  accent: "#d65d0e",
  neutral: "#a89984",
};
const HIGH_CONTRAST_TONES = {
  good: "CanvasText",
  warn: "CanvasText",
  danger: "CanvasText",
  accent: "Highlight",
  neutral: "CanvasText",
};

export class FieldCanvasController {
  constructor(canvas, dispatchUi, instanceKey) {
    this.canvas = canvas;
    this.context = canvas.getContext?.("2d") || null;
    this.dispatchUi = dispatchUi;
    this.instanceKey = instanceKey;
    this.layout = emptyLayout();
    this.structuralRevision = null;
    this.viewport = { x: 20, y: 20, zoom: 1 };
    this.frame = null;
    this.drag = null;
    this.layoutGeneration = 0;
    this.layoutCount = 0;
    this.tombstones = new Map();
    this.unmounted = false;
    this.wireEvents();
  }

  update(vm) {
    this.vm = vm;
    this.captureExited(vm);
    const layoutRevision = `${vm.structural_revision || ""}\0${fieldLayoutMembershipKey(vm)}`;
    if (this.structuralRevision !== layoutRevision) {
      this.structuralRevision = layoutRevision;
      this.scheduleLayout(vm);
      return;
    }
    rebindFieldLayout(this.layout, vm);
    this.draw();
  }

  resize() {
    const rect = this.canvas.getBoundingClientRect?.() || { width: 640, height: 360 };
    const ratio = globalThis.devicePixelRatio || 1;
    const width = Math.max(1, Math.floor(rect.width * ratio));
    const height = Math.max(1, Math.floor(rect.height * ratio));
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
    this.draw();
  }

  draw() {
    const context = this.context;
    if (!context) return;
    const ratio = globalThis.devicePixelRatio || 1;
    const highContrast = preference("(forced-colors: active)");
    context.setTransform(
      ratio * this.viewport.zoom,
      0,
      0,
      ratio * this.viewport.zoom,
      ratio * this.viewport.x,
      ratio * this.viewport.y,
    );
    context.clearRect(
      -this.viewport.x / this.viewport.zoom,
      -this.viewport.y / this.viewport.zoom,
      this.canvas.width / ratio / this.viewport.zoom,
      this.canvas.height / ratio / this.viewport.zoom,
    );
    drawGroups(context, this.layout.groups, highContrast);
    const changes = new Map((this.vm?.changes || []).map((change) => [change.id, change]));
    drawRelations(context, this.layout.edges, changes, this.viewport.zoom, highContrast);
    for (const node of this.layout.nodes.values()) {
      drawEntity(context, node, changes.get(node.id), this.viewport.zoom, highContrast);
    }
    drawTombstones(context, this.tombstones, highContrast);
  }

  scheduleLayout(vm) {
    const token = ++this.layoutGeneration;
    const previous = new Map(this.layout.nodes);
    const commit = (layout) => {
      if (this.unmounted || token !== this.layoutGeneration) return;
      rebindFieldLayout(layout, this.vm);
      this.layout = layout;
      this.layoutCount += 1;
      this.animate(token);
    };
    if (!fieldLayoutIsLarge(vm)) {
      commit(layoutField(vm, previous));
      return;
    }
    layoutFieldChunked(vm, previous, {
      isStale: () => this.unmounted || token !== this.layoutGeneration,
    }).then(commit).catch((error) => {
      if (!(error instanceof StaleFieldLayoutError)) {
        this.structuralRevision = null;
        // A renderer failure must stay visible without poisoning shared state.
        this.canvas.dataset.layoutError = String(error?.message || error);
      }
    });
  }

  animate(token) {
    this.cancelAnimation();
    if (preference("(prefers-reduced-motion: reduce)")) {
      settleLayout(this.layout, 1);
      this.draw();
      this.tombstones.clear();
      return;
    }
    let remaining = this.tombstones.size ? 22 : 12;
    const tick = () => {
      if (this.unmounted || token !== this.layoutGeneration) return;
      settleLayout(this.layout, remaining <= 1 ? 1 : 0.24);
      fadeTombstones(this.tombstones, remaining);
      this.draw();
      remaining -= 1;
      this.frame = remaining > 0 && globalThis.requestAnimationFrame
        ? requestAnimationFrame(tick)
        : null;
    };
    tick();
  }

  captureExited(vm) {
    for (const change of vm.changes || []) {
      if (change.kind !== "exited" || !change.tombstone) continue;
      const prior = this.layout.nodes.get(change.id);
      if (!prior) continue;
      this.tombstones.set(change.id, {
        id: change.id,
        label: change.tombstone.label || change.id,
        traits: change.tombstone.traits || {},
        x: prior.x,
        y: prior.y,
        width: prior.width,
        height: prior.height,
        alpha: 0.72,
      });
    }
  }

  cancelAnimation() {
    if (this.frame != null && globalThis.cancelAnimationFrame) cancelAnimationFrame(this.frame);
    this.frame = null;
  }

  unmount() {
    this.unmounted = true;
    this.layoutGeneration += 1;
    this.cancelAnimation();
    this.canvas.onpointerdown = null;
    this.canvas.onpointermove = null;
    this.canvas.onpointerup = null;
    this.canvas.onpointercancel = null;
    this.canvas.onwheel = null;
    this.tombstones.clear();
  }

  wireEvents() {
    this.canvas.onpointerdown = (event) => {
      const point = this.fieldPoint(event);
      const hit = hitTest(this.layout, point.x, point.y);
      if (hit) {
        this.dispatchUi({
          type: "set_field_selection",
          instance_key: this.instanceKey,
          entity_id: hit.id,
        });
        if (event.shiftKey && canCompareEntity(this.vm, hit.id)) {
          this.dispatchUi({
            type: "toggle_field_compare",
            instance_key: this.instanceKey,
            entity_id: hit.id,
          });
        }
        if (event.detail >= 2 && hit.entity.activate_intent) {
          this.dispatchUi({ type: "activate", intent: hit.entity.activate_intent });
        }
        return;
      }
      const group = hitTestGroup(this.layout, point.x, point.y);
      if (group) {
        this.dispatchUi({
          type: "set_field_group_collapsed",
          instance_key: this.instanceKey,
          group_id: group.id,
          collapsed: !group.collapsed,
        });
        return;
      }
      this.drag = {
        pointerId: event.pointerId,
        clientX: event.clientX,
        clientY: event.clientY,
        viewportX: this.viewport.x,
        viewportY: this.viewport.y,
      };
      this.canvas.setPointerCapture?.(event.pointerId);
    };
    this.canvas.onpointermove = (event) => {
      if (!this.drag || this.drag.pointerId !== event.pointerId) return;
      this.viewport.x = this.drag.viewportX + event.clientX - this.drag.clientX;
      this.viewport.y = this.drag.viewportY + event.clientY - this.drag.clientY;
      this.draw();
    };
    const end = (event) => {
      if (this.drag?.pointerId === event.pointerId) this.drag = null;
    };
    this.canvas.onpointerup = end;
    this.canvas.onpointercancel = end;
    this.canvas.onwheel = (event) => {
      event.preventDefault();
      const rect = this.canvas.getBoundingClientRect();
      const screenX = event.clientX - rect.left;
      const screenY = event.clientY - rect.top;
      const fieldX = (screenX - this.viewport.x) / this.viewport.zoom;
      const fieldY = (screenY - this.viewport.y) / this.viewport.zoom;
      const zoom = Math.max(
        0.25,
        Math.min(3.5, this.viewport.zoom * Math.exp(-event.deltaY * 0.001)),
      );
      this.viewport.zoom = zoom;
      this.viewport.x = screenX - fieldX * zoom;
      this.viewport.y = screenY - fieldY * zoom;
      this.draw();
    };
  }

  fieldPoint(event) {
    const rect = this.canvas.getBoundingClientRect();
    return {
      x: (event.clientX - rect.left - this.viewport.x) / this.viewport.zoom,
      y: (event.clientY - rect.top - this.viewport.y) / this.viewport.zoom,
    };
  }
}

export function canCompareEntity(vm, entityId) {
  const entity = (vm?.entities || []).find((item) => item.id === entityId);
  if (!entity || !(entity.preview_ids || []).length) return false;
  if ((vm.compare || []).includes(entityId)) return true;
  const candidate = previewForEntity(vm, entityId);
  if (!candidate?.comparison_key) return false;
  const anchorId = (vm.compare || [])[0];
  if (!anchorId) return true;
  const anchor = previewForEntity(vm, anchorId);
  return comparablePreviews(anchor, candidate);
}

function previewForEntity(vm, entityId) {
  const entity = (vm.entities || []).find((item) => item.id === entityId);
  return (entity?.preview_ids || [])
    .map((id) => (vm.previews || []).find((preview) => preview.id === id))
    .find((preview) => preview?.grid);
}

function comparablePreviews(left, right) {
  if (!left || !right || !left.grid || !right.grid) return false;
  return left.comparison_key === right.comparison_key
    && left.kind === right.kind
    && left.grid.width === right.grid.width
    && left.grid.height === right.grid.height
    && JSON.stringify(left.grid.palette || []) === JSON.stringify(right.grid.palette || []);
}

function drawGroups(context, groups, highContrast) {
  context.font = "12px ui-monospace, monospace";
  for (const group of groups) {
    context.strokeStyle = highContrast ? "CanvasText" : "rgba(168,153,132,.52)";
    context.setLineDash([6, 5]);
    context.strokeRect(group.x, group.y, group.width, group.height);
    context.setLineDash([]);
    context.fillStyle = highContrast ? "Canvas" : "rgba(29,32,33,.94)";
    context.fillRect(group.x, group.y, group.width, 26);
    context.fillStyle = highContrast ? "CanvasText" : "#a89984";
    context.textAlign = "left";
    context.textBaseline = "alphabetic";
    context.fillText(
      `${group.collapsed ? "▸" : "▾"} ${group.label || group.id}`,
      group.x + 8,
      group.y + 17,
    );
  }
}

function drawRelations(context, edges, changes, zoom, highContrast) {
  for (const edge of edges) {
    if (edge.points.length < 2) continue;
    const relation = edge.relation;
    if (zoom < 0.4
      && !relation.selected
      && relation.emphasis !== "strong"
      && relation.motion !== "flow") continue;
    const tones = highContrast ? HIGH_CONTRAST_TONES : TONES;
    context.strokeStyle = tones[relation.tone] || tones.neutral;
    const changed = changes.get(relation.id);
    context.lineWidth = changed
      ? 4
      : relation.emphasis === "strong" ? 2.5 : relation.emphasis === "quiet" ? 0.7 : 1.3;
    context.setLineDash(relation.stroke === "dashed"
      ? [7, 5]
      : relation.stroke === "dotted" ? [2, 4] : []);
    context.beginPath();
    edge.points.forEach(([x, y], index) => (
      index ? context.lineTo(x, y) : context.moveTo(x, y)
    ));
    context.stroke();
  }
  context.setLineDash([]);
}

function drawEntity(context, node, change, zoom, highContrast) {
  const entity = node.entity;
  const tones = highContrast ? HIGH_CONTRAST_TONES : TONES;
  const tone = tones[entity.tone] || tones.neutral;
  context.save();
  context.translate(node.x, node.y);
  context.fillStyle = entity.traits?.fill === "hollow"
    ? (highContrast ? "Canvas" : "#1d2021")
    : tone;
  context.strokeStyle = entity.selected ? (highContrast ? "Highlight" : "#ebdbb2") : tone;
  context.lineWidth = entity.selected ? 4 : 2;
  context.setLineDash(entity.traits?.stroke === "dashed"
    ? [7, 4]
    : entity.traits?.stroke === "dotted" ? [2, 3] : []);
  if (change) {
    context.save();
    context.globalAlpha = 0.32;
    context.strokeStyle = tone;
    context.lineWidth = 8;
    entityPath(context, entity.traits?.shape, node.width + 14, node.height + 14);
    context.stroke();
    context.restore();
  }
  const width = zoom < 0.4 ? Math.min(36, node.width) : node.width;
  const height = zoom < 0.4 ? Math.min(24, node.height) : node.height;
  entityPath(context, entity.traits?.shape, width, height);
  context.fill();
  context.stroke();
  context.setLineDash([]);
  if (zoom >= 0.55) {
    if (node.componentSize > 1) {
      context.fillStyle = highContrast ? "CanvasText" : "#fabd2f";
      context.fillText(`↻${node.componentSize}`, node.width / 2 - 24, -node.height / 2 + 13);
    }
    context.fillStyle = entity.traits?.fill === "hollow"
      ? (highContrast ? "CanvasText" : "#ebdbb2")
      : (highContrast ? "Canvas" : "#1d2021");
    context.textAlign = "center";
    context.textBaseline = "middle";
    context.font = "600 12px ui-monospace, monospace";
    context.fillText(clipLabel(entity.label || entity.id, zoom >= 1.25 ? 34 : 20), 0, -4);
    if (zoom >= 0.9) {
      context.font = "10px ui-monospace, monospace";
      context.fillText(entity.status || entity.kind || "", 0, 12);
    }
  }
  context.restore();
}

function drawTombstones(context, tombstones, highContrast) {
  for (const tombstone of tombstones.values()) {
    context.save();
    context.globalAlpha = tombstone.alpha;
    context.translate(tombstone.x, tombstone.y);
    context.strokeStyle = highContrast ? "CanvasText" : TONES.neutral;
    context.fillStyle = highContrast ? "Canvas" : "#1d2021";
    context.setLineDash([4, 5]);
    entityPath(
      context,
      tombstone.traits?.shape,
      tombstone.width,
      tombstone.height,
    );
    context.fill();
    context.stroke();
    context.fillStyle = highContrast ? "CanvasText" : "#a89984";
    context.font = "11px ui-monospace, monospace";
    context.textAlign = "center";
    context.fillText(clipLabel(tombstone.label, 20), 0, 3);
    context.restore();
  }
}

function fadeTombstones(tombstones, remaining) {
  for (const [id, tombstone] of tombstones) {
    tombstone.alpha = Math.min(0.72, remaining / 22);
    if (remaining <= 1) tombstones.delete(id);
  }
}

function entityPath(context, shape, width, height) {
  context.beginPath();
  if (shape === "disc" || shape === "ring" || shape === "dot") {
    context.ellipse(0, 0, width / 2, height / 2, 0, 0, Math.PI * 2);
  } else if (shape === "diamond") {
    context.moveTo(0, -height / 2);
    context.lineTo(width / 2, 0);
    context.lineTo(0, height / 2);
    context.lineTo(-width / 2, 0);
    context.closePath();
  } else if (shape === "hex") {
    for (let index = 0; index < 6; index += 1) {
      const angle = Math.PI / 3 * index;
      const x = Math.cos(angle) * width / 2;
      const y = Math.sin(angle) * height / 2;
      if (index) context.lineTo(x, y);
      else context.moveTo(x, y);
    }
    context.closePath();
  } else {
    context.roundRect(
      -width / 2,
      -height / 2,
      width,
      height,
      shape === "capsule" ? height / 2 : 7,
    );
  }
}

function emptyLayout() {
  return {
    nodes: new Map(),
    edges: [],
    groups: [],
    groupDefinitions: [],
    width: 640,
    height: 360,
  };
}

function preference(query) {
  return !!globalThis.matchMedia?.(query)?.matches;
}

function clipLabel(value, length) {
  return value.length <= length ? value : `${value.slice(0, length - 1)}…`;
}

import { FieldCanvasController, canCompareEntity } from "/ui/assets/ryeos_field_canvas.js";
import {
  fieldPreferenceModel,
  mountFieldAccessibility,
} from "/ui/assets/ryeos_field_accessibility.js";
import { drawIndexedGrid } from "/ui/assets/ryeos_grid_canvas.js";

const controllers = new Map();
let frameSeen = new Set();

export function beginFieldFrame() {
  frameSeen = new Set();
}

export function endFieldFrame() {
  for (const [key, controller] of controllers) {
    if (frameSeen.has(key)) continue;
    controller.unmount();
    controllers.delete(key);
  }
}

export function fieldComponent(vm, instanceKey, dispatchUi) {
  const key = `${instanceKey}\0${vm.id}`;
  frameSeen.add(key);
  let controller = controllers.get(key);
  if (!controller) {
    controller = new FieldComponentController(instanceKey, dispatchUi);
    controllers.set(key, controller);
  }
  controller.attach(dispatchUi);
  controller.update(vm);
  return controller.root;
}

export function fieldControllerCount() {
  return controllers.size;
}

class FieldComponentController {
  constructor(instanceKey, dispatchUi) {
    this.instanceKey = instanceKey;
    this.dispatchUi = dispatchUi;
    this.root = document.createElement("section");
    this.root.className = "ryeos-field";
    this.toolbar = document.createElement("header");
    this.toolbar.className = "ryeos-field-toolbar";
    this.heading = document.createElement("div");
    this.heading.className = "ryeos-field-heading";
    this.title = document.createElement("strong");
    this.health = document.createElement("span");
    this.replayMode = document.createElement("span");
    this.heading.append(this.title, this.health, this.replayMode);
    this.controls = document.createElement("div");
    this.controls.className = "ryeos-field-controls";
    this.search = document.createElement("input");
    this.search.type = "search";
    this.search.placeholder = "Search field";
    this.search.setAttribute("aria-label", "Search field entities");
    this.search.addEventListener("input", () => this.dispatch({
      type: "set_field_query", query: this.search.value,
    }));
    this.search.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
      event.preventDefault();
      this.dispatch({
        type: "move_field_search_match",
        delta: event.key === "ArrowUp" ? -1 : 1,
      });
    });
    this.toolbar.append(this.heading, this.controls, this.search);
    this.rail = document.createElement("nav");
    this.rail.className = "ryeos-field-rail";
    this.rail.setAttribute("aria-label", "Durable execution events");
    this.canvas = document.createElement("canvas");
    this.canvas.className = "ryeos-field-canvas";
    this.canvas.setAttribute("aria-hidden", "true");
    this.accessibility = document.createElement("div");
    this.detail = document.createElement("aside");
    this.detail.className = "ryeos-field-detail";
    this.canvasController = new FieldCanvasController(this.canvas, dispatchUi, instanceKey);
    this.root.append(this.toolbar, this.rail, this.canvas, this.detail, this.accessibility);
    this.resizeObserver = globalThis.ResizeObserver
      ? new ResizeObserver(() => this.canvasController.resize())
      : null;
    this.resizeObserver?.observe(this.canvas);
  }

  attach(dispatchUi) {
    this.dispatchUi = dispatchUi;
    this.canvasController.dispatchUi = dispatchUi;
  }

  update(vm) {
    this.vm = vm;
    this.root.dataset.fieldId = vm.id || "";
    this.root.dataset.revision = vm.revision || "";
    const preferences = fieldPreferenceModel();
    this.root.dataset.reducedMotion = preferences.reducedMotion ? "true" : "false";
    this.root.dataset.highContrast = preferences.highContrast ? "true" : "false";
    this.title.textContent = vm.title || "Field";
    this.health.textContent = (vm.sources || []).map((source) => `${source.name}:${source.phase}`).join(" · ");
    this.replayMode.textContent = vm.replay?.mode === "braid_cut"
      ? `CUT ${vm.replay?.anchor?.chain_seq ?? ""}`
      : "LIVE";
    if (document.activeElement !== this.search && this.search.value !== (vm.search?.query || "")) {
      this.search.value = vm.search?.query || "";
    }
    this.renderControls();
    this.renderRail();
    this.renderDetail();
    this.canvasController.update(vm);
    this.canvasController.resize();
    mountFieldAccessibility(this.accessibility, vm, this.instanceKey, this.dispatchUi);
  }

  dispatch(event) {
    this.dispatchUi({ ...event, instance_key: this.instanceKey });
  }

  renderControls() {
    const vm = this.vm;
    this.controls.replaceChildren();
    this.controls.append(
      button("◀", "Previous event", !vm.replay?.previous, () => this.dispatch({
        type: "step_field_cursor", direction: "previous",
      })),
      button(vm.replay?.playing ? "Pause" : "Play", "Play or pause replay", !vm.replay?.playing && !vm.replay?.next, () => this.dispatch({
        type: "set_field_playback", playing: !vm.replay?.playing,
      })),
      button("▶", "Next event", !vm.replay?.next, () => this.dispatch({
        type: "step_field_cursor", direction: "next",
      })),
      button("Live", "Return to live head", vm.replay?.mode === "live", () => this.dispatch({
        type: "step_field_cursor", direction: "live",
      })),
    );
    for (const group of vm.groups || []) {
      this.controls.append(button(
        `${group.collapsed ? "▸" : "▾"} ${group.label}`,
        `${group.collapsed ? "Expand" : "Collapse"} ${group.label}`,
        false,
        () => this.dispatch({
          type: "set_field_group_collapsed",
          group_id: group.id,
          collapsed: !group.collapsed,
        }),
      ));
    }
    for (const layer of vm.layers || []) {
      this.controls.append(button(
        layer.visible ? `● ${layer.label}` : `○ ${layer.label}`,
        `${layer.visible ? "Hide" : "Show"} ${layer.label} layer`,
        false,
        () => this.dispatch({
          type: "set_field_layer_visible", layer_id: layer.id, visible: !layer.visible,
        }),
      ));
    }
    const selected = (vm.entities || []).find((entity) => entity.id === vm.selected);
    if (selected) {
      this.controls.append(button(
        (vm.compare || []).includes(selected.id) ? "Uncompare" : "Compare",
        "Toggle selected preview comparison",
        !canCompareEntity(vm, selected.id),
        () => this.dispatch({ type: "toggle_field_compare", entity_id: selected.id }),
      ));
      const expansion = (vm.expansions || []).find(
        (item) => item.source === selected.source && item.root_id === selected.id,
      );
      this.controls.append(button(
        expansion?.can_continue ? "Continue" : expansion ? "Expanded" : "Expand",
        "Request bounded expansion",
        !!expansion && !expansion.can_continue,
        () => this.dispatch({
          type: expansion?.can_continue ? "continue_field_expansion" : "request_field_expansion",
          source: selected.source,
          root_id: selected.id,
        }),
      ));
      if (expansion) this.controls.append(button("Clear", "Clear bounded expansion", false, () => this.dispatch({
        type: "clear_field_expansion", source: selected.source, root_id: selected.id,
      })));
    }
    if ((vm.search?.match_ids || []).length) {
      this.controls.append(
        button("↑", "Previous search match", false, () => this.dispatch({
          type: "move_field_search_match", delta: -1,
        })),
        button("↓", "Next search match", false, () => this.dispatch({
          type: "move_field_search_match", delta: 1,
        })),
      );
    }
  }

  renderRail() {
    this.rail.replaceChildren();
    const entries = this.vm.replay?.rail || [];
    this.rail.hidden = entries.length === 0;
    for (const entry of entries) {
      const node = button(
        entry.label || `event ${entry.event?.chain_seq ?? ""}`,
        `Replay through durable event ${entry.event?.chain_seq ?? ""}`,
        false,
        () => this.dispatch({
          type: "set_field_cursor",
          cursor: { mode: "braid_cut", anchor: entry.event },
        }),
      );
      node.className = "ryeos-field-rail-event";
      node.dataset.chainSeq = String(entry.event?.chain_seq ?? "");
      node.setAttribute("aria-pressed", entry.selected ? "true" : "false");
      if (entry.selected) node.classList.add("selected");
      this.rail.append(node);
    }
  }

  renderDetail() {
    const vm = this.vm;
    this.detail.replaceChildren();
    const selected = (vm.entities || []).find((entity) => entity.id === vm.selected);
    if (!selected) {
      this.detail.hidden = true;
      return;
    }
    this.detail.hidden = false;
    const heading = document.createElement("strong");
    heading.textContent = selected.label || selected.id;
    const meta = document.createElement("small");
    meta.textContent = [selected.kind, selected.status, selected.source].filter(Boolean).join(" · ");
    this.detail.append(heading, meta);
    const previewIds = new Set([...(selected.preview_ids || []), ...(vm.compare || []).flatMap((id) =>
      (vm.entities || []).find((entity) => entity.id === id)?.preview_ids || [])]);
    for (const preview of (vm.previews || []).filter((item) => previewIds.has(item.id))) {
      const figure = document.createElement("figure");
      const caption = document.createElement("figcaption");
      caption.textContent = preview.label || preview.id;
      const canvas = document.createElement("canvas");
      canvas.setAttribute("aria-label", caption.textContent);
      drawIndexedGrid(canvas, preview, { scale: 9 });
      canvas.title = "Shift-click to add this artifact to comparison";
      canvas.addEventListener("click", (event) => {
        if (!event.shiftKey || !canCompareEntity(vm, selected.id)) return;
        this.dispatch({ type: "toggle_field_compare", entity_id: selected.id });
      });
      figure.append(caption, canvas);
      this.detail.append(figure);
    }
    const change = (vm.changes || []).find((item) => item.id === selected.id);
    if (change) {
      const status = document.createElement("small");
      status.className = "ryeos-field-change";
      status.textContent = change.kind.replaceAll("_", " ");
      this.detail.append(status);
    }
    if ((vm.warnings || []).length) {
      const warning = document.createElement("small");
      warning.className = "ryeos-field-warning";
      warning.textContent = vm.warnings[0];
      this.detail.append(warning);
    }
  }

  unmount() {
    this.resizeObserver?.disconnect();
    this.canvasController.unmount();
    this.root.remove();
  }
}

function button(label, title, disabled, activate) {
  const node = document.createElement("button");
  node.type = "button";
  node.textContent = label;
  node.title = title;
  node.disabled = !!disabled;
  node.addEventListener("click", activate);
  return node;
}

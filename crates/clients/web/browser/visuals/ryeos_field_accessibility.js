import { canCompareEntity } from "./ryeos_field_canvas.js";

export function fieldAccessibilityModel(vm) {
  const byId = new Map((vm.entities || []).map((entity) => [entity.id, entity]));
  const groups = new Map((vm.groups || []).map((group) => [group.id, group]));
  const neighbors = new Map((vm.entities || []).map((entity) => [entity.id, []]));
  for (const relation of vm.relations || []) {
    if (neighbors.has(relation.source_id) && byId.has(relation.target_id)) {
      neighbors
        .get(relation.source_id)
        .push(`${relation.kind} to ${byId.get(relation.target_id).label}`);
    }
    if (neighbors.has(relation.target_id) && byId.has(relation.source_id)) {
      neighbors
        .get(relation.target_id)
        .push(`${relation.kind} from ${byId.get(relation.source_id).label}`);
    }
  }
  const ordered = (vm.traversal || []).map((id) => byId.get(id)).filter(Boolean);
  return ordered.map((entity, index) => {
    const group = entity.group_id ? groups.get(entity.group_id) : null;
    return {
      id: entity.id,
      domId: `ryeos-field-option-${safeId(entity.id)}`,
      label: entity.accessibility_label || entity.label || entity.id,
      selected: vm.selected === entity.id,
      position: index + 1,
      size: ordered.length,
      level: entityLevel(entity, byId),
      groupId: group?.id || null,
      groupLabel: group?.label || null,
      expanded: group ? !group.collapsed : null,
      neighbors: (neighbors.get(entity.id) || []).sort().join("; "),
      selectIntent: entity.select_intent || null,
      activateIntent: entity.activate_intent || null,
      compare: canCompareEntity(vm, entity.id),
    };
  });
}

export function fieldPreferenceModel(matchMedia = globalThis.matchMedia) {
  const matches = (query) => !!matchMedia?.(query)?.matches;
  return {
    reducedMotion: matches("(prefers-reduced-motion: reduce)"),
    highContrast: matches("(forced-colors: active)"),
  };
}

export function mountFieldAccessibility(host, vm, instanceKey, dispatchUi) {
  host.replaceChildren();
  const model = fieldAccessibilityModel(vm);
  const preferences = fieldPreferenceModel();
  host.className = "ryeos-field-a11y";
  host.tabIndex = 0;
  host.dataset.reducedMotion = preferences.reducedMotion ? "true" : "false";
  host.dataset.highContrast = preferences.highContrast ? "true" : "false";
  host.setAttribute("role", "listbox");
  host.setAttribute("aria-label", `${vm.title || "Field"} entities`);
  const selected = model.find((item) => item.selected) || model[0];
  if (selected) host.setAttribute("aria-activedescendant", selected.domId);
  else host.removeAttribute("aria-activedescendant");

  for (const item of model) {
    const option = document.createElement("div");
    option.id = item.domId;
    option.tabIndex = -1;
    option.dataset.entityId = item.id;
    if (item.groupId) option.dataset.groupId = item.groupId;
    option.setAttribute("role", "option");
    option.setAttribute("aria-selected", item.selected ? "true" : "false");
    option.setAttribute("aria-posinset", String(item.position));
    option.setAttribute("aria-setsize", String(item.size));
    option.setAttribute("aria-level", String(item.level));
    if (item.expanded != null) {
      option.setAttribute("aria-expanded", item.expanded ? "true" : "false");
    }
    const groupLabel = item.groupLabel ? `Group ${item.groupLabel}. ` : "";
    option.textContent = item.neighbors
      ? `${groupLabel}${item.label}. ${item.neighbors}`
      : `${groupLabel}${item.label}`;
    option.addEventListener("click", () => select(item));
    option.addEventListener("dblclick", () => activate(item));
    host.append(option);
  }

  host.onfocus = () => {
    // Restoring focus after a shell render must be silent when the shared VM
    // already owns a selection. Re-dispatching that same selection causes a
    // render -> refocus -> dispatch loop for as long as the listbox is focused.
    if (!model.some((item) => item.selected) && model[0]) select(model[0]);
  };
  host.onkeydown = (event) => {
    const currentIndex = Math.max(0, model.findIndex((item) => item.selected));
    const current = model[currentIndex];
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const delta = event.key === "ArrowDown" ? 1 : -1;
      const next = model[Math.max(0, Math.min(model.length - 1, currentIndex + delta))];
      if (next) select(next);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      const next = event.key === "Home" ? model[0] : model.at(-1);
      if (next) select(next);
    } else if ((event.key === "ArrowLeft" || event.key === "ArrowRight") && current?.groupId) {
      const collapsed = event.key === "ArrowLeft";
      if (current.expanded === collapsed) {
        event.preventDefault();
        dispatchUi({
          type: "set_field_group_collapsed",
          instance_key: instanceKey,
          group_id: current.groupId,
          collapsed,
        });
      }
    } else if (event.key === "Enter") {
      event.preventDefault();
      if (current) activate(current);
    } else if (event.key === " " && current?.compare) {
      event.preventDefault();
      select(current);
      dispatchUi({
        type: "toggle_field_compare",
        instance_key: instanceKey,
        entity_id: current.id,
      });
    }
  };

  function select(item) {
    dispatchUi({
      type: "set_field_selection",
      instance_key: instanceKey,
      entity_id: item.id,
    });
  }

  function activate(item) {
    select(item);
    if (item.activateIntent) dispatchUi({ type: "activate", intent: item.activateIntent });
  }
}

function entityLevel(entity, byId) {
  let level = 1;
  let parentId = entity.parent_id;
  const visited = new Set([entity.id]);
  while (parentId && byId.has(parentId) && !visited.has(parentId)) {
    visited.add(parentId);
    level += 1;
    parentId = byId.get(parentId).parent_id;
  }
  return level;
}

function safeId(value) {
  return [...String(value)].map((character) => (
    /[a-zA-Z0-9-]/.test(character)
      ? character
      : `_${character.codePointAt(0).toString(16)}_`
  )).join("");
}

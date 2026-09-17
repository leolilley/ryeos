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
      compare: entity.compare_available,
    };
  });
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

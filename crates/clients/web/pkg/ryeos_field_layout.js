// Deterministic compound layered geometry for the generic field VM.
// Semantic rank/lane/group drive placement; stable IDs break every tie.

const GROUP_WIDTH = 520;
const RANK_SPACING = 210;
const GROUP_RIGHT_GUTTER = 300;
const GROUP_HEADER_HEIGHT = 26;
const LARGE_FIELD_ENTITY_COUNT = 240;
const LARGE_FIELD_RELATION_COUNT = 720;

export class StaleFieldLayoutError extends Error {
  constructor() {
    super("field layout superseded");
    this.name = "StaleFieldLayoutError";
  }
}

export function layoutField(vm, previous = new Map()) {
  const prepared = prepareField(vm);
  const components = stronglyConnectedComponents(prepared.ids, prepared.relations);
  return placeField(vm, prepared, components, previous);
}

// Large layouts yield at semantic phase boundaries. Every continuation checks
// the controller's structural-revision token, so obsolete work never commits.
export async function layoutFieldChunked(
  vm,
  previous = new Map(),
  { schedule = nextFrame, isStale = () => false } = {},
) {
  const prepared = prepareField(vm);
  if (!isLarge(prepared)) return layoutField(vm, previous);
  await schedule();
  if (isStale()) throw new StaleFieldLayoutError();
  const components = stronglyConnectedComponents(prepared.ids, prepared.relations);
  await schedule();
  if (isStale()) throw new StaleFieldLayoutError();
  const layout = placeField(vm, prepared, components, previous);
  if (isStale()) throw new StaleFieldLayoutError();
  return layout;
}

export function settleLayout(layout, amount = 1) {
  for (const node of layout.nodes.values()) {
    node.x += (node.targetX - node.x) * amount;
    node.y += (node.targetY - node.y) * amount;
  }
  for (const edge of layout.edges) {
    edge.points = routeEdge(
      layout.nodes.get(edge.relation.source_id),
      layout.nodes.get(edge.relation.target_id),
    );
  }
  layout.groups = groupBounds(layout.groupDefinitions, layout.nodes, layout.groupOrigins);
  return layout;
}

export function hitTest(layout, x, y) {
  const nodes = [...layout.nodes.values()].reverse();
  return nodes.find(
    (node) => Math.abs(x - node.x) <= node.width / 2
      && Math.abs(y - node.y) <= node.height / 2,
  ) || null;
}

export function hitTestGroup(layout, x, y) {
  return [...layout.groups].reverse().find((group) => (
    x >= group.x
    && x <= group.x + group.width
    && y >= group.y
    && y <= group.y + GROUP_HEADER_HEIGHT
  )) || null;
}

export function fieldLayoutIsLarge(vm) {
  return (vm.entities || []).length >= LARGE_FIELD_ENTITY_COUNT
    || (vm.relations || []).length >= LARGE_FIELD_RELATION_COUNT;
}

// Selection can reveal an entity hidden by a collapsed group or invisible
// layer without changing the shared structural revision. Track only actual
// layout membership so that reveal transitions relayout while ordinary
// visible-to-visible selection preserves spatial memory.
export function fieldLayoutMembershipKey(vm) {
  return JSON.stringify(visibleEntities(vm).map((entity) => entity.id));
}

// Rebind data-only VM changes onto existing geometry. Structural revisions are
// the sole authority for relayout; status, selection, and cursor overlays must
// not disturb the operator's spatial memory.
export function rebindFieldLayout(layout, vm) {
  const entities = new Map((vm?.entities || []).map((entity) => [entity.id, entity]));
  const relations = new Map((vm?.relations || []).map((relation) => [relation.id, relation]));
  for (const node of layout.nodes.values()) node.entity = entities.get(node.id) || node.entity;
  for (const edge of layout.edges) {
    edge.relation = relations.get(edge.relation.id) || edge.relation;
  }
  return layout;
}

function prepareField(vm) {
  const visible = visibleEntities(vm);
  const ids = visible.map((entity) => entity.id);
  const idSet = new Set(ids);
  const relations = (vm.relations || []).filter(
    (relation) => idSet.has(relation.source_id) && idSet.has(relation.target_id),
  );
  return { visible, ids, relations };
}

function placeField(vm, prepared, components, previous) {
  const { visible, relations } = prepared;
  const componentById = new Map();
  components.forEach((members, index) => {
    members.forEach((id) => componentById.set(id, index));
  });
  const componentRanks = rankComponents(components, componentById, relations, visible);
  const groupDefinitions = (vm.groups || []).map((group) => ({ ...group }));
  const groupOrder = new Map(groupDefinitions.map((group, index) => [group.id, index]));
  const laneNames = [...new Set(visible.map((entity) => entity.lane || "").filter(Boolean))].sort();
  const laneOrder = new Map(laneNames.map((lane, index) => [lane, index]));
  const buckets = new Map();
  for (const entity of visible) {
    const group = groupOrder.get(entity.group_id) ?? groupOrder.size;
    const rank = Number.isFinite(entity.rank)
      ? Number(entity.rank)
      : componentRanks.get(componentById.get(entity.id)) || 0;
    const lane = laneOrder.get(entity.lane) ?? laneOrder.size;
    const key = `${group}\0${rank}\0${lane}`;
    if (!buckets.has(key)) buckets.set(key, []);
    buckets.get(key).push(entity);
  }
  for (const entities of buckets.values()) {
    entities.sort((left, right) => (
      number(left.order) - number(right.order)
      || left.id.localeCompare(right.id)
    ));
  }

  // A group's column is as wide as its deepest semantic rank. Fixed-width
  // origins make rank >= 2 escape into the next group and destroy compound
  // boundaries, so later groups start after the actual occupied span.
  const maximumRankByGroup = new Map();
  groupDefinitions.forEach((_, index) => maximumRankByGroup.set(index, 0));
  for (const key of buckets.keys()) {
    const [groupText, rankText] = key.split("\0");
    const group = Number(groupText);
    const rank = Number(rankText);
    maximumRankByGroup.set(group, Math.max(maximumRankByGroup.get(group) ?? 0, rank));
  }
  const groupOrigins = new Map();
  let nextGroupOrigin = 0;
  for (const group of [...maximumRankByGroup.keys()].sort((left, right) => left - right)) {
    groupOrigins.set(group, nextGroupOrigin);
    nextGroupOrigin += Math.max(
      GROUP_WIDTH,
      (maximumRankByGroup.get(group) || 0) * RANK_SPACING + GROUP_RIGHT_GUTTER,
    );
  }

  const nodes = new Map();
  const incoming = incomingSources(relations);
  for (const [key, entities] of [...buckets.entries()].sort(([left], [right]) => (
    left.localeCompare(right)
  ))) {
    const [groupText, rankText, laneText] = key.split("\0");
    const group = Number(groupText);
    const rank = Number(rankText);
    const lane = Number(laneText);
    entities.forEach((entity, index) => {
      const targetX = 120 + (groupOrigins.get(group) || 0) + rank * RANK_SPACING;
      const targetY = 90 + lane * 150 + index * 86;
      const seed = seedPosition(entity, previous, incoming, targetX, targetY);
      nodes.set(entity.id, {
        id: entity.id,
        entity,
        x: seed.x,
        y: seed.y,
        targetX,
        targetY,
        width: entity.traits?.shape === "aggregate" ? 156 : 136,
        height: entity.traits?.shape === "grid" ? 74 : 52,
        componentSize: components[componentById.get(entity.id)]?.length || 1,
      });
    });
  }
  const edges = relations.map((relation) => ({
    relation,
    points: routeEdge(nodes.get(relation.source_id), nodes.get(relation.target_id)),
  }));
  const groups = groupBounds(groupDefinitions, nodes, groupOrigins);
  const width = Math.max(
    640,
    ...[...nodes.values()].map((node) => node.targetX + node.width + 100),
    ...groups.map((group) => group.x + group.width + 40),
  );
  const height = Math.max(
    360,
    ...[...nodes.values()].map((node) => node.targetY + node.height + 100),
    ...groups.map((group) => group.y + group.height + 40),
  );
  return { nodes, edges, groups, groupDefinitions, groupOrigins, width, height };
}

function visibleEntities(vm) {
  const hidden = new Set((vm.layers || []).filter((layer) => !layer.visible).map((layer) => layer.id));
  const collapsed = new Set((vm.groups || []).filter((group) => group.collapsed).map((group) => group.id));
  return (vm.entities || []).filter((entity) => {
    // Selection is an explicit reveal carrier (not only a color change). This
    // keeps a search match reachable while the shared reducer publishes the
    // corresponding layer/group reveal state.
    if (entity.selected) return true;
    if (entity.group_id && collapsed.has(entity.group_id)) return false;
    if (!(entity.layer_ids || []).length) return true;
    return entity.layer_ids.some((layer) => !hidden.has(layer));
  });
}

function seedPosition(entity, previous, incoming, targetX, targetY) {
  const prior = previous.get(entity.id);
  if (prior) return { x: prior.x, y: prior.y };
  const parent = entity.parent_id && previous.get(entity.parent_id);
  if (parent) return { x: parent.x, y: parent.y };
  for (const sourceId of incoming.get(entity.id) || []) {
    const source = previous.get(sourceId);
    if (source) return { x: source.x, y: source.y };
  }
  return { x: targetX - 28, y: targetY };
}

function incomingSources(relations) {
  const incoming = new Map();
  for (const relation of relations) {
    if (!incoming.has(relation.target_id)) incoming.set(relation.target_id, []);
    incoming.get(relation.target_id).push(relation.source_id);
  }
  for (const sources of incoming.values()) sources.sort();
  return incoming;
}

function stronglyConnectedComponents(ids, relations) {
  const graph = new Map(ids.map((id) => [id, []]));
  for (const relation of relations) graph.get(relation.source_id)?.push(relation.target_id);
  for (const targets of graph.values()) targets.sort();
  let index = 0;
  const stack = [];
  const onStack = new Set();
  const indices = new Map();
  const low = new Map();
  const components = [];
  const visit = (id) => {
    indices.set(id, index);
    low.set(id, index);
    index += 1;
    stack.push(id);
    onStack.add(id);
    for (const next of graph.get(id) || []) {
      if (!indices.has(next)) {
        visit(next);
        low.set(id, Math.min(low.get(id), low.get(next)));
      } else if (onStack.has(next)) {
        low.set(id, Math.min(low.get(id), indices.get(next)));
      }
    }
    if (low.get(id) === indices.get(id)) {
      const component = [];
      let member;
      do {
        member = stack.pop();
        onStack.delete(member);
        component.push(member);
      } while (member !== id);
      component.sort();
      components.push(component);
    }
  };
  [...ids].sort().forEach((id) => {
    if (!indices.has(id)) visit(id);
  });
  return components;
}

function rankComponents(components, componentById, relations, entities) {
  const ranks = new Map();
  const authored = new Map(entities
    .filter((entity) => Number.isFinite(entity.rank))
    .map((entity) => [componentById.get(entity.id), Number(entity.rank)]));
  const incoming = new Map(components.map((_, index) => [index, new Set()]));
  const outgoing = new Map(components.map((_, index) => [index, new Set()]));
  for (const relation of relations) {
    const source = componentById.get(relation.source_id);
    const target = componentById.get(relation.target_id);
    if (source === target) continue;
    outgoing.get(source).add(target);
    incoming.get(target).add(source);
  }
  const remainingIncoming = new Map(
    [...incoming].map(([component, parents]) => [component, new Set(parents)]),
  );
  const queue = components
    .map((_, index) => index)
    .filter((index) => remainingIncoming.get(index).size === 0)
    .sort((left, right) => left - right);
  while (queue.length) {
    const component = queue.shift();
    const rank = authored.get(component) ?? Math.max(
      0,
      ...[...incoming.get(component)].map((parent) => (ranks.get(parent) || 0) + 1),
    );
    ranks.set(component, rank);
    for (const next of [...outgoing.get(component)].sort((left, right) => left - right)) {
      remainingIncoming.get(next).delete(component);
      if (remainingIncoming.get(next).size === 0) queue.push(next);
    }
    queue.sort((left, right) => left - right);
  }
  components.forEach((_, index) => {
    if (!ranks.has(index)) ranks.set(index, authored.get(index) || 0);
  });
  return ranks;
}

function routeEdge(source, target) {
  if (!source || !target) return [];
  const middle = (source.x + target.x) / 2;
  return [
    [source.x + source.width / 2, source.y],
    [middle, source.y],
    [middle, target.y],
    [target.x - target.width / 2, target.y],
  ];
}

function groupBounds(groups, nodes, groupOrigins = new Map()) {
  return groups.map((group, index) => {
    const members = [...nodes.values()].filter((node) => node.entity.group_id === group.id);
    if (!members.length) {
      return {
        id: group.id,
        label: group.label,
        collapsed: !!group.collapsed,
        x: 20 + (groupOrigins.get(index) ?? index * GROUP_WIDTH),
        y: 20,
        width: 200,
        height: GROUP_HEADER_HEIGHT + 12,
      };
    }
    const minX = Math.min(...members.map((node) => node.x - node.width / 2)) - 32;
    const maxX = Math.max(...members.map((node) => node.x + node.width / 2)) + 32;
    const minY = Math.min(...members.map((node) => node.y - node.height / 2)) - 42;
    const maxY = Math.max(...members.map((node) => node.y + node.height / 2)) + 28;
    return {
      id: group.id,
      label: group.label,
      collapsed: !!group.collapsed,
      x: minX,
      y: minY,
      width: maxX - minX,
      height: maxY - minY,
    };
  });
}

function isLarge(prepared) {
  return prepared.visible.length >= LARGE_FIELD_ENTITY_COUNT
    || prepared.relations.length >= LARGE_FIELD_RELATION_COUNT;
}

function nextFrame() {
  return new Promise((resolve) => {
    if (globalThis.requestAnimationFrame) requestAnimationFrame(() => resolve());
    else setTimeout(resolve, 0);
  });
}

function number(value) {
  return Number.isFinite(value) ? Number(value) : 0;
}

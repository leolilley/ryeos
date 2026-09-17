<script lang="ts">
  import type { RyeOsSceneActionVm, RyeOsSceneModel, RyeOsSceneObjectVm, RyeOsUiIntent } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props { scene: RyeOsSceneModel }
  let { scene }: Props = $props();
  const dispatch = dispatchUi();

  const points = $derived(scene.objects.flatMap((object) => [object.position, object.end].filter((point): point is number[] => point != null)));
  const bounds = $derived.by(() => {
    const xs = points.map((point) => Number(point[0] ?? 0));
    const ys = points.map((point) => -Number(point[1] ?? 0));
    const minX = xs.length ? Math.min(...xs) : 0;
    const maxX = xs.length ? Math.max(...xs) : 1;
    const minY = ys.length ? Math.min(...ys) : 0;
    const maxY = ys.length ? Math.max(...ys) : 1;
    const scale = Math.max(0.25, scene.camera.fov_degrees / 45);
    const width = Math.max(40, maxX - minX + 40) * scale;
    const height = Math.max(30, maxY - minY + 30) * scale;
    return `${Number(scene.camera.target[0] ?? 0) - width / 2} ${-Number(scene.camera.target[1] ?? 0) - height / 2} ${width} ${height}`;
  });
  const accessibleLabel = $derived(scene.objects.flatMap((object) => object.label ? [object.label] : []).join("; ") || "Scene");
  const activate = (intent: RyeOsUiIntent | null) => intent && dispatch({ type: "activate", intent });
  const radius = (object: RyeOsSceneObjectVm) => Math.max(1, Number(object.scale?.[0] ?? 5));
  const x = (object: RyeOsSceneObjectVm) => Number(object.position?.[0] ?? 0);
  const y = (object: RyeOsSceneObjectVm) => -Number(object.position?.[1] ?? 0);
  const atlasNodes = $derived(new Map((scene.atlas?.nodes ?? []).map((node) => [node.id, node])));
  const atlasBounds = $derived.by(() => {
    const bounds = scene.atlas?.bounds;
    if (!bounds) return "-10 -10 20 20";
    const scale = Math.max(0.25, scene.camera.fov_degrees / 45);
    const width = Math.max(4, bounds.x_max - bounds.x_min + 4) * scale;
    const height = Math.max(4, bounds.z_max - bounds.z_min + 4) * scale;
    return `${Number(scene.camera.target[0] ?? 0) - width / 2} ${Number(scene.camera.target[2] ?? 0) - height / 2} ${width} ${height}`;
  });
  const atlasPoint = (position: number[]) => ({ x: Number(position[0] ?? 0), y: Number(position[2] ?? position[1] ?? 0) });
  const actionById = $derived(new Map(scene.actions.map((action) => [action.id, action])));
  const actions = (group: string) => scene.actions.filter((action) => action.group === group);
  const runAction = (action: RyeOsSceneActionVm | undefined) => action && dispatch(action.event);
  const keyAction = (event: KeyboardEvent, action: RyeOsSceneActionVm | undefined) => {
    if (event.key === "Enter" || event.key === " ") { event.preventDefault(); runAction(action); }
  };
</script>

<div class="scene-view">
  {#if scene.atlas}
    <div class="atlas-toolbar">
      <div class="atlas-identity"><strong>{scene.atlas.root_label}</strong><span>{scene.atlas.nodes.length} regions</span></div>
      <div class="atlas-projections" aria-label="Atlas projection">
        {#each actions("projection") as action (action.id)}<button class:active={action.active} aria-pressed={action.active} onclick={() => runAction(action)}>{action.label}</button>{/each}
      </div>
      {#if scene.atlas.projection === "ai_space"}
        <div class="atlas-layers">
          {#each actions("layer") as action (action.id)}<button class:active={action.active} aria-pressed={action.active} onclick={() => runAction(action)}>{action.label}</button>{/each}
        </div>
        <div class="atlas-lenses" aria-label="Atlas lens">
          {#each actions("lens") as action (action.id)}<button class:active={action.active} aria-pressed={action.active} onclick={() => runAction(action)}>{action.label}</button>{/each}
        </div>
      {/if}
    </div>
  {/if}
  <svg class:atlas-canvas={scene.atlas != null} viewBox={scene.atlas ? atlasBounds : bounds} role="img" aria-label={scene.atlas ? `${scene.atlas.root_label} atlas` : accessibleLabel} preserveAspectRatio="xMidYMid meet">
    {#each scene.objects as object (object.id)}
      {@const r = radius(object)}
      {@const ox = x(object)}
      {@const oy = y(object)}
      {@const end = object.end}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <g class:interactive={object.intent != null} data-tone={object.tone} opacity={object.opacity}
        role={object.intent ? "button" : undefined} tabindex={object.intent ? 0 : undefined}
        onclick={() => activate(object.intent)}
        onkeydown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); activate(object.intent); } }}>
        {#if end}
          <line x1={ox} y1={oy} x2={Number(end[0] ?? 0)} y2={-Number(end[1] ?? 0)} stroke={object.color} />
        {:else if object.kind === "text"}
          <text x={ox} y={oy} fill={object.color}>{object.label ?? ""}</text>
        {:else if object.glyph === "diamond"}
          <polygon points={`${ox},${oy-r} ${ox+r},${oy} ${ox},${oy+r} ${ox-r},${oy}`} stroke={object.color} />
        {:else if object.glyph === "square"}
          <rect x={ox-r} y={oy-r} width={r*2} height={r*2} stroke={object.color} />
        {:else}
          <circle cx={ox} cy={oy} r={r} stroke={object.color} />
        {/if}
        {#if object.label && object.kind !== "text"}<title>{object.label}</title>{/if}
      </g>
    {/each}
    {#if scene.atlas}
      {#each scene.atlas.links as link (link.id)}
        {@const from = atlasNodes.get(link.from)}
        {@const to = atlasNodes.get(link.to)}
        {#if from && to}
          {@const start = atlasPoint(from.position)}
          {@const end = atlasPoint(to.position)}
          <line x1={start.x} y1={start.y} x2={end.x} y2={end.y} data-kind={link.kind} />
        {/if}
      {/each}
      {#each scene.atlas.nodes as node (node.id)}
        {@const point = atlasPoint(node.position)}
        {@const nodeAction = actionById.get(`node:${node.id}`)}
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <g class="atlas-node" class:selected={node.state.selected} class:highlighted={node.state.highlighted} class:dimmed={node.state.dimmed}
          role={nodeAction ? "button" : undefined} tabindex={nodeAction ? 0 : undefined}
          onclick={() => runAction(nodeAction)} onkeydown={(event) => keyAction(event, nodeAction)}>
          <circle cx={point.x} cy={point.y} r={Math.max(0.18, 0.16 + node.stack.length * 0.045)} />
          <text x={point.x + 0.28} y={point.y + 0.08}>{node.label}</text>
        </g>
        {#each node.stack.slice(0, 4) as item, index (item.id)}
          {@const itemAction = actionById.get(`item:${item.id}`)}
          <!-- SVG has no native button primitive. This exact painted mark is also
               the focusable control, with button semantics and full key handling. -->
          <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
          <circle class="atlas-stack-item" data-kind={item.kind} cx={point.x + index * 0.11} cy={point.y - 0.24} r="0.055"
            role={itemAction ? "button" : undefined} tabindex={itemAction ? 0 : undefined}
            onclick={() => runAction(itemAction)}
            onkeydown={(event) => keyAction(event, itemAction)}><title>{item.canonical_ref}</title></circle>
        {/each}
      {/each}
    {/if}
  </svg>
</div>

<script lang="ts">
  import type { AtlasInteractionVm, AtlasItemKind, RyeOsSceneModel, RyeOsSceneObjectVm, RyeOsUiIntent } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props { scene: RyeOsSceneModel; tileId: string }
  let { scene, tileId }: Props = $props();
  const dispatch = dispatchUi();

  const points = $derived(scene.objects.flatMap((object) => [object.position, object.end].filter((point): point is number[] => point != null)));
  const bounds = $derived.by(() => {
    const xs = points.map((point) => Number(point[0] ?? 0));
    const ys = points.map((point) => -Number(point[1] ?? 0));
    const minX = xs.length ? Math.min(...xs) : 0;
    const maxX = xs.length ? Math.max(...xs) : 1;
    const minY = ys.length ? Math.min(...ys) : 0;
    const maxY = ys.length ? Math.max(...ys) : 1;
    return `${minX - 20} ${minY - 15} ${Math.max(40, maxX - minX + 40)} ${Math.max(30, maxY - minY + 30)}`;
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
    return `${bounds.x_min - 2} ${bounds.z_min - 2} ${Math.max(4, bounds.x_max - bounds.x_min + 4)} ${Math.max(4, bounds.z_max - bounds.z_min + 4)}`;
  });
  const atlasPoint = (position: number[]) => ({ x: Number(position[0] ?? 0), y: Number(position[2] ?? position[1] ?? 0) });
  const atlasInteraction = (interaction: AtlasInteractionVm | null | undefined) => {
    if (!interaction) return;
    if (interaction.type === "inspect_item") dispatch({ type: "activate", intent: interaction });
    else if (interaction.type === "read_file") dispatch({ type: "activate", intent: interaction });
    else dispatch({ type: "set_atlas_file_space_path", tile_id: tileId, root: interaction.root ?? scene.atlas?.ui.file_space_root ?? "project", path: interaction.path });
  };
  const layerLabels: ReadonlyArray<[AtlasItemKind, string]> = [["directive", "Directives"], ["tool", "Tools"], ["knowledge", "Knowledge"], ["config", "Config"]];
</script>

<div class="scene-view">
  {#if scene.atlas}
    <div class="atlas-toolbar">
      <div class="atlas-identity"><strong>{scene.atlas.root_label}</strong><span>{scene.atlas.nodes.length} regions</span></div>
      <div class="atlas-projections" aria-label="Atlas projection">
        <button class:active={scene.atlas.projection === "ai_space"} onclick={() => dispatch({ type: "set_atlas_projection", tile_id: tileId, projection: "ai_space", root: null })}>AI space</button>
        <button class:active={scene.atlas.projection === "file_space"} onclick={() => dispatch({ type: "set_atlas_projection", tile_id: tileId, projection: "file_space", root: scene.atlas?.ui.file_space_root ?? null })}>Files</button>
      </div>
      {#if scene.atlas.projection === "ai_space"}
        <div class="atlas-layers">
          {#each layerLabels as [kind, label]}
            {@const active = scene.atlas.ui.visible_layers.includes(kind)}
            <button class:active aria-pressed={active} onclick={() => dispatch({ type: "set_atlas_layer_visible", tile_id: tileId, kind, visible: !active })}>{label}</button>
          {/each}
        </div>
      {/if}
    </div>
    <svg class="atlas-canvas" viewBox={atlasBounds} role="img" aria-label={`${scene.atlas.root_label} atlas`} preserveAspectRatio="xMidYMid meet">
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
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <g class="atlas-node" class:selected={node.state.selected} class:highlighted={node.state.highlighted} class:dimmed={node.state.dimmed}
          role={node.interaction ? "button" : undefined} tabindex={node.interaction ? 0 : undefined}
          onclick={() => atlasInteraction(node.interaction)} onkeydown={(event) => { if (event.key === "Enter" || event.key === " ") atlasInteraction(node.interaction); }}>
          <circle cx={point.x} cy={point.y} r={Math.max(0.18, 0.16 + node.stack.length * 0.045)} />
          <text x={point.x + 0.28} y={point.y + 0.08}>{node.label}</text>
          {#each node.stack.slice(0, 4) as item, index (item.id)}
            <circle class="atlas-stack-item" data-kind={item.kind} cx={point.x + index * 0.11} cy={point.y - 0.24} r="0.055"
              role="button" tabindex="-1"
              onclick={(event) => { event.stopPropagation(); atlasInteraction(item.interaction); }}
              onkeydown={(event) => { if (event.key === "Enter" || event.key === " ") atlasInteraction(item.interaction); }}><title>{item.canonical_ref}</title></circle>
          {/each}
        </g>
      {/each}
    </svg>
  {:else}
  <svg viewBox={bounds} role="img" aria-label={accessibleLabel} preserveAspectRatio="xMidYMid meet">
    {#each scene.objects as object (object.id)}
      {@const r = radius(object)}
      {@const ox = x(object)}
      {@const oy = y(object)}
      {@const end = object.end}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <g class:interactive={object.intent != null} data-tone={object.tone} opacity={object.opacity}
        role={object.intent ? "button" : undefined} tabindex={object.intent ? 0 : undefined}
        onclick={() => activate(object.intent)}
        onkeydown={(event) => { if (event.key === "Enter" || event.key === " ") activate(object.intent); }}>
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
  </svg>
  {/if}
</div>

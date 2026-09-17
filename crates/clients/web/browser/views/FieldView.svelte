<script lang="ts">
  import { onMount } from "svelte";
  import type { RyeOsFieldVm, RyeOsViewInstanceKey } from "../generated";
  import { dispatchUi } from "../runtime/context";
  import { canCompareEntity, FieldCanvasController } from "../visuals/ryeos_field_canvas.js";
  import GridPreview from "./GridPreview.svelte";

  interface Props { field: RyeOsFieldVm; instanceKey: RyeOsViewInstanceKey }
  let { field, instanceKey }: Props = $props();
  const dispatch = dispatchUi();
  let canvas: HTMLCanvasElement;
  let controller: FieldCanvasController | null = null;
  const selected = $derived(field.entities.find((entity) => entity.id === field.selected) ?? null);
  const selectedRelations = $derived(selected ? field.relations.filter((relation) => relation.source_id === selected.id || relation.target_id === selected.id) : []);
  const previewIds = $derived.by(() => {
    const ids = new Set(selected?.preview_ids ?? []);
    for (const id of field.compare) for (const preview of field.entities.find((entity) => entity.id === id)?.preview_ids ?? []) ids.add(preview);
    return ids;
  });
  const previews = $derived(field.previews.filter((preview) => previewIds.has(preview.id)));
  const expansion = $derived(selected ? field.expansions.find((item) => item.source === selected.source && item.root_id === selected.id) ?? null : null);

  onMount(() => {
    controller = new FieldCanvasController(canvas, dispatch, instanceKey);
    const observer = new ResizeObserver(() => controller?.resize());
    observer.observe(canvas);
    controller.update(field);
    controller.resize();
    return () => { observer.disconnect(); controller?.unmount(); controller = null; };
  });
  $effect(() => controller?.update(field));
</script>

<section class="field-view" aria-label={field.title}>
  <header class="field-toolbar">
    <div class="field-identity"><strong>{field.title}</strong><span>{field.sources.map((source) => `${source.name}:${source.phase}`).join(" · ")}</span></div>
    <div class="field-controls">
      <button disabled={!field.replay.previous} onclick={() => dispatch({ type: "step_field_cursor", instance_key: instanceKey, direction: "previous" })}>◀</button>
      <button disabled={!field.replay.playing && !field.replay.next} onclick={() => dispatch({ type: "set_field_playback", instance_key: instanceKey, playing: !field.replay.playing })}>{field.replay.playing ? "Pause" : "Play"}</button>
      <button disabled={!field.replay.next} onclick={() => dispatch({ type: "step_field_cursor", instance_key: instanceKey, direction: "next" })}>▶</button>
      <button disabled={field.replay.mode === "live"} onclick={() => dispatch({ type: "step_field_cursor", instance_key: instanceKey, direction: "live" })}>Live</button>
      {#each field.groups as group (group.id)}<button onclick={() => dispatch({ type: "set_field_group_collapsed", instance_key: instanceKey, group_id: group.id, collapsed: !group.collapsed })}>{group.collapsed ? "▸" : "▾"} {group.label}</button>{/each}
      {#each field.layers as layer (layer.id)}<button aria-pressed={layer.visible} onclick={() => dispatch({ type: "set_field_layer_visible", instance_key: instanceKey, layer_id: layer.id, visible: !layer.visible })}>{layer.visible ? "●" : "○"} {layer.label}</button>{/each}
      {#if selected}
        <button disabled={!canCompareEntity(field, selected.id)} onclick={() => dispatch({ type: "toggle_field_compare", instance_key: instanceKey, entity_id: selected.id })}>{field.compare.includes(selected.id) ? "Uncompare" : "Compare"}</button>
        <button disabled={!!expansion && !expansion.can_continue} onclick={() => dispatch({ type: expansion?.can_continue ? "continue_field_expansion" : "request_field_expansion", instance_key: instanceKey, source: selected.source, root_id: selected.id })}>{expansion?.can_continue ? "Continue" : expansion ? "Expanded" : "Expand"}</button>
        {#if expansion}<button onclick={() => dispatch({ type: "clear_field_expansion", instance_key: instanceKey, source: selected.source, root_id: selected.id })}>Clear</button>{/if}
      {/if}
    </div>
    <input type="search" value={field.search.query} placeholder="Search field" aria-label="Search field entities"
      oninput={(event) => dispatch({ type: "set_field_query", instance_key: instanceKey, query: event.currentTarget.value })}
      onkeydown={(event) => { if (event.key === "ArrowUp" || event.key === "ArrowDown") { event.preventDefault(); dispatch({ type: "move_field_search_match", instance_key: instanceKey, delta: event.key === "ArrowUp" ? -1 : 1 }); } }} />
  </header>
  {#if field.replay.rail.length}
    <nav class="field-rail" aria-label="Durable execution events">{#each field.replay.rail as entry}<button class:selected={entry.selected} aria-pressed={entry.selected} onclick={() => dispatch({ type: "set_field_cursor", instance_key: instanceKey, cursor: { mode: "braid_cut", anchor: entry.event } })}>{entry.label}</button>{/each}</nav>
  {/if}
  <div class="field-stage"><canvas bind:this={canvas} aria-hidden="true"></canvas></div>
  <div class="field-accessibility" role="listbox" aria-label={`${field.title} entities`}>
    {#each field.traversal as entityId}
      {@const entity = field.entities.find((candidate) => candidate.id === entityId)}
      {#if entity}<button role="option" aria-selected={field.selected === entity.id} onclick={() => dispatch({ type: "set_field_selection", instance_key: instanceKey, entity_id: entity.id })}>{entity.accessibility_label}</button>{/if}
    {/each}
  </div>
  {#if selected || field.warnings.length}
    <aside class="field-detail">
      {#if selected}
        <strong>{selected.label}</strong><small>{[selected.kind, selected.status, selected.source].filter(Boolean).join(" · ")}</small>
        {#each selectedRelations as relation (relation.id)}<button disabled={!relation.activate_intent} onclick={() => relation.activate_intent && dispatch({ type: "activate", intent: relation.activate_intent })}>{relation.label}</button>{/each}
        {#each previews as preview (preview.id)}<GridPreview {preview} compareEnabled={canCompareEntity(field, selected.id)} oncompare={() => dispatch({ type: "toggle_field_compare", instance_key: instanceKey, entity_id: selected.id })} />{/each}
      {/if}
      {#each field.warnings as warning}<small class="field-warning">{warning}</small>{/each}
    </aside>
  {/if}
</section>

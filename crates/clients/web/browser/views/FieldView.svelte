<script lang="ts">
  import { onMount } from "svelte";
  import type { RyeOsFieldVm, RyeOsViewInstanceKey } from "../generated";
  import { dispatchUi } from "../runtime/context";
  import { fieldAccessibilityModel } from "../visuals/ryeos_field_accessibility.js";
  import { FieldCanvasController } from "../visuals/ryeos_field_canvas.js";
  import { fieldPreviewGroups } from "./field_preview_groups.js";
  import { fieldEmptyState } from "./field_empty_state.js";
  import GridPreview from "./GridPreview.svelte";
  import RowDetails from "../components/RowDetails.svelte";

  interface Props { field: RyeOsFieldVm; instanceKey: RyeOsViewInstanceKey }
  let { field, instanceKey }: Props = $props();
  const dispatch = dispatchUi();
  let canvas: HTMLCanvasElement;
  let controller: FieldCanvasController | null = null;
  const selected = $derived(field.entities.find((entity) => entity.id === field.selected) ?? null);
  const selectedRelations = $derived(selected ? field.relations.filter((relation) => relation.source_id === selected.id || relation.target_id === selected.id) : []);
  const previewGroups = $derived(fieldPreviewGroups(field));
  const emptyState = $derived(fieldEmptyState(field));
  const expansion = $derived(selected ? field.expansions.find((item) => item.source === selected.source && item.root_id === selected.id) ?? null : null);
  const accessibilityItems = $derived(fieldAccessibilityModel(field, instanceKey));
  const activeOptionId = $derived(accessibilityItems.find((item) => item.selected)?.domId);

  const selectEntity = (entityId: string) => dispatch({ type: "set_field_selection", instance_key: instanceKey, entity_id: entityId });
  const activateEntity = (entityId: string) => {
    const entity = field.entities.find((candidate) => candidate.id === entityId);
    if (entity?.activate_intent) dispatch({ type: "activate", intent: entity.activate_intent });
  };
  const compareEntity = (entityId: string) => {
    const entity = field.entities.find((candidate) => candidate.id === entityId);
    if (entity?.compare_available) dispatch({ type: "toggle_field_compare", instance_key: instanceKey, entity_id: entityId });
  };

  onMount(() => {
    controller = new FieldCanvasController(canvas, {
      entity: ({ entityId, compare, activate }) => {
        selectEntity(entityId);
        if (compare) compareEntity(entityId);
        if (activate) activateEntity(entityId);
      },
      group: ({ groupId, collapsed }) => dispatch({ type: "set_field_group_collapsed", instance_key: instanceKey, group_id: groupId, collapsed }),
    });
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
        {#if selected.activate_intent}<button aria-label={`Activate ${selected.label}`} onclick={() => activateEntity(selected.id)}>Activate</button>{/if}
        <button disabled={!selected.compare_available} onclick={() => compareEntity(selected.id)}>{field.compare.includes(selected.id) ? "Uncompare" : "Compare"}</button>
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
  <div class="field-stage">
    <canvas bind:this={canvas} aria-hidden="true"></canvas>
    {#if emptyState}
      <div class="field-empty-state" data-tone={emptyState.tone} role="status">
        <strong>{emptyState.title}</strong><p>{emptyState.detail}</p>
      </div>
    {/if}
  </div>
  {#if field.sources.length || field.subjects.length || field.metrics.length || field.provenance.length || field.replay.outside_cut.length || field.search.truncated || expansion}
    <details class="field-source-status" class:has-rail={field.replay.rail.length > 0}>
      <summary>
        <span>Evidence</span>
        {#each field.sources as source (source.name)}
          <span data-phase={source.phase}>{source.name} <small>{source.phase}</small>{#if source.truncated}<b>bounded</b>{/if}{#if source.error}<b>error</b>{/if}</span>
        {/each}
        {#if field.subjects.length}<span><b>subjects</b></span>{/if}
        {#if field.metrics.length}<span><b>metrics</b></span>{/if}
        {#if field.provenance.length}<span><b>provenance</b></span>{/if}
        {#if field.replay.outside_cut.length}<span><b>outside cut</b></span>{/if}
        {#if field.search.truncated}<span><b>bounded search</b></span>{/if}
        {#if expansion?.truncated}<span><b>bounded expansion</b></span>{/if}
      </summary>
      <div class="field-source-evidence">
        {#each field.sources as source (source.name)}
          <section aria-label={`${source.name} source evidence`}>
            <strong>{source.name} · {source.phase}</strong>
            <p class="field-source-coordinate">Source <code>{source.source_ref}</code></p>
            {#if source.subject_fingerprint}<p class="field-source-coordinate">Subject fingerprint <code>{source.subject_fingerprint}</code></p>{/if}
            {#if source.truncated}<p>Truncated response — this is not the complete retained field.</p>{/if}
            {#if source.error}<p class="field-source-error">{source.error}</p>{/if}
            {#if source.revision}<small>Revision {source.revision}</small>{/if}
            {#each source.evidence as evidence}<p>{evidence}</p>{/each}
            {#if source.evidence_omitted > 0}<p>{source.evidence_omitted} further entity evidence references omitted from this display.</p>{/if}
          </section>
        {/each}
        {#if field.subjects.length}
          <section aria-label="Projected source subjects">
            <strong>Projected source subjects</strong>
            <ul>
              {#each field.subjects as subject, index (`${subject.source}:${subject.kind}:${subject.id}:${index}`)}
                <li>
                  <code>{subject.source}</code> · {subject.kind} · <code>{subject.id}</code>
                  {#if subject.definition_ref}<br />Definition <code>{subject.definition_ref}</code>{/if}
                  {#if subject.effective_definition_digest}<br />Effective digest <code>{subject.effective_definition_digest}</code>{/if}
                </li>
              {/each}
            </ul>
          </section>
        {/if}
        {#if field.metrics.length}
          <section aria-label="Projected field metrics">
            <strong>Projected field metrics</strong>
            <ul>
              {#each field.metrics as metric, index (`${metric.id}:${index}`)}
                <li class="field-metric"><span>{metric.label}</span> <code>{JSON.stringify(metric.value)}</code>{#if metric.unit} <span>{metric.unit}</span>{/if}</li>
              {/each}
            </ul>
          </section>
        {/if}
        {#if field.provenance.length}
          <section aria-label="Projected entity and relation source references">
            <strong>Projected entity/relation source refs</strong>
            <ul>{#each field.provenance as sourceRef, index (`${sourceRef}:${index}`)}<li><code>{sourceRef}</code></li>{/each}</ul>
          </section>
        {/if}
        {#if expansion}
          <section aria-label="Selected entity expansion evidence">
            <strong>Expansion · {expansion.phase}</strong>
            <p>Applied depth {expansion.applied_depth}; {expansion.entity_count} retained entities.</p>
            {#if expansion.truncated}<p>Expansion is bounded — additional retained evidence may exist.</p>{/if}
          </section>
        {/if}
        {#if field.replay.outside_cut.length}
          <section aria-label="Concurrent events outside replay cut">
            <strong>Outside replay cut</strong>
            <p>Concurrent retained context is not included in the selected braid cut.</p>
            <ul>{#each field.replay.outside_cut as event, index (`${event}:${index}`)}<li>{event}</li>{/each}</ul>
          </section>
        {/if}
        {#if field.search.truncated}
          <section aria-label="Bounded field search">
            <strong>Bounded search results</strong>
            <p>The visible matches are not the complete matching field.</p>
          </section>
        {/if}
      </div>
    </details>
  {/if}
  <div class="field-accessibility" role="tree" tabindex="0" aria-label={`${field.title} entities`} aria-activedescendant={activeOptionId}
    onfocus={() => { if (!field.selected && accessibilityItems[0]) selectEntity(accessibilityItems[0].id); }}
    onkeydown={(event) => {
      const current = accessibilityItems.find((item) => item.selected) ?? accessibilityItems[0];
      if (event.key === "ArrowUp" || event.key === "ArrowDown") {
        event.preventDefault();
        dispatch({ type: "move_field_selection", instance_key: instanceKey, delta: event.key === "ArrowUp" ? -1 : 1 });
      } else if (event.key === "Home" || event.key === "End") {
        event.preventDefault();
        const item = event.key === "Home" ? accessibilityItems[0] : accessibilityItems.at(-1);
        if (item) selectEntity(item.id);
      } else if ((event.key === "ArrowLeft" || event.key === "ArrowRight") && current?.groupId) {
        const collapsed = event.key === "ArrowLeft";
        if (current.expanded === collapsed) {
          event.preventDefault();
          dispatch({ type: "set_field_group_collapsed", instance_key: instanceKey, group_id: current.groupId, collapsed });
        }
      } else if (event.key === "Enter" && field.selected) {
        event.preventDefault(); activateEntity(field.selected);
      } else if (event.key === " " && field.selected) {
        event.preventDefault(); compareEntity(field.selected);
      }
    }}>
    {#each accessibilityItems as item (item.id)}
      <div id={item.domId} role="treeitem" tabindex="-1" aria-selected={item.selected} aria-posinset={item.position} aria-setsize={item.size}
        aria-level={item.level} aria-expanded={item.expanded ?? undefined}
        onclick={() => selectEntity(item.id)} ondblclick={() => activateEntity(item.id)}
        onkeydown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            event.stopPropagation();
            activateEntity(item.id);
          }
        }}>{item.groupLabel ? `Group ${item.groupLabel}. ` : ""}{item.label}{item.neighbors ? `. ${item.neighbors}` : ""}</div>
    {/each}
  </div>
  {#if selected || previewGroups.compared.length || field.warnings.length}
    <aside class="field-detail">
      {#if selected}
        <strong>{selected.label}</strong><small>{[selected.kind, selected.status, selected.source].filter(Boolean).join(" · ")}</small>
        <RowDetails details={selected.detail} label={`${selected.label} details`} />
        {#if selected.artifact_ref}
          <dl class="row-details" aria-label="Retained artifact coordinate">
            <div><dt>Thread</dt><dd>{selected.artifact_ref.thread_id}</dd></div>
            <div><dt>Artifact</dt><dd>{selected.artifact_ref.artifact_id}</dd></div>
            <div><dt>Content hash</dt><dd>{selected.artifact_ref.content_hash ?? "Not recorded"}</dd></div>
          </dl>
        {/if}
        {#each selectedRelations as relation (relation.id)}<button disabled={!relation.activate_intent} onclick={() => relation.activate_intent && dispatch({ type: "activate", intent: relation.activate_intent })}>{relation.label}</button>{/each}
        {#each previewGroups.selectedPreviews as preview (preview.id)}
          {#if preview.grid}<GridPreview {preview} compareEnabled={selected.compare_available} oncompare={() => compareEntity(selected.id)} />
          {:else}<p class="field-preview-unavailable">This projected preview has no grid payload.</p>{/if}
        {/each}
      {/if}
      {#if previewGroups.compared.length}
        <section class="field-comparison" aria-label="Compared field entities">
          <strong>Compared entities</strong>
          {#each previewGroups.compared as entry (entry.entity.id)}
            <article aria-label={`${entry.entity.label} (${entry.entity.id}) comparison`}>
              <h3>{entry.entity.label}</h3><small><code>{entry.entity.id}</code> · {entry.entity.source}</small>
              {#each entry.previews as preview (preview.id)}
                {#if preview.grid}<GridPreview {preview} compareEnabled={entry.entity.compare_available} oncompare={() => compareEntity(entry.entity.id)} />
                {:else}<p class="field-preview-unavailable">This projected preview has no grid payload.</p>{/if}
              {:else}<p class="field-preview-unavailable">No grid preview is present in this projection.</p>{/each}
            </article>
          {/each}
        </section>
      {/if}
      {#each field.warnings as warning}<small class="field-warning">{warning}</small>{/each}
    </aside>
  {/if}
</section>

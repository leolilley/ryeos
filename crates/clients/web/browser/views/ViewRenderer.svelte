<script lang="ts">
  import type { RyeOsViewInstanceKey, RyeOsViewVm } from "../generated";
  import EmptyState from "../components/EmptyState.svelte";
  import RowDetails from "../components/RowDetails.svelte";
  import FieldView from "./FieldView.svelte";
  import SceneView from "./SceneView.svelte";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsViewVm; tileId: string; instanceKey: RyeOsViewInstanceKey }
  let { model, tileId, instanceKey }: Props = $props();
  const dispatch = dispatchUi();
  const select = (itemId: string | undefined) => {
    if (!itemId) return;
    dispatch({
      type: "choose_view_item",
      instance_key: instanceKey,
      item_id: itemId,
      activate: true,
    });
  };
  const expand = (itemId: string, expanded: boolean) => dispatch({
    type: "toggle_view_item_expansion",
    instance_key: instanceKey,
    item_id: itemId,
    expand: !expanded,
  });
  const expandTimeline = (index: number) => {
    const itemId = model.type === "timeline" ? model.entry_ids[index] : undefined;
    if (itemId) expand(itemId, model.type === "timeline" && (model.entry_expanded?.[index] ?? false));
  };
</script>

<div class="view" data-view={model.type}>
  {#if model.type === "text"}
    <div class="text-view" aria-label={model.title}>
      {#each model.lines as line}<div data-tone={line.tone}>{line.text}</div>{/each}
    </div>
  {:else if model.type === "document"}
    <article class="document-view" aria-label={model.title}>
      <header><span>{model.path}</span>{#if model.truncated}<small>bounded preview</small>{/if}</header>
      <pre>{model.content}</pre>
    </article>
  {:else if model.type === "rows"}
    <div class="rows-view" role="list" aria-label={model.title}>
      {#each model.rows as row (row.id)}
        <div class="view-record" role="listitem" data-expanded={row.expanded}>
          <div class="view-record-line">
            <button data-focus-key={`view:${instanceKey}:item:${row.id}`} class:selected={row.selected} data-tone={row.tone} onclick={() => select(row.id)}>
              <span class="row-glyph">{row.glyph ?? "◇"}</span>
              <span class="row-copy"><strong>{row.primary}</strong>{#if row.secondary}<small>{row.secondary}</small>{/if}</span>
              {#if row.meta}<span class="row-meta">{row.meta}</span>{/if}
            </button>
            {#if row.expandable}<button class="row-disclosure" aria-label={`${row.expanded ? "Collapse" : "Expand"} ${row.primary}`} aria-expanded={row.expanded} onclick={() => expand(row.id, row.expanded)}>{row.expanded ? "▾" : "▸"}</button>{/if}
          </div>
          {#if row.expanded}<RowDetails details={row.detail ?? []} label={`${row.primary} details`} />{/if}
        </div>
      {/each}
    </div>
  {:else if model.type === "table"}
    <div class="table-view" role="table" aria-label={model.title} style={`--columns:${model.columns.length}`}>
      <div class="table-head" role="row">{#each model.columns as column}<span role="columnheader">{column}</span>{/each}<span class="table-action-head" role="columnheader">detail</span></div>
      {#each model.rows as row (row.id)}
        <div class="table-record" role="rowgroup" data-expanded={row.expanded}>
          <div class:selected={row.selected} class="table-record-line" role="row" data-tone={row.tone}>
            {#each row.cells as cell, index}<div role="cell" data-tone={row.cell_tones?.[index] ?? undefined}><button data-focus-key={index === 0 ? `view:${instanceKey}:item:${row.id}` : undefined} aria-label={`Select row: ${cell}`} onclick={() => select(row.id)}>{cell}</button></div>{/each}
            <div class="table-action-cell" role="cell">{#if row.expandable}<button class="row-disclosure" aria-label={`${row.expanded ? "Collapse" : "Expand"} row`} aria-expanded={row.expanded} onclick={() => expand(row.id, row.expanded)}>{row.expanded ? "▾" : "▸"}</button>{/if}</div>
          </div>
          {#if row.expanded}<div class="table-detail" role="row"><div role="cell" aria-colspan={model.columns.length + 1}><RowDetails details={row.detail ?? []} label="Row details" /></div></div>{/if}
        </div>
      {/each}
    </div>
  {:else if model.type === "timeline"}
    <div class="timeline-view" aria-label={model.title}>
      {#each model.entries as entry, index}
        <div class="view-record" data-expanded={model.entry_expanded?.[index] ?? false}>
          <div class="view-record-line">
            <button data-focus-key={`view:${instanceKey}:item:${model.entry_ids[index]}`} class="timeline-entry" class:selected={model.selected === BigInt(index)} data-kind={entry.type} style={`--indent:${model.entry_indents[index] ?? 0}`} onclick={() => select(model.entry_ids[index])}>
              {#if entry.type === "block"}<p data-tone={entry.tone}>{entry.text}</p>
              {:else if entry.type === "line"}<strong>{entry.primary}</strong>{#if entry.meta}<small>{entry.meta}</small>{/if}
              {:else if entry.type === "pair"}<span>{entry.summary}</span>{#if entry.meta}<small>{entry.meta}</small>{/if}
              {:else}<span class="timeline-separator">{entry.label}</span>{/if}
            </button>
            {#if model.entry_expandable?.[index]}<button class="row-disclosure" aria-label={`${model.entry_expanded?.[index] ? "Collapse" : "Expand"} timeline entry`} aria-expanded={model.entry_expanded?.[index] ?? false} onclick={() => expandTimeline(index)}>{model.entry_expanded?.[index] ? "▾" : "▸"}</button>{/if}
          </div>
          {#if model.entry_expanded?.[index]}<RowDetails details={model.entry_details?.[index] ?? []} label="Timeline entry details" />{/if}
        </div>
      {/each}
    </div>
  {:else if model.type === "sections"}
    <div class="sections-view" aria-label={model.title}>
      {#each model.sections as section, sectionIndex}
        <section class:collapsed={section.collapsed}>
          <button data-focus-key={`view:${instanceKey}:section:${section.id}`} class:selected={section.header_selected} onclick={() => dispatch({ type: "toggle_view_section", instance_key: instanceKey, section_id: section.id })}>
            <span>{section.collapsed ? "▸" : "▾"} {section.title}</span><span>{String(section.count).padStart(2, "0")}</span>
          </button>
          {#if !section.collapsed}
            {#each section.rows as row (row.id)}
              <div class="view-record" data-expanded={row.expanded}>
                <div class="view-record-line"><button data-focus-key={`view:${instanceKey}:item:${row.id}`} class="section-row" class:selected={row.selected} data-tone={row.tone} onclick={() => select(row.id)}><span>{row.primary}</span><small>{row.meta ?? row.secondary ?? ""}</small></button>{#if row.expandable}<button class="row-disclosure" aria-label={`${row.expanded ? "Collapse" : "Expand"} ${row.primary}`} aria-expanded={row.expanded} onclick={() => expand(row.id, row.expanded)}>{row.expanded ? "▾" : "▸"}</button>{/if}</div>
                {#if row.expanded}<RowDetails details={row.detail ?? []} label={`${row.primary} details`} />{/if}
              </div>
            {/each}
          {/if}
        </section>
      {/each}
    </div>
  {:else if model.type === "placeholder"}
    <EmptyState title={model.title} message={model.message} />
  {:else if model.type === "map" || model.type === "atlas"}
    <SceneView scene={model.scene} />
  {:else if model.type === "field"}
    <FieldView field={model.field} {instanceKey} />
  {/if}
</div>

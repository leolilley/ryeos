<script lang="ts">
  import type { RyeOsUiIntent, RyeOsViewInstanceKey, RyeOsViewVm } from "../generated";
  import EmptyState from "../components/EmptyState.svelte";
  import FieldView from "./FieldView.svelte";
  import SceneView from "./SceneView.svelte";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsViewVm; tileId: string; instanceKey: RyeOsViewInstanceKey }
  let { model, tileId, instanceKey }: Props = $props();
  const dispatch = dispatchUi();
  const activate = (intent: RyeOsUiIntent) => dispatch({ type: "activate", intent });
</script>

<div class="view" data-view={model.type}>
  {#if model.type === "text"}
    <div class="text-view" aria-label={model.title}>
      {#each model.lines as line}<div data-tone={line.tone}>{line.text}</div>{/each}
    </div>
  {:else if model.type === "rows"}
    <div class="rows-view" role="list" aria-label={model.title}>
      {#each model.rows as row (row.id)}
        <button class:selected={row.selected} data-tone={row.tone} disabled={!row.intent} onclick={() => row.intent && activate(row.intent)}>
          <span class="row-glyph">{row.glyph ?? "◇"}</span>
          <span class="row-copy"><strong>{row.primary}</strong>{#if row.secondary}<small>{row.secondary}</small>{/if}</span>
          {#if row.meta}<span class="row-meta">{row.meta}</span>{/if}
        </button>
      {/each}
    </div>
  {:else if model.type === "table"}
    <div class="table-view" role="table" aria-label={model.title} style={`--columns:${model.columns.length}`}>
      <div class="table-head" role="row">{#each model.columns as column}<span role="columnheader">{column}</span>{/each}</div>
      {#each model.rows as row (row.id)}
        <button role="row" class:selected={row.selected} data-tone={row.tone} disabled={!row.intent} onclick={() => row.intent && activate(row.intent)}>
          {#each row.cells as cell, index}<span role="cell" data-tone={row.cell_tones?.[index] ?? undefined}>{cell}</span>{/each}
        </button>
      {/each}
    </div>
  {:else if model.type === "timeline"}
    <div class="timeline-view" aria-label={model.title}>
      {#each model.entries as entry, index}
        <div class="timeline-entry" class:selected={model.selected === BigInt(index)} data-kind={entry.type} style={`--indent:${model.entry_indents[index] ?? 0}`}>
          {#if entry.type === "block"}<p data-tone={entry.tone}>{entry.text}</p>
          {:else if entry.type === "line"}<button disabled={!entry.intent} onclick={() => entry.intent && activate(entry.intent)}><strong>{entry.primary}</strong>{#if entry.meta}<small>{entry.meta}</small>{/if}</button>
          {:else if entry.type === "pair"}<span>{entry.summary}</span>{#if entry.meta}<small>{entry.meta}</small>{/if}
          {:else}<span class="timeline-separator">{entry.label}</span>{/if}
        </div>
      {/each}
    </div>
  {:else if model.type === "sections"}
    <div class="sections-view" aria-label={model.title}>
      {#each model.sections as section, sectionIndex}
        <section class:collapsed={section.collapsed}>
          <button class:selected={section.header_selected} onclick={() => dispatch({ type: "set_fold", tile_id: tileId, section: BigInt(sectionIndex), collapsed: !section.collapsed })}>
            <span>{section.collapsed ? "▸" : "▾"} {section.title}</span><span>{String(section.count).padStart(2, "0")}</span>
          </button>
          {#if !section.collapsed}
            {#each section.rows as row (row.id)}<button class="section-row" class:selected={row.selected} disabled={!row.intent} onclick={() => row.intent && activate(row.intent)}><span>{row.primary}</span><small>{row.meta ?? row.secondary ?? ""}</small></button>{/each}
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

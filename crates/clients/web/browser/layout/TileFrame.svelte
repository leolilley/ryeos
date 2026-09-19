<script lang="ts">
  import type { RyeOsLayoutNodeVm } from "../generated";
  import InputComposer from "../components/InputComposer.svelte";
  import ViewRenderer from "../views/ViewRenderer.svelte";
  import { dispatchUi } from "../runtime/context";
  type Tile = Extract<RyeOsLayoutNodeVm, { type: "tile" }>;
  interface Props { model: Tile }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>

<article class="tile-frame" class:focused={model.focused} class:transparent={model.background_transparent} data-instance={model.instance_key} data-scroll-key={`tile:${model.instance_key}`} onpointerdown={() => { if (!model.focused) dispatch({ type: "focus_changed", target: model.tile_id }); }} onfocusin={() => { if (!model.focused) dispatch({ type: "focus_changed", target: model.tile_id }); }}>
  {#if !model.chrome_hidden}
    <header class="tile-header" class:grouped={model.group_label}>
      <div class="tile-title-row">
        <div class="tile-identity"><span class="tile-signal"></span><strong>{model.group_label ?? model.title}</strong></div>
        <div class="tile-tools">
        <button
          class="tile-tool"
          aria-label={model.maximized ? "Restore view" : "Maximize view"}
          title={model.maximized ? "Restore view" : "Maximize view"}
          onclick={() => dispatch({ type: "activate", intent: { type: "toggle_tile_maximized", tile_id: model.tile_id } })}
        >{model.maximized ? "↙" : "↗"}</button>
        </div>
      </div>
      {#if model.tabs.length > 1}<div class="view-tabs" role="tablist">{#each model.tabs as tab (tab.tile_id)}<button role="tab" aria-selected={tab.active} class:active={tab.active} onclick={() => dispatch({ type: "focus_changed", target: tab.tile_id })}>{tab.title}</button>{/each}</div>{/if}
    </header>
  {/if}
  {#if model.heading}<div class="content-heading"><small>{model.heading.eyebrow}</small><h1>{model.heading.title}</h1>{#if model.heading.summary}<p>{model.heading.summary}</p>{/if}{#if model.heading.metadata.length}<div class="heading-meta">{model.heading.metadata.join("  /  ")}</div>{/if}</div>{/if}
  {#if model.input?.live_filter}<InputComposer model={model.input} />{/if}
  <ViewRenderer model={model.view} tileId={model.tile_id} instanceKey={model.instance_key} />
  {#if model.input && !model.input.live_filter}<InputComposer model={model.input} />{/if}
</article>

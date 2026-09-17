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

<article class="tile-frame" class:focused={model.focused} class:transparent={model.background_transparent} data-instance={model.instance_key}>
  {#if !model.chrome_hidden}
    <header class="tile-header">
      <div class="tile-identity"><span class="tile-signal"></span><strong>{model.title}</strong></div>
      {#if model.tabs.length > 1}<div class="view-tabs" role="tablist">{#each model.tabs as tab, index (tab.tile_id)}<button role="tab" aria-selected={tab.active} class:active={tab.active} onclick={() => dispatch({ type: "activate", intent: { type: "switch_tab", index: BigInt(index) } })}>{tab.title}</button>{/each}</div>{/if}
      <div class="tile-tools">↗ <span aria-hidden="true">⋮</span></div>
    </header>
  {/if}
  {#if model.heading}<div class="content-heading"><small>{model.heading.eyebrow}</small><h1>{model.heading.title}</h1>{#if model.heading.summary}<p>{model.heading.summary}</p>{/if}{#if model.heading.metadata.length}<div class="heading-meta">{model.heading.metadata.join("  /  ")}</div>{/if}</div>{/if}
  <ViewRenderer model={model.view} tileId={model.tile_id} />
  {#if model.input}<InputComposer model={model.input} />{/if}
</article>

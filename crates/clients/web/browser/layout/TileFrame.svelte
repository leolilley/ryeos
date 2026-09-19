<script lang="ts">
  import type { RyeOsLayoutNodeVm } from "../generated";
  import InputComposer from "../components/InputComposer.svelte";
  import ViewSupplement from "../components/ViewSupplement.svelte";
  import ViewRenderer from "../views/ViewRenderer.svelte";
  import { dispatchUi } from "../runtime/context";
  type Tile = Extract<RyeOsLayoutNodeVm, { type: "tile" }>;
  interface Props { model: Tile }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
  let actionSummary = $state<HTMLElement>();
</script>

<article class="tile-frame" class:focused={model.focused} class:transparent={model.background_transparent} data-instance={model.instance_key} data-keyboard-focus={model.focused ? "current" : undefined} data-scroll-key={`tile:${model.instance_key}`} onpointerdown={() => { if (!model.focused) dispatch({ type: "focus_changed", target: model.tile_id }); }} onfocusin={() => { if (!model.focused) dispatch({ type: "focus_changed", target: model.tile_id }); }}>
  {#if !model.chrome_hidden}
    <header class="tile-header" class:grouped={model.group_label}>
      <div class="tile-title-row">
        <div class="tile-identity"><span class="tile-signal"></span><strong>{model.supplement?.frame_label || model.group_label || model.title}</strong>{#if model.supplement?.frame_detail}<span class="tile-frame-detail">{model.supplement.frame_detail}</span>{/if}</div>
        <div class="tile-tools">
        {#if model.attachment_label}<span class="tile-frame-detail" title="Selection attachment">{model.attachment_label}</span>{/if}
        <button
          class="tile-tool"
          aria-label={model.maximized ? "Restore view" : "Maximize view"}
          title={model.maximized ? "Restore view" : "Maximize view"}
          onclick={() => dispatch({ type: "activate", intent: { type: "toggle_tile_maximized", tile_id: model.tile_id } })}
        >{model.maximized ? "↙" : "↗"}</button>
        {#if model.intents.length}
          <details class="tile-action-menu">
            <summary bind:this={actionSummary} aria-label={`Actions for ${model.title}`} title="View actions">⋮</summary>
            <div class="tile-action-list">
              {#each model.intents as action}
                <button title={action.title} onclick={(event) => {
                  const menu = event.currentTarget.closest("details");
                  if (menu) menu.open = false;
                  actionSummary?.focus();
                  dispatch({ type: "activate", intent: action.intent });
                }}>{action.label}</button>
              {/each}
            </div>
          </details>
        {/if}
        </div>
      </div>
      {#if model.tabs.length > 1}<div class="view-tabs" role="tablist">{#each model.tabs as tab (tab.tile_id)}<button role="tab" aria-selected={tab.active} class:active={tab.active} onclick={() => dispatch({ type: "focus_changed", target: tab.tile_id })}>{tab.title}</button>{/each}</div>{/if}
    </header>
  {/if}
  {#if model.heading}<div class="content-heading"><small>{model.heading.eyebrow}</small><h1>{model.heading.title}</h1>{#if model.heading.summary}<p>{model.heading.summary}</p>{/if}{#if model.heading.metadata.length}<div class="heading-meta">{model.heading.metadata.join("  /  ")}</div>{/if}</div>{/if}
  {#if model.input?.live_filter}<InputComposer model={model.input} />{/if}
  {#if model.supplement}<ViewSupplement model={model.supplement} phase="content" />{/if}
  <ViewRenderer model={model.view} tileId={model.tile_id} instanceKey={model.instance_key} />
  {#if model.supplement}<ViewSupplement model={model.supplement} phase="footer" />{/if}
  {#if model.input && !model.input.live_filter}<InputComposer model={model.input} />{/if}
</article>

<script lang="ts">
  import type { RyeOsDockTileVm } from "../generated";
  import InputComposer from "../components/InputComposer.svelte";
  import ViewSupplement from "../components/ViewSupplement.svelte";
  import ViewRenderer from "../views/ViewRenderer.svelte";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsDockTileVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>
<aside class="dock-slot" class:focused={model.focused} data-keyboard-focus={model.focused ? "current" : undefined} data-edge={model.edge} style={`--dock-size:${model.size}`} onpointerdown={() => { if (!model.focused) dispatch({ type: "focus_dock", edge: model.edge }); }} onfocusin={() => { if (!model.focused) dispatch({ type: "focus_dock", edge: model.edge }); }}>
  <header><span>{model.supplement?.frame_label || model.title}</span><span>{model.attachment_label || model.supplement?.frame_detail || model.edge}</span></header>
  {#if model.heading}<div class="content-heading"><small>{model.heading.eyebrow}</small><h1>{model.heading.title}</h1>{#if model.heading.summary}<p>{model.heading.summary}</p>{/if}{#if model.heading.metadata.length}<div class="heading-meta">{model.heading.metadata.join("  /  ")}</div>{/if}</div>{/if}
  {#if model.input?.live_filter}<InputComposer model={model.input} />{/if}
  {#if model.supplement}<ViewSupplement model={model.supplement} phase="content" />{/if}
  <ViewRenderer model={model.view} tileId={String(model.instance_key)} instanceKey={model.instance_key} />
  {#if model.supplement}<ViewSupplement model={model.supplement} phase="footer" />{/if}
  {#if model.input && !model.input.live_filter}<InputComposer model={model.input} />{/if}
</aside>

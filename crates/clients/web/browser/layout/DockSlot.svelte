<script lang="ts">
  import type { RyeOsDockTileVm } from "../generated";
  import InputComposer from "../components/InputComposer.svelte";
  import ViewRenderer from "../views/ViewRenderer.svelte";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsDockTileVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>
<aside class="dock-slot" class:focused={model.focused} data-edge={model.edge} style={`--dock-size:${model.size}`} onpointerdown={() => { if (!model.focused) dispatch({ type: "focus_dock", edge: model.edge }); }} onfocusin={() => { if (!model.focused) dispatch({ type: "focus_dock", edge: model.edge }); }}>
  <header><span>{model.title}</span><span>{model.edge}</span></header>
  <ViewRenderer model={model.view} tileId={String(model.instance_key)} instanceKey={model.instance_key} />
  {#if model.input}<InputComposer model={model.input} />{/if}
</aside>

<script lang="ts">
  import type { RyeOsDockTileVm } from "../generated";
  import InputComposer from "../components/InputComposer.svelte";
  import ViewRenderer from "../views/ViewRenderer.svelte";
  interface Props { model: RyeOsDockTileVm }
  let { model }: Props = $props();
</script>
<aside class="dock-slot" class:focused={model.focused} data-edge={model.edge} style={`--dock-size:${model.size}`}>
  <header><span>{model.title}</span><span>{model.edge}</span></header>
  <ViewRenderer model={model.view} tileId={String(model.instance_key)} />
  {#if model.input}<InputComposer model={model.input} />{/if}
</aside>

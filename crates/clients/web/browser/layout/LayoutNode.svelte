<script lang="ts">
  import type { RyeOsLayoutNodeVm } from "../generated";
  import TileFrame from "./TileFrame.svelte";
  import LayoutNode from "./LayoutNode.svelte";
  interface Props { model: RyeOsLayoutNodeVm }
  let { model }: Props = $props();
</script>

{#if model.type === "tile"}
  <TileFrame model={model} />
{:else}
  <div class="split" data-axis={model.axis} style={`--split:${model.ratio}`}>
    <LayoutNode model={model.first} />
    <div class="split-divider" role="separator" aria-orientation={model.axis === "horizontal" ? "vertical" : "horizontal"}></div>
    <LayoutNode model={model.second} />
  </div>
{/if}

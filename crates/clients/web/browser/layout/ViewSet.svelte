<script lang="ts">
  import type { RyeOsViewSetVm } from "../generated";
  import DockSlot from "./DockSlot.svelte";
  import LayoutNode from "./LayoutNode.svelte";
  import SceneView from "../views/SceneView.svelte";
  interface Props { model: RyeOsViewSetVm }
  let { model }: Props = $props();
  const maximized = $derived(model.root?.type === "tile" && model.root.maximized);
</script>

<main class="view-set" class:empty={model.center_is_empty} class:maximized>
  {#if !maximized && model.docks.top}<DockSlot model={model.docks.top} />{/if}
  <div class="view-set-middle">
    {#if !maximized && model.docks.left}<DockSlot model={model.docks.left} />{/if}
    <section class="view-set-center">
      {#if model.root}
        <LayoutNode model={model.root} />
      {:else if model.backdrop}
        <SceneView scene={model.backdrop} />
      {:else}
        <div class="view-set-backdrop" aria-label="Empty view set"></div>
      {/if}
    </section>
    {#if !maximized && model.docks.right}<DockSlot model={model.docks.right} />{/if}
  </div>
  {#if !maximized && model.docks.bottom}<DockSlot model={model.docks.bottom} />{/if}
</main>

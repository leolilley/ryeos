<script lang="ts">
  import type { RyeOsViewSetVm, RyeOsDockPlaneVm, RyeOsSceneModel } from "../generated";
  import DockSlot from "./DockSlot.svelte";
  import LayoutNode from "./LayoutNode.svelte";
  import SceneView from "../views/SceneView.svelte";
  interface Props { model: RyeOsViewSetVm | null; docks: RyeOsDockPlaneVm; backdrop: RyeOsSceneModel | null }
  let { model, docks, backdrop }: Props = $props();
  const maximized = $derived(model?.root?.type === "tile" && model.root.maximized);
</script>

<main class="view-set" class:empty={!model || model.center_is_empty} class:maximized>
  {#if !maximized && docks.top}
    {#key docks.top.instance_key}<DockSlot model={docks.top} />{/key}
  {/if}
  <div class="view-set-middle">
    {#if !maximized && docks.left}
      {#key docks.left.instance_key}<DockSlot model={docks.left} />{/key}
    {/if}
    <section class="view-set-center">
      {#key model?.view_set_id}
        {#if model?.root}
          <LayoutNode model={model.root} guard={model.layout_guard} minRatio={model.split_min_ratio} maxRatio={model.split_max_ratio} />
        {:else if backdrop}
          <SceneView scene={backdrop} />
        {:else}
          <div class="view-set-backdrop" aria-label="Empty view set"></div>
        {/if}
      {/key}
    </section>
    {#if !maximized && docks.right}
      {#key docks.right.instance_key}<DockSlot model={docks.right} />{/key}
    {/if}
  </div>
  {#if !maximized && docks.bottom}
    {#key docks.bottom.instance_key}<DockSlot model={docks.bottom} />{/key}
  {/if}
</main>

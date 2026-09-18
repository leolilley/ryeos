<script lang="ts">
  import type { RyeOsViewSetVm } from "../generated";
  import DockSlot from "./DockSlot.svelte";
  import LayoutNode from "./LayoutNode.svelte";
  interface Props { model: RyeOsViewSetVm }
  let { model }: Props = $props();
</script>

<main class="view-set" class:empty={model.center_is_empty}>
  {#if model.docks.top}<DockSlot model={model.docks.top} />{/if}
  <div class="view-set-middle">
    {#if model.docks.left}<DockSlot model={model.docks.left} />{/if}
    <section class="view-set-center">
      {#if model.root}<LayoutNode model={model.root} />{:else}<div class="view-set-backdrop" aria-label="Empty view set"></div>{/if}
    </section>
    {#if model.docks.right}<DockSlot model={model.docks.right} />{/if}
  </div>
  {#if model.docks.bottom}<DockSlot model={model.docks.bottom} />{/if}
</main>

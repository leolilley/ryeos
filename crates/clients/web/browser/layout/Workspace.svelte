<script lang="ts">
  import type { RyeOsWorkspaceVm } from "../generated";
  import DockSlot from "./DockSlot.svelte";
  import LayoutNode from "./LayoutNode.svelte";
  interface Props { model: RyeOsWorkspaceVm }
  let { model }: Props = $props();
</script>

<main class="workspace" class:empty={model.center_is_empty}>
  {#if model.docks.top}<DockSlot model={model.docks.top} />{/if}
  <div class="workspace-middle">
    {#if model.docks.left}<DockSlot model={model.docks.left} />{/if}
    <section class="workspace-center">
      {#if model.root}<LayoutNode model={model.root} />{:else}<div class="workspace-backdrop" aria-label="Empty workspace"></div>{/if}
    </section>
    {#if model.docks.right}<DockSlot model={model.docks.right} />{/if}
  </div>
  {#if model.docks.bottom}<DockSlot model={model.docks.bottom} />{/if}
</main>

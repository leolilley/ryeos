<script lang="ts">
  import type { RyeOsStatusBarVm } from "../generated";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsStatusBarVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>

{#if model.visible}
  <footer class="status-bar">
    {#each model.segments as segment}
      <span class:status-spacer={segment.grow} data-tone={segment.tone}>{segment.label ? `${segment.label} ` : ""}{segment.value}</span>
    {/each}
    <span class="key-hint">{model.key_hint}</span>
    <button class="ryeos-launcher" data-focus-key="shell:launch" onclick={() => dispatch({ type: "open_launcher" })}>RyeOS</button>
  </footer>
{/if}

<style>
  .ryeos-launcher { margin-left: auto; align-self: stretch; border: 0; border-radius: 0; padding: 0 10px; color: #282828; background: #fe8019; font: inherit; cursor: pointer; }
</style>

<script lang="ts">
  import type { RyeOsStatusBarVm } from "../generated";
  interface Props { model: RyeOsStatusBarVm }
  let { model }: Props = $props();
</script>

{#if model.visible}
  <footer class="status-bar">
    {#each model.segments as segment}
      <span class:status-spacer={segment.grow} data-tone={segment.tone}>{segment.label ? `${segment.label} ` : ""}{segment.value}</span>
    {/each}
    <span class="key-hint">{model.key_hint}</span>
  </footer>
{/if}

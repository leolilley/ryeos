<script lang="ts">
  import type { RyeOsRowDetailVm } from "../generated";

  interface Props { details: RyeOsRowDetailVm[]; label?: string }
  let { details, label = "Details" }: Props = $props();
</script>

{#if details.length}
  <dl class="row-details" aria-label={label}>
    {#each details as detail, index (`${detail.field}:${index}`)}
      <div data-tone={detail.tone ?? undefined}>
        <dt>{detail.field || "detail"}</dt>
        <dd>{detail.value}</dd>
      </div>
    {/each}
  </dl>
{/if}

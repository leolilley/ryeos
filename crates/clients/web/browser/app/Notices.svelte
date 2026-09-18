<script lang="ts">
  import type { RyeOsNoticeVm } from "../generated";
  import { dispatchUi } from "../runtime/context";
  interface Props { notices: RyeOsNoticeVm[] }
  let { notices }: Props = $props();
  const dispatch = dispatchUi();
</script>

{#if notices.length > 0}
  <aside class="notice-stack" aria-label="RyeOS notices" aria-live="polite" aria-atomic="false">
    {#each notices as notice (notice.id)}
      <div class="notice" data-tone={notice.tone} role={notice.tone === "danger" ? "alert" : "status"}>
        <span>{notice.message}</span>
        <button data-focus-key={`notice:${notice.id}:dismiss`} aria-label="Dismiss notice" onclick={() => dispatch({ type: "dismiss_notice", id: notice.id })}>×</button>
      </div>
    {/each}
  </aside>
{/if}

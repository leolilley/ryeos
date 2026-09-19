<script lang="ts">
  import type { RyeOsTopBarVm } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props { model: RyeOsTopBarVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>

{#if model.visible}
  <nav class="view-set-strip" aria-label="View sets">
    {#each model.tabs as tab (tab.view_set_id)}
      <div class="view-set-tab" class:active={tab.active}>
        <button
          class="view-set-select"
          aria-current={tab.active ? "page" : undefined}
          onclick={() => dispatch({ type: "activate", intent: { type: "select_view_set", view_set_id: tab.view_set_id } })}
        >
          <span class="ordinal">{String(tab.number).padStart(2, "0")}</span>
          <span>{tab.title}</span>
        </button>
        {#if tab.active}
          <span class="view-set-actions" aria-label={`${tab.title} view-set actions`}>
            <button
              aria-label={`Duplicate ${tab.title}`}
              title="Duplicate view set"
              onclick={() => dispatch({ type: "activate", intent: { type: "duplicate_view_set", view_set_id: tab.view_set_id } })}
            >⧉</button>
            <button
              aria-label={`Close ${tab.title}`}
              title="Close view set"
              onclick={() => dispatch({ type: "activate", intent: { type: "close_view_set", view_set_id: tab.view_set_id } })}
            >×</button>
          </span>
        {/if}
      </div>
    {/each}
    <button class="new-view-set" aria-label="New view set" onclick={() => dispatch({ type: "activate", intent: { type: "new_view_set" } })}>＋</button>
  </nav>
{/if}

<script lang="ts">
  import type { RyeOsTopBarVm } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props { model: RyeOsTopBarVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>

{#if model.visible}
  <nav class="workspace-strip" aria-label="Workspaces">
    {#each model.tabs as tab (tab.workspace_id)}
      <button
        class:active={tab.active}
        aria-current={tab.active ? "page" : undefined}
        onclick={() => dispatch({ type: "activate", intent: { type: "select_workspace", workspace_id: tab.workspace_id } })}
      >
        <span class="ordinal">{String(tab.number).padStart(2, "0")}</span>
        <span>{tab.title}</span>
      </button>
    {/each}
    <button class="new-workspace" aria-label="New workspace" onclick={() => dispatch({ type: "activate", intent: { type: "new_workspace" } })}>＋</button>
  </nav>
{/if}

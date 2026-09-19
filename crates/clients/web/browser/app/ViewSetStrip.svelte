<script lang="ts">
  import type { RyeOsTopBarVm } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props { model: RyeOsTopBarVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
  // Only the unfinished text edit lives in the renderer. Committing a name
  // goes through the shared reducer and its existing persistence owner.
  let editingId = $state<bigint | null>(null);
  let draft = $state("");
  function finishRename() {
    const title = draft.trim();
    if (editingId !== null && title) {
      dispatch({ type: "activate", intent: { type: "rename_view_set", view_set_id: editingId, title } });
    }
    editingId = null;
  }
  $effect(() => {
    if (editingId !== null && !model.tabs.some(tab => tab.view_set_id === editingId && tab.active)) editingId = null;
  });
</script>

{#if model.visible}
  <nav class="view-set-strip" aria-label="View sets">
    {#each model.tabs as tab (tab.view_set_id)}
      <div class="view-set-tab" class:active={tab.active}>
        {#if editingId === tab.view_set_id}
          <form class="view-set-rename" onsubmit={(event) => { event.preventDefault(); finishRename(); }}>
            <input aria-label="View set name" bind:value={draft} onkeydown={(event) => { event.stopPropagation(); if (event.key === "Escape") { event.preventDefault(); editingId = null; } }} />
            <button type="submit" aria-label="Save view set name">✓</button>
            <button type="button" aria-label="Cancel rename" onclick={() => editingId = null}>×</button>
          </form>
        {:else}
        <button
          class="view-set-select"
          aria-current={tab.active ? "page" : undefined}
          onclick={() => dispatch({ type: "activate", intent: { type: "select_view_set", view_set_id: tab.view_set_id } })}
        >
          <span class="ordinal">{String(tab.number).padStart(2, "0")}</span>
          <span>{tab.title}</span>
        </button>
        {/if}
        {#if tab.active}
          <span class="view-set-actions" role="group" aria-label={`${tab.title} view-set actions`}>
            <button aria-label={`Rename ${tab.title}`} title="Rename view set" onclick={() => { draft = tab.title; editingId = tab.view_set_id; }}>✎</button>
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
    <div class="view-set-context" aria-label="Focused view and layout">
      <span title={model.focused_title}>{model.focused_title}</span>
      <span aria-label="Layout">{model.layout_symbol}</span>
    </div>
  </nav>
{/if}

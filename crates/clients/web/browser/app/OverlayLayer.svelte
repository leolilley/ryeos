<script lang="ts">
  import { onMount } from "svelte";
  import type { RyeOsOverlayVm } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props { model: RyeOsOverlayVm }
  let { model }: Props = $props();
  let queryInput: HTMLInputElement;
  let panel: HTMLElement;
  const dispatch = dispatchUi();

  onMount(() => queryInput?.focus());

  function select(itemId: string): void {
    dispatch({ type: "set_overlay_selection", item_id: itemId });
  }

  function choose(itemId: string, secondary: boolean): void {
    dispatch({ type: "choose_overlay_at", item_id: itemId, secondary });
  }

  function handleKey(event: KeyboardEvent): void {
    if (event.isComposing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      dispatch({ type: "close_overlay" });
    } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      dispatch({ type: "move_overlay_selection", delta: event.key === "ArrowDown" ? 1 : -1 });
    } else if (event.key === "Enter" && event.target === queryInput) {
      event.preventDefault();
      dispatch({ type: "choose_overlay", secondary: event.shiftKey || event.altKey });
    } else if (event.key === "Tab" && panel) {
      const focusable = [...panel.querySelectorAll<HTMLElement>('input,button:not([disabled])')];
      if (focusable.length === 0) return;
      const current = focusable.indexOf(document.activeElement as HTMLElement);
      const next = event.shiftKey
        ? (current <= 0 ? focusable.length - 1 : current - 1)
        : (current >= focusable.length - 1 ? 0 : current + 1);
      event.preventDefault();
      focusable[next]?.focus();
    }
  }
</script>

<div class="overlay-scrim" role="presentation" onclick={(event) => event.target === event.currentTarget && dispatch({ type: "close_overlay" })}>
  <div class="overlay-panel" bind:this={panel} role="dialog" aria-modal="true" aria-labelledby={`overlay-title-${model.id}`} tabindex="-1" onkeydown={handleKey}>
    <header>
      <div><small>{model.widget}</small><h2 id={`overlay-title-${model.id}`}>{model.title}</h2></div>
      <button class="overlay-close" aria-label={`Close ${model.title}`} onclick={() => dispatch({ type: "close_overlay" })}>×</button>
    </header>
    <input
      bind:this={queryInput}
      class="overlay-query"
      data-focus-key={`overlay:${model.id}:query`}
      type="search"
      value={model.query}
      aria-label={`Filter ${model.title}`}
      autocomplete="off"
      spellcheck="false"
      oninput={(event) => dispatch({ type: "set_overlay_query", query: event.currentTarget.value })}
    />
    {#if model.columns.length > 0}
      <div class="overlay-columns" style={`--columns:${model.columns.length}`}>{#each model.columns as column}<span>{column}</span>{/each}</div>
    {/if}
    <div class="overlay-items" role="listbox" aria-label={model.title}>
      {#each model.items as item, index (`${item.category}:${item.primary}:${index}`)}
        <div class="overlay-item" class:selected={model.selected === BigInt(index)} class:header={item.header} style={`--depth:${item.depth}`}>
          <button
            role="option"
            aria-selected={model.selected === BigInt(index)}
            aria-describedby={item.disabled_reason ? `overlay-reason-${item.id}` : undefined}
            data-focus-key={`overlay:${model.id}:item:${item.id}`}
            disabled={!item.enabled}
            onclick={() => choose(item.id, false)}
            onpointerenter={() => select(item.id)}
          >
            <span class="overlay-primary">{#if item.header}{item.expanded ? "▾" : "▸"} {/if}{item.primary}</span>
            {#if item.secondary && !item.disabled_reason}<span class="overlay-secondary">{item.secondary}</span>{/if}
            {#if item.disabled_reason}<span id={`overlay-reason-${item.id}`} class="overlay-secondary overlay-disabled-reason">{item.disabled_reason}</span>{/if}
            {#if item.meta}<small>{item.meta}</small>{/if}
          </button>
          {#if item.secondary_intent && item.enabled}
            <button class="overlay-secondary-action" data-focus-key={`overlay:${model.id}:item:${item.id}:alternate`} aria-label={`Alternate action for ${item.primary}`} onclick={() => choose(item.id, true)}>↗</button>
          {/if}
        </div>
      {:else}
        <p class="overlay-empty">No matching entries</p>
      {/each}
    </div>
    {#if model.hint}<footer>{model.hint}</footer>{/if}
  </div>
</div>

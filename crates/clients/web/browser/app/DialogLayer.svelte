<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import type { RyeOsViewModel } from "../generated";
  import InputComposer from "../components/InputComposer.svelte";
  import ViewRenderer from "../views/ViewRenderer.svelte";
  import { dispatchUi } from "../runtime/context";
  import { dialogFocusables, restoreDialogFocus, trapDialogTab } from "../runtime/dialog-focus";

  interface Props { model: RyeOsViewModel["dialogs"][number] }
  let { model }: Props = $props();
  let panel: HTMLElement;
  let opener: HTMLElement | null = null;
  const dispatch = dispatchUi();
  const close = () => dispatch({ type: "close_dialog", instance_key: model.instance_key });

  onMount(() => {
    const active = panel.ownerDocument.activeElement;
    opener = active instanceof HTMLElement ? active : null;
    const input = panel.querySelector<HTMLElement>('textarea:not([disabled]),input:not([disabled])');
    (input ?? dialogFocusables(panel)[0] ?? panel).focus();
  });
  onDestroy(() => { if (panel) restoreDialogFocus(panel, opener); });

  function handleKey(event: KeyboardEvent): void {
    if (event.isComposing) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close();
    } else if (event.key === "Tab") trapDialogTab(panel, event);
    // All other keys belong to the mounted ordinary View/input and shared
    // Rust keymap. This frame has no second query, cursor or action catalogue.
  }
</script>

<div class="dialog-scrim" role="presentation" onclick={(event) => event.target === event.currentTarget && close()}>
  <div class="dialog-panel" bind:this={panel} role="dialog" aria-modal="true"
    aria-labelledby={`dialog-title-${model.instance_key}`} tabindex="-1" onkeydown={handleKey}>
    <header>
      <h2 id={`dialog-title-${model.instance_key}`}>{model.title}</h2>
      <button class="dialog-close" data-focus-key={`dialog:${model.instance_key}:close`}
        aria-label={`Close ${model.title}`} onclick={close}>×</button>
    </header>
    <div class="dialog-content">
      {#if model.input?.live_filter}<InputComposer model={model.input} />{/if}
      <ViewRenderer model={model.view} tileId={String(model.instance_key)} instanceKey={model.instance_key} />
      {#if model.input && !model.input.live_filter}<InputComposer model={model.input} />{/if}
    </div>
  </div>
</div>

<script lang="ts">
  import { tick } from "svelte";
  import type { RyeOsNavigationVm } from "../generated";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsNavigationVm; open: boolean; onClose: () => void }
  let { model, open, onClose }: Props = $props();
  const dispatch = dispatchUi();
  let panel: HTMLElement;

  $effect(() => {
    if (open) {
      void tick().then(() => {
        panel?.querySelector<HTMLElement>("button:not([disabled])")?.focus();
      });
    }
  });

  function handleKeys(event: KeyboardEvent): void {
    if (!open) return;
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key !== "Tab") return;
    const controls = Array.from(panel.querySelectorAll<HTMLElement>("button:not([disabled]), [href], [tabindex]:not([tabindex='-1'])"));
    if (controls.length === 0) return;
    const first = controls.at(0);
    const last = controls.at(-1);
    if (!first || !last) return;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }
</script>

<svelte:window onkeydown={handleKeys} />
<button class="navigation-scrim" class:open aria-label="Close view navigation" tabindex={open ? 0 : -1} onclick={onClose}></button>
<aside id="ryeos-navigation" bind:this={panel} class="navigation" class:open aria-label="RyeOS view navigation">
  <div class="navigation-heading">Explorer <span>{String(model.items.length).padStart(2, "0")}</span></div>
  <nav>
    {#each model.items as item (item.id)}
      <button class:active={item.selected} aria-current={item.selected ? "page" : undefined} onclick={() => { dispatch({ type: "activate", intent: item.intent }); onClose(); }}>
        <span class="navigation-glyph" aria-hidden="true">◇</span>
        <span class="navigation-copy"><strong>{item.label}</strong></span>
      </button>
    {/each}
  </nav>
</aside>

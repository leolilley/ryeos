<script lang="ts">
  import { tick } from "svelte";
  import type { RyeOsChromeVm, RyeOsSessionVm, RyeOsSurfaceChoiceVm, RyeOsTransportVm } from "../generated";
  import { dispatchUi } from "../runtime/context";

  interface Props {
    chrome: RyeOsChromeVm;
    session: RyeOsSessionVm;
    transport: RyeOsTransportVm;
    navigationAvailable: boolean;
    navigationOpen: boolean;
    onToggleNavigation: (opener: HTMLElement) => void;
  }

  let { chrome, session, transport, navigationAvailable, navigationOpen, onToggleNavigation }: Props = $props();
  const dispatch = dispatchUi();
  let contextRoot: HTMLDivElement;
  let renameTarget = $state<RyeOsSurfaceChoiceVm | null>(null);
  let renameDraft = $state("");
  let renameInput = $state<HTMLInputElement>();
  async function beginRename(surface: RyeOsSurfaceChoiceVm): Promise<void> {
    renameTarget = surface;
    renameDraft = surface.title;
    await tick();
    if (renameTarget?.surface_instance_id === surface.surface_instance_id) {
      renameInput?.focus();
      renameInput?.select();
    }
  }
  async function cancelRename(): Promise<void> {
    const id = renameTarget?.surface_instance_id;
    renameTarget = null;
    await tick();
    if (id) contextRoot?.querySelector<HTMLElement>(`[data-surface-select="${id}"]`)?.focus();
  }
  $effect(() => {
    if (renameTarget && !session.surfaces.some(surface =>
      surface.surface_instance_id === renameTarget?.surface_instance_id && surface.rename_intent)) {
      renameTarget = null;
    }
  });

  function submitRename(): void {
    const intent = renameTarget?.rename_intent;
    if (!intent || intent.type !== "rename_surface") return;
    dispatch({ type: "activate", intent: { ...intent, title: renameDraft } });
    void cancelRename();
  }

  function dismissSurfacePicker(details: HTMLDetailsElement, restoreFocus = true): void {
    details.removeAttribute("open");
    if (restoreFocus) details.querySelector<HTMLElement>("summary")?.focus();
  }

  function handleSurfacePickerKey(event: KeyboardEvent & { currentTarget: HTMLElement }): void {
    if (event.key !== "Escape" || event.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    event.preventDefault();
    event.stopPropagation();
    // Chrome owns Escape even when its summary is focused after dismissal.
    // Letting a closed disclosure bubble would close the focused RyeOS View.
    const picker = event.currentTarget.closest("details");
    if (picker?.open) dismissSurfacePicker(picker);
  }
</script>

<header class="system-bar">
  <div class="system-context" role="group" aria-label="Surface context" tabindex="-1" bind:this={contextRoot}>
    {#if session.surfaces.length > 0}
      <details class="surface-picker">
        <summary aria-label="Select an open Surface" onkeydown={handleSurfacePickerKey}>
          {#if session.surface_ordinal !== null}<span class="surface-number">{String(session.surface_ordinal).padStart(2, "0")}</span>{/if}
          <span>{session.title}</span><span class="surface-chevron" aria-hidden="true">⌄</span>
        </summary>
        <div class="surface-choices" role="group" aria-label="Open Surfaces">
          {#each session.surfaces as surface (surface.surface_instance_id)}
            <div class="surface-choice" class:active={surface.active}>
              {#if renameTarget?.surface_instance_id === surface.surface_instance_id}
                <form onsubmit={(event) => { event.preventDefault(); submitRename(); }}>
                  <input aria-label="Surface name" bind:this={renameInput} bind:value={renameDraft} onkeydown={(event) => {
                    event.stopPropagation();
                    if (event.key === "Escape" && !event.isComposing) { event.preventDefault(); void cancelRename(); }
                  }} />
                  <button type="submit" onkeydown={handleSurfacePickerKey}>Save</button>
                  <button type="button" onkeydown={handleSurfacePickerKey} onclick={cancelRename}>Cancel</button>
                </form>
              {:else}
              <button data-surface-select={surface.surface_instance_id} aria-current={surface.active ? "true" : undefined} onkeydown={handleSurfacePickerKey} onclick={(event) => {
                const picker = event.currentTarget.closest("details");
                if (picker) dismissSurfacePicker(picker);
                dispatch({ type: "activate", intent: surface.select_intent });
              }}><span class="surface-number">{String(surface.ordinal).padStart(2, "0")}</span>{surface.title}</button>
              <button disabled={!surface.rename_intent} aria-label={`Rename ${surface.title}`} onkeydown={handleSurfacePickerKey} onclick={() => beginRename(surface)}>✎</button>
              {/if}
              <button class="surface-close" disabled={!surface.close_intent} aria-label={`Close ${surface.title}; running work is unaffected`} onkeydown={handleSurfacePickerKey} onclick={(event) => {
                if (!surface.close_intent) return;
                const picker = event.currentTarget.closest("details");
                const closingLast = session.surfaces.length === 1;
                if (picker) dismissSurfacePicker(picker, !closingLast);
                // The last close removes the picker summary on the next render.
                // Focus its persistent chrome parent before that can happen.
                if (closingLast) contextRoot?.focus();
                dispatch({ type: "activate", intent: surface.close_intent });
              }}>×</button>
            </div>
          {/each}
        </div>
      </details>
    {:else}<span>{session.title}</span>{/if}
  </div>
  <div class="system-state">
    <span>{chrome.health_label}</span>
    <span class="transport" data-freshness={transport.freshness}>{transport.freshness}</span>
    {#if navigationAvailable}
      <button class="system-navigation" aria-controls="ryeos-navigation" aria-expanded={navigationOpen} onclick={(event) => onToggleNavigation(event.currentTarget)}>Views</button>
    {/if}
  </div>
</header>

<style>
  .system-bar { grid-template-columns: minmax(0, 1fr) auto; min-height: 24px; height: 24px; padding: 0 12px; font-size: 11px; }
  .system-context { gap: 10px; }
  .system-context:focus-visible { outline: 1px solid var(--accent); outline-offset: -1px; }
  .surface-number { color: var(--accent, #fe8019); }
  .surface-picker { position: relative; min-width: 0; }
  .surface-picker summary { display: flex; align-items: center; gap: 10px; cursor: pointer; list-style: none; }
  .surface-picker summary::-webkit-details-marker { display: none; }
  .surface-chevron { color: var(--muted); }
  .surface-choices { position: absolute; z-index: 50; top: calc(100% + 6px); left: -4px; min-width: 250px; max-width: min(480px, 85vw); padding: 4px; border: 1px solid var(--line); background: var(--plane); }
  .surface-choice { display: flex; align-items: center; }
  .surface-choice form { display: flex; align-items: center; flex: 1; gap: 4px; min-width: 0; }
  .surface-choice input { min-width: 0; flex: 1; color: var(--text); background: var(--plane); border: 1px solid var(--line); font: inherit; padding: 4px 8px; }
  .surface-choice button { background: transparent; border: 0; color: var(--text); min-height: 30px; }
  .surface-choice > button:first-child { flex: 1; display: flex; align-items: center; gap: 10px; padding: 0 8px; text-align: left; }
  .surface-choice.active { background: color-mix(in srgb, var(--accent) 12%, transparent); }
  .surface-choice button:focus-visible { outline: 1px solid var(--accent); outline-offset: -1px; }
  .surface-choice .surface-close { padding: 0 8px; color: var(--muted); }
  .surface-close:disabled { opacity: 0.35; }
</style>

<script lang="ts">
  import { tick } from "svelte";
  import type { RyeOsEnvelope, RyeOsUiEvent } from "../generated";
  import Navigation from "./Navigation.svelte";
  import Notices from "./Notices.svelte";
  import OverlayLayer from "./OverlayLayer.svelte";
  import AmbientLayer from "./AmbientLayer.svelte";
  import StatusBar from "./StatusBar.svelte";
  import SystemBar from "./SystemBar.svelte";
  import ViewSetStrip from "./ViewSetStrip.svelte";
  import ViewSet from "../layout/ViewSet.svelte";
  import { provideDispatchUi } from "../runtime/context";

  interface Props {
    initialEnvelope: RyeOsEnvelope;
    dispatchUi: (event: RyeOsUiEvent) => void;
  }

  let { initialEnvelope, dispatchUi }: Props = $props();
  let replacement = $state<RyeOsEnvelope | null>(null);
  let navigationOpen = $state(false);
  let navigationOpener: HTMLElement | null = null;
  let envelope = $derived(replacement ?? initialEnvelope);
  provideDispatchUi((event) => dispatchUi(event));

  /** Replace the sole renderer-owned root cell with one committed envelope. */
  export function replaceEnvelope(next: RyeOsEnvelope): void {
    replacement = next;
  }

  function toggleNavigation(opener: HTMLElement): void {
    if (navigationOpen) {
      closeNavigation();
      return;
    }
    navigationOpener = opener;
    navigationOpen = true;
  }

  function closeNavigation(): void {
    const active = document.activeElement;
    const panel = document.getElementById("ryeos-navigation");
    const ownsFocus = active === document.body
      || active === null
      || (active instanceof Element && (panel?.contains(active) || active.classList.contains("navigation-scrim")));
    navigationOpen = false;
    if (ownsFocus && navigationOpener?.isConnected) {
      const opener = navigationOpener;
      void tick().then(() => opener.focus());
    }
  }
</script>

<div class="ryeos-shell" class:without-set-strip={!envelope.view_model.presentation.chrome.top_bar.visible} class:without-status-bar={!envelope.view_model.presentation.chrome.status_bar.visible} data-generation={String(envelope.generation)} data-theme={envelope.view_model.presentation.theme.id}>
  {#if envelope.view_model.session.ambient.show_background}
    <AmbientLayer ambient={envelope.view_model.session.ambient} scene={envelope.scene_model} />
  {/if}
  <SystemBar
    chrome={envelope.view_model.chrome}
    session={envelope.view_model.session}
    transport={envelope.view_model.transport}
    navigationAvailable={envelope.view_model.navigation.items.length > 0}
    {navigationOpen}
    onToggleNavigation={toggleNavigation}
  />
  <ViewSetStrip model={envelope.view_model.presentation.chrome.top_bar} />
  <div class="shell-body" class:with-navigation={envelope.view_model.navigation.items.length > 0}>
    {#if envelope.view_model.navigation.items.length > 0}<Navigation model={envelope.view_model.navigation} open={navigationOpen} onClose={closeNavigation} />{/if}
    <ViewSet model={envelope.view_model.view_set} />
  </div>
  <StatusBar model={envelope.view_model.presentation.chrome.status_bar} />
  <Notices notices={envelope.view_model.notices} />
  {#each envelope.view_model.overlays as overlay (overlay.id)}
    <OverlayLayer model={overlay} />
  {/each}
</div>

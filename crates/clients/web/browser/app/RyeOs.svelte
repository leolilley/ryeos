<script lang="ts">
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
  let envelope = $derived(replacement ?? initialEnvelope);
  provideDispatchUi((event) => dispatchUi(event));

  /** Replace the sole renderer-owned root cell with one committed envelope. */
  export function replaceEnvelope(next: RyeOsEnvelope): void {
    replacement = next;
  }
</script>

<div class="ryeos-shell" data-generation={String(envelope.generation)} data-theme={envelope.view_model.presentation.theme.id}>
  {#if envelope.view_model.session.ambient.show_background}
    <AmbientLayer ambient={envelope.view_model.session.ambient} scene={envelope.scene_model} />
  {/if}
  <SystemBar chrome={envelope.view_model.chrome} session={envelope.view_model.session} transport={envelope.view_model.transport} />
  <ViewSetStrip model={envelope.view_model.presentation.chrome.top_bar} />
  <div class="shell-body" class:with-navigation={envelope.view_model.navigation.items.length > 0}>
    {#if envelope.view_model.navigation.items.length > 0}<Navigation model={envelope.view_model.navigation} />{/if}
    <ViewSet model={envelope.view_model.view_set} />
  </div>
  <StatusBar model={envelope.view_model.presentation.chrome.status_bar} />
  <Notices notices={envelope.view_model.notices} />
  {#each envelope.view_model.overlays as overlay (overlay.id)}
    <OverlayLayer model={overlay} />
  {/each}
</div>

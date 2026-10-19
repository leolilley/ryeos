<script lang="ts">
  import { onMount } from "svelte";
  import type { RyeOsArtifactPreviewVm } from "../generated";
  import { drawIndexedGrid, indexedGridAccessibilityLabel } from "../visuals/ryeos_grid_canvas.js";
  interface Props { preview: RyeOsArtifactPreviewVm; oncompare: () => void; compareEnabled: boolean }
  let { preview, oncompare, compareEnabled }: Props = $props();
  let canvas: HTMLCanvasElement;
  const label = $derived(indexedGridAccessibilityLabel(preview, preview.label));
  onMount(() => drawIndexedGrid(canvas, preview, { scale: 9 }));
  $effect(() => { if (canvas) drawIndexedGrid(canvas, preview, { scale: 9 }); });
</script>
<figure class="field-preview">
  <figcaption>{preview.label}</figcaption>
  <canvas bind:this={canvas} aria-label={label} title={compareEnabled ? "Shift-click to compare" : undefined}
    onclick={(event) => { if (event.shiftKey && compareEnabled) oncompare(); }}></canvas>
</figure>

<script lang="ts">
  import { onMount } from "svelte";
  import type { RyeOsArtifactPreviewVm } from "../generated";
  import { drawIndexedGrid, indexedGridAccessibilityLabel, indexedGridTranscriptPage } from "../visuals/ryeos_grid_canvas.js";
  interface Props { preview: RyeOsArtifactPreviewVm; oncompare: () => void; compareEnabled: boolean }
  let { preview, oncompare, compareEnabled }: Props = $props();
  let canvas: HTMLCanvasElement;
  let transcriptOpen = $state(false);
  let transcriptRow = $state(0);
  const label = $derived(indexedGridAccessibilityLabel(preview, preview.label));
  const transcript = $derived(transcriptOpen ? indexedGridTranscriptPage(preview, transcriptRow) : null);
  onMount(() => drawIndexedGrid(canvas, preview, { scale: 9 }));
  $effect(() => { if (canvas) drawIndexedGrid(canvas, preview, { scale: 9 }); });
</script>
<figure class="field-preview">
  <figcaption>{preview.label}</figcaption>
  <canvas bind:this={canvas} aria-label={label} title={compareEnabled ? "Shift-click to compare" : undefined}
    onclick={(event) => { if (event.shiftKey && compareEnabled) oncompare(); }}></canvas>
  <button type="button" class="field-grid-transcript-toggle" aria-expanded={transcriptOpen}
    onclick={() => { transcriptOpen = !transcriptOpen; transcriptRow = 0; }}>
    {transcriptOpen ? "Hide grid cells" : "Read grid cells"}
  </button>
  {#if transcriptOpen && transcript}
    <section class="field-grid-transcript" aria-label={`${preview.label} cells`}>
      <div class="field-grid-transcript-nav">
        <button type="button" disabled={transcript.start === 0}
          onclick={() => { transcriptRow = Math.max(0, transcript.start - 16); }}>Previous rows</button>
        <span aria-live="polite">Rows {transcript.start + 1}–{transcript.end} of {transcript.height}</span>
        <button type="button" disabled={transcript.end === transcript.height}
          onclick={() => { transcriptRow = transcript.end; }}>Next rows</button>
      </div>
      <div class="field-grid-transcript-rows">
        {#each transcript.rows as row (row.number)}
          <p><strong>Row {row.number}</strong><span>{row.cells.join(" | ")}</span></p>
        {/each}
      </div>
    </section>
  {/if}
</figure>

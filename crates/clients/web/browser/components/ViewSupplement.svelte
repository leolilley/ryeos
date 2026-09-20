<script lang="ts">
  import type { RyeOsViewSupplementVm } from "../generated";
  import SceneView from "../views/SceneView.svelte";

  interface Props {
    model: RyeOsViewSupplementVm;
    phase: "content" | "footer";
  }

  let { model, phase }: Props = $props();
</script>

{#if phase === "content" && model.scene}
  <section class="view-supplement-scene" aria-label="View topology">
    <SceneView scene={model.scene} />
  </section>
{:else if phase === "footer"}
  {#if model.excerpt.length}
    <section class="view-supplement-excerpt" aria-label={model.excerpt_title || "Excerpt"}>
      {#if model.excerpt_title}<header>{model.excerpt_title}</header>{/if}
      <div class="view-supplement-code">
        {#each model.excerpt as row}
          <div data-tone={row.tone}><span>{row.field}</span><code>{row.value}</code></div>
        {/each}
      </div>
    </section>
  {/if}
  {#if model.footer || model.footer_rows.length}
    <footer class="view-supplement-footer">
      {#if model.footer}<strong>{model.footer}</strong>{/if}
      {#each model.footer_rows as row}
        <div data-tone={row.tone}><span>{row.field}</span><small>{row.value}</small></div>
      {/each}
    </footer>
  {/if}
{/if}

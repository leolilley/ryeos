<script lang="ts">
  import type { RyeOsNavigationVm } from "../generated";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsNavigationVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
</script>

<aside class="navigation" aria-label="RyeOS navigation">
  <div class="navigation-heading">Explorer <span>{String(model.items.length).padStart(2, "0")}</span></div>
  <nav>
    {#each model.items as item (item.id)}
      <button class:active={item.selected} onclick={() => dispatch({ type: "activate", intent: item.intent })}>
        <span class="navigation-glyph" aria-hidden="true">◇</span>
        <span class="navigation-copy"><strong>{item.label}</strong></span>
      </button>
    {/each}
  </nav>
</aside>

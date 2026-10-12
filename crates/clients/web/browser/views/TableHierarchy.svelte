<script lang="ts">
  import type { RyeOsTableHierarchyVm } from "../generated";
  interface Props { hierarchy: RyeOsTableHierarchyVm; label: string; focusKey: string; onToggle: () => void }
  let { hierarchy, label, focusKey, onToggle }: Props = $props();
</script>

<!-- Structure comes from Core's retained tree projection, never inferred here. -->
<span class="table-hierarchy">
  {#each hierarchy.ancestor_continues ?? [] as continues}<span aria-hidden="true">{continues ? "│" : "\u00a0"}</span>{/each}
  <span aria-hidden="true">{hierarchy.is_last ? "└" : "├"}</span>
  {#if hierarchy.has_children}
    <button class="tree-fold" data-focus-key={focusKey} aria-label={`${hierarchy.collapsed ? "Expand" : "Collapse"} descendants of ${label}`} aria-expanded={!hierarchy.collapsed} onclick={onToggle}>{hierarchy.collapsed ? "▸" : "▾"}</button>
  {:else}
    <span aria-hidden="true">─</span>
  {/if}
</span>

<style>
  .table-hierarchy { flex: none; align-items: center; padding-left: 9px; }
  .tree-fold { min-width: 20px; padding: 4px; color: var(--muted); }
  .tree-fold:hover, .tree-fold:focus-visible { color: var(--yellow); background: var(--plane-selected); }
</style>

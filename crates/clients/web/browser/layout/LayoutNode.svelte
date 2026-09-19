<script lang="ts">
  import type { RyeOsLayoutNodeVm, SplitBranch } from "../generated";
  import { dispatchUi } from "../runtime/context";
  import TileFrame from "./TileFrame.svelte";
  import LayoutNode from "./LayoutNode.svelte";
  interface Props { model: RyeOsLayoutNodeVm; guard: string; minRatio: number; maxRatio: number; path?: SplitBranch[] }
  let { model, guard, minRatio, maxRatio, path = [] }: Props = $props();
  const dispatch = dispatchUi();
  let region = $state<HTMLDivElement>();
  let drag: { pointer: number; guard: string; path: SplitBranch[]; start: number; extent: number; horizontal: boolean; min: number; max: number } | null = null;
  function resize(ratio: number, layoutGuard = guard, splitPath = path) {
    dispatch({ type: "activate", intent: { type: "resize_split", layout_guard: layoutGuard, path: splitPath, ratio } });
  }
  function begin(event: PointerEvent) {
    if (event.button !== 0 || model.type !== "split" || !region) return;
    const rect = region.getBoundingClientRect();
    const horizontal = model.axis === "horizontal";
    drag = { pointer: event.pointerId, guard, path: [...path], start: horizontal ? rect.left : rect.top, extent: horizontal ? rect.width : rect.height, horizontal, min: minRatio, max: maxRatio };
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    event.preventDefault();
  }
  function end(event: PointerEvent) {
    if (!drag || drag.pointer !== event.pointerId) return;
    const retained = drag;
    drag = null;
    if (retained.extent <= 0) return;
    const position = retained.horizontal ? event.clientX : event.clientY;
    resize(Math.max(retained.min, Math.min(retained.max, (position - retained.start) / retained.extent)), retained.guard, retained.path);
  }
  function key(event: KeyboardEvent) {
    if (model.type !== "split") return;
    const negative = model.axis === "horizontal" ? "ArrowLeft" : "ArrowUp";
    const positive = model.axis === "horizontal" ? "ArrowRight" : "ArrowDown";
    if (![negative, positive, "Home", "End"].includes(event.key)) return;
    event.preventDefault(); event.stopPropagation();
    const ratio = event.key === "Home" ? minRatio : event.key === "End" ? maxRatio : model.ratio + (event.key === negative ? -0.05 : 0.05);
    resize(Math.max(minRatio, Math.min(maxRatio, ratio)));
  }
</script>

{#if model.type === "tile"}
  <TileFrame model={model} />
{:else}
  <div bind:this={region} class="split" data-axis={model.axis} style={`--split:${model.ratio}fr;--split-rest:${1 - model.ratio}fr`}>
    <LayoutNode model={model.first} {guard} {minRatio} {maxRatio} path={[...path, "first"]} />
    <!-- A focusable separator is the ARIA window-splitter pattern: arrows,
         Home/End and a bounded value resize the adjacent panes. -->
    <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
    <div class="split-divider" role="separator" tabindex="0" aria-label="Resize views" aria-valuemin={Math.round(minRatio * 100)} aria-valuemax={Math.round(maxRatio * 100)} aria-valuenow={Math.round(model.ratio * 100)} aria-orientation={model.axis === "horizontal" ? "vertical" : "horizontal"} onpointerdown={begin} onpointerup={end} onpointercancel={() => drag = null} onlostpointercapture={() => drag = null} onkeydown={key}></div>
    <LayoutNode model={model.second} {guard} {minRatio} {maxRatio} path={[...path, "second"]} />
  </div>
{/if}

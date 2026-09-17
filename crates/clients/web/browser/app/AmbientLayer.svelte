<script lang="ts">
  import { onMount } from "svelte";
  import type { RyeOsAmbientVm, RyeOsSceneModel } from "../generated";
  import { mountRyeOsAmbientScene, type AmbientController } from "../visuals/ryeos_ambient_scene.js";

  interface Props { ambient: RyeOsAmbientVm; scene: RyeOsSceneModel }
  let { ambient, scene }: Props = $props();
  let canvas: HTMLCanvasElement;
  let controller: AmbientController | null = null;
  const options = $derived({
    mode: ambient.mode,
    ...(ambient.atlas ? { atlasStyle: ambient.atlas.style } : {}),
  });

  onMount(() => {
    controller = mountRyeOsAmbientScene(canvas, scene, options);
    return () => { controller?.dispose(); controller = null; };
  });

  $effect(() => { controller?.update(scene, options); });
</script>

<div class="ambient-layer" style={`--ambient-opacity:${ambient.opacity ?? 1}`} aria-hidden="true">
  <canvas bind:this={canvas}></canvas>
</div>

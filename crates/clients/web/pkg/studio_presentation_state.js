export function presentationState(vm, scene) {
  const workspace = vm.workspace || {};
  const motion = vm.presentation?.motion || [];
  const tileCount = workspace.tile_count || 0;
  const motionCount = motion.length;
  const opticEnergy = clamp01((tileCount + motionCount) / 6);

  return {
    mode: workspace.is_home ? "home" : "workspace",
    theme: vm.presentation?.theme?.id || "gruvbox-optic",
    motion,
    motionNames: motion.length > 0 ? motion.map((event) => event.type).join(" ") : "idle",
    metrics: {
      tileCount,
      sceneObjectCount: scene?.objects?.length || 0,
      motionCount,
      opticEnergy,
      cornerSize: 42 + opticEnergy * 18,
      cornerOpacity: 0.5 + opticEnergy * 0.18,
    },
  };
}

export function applyPresentationState(root, state) {
  root.dataset.surfaceMode = state.mode;
  root.dataset.theme = state.theme;
  root.dataset.motion = state.motionNames;
  root.style.setProperty("--studio-tile-count", String(state.metrics.tileCount));
  root.style.setProperty("--studio-scene-count", String(state.metrics.sceneObjectCount));
  root.style.setProperty("--studio-motion-count", String(state.metrics.motionCount));
  root.style.setProperty("--studio-optic-energy", String(state.metrics.opticEnergy));
  root.style.setProperty("--studio-corner-size", `${state.metrics.cornerSize}px`);
  root.style.setProperty("--studio-corner-opacity", String(state.metrics.cornerOpacity));
}

function clamp01(value) {
  return Math.min(1, Math.max(0, value));
}

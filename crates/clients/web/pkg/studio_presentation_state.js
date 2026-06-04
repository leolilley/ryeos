const MOTION_LATCH_MS = 280;
const recentMotion = new Map();

export function presentationState(vm, scene) {
  const workspace = vm.workspace || {};
  const semanticMetrics = vm.presentation?.metrics || {};
  const currentMotion = vm.presentation?.motion || [];
  const motion = latchedMotion(vm.generation, currentMotion);
  const tileCount = semanticMetrics.tile_count ?? workspace.tile_count ?? 0;
  const motionCount = motion.length;
  const sceneObjectCount = semanticMetrics.scene_object_count ?? scene?.objects?.length ?? 0;
  const opticEnergy = clamp01((semanticMetrics.activity_level ?? 0) + motionCount * 0.12);

  return {
    mode: workspace.is_home ? "home" : "workspace",
    theme: vm.presentation?.theme?.id || "gruvbox-optic",
    motion,
    currentMotion,
    motionNames: motion.length > 0 ? motion.map((event) => event.type).join(" ") : "idle",
    metrics: {
      tileCount,
      sceneObjectCount,
      itemCount: semanticMetrics.item_count || 0,
      threadCount: semanticMetrics.thread_count || 0,
      projectCount: semanticMetrics.project_count || 0,
      serviceCount: semanticMetrics.service_count || 0,
      scheduleCount: semanticMetrics.schedule_count || 0,
      activeThreadCount: semanticMetrics.active_thread_count || 0,
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
  root.style.setProperty("--studio-item-count", String(state.metrics.itemCount));
  root.style.setProperty("--studio-thread-count", String(state.metrics.threadCount));
  root.style.setProperty("--studio-project-count", String(state.metrics.projectCount));
  root.style.setProperty("--studio-service-count", String(state.metrics.serviceCount));
  root.style.setProperty("--studio-schedule-count", String(state.metrics.scheduleCount));
  root.style.setProperty("--studio-active-thread-count", String(state.metrics.activeThreadCount));
  root.style.setProperty("--studio-motion-count", String(state.metrics.motionCount));
  root.style.setProperty("--studio-optic-energy", String(state.metrics.opticEnergy));
  root.style.setProperty("--studio-corner-size", `${state.metrics.cornerSize}px`);
  root.style.setProperty("--studio-corner-opacity", String(state.metrics.cornerOpacity));
}

function clamp01(value) {
  return Math.min(1, Math.max(0, value));
}

function latchedMotion(generation, events) {
  const now = performance.now();
  for (const [key, entry] of recentMotion) {
    if (entry.expiresAt <= now) recentMotion.delete(key);
  }

  events.forEach((event, index) => {
    recentMotion.set(motionKey(generation, index, event), {
      event,
      expiresAt: now + MOTION_LATCH_MS,
    });
  });

  return [...recentMotion.values()].map((entry) => entry.event);
}

function motionKey(generation, index, event) {
  return [
    generation ?? "g",
    index,
    event.type || "motion",
    event.tile_id || "",
    event.source_tile_id || "",
    event.new_tile_id || "",
    event.axis || "",
  ].join(":");
}

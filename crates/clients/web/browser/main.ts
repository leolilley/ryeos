import { mount, unmount } from "svelte";

import RyeOs from "./app/RyeOs.svelte";
import "./styles/layers.css";
import type { RyeOsEnvelope, RyeOsUiEvent } from "./generated";

export { createCommitRuntime } from "./runtime/commit";

export interface RyeOsRenderer {
  replaceEnvelope(envelope: RyeOsEnvelope): void;
  destroy(): Promise<void>;
}

/**
 * Mount one renderer instance. Runtime and effect ownership remains outside
 * Svelte; this adapter accepts complete committed envelopes and one narrow UI
 * dispatcher only.
 */
export function mountRyeOsRenderer(
  target: Element,
  initialEnvelope: RyeOsEnvelope,
  dispatchUi: (event: RyeOsUiEvent) => void,
): RyeOsRenderer {
  const component = mount(RyeOs, {
    target,
    props: { initialEnvelope, dispatchUi },
  });
  let destroyed = false;

  return {
    replaceEnvelope(envelope) {
      if (destroyed) throw new Error("RyeOS renderer has been destroyed");
      component.replaceEnvelope(envelope);
    },
    async destroy() {
      if (destroyed) return;
      destroyed = true;
      await unmount(component);
    },
  };
}

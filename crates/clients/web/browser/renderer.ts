import { mount, unmount } from "svelte";

import RyeOs from "./app/RyeOs.svelte";
import type { RyeOsEnvelope, RyeOsUiEvent } from "./generated";

export interface RyeOsRenderer {
  replaceEnvelope(envelope: RyeOsEnvelope): void;
  destroy(): Promise<void>;
}

/** Mount one presentation adapter over complete Rust-owned envelopes. */
export function mountRyeOsRenderer(
  target: Element,
  initialEnvelope: RyeOsEnvelope,
  dispatchUi: (event: RyeOsUiEvent) => void,
): RyeOsRenderer {
  const component = mount(RyeOs, { target, props: { initialEnvelope, dispatchUi } });
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

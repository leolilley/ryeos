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
  // The signed document supplies a non-executable boot panel inside this
  // target. Svelte appends by design, so the presentation boundary must retire
  // that bootstrap markup before it takes ownership of the root; otherwise a
  // successful renderer remains visually trapped behind the loader.
  target.replaceChildren();
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

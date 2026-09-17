import type { RyeOsKey, RyeOsKeyEvent } from "../generated";

export function keyEvent(event: KeyboardEvent): RyeOsKeyEvent | null {
  const key = keyName(event.key);
  if (key === null) return null;
  return {
    key,
    modifiers: {
      ctrl: event.ctrlKey,
      alt: event.altKey,
      shift: event.shiftKey,
      meta: event.metaKey,
    },
  };
}

export function keyName(domKey: string): RyeOsKey | null {
  switch (domKey) {
    case "ArrowUp": return "arrow_up";
    case "ArrowDown": return "arrow_down";
    case "ArrowLeft": return "arrow_left";
    case "ArrowRight": return "arrow_right";
    case "Enter": return "enter";
    case "Escape": return "escape";
    case "Backspace": return "backspace";
    case "Tab": return "tab";
    default: return [...domKey].length === 1 ? { char: domKey } : null;
  }
}

export function hasModifiers(event: RyeOsKeyEvent): boolean {
  const { ctrl, alt, shift, meta } = event.modifiers;
  return ctrl || alt || shift || meta;
}

export function isTypingTarget(target: EventTarget | null): boolean {
  return target instanceof Element
    && target.closest("input, textarea, select, [contenteditable='true']") !== null;
}

export function isNativeActivationTarget(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest("button, a, summary") !== null;
}

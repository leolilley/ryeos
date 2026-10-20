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

/** The launcher is a shell control even while an editable view owns focus. */
export function isLauncherChord(event: KeyboardEvent): boolean {
  return !event.isComposing && event.key.toLowerCase() === "k"
    && !event.shiftKey && !event.metaKey
    && (event.ctrlKey !== event.altKey);
}

export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false;
  if (target.closest("input, textarea, select")) return true;
  // The effective DOM state includes empty/plaintext-only attributes and
  // inheritance, while an explicit false descendant must stay non-editable.
  // A focused SVG descendant can inherit from its nearest HTML ancestor.
  let element: Element | null = target;
  while (element) {
    if (element instanceof HTMLElement) return element.isContentEditable;
    element = element.parentElement;
  }
  return false;
}

/** Key events from an open shadow tree may be retargeted to its host. */
export function isTypingEvent(event: KeyboardEvent): boolean {
  const inner = event.composedPath().find((entry): entry is Element => entry instanceof Element);
  return isTypingTarget(inner ?? event.target);
}

export function isNativeActivationTarget(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest("button, a, summary") !== null;
}

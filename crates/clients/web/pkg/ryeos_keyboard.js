// Pure DOM-key translation. Product bindings remain in the shared Rust keymap;
// this module only normalizes browser events into its neutral wire shape.
export function ryeosKeyEvent(event) {
  const key = ryeosKeyName(event.key);
  if (!key) return null;
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

export function ryeosKeyName(domKey) {
  switch (domKey) {
    case "ArrowUp": return "arrow_up";
    case "ArrowDown": return "arrow_down";
    case "ArrowLeft": return "arrow_left";
    case "ArrowRight": return "arrow_right";
    case "Enter": return "enter";
    case "Escape": return "escape";
    case "Backspace": return "backspace";
    case "Tab": return "tab";
    default:
      return domKey.length === 1 ? { char: domKey } : null;
  }
}

export function hasModifiers(key) {
  const modifiers = key.modifiers || {};
  return !!(modifiers.ctrl || modifiers.alt || modifiers.shift || modifiers.meta);
}

export function isTypingTarget(target) {
  return !!target?.closest?.("input, textarea, select, [contenteditable='true']");
}

export function isNativeActivationTarget(target) {
  return !!target?.closest?.("button, a, summary");
}

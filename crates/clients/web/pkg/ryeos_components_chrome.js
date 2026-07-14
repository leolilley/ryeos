import { el, textEl } from "/ui/assets/ryeos_components_primitives.js";

let overlayWasOpen = false;

export function overlayDialog(state, shell) {
  const opening = Boolean(state.open && !overlayWasOpen);
  overlayWasOpen = Boolean(state.open);
  const overlay = el("div", `ryeos-command-overlay${state.open ? " open" : ""}${opening ? " opening" : ""}`);
  if (!state.open) return overlay;

  const choices = state.items || [];
  const selected = Math.min(state.selected || 0, Math.max(choices.length - 1, 0));
  const dialog = el("section", "ryeos-command-dialog");
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-label", state.title || "Open RyeOS tile");
  dialog.setAttribute("aria-modal", "true");

  const input = document.createElement("input");
  input.type = "search";
  input.placeholder = state.title ? `${state.title.toLowerCase()}…` : "open tile…";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.value = state.query || "";
  input.setAttribute("aria-label", state.title || "Search RyeOS commands");
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-controls", "ryeos-command-listbox");
  input.setAttribute("aria-expanded", "true");
  if (choices.length > 0) input.setAttribute("aria-activedescendant", `ryeos-command-option-${selected}`);
  input.setAttribute("data-ryeos-overlay-input", "");
  input.addEventListener("input", () => shell.setOverlayQuery?.(input.value));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      shell.closeOverlay?.();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      shell.moveOverlay?.(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      shell.moveOverlay?.(-1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      shell.chooseOverlay?.(event.shiftKey);
    }
  });

  const list = el("div", "ryeos-command-list");
  list.id = "ryeos-command-listbox";
  list.setAttribute("role", "listbox");
  if (choices.length === 0) {
    const empty = el("div", "ryeos-command-empty");
    empty.textContent = "No matches.";
    list.append(empty);
  }
  choices.forEach((item, index) => {
    const row = el("button", `ryeos-command-choice${index === selected ? " selected" : ""}`);
    row.type = "button";
    row.id = `ryeos-command-option-${index}`;
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", index === selected ? "true" : "false");
    row.disabled = item.enabled === false;
    row.append(
      textEl("strong", item.label || item.primary || "View"),
      textEl("span", item.hint || item.secondary || item.meta || ""),
    );
    row.addEventListener("click", () => {
      if (item.enabled === false) return;
      if (item.intent && shell.dispatchUi) {
        shell.dispatchUi({ type: "activate", intent: item.intent });
        shell.closeOverlay?.();
      } else {
        shell.chooseOverlay?.(false);
      }
    });
    list.append(row);
  });

  const hint = textEl("div", state.hint || "Alt+K open · ↑/↓ select · Enter choose · Shift+Enter new tile · Esc close");
  hint.className = "ryeos-command-hint";
  dialog.append(input, list, hint);
  overlay.append(dialog);
  overlay.addEventListener("keydown", (event) => {
    if (event.key !== "Tab") return;
    const focusable = [...dialog.querySelectorAll('input, button:not([disabled]), [tabindex]:not([tabindex="-1"])')];
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  });
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) shell.closeOverlay?.();
  });
  return overlay;
}

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

  const input = document.createElement("input");
  input.type = "search";
  input.placeholder = state.title ? `${state.title.toLowerCase()}…` : "open tile…";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.value = state.query || "";
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
  if (choices.length === 0) {
    const empty = el("div", "ryeos-command-empty");
    empty.textContent = "No matches.";
    list.append(empty);
  }
  choices.forEach((item, index) => {
    const row = el("button", `ryeos-command-choice${index === selected ? " selected" : ""}`);
    row.type = "button";
    row.disabled = item.enabled === false;
    row.append(
      textEl("strong", item.label || item.primary || "View"),
      textEl("span", item.hint || item.secondary || item.meta || ""),
    );
    row.addEventListener("click", () => {
      if (item.enabled === false) return;
      if (item.action && shell.dispatchUi) {
        shell.dispatchUi({ type: "activate", action: item.action });
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
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) shell.closeOverlay?.();
  });
  return overlay;
}

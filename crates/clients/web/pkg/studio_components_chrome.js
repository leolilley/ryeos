import { el, textEl } from "/ui/assets/studio_components_primitives.js";

let launcherWasOpen = false;
const dismissedNoticeKeys = new Set();

export function launcherDialog(state, shell) {
  const opening = Boolean(state.open && !launcherWasOpen);
  launcherWasOpen = Boolean(state.open);
  const overlay = el("div", `studio-command-overlay${state.open ? " open" : ""}${opening ? " opening" : ""}`);
  if (!state.open) return overlay;

  const choices = state.items || [];
  const selected = Math.min(state.selected || 0, Math.max(choices.length - 1, 0));
  const dialog = el("section", "studio-command-dialog");
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-label", "Open RyeOS tile");

  const input = document.createElement("input");
  input.type = "search";
  input.placeholder = "open tile…";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.value = state.query || "";
  input.setAttribute("data-studio-launcher-input", "");
  input.addEventListener("input", () => shell.setLauncherQuery?.(input.value));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      shell.closeLauncher?.();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      shell.moveLauncher?.(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      shell.moveLauncher?.(-1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      shell.chooseLauncher?.(event.shiftKey);
    }
  });

  const list = el("div", "studio-command-list");
  if (choices.length === 0) {
    const empty = el("div", "studio-command-empty");
    empty.textContent = "No matching tile.";
    list.append(empty);
  }
  choices.forEach((item, index) => {
    const row = el("button", `studio-command-choice${index === selected ? " selected" : ""}`);
    row.type = "button";
    row.append(textEl("strong", item.label || "View"), textEl("span", item.hint || ""));
    row.addEventListener("mouseenter", () => {
      if (index !== state.selected) shell.moveLauncher?.(index - (state.selected || 0));
    });
    row.addEventListener("click", () => shell.chooseLauncher?.(false));
    list.append(row);
  });

  const hint = textEl("div", state.hint || "Alt+K open · ↑/↓ select · Enter choose · Shift+Enter new tile · Esc close");
  hint.className = "studio-command-hint";
  dialog.append(input, list, hint);
  overlay.append(dialog);
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) shell.closeLauncher?.();
  });
  return overlay;
}

export function notices(items) {
  const wrap = el("div", "studio-notices");
  for (const item of items) {
    const key = noticeKey(item);
    if (dismissedNoticeKeys.has(key)) continue;
    const notice = el("button", `studio-notice ${item.tone || "neutral"}`);
    notice.type = "button";
    notice.title = "Dismiss notice";
    notice.textContent = item.message || "";
    notice.addEventListener("click", () => {
      dismissedNoticeKeys.add(key);
      notice.remove();
    });
    wrap.append(notice);
  }
  return wrap;
}

function noticeKey(item) {
  return `${item.id || "notice"}:${item.tone || "neutral"}:${item.message || ""}`;
}

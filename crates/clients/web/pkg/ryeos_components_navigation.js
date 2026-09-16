import { el, textEl } from "/ui/assets/ryeos_components_primitives.js";

// Pure renderer for the ordered navigation already compiled by RyeOS. This
// module knows no page names, view refs, or authority rules; it can dispatch
// only the intents carried by the semantic VM.
export function ryeosNavigation(navigation, dispatchUi) {
  const items = navigation?.items || [];
  const nav = el("nav", "ryeos-navigation");
  nav.setAttribute("aria-label", "RyeOS");
  nav.hidden = items.length === 0;
  for (const item of items) {
    const button = el("button", `ryeos-navigation-item${item.selected ? " selected" : ""}`);
    button.type = "button";
    button.dataset.destination = item.id || "";
    button.setAttribute("aria-current", item.selected ? "page" : "false");
    button.append(textEl("span", item.label || item.id || "View"));
    button.addEventListener("click", () => {
      if (item.intent) dispatchUi({ type: "activate", intent: item.intent });
    });
    nav.append(button);
  }
  return nav;
}

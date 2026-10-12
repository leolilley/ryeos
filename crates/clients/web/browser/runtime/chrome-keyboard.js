/** Shell chrome owns an unmodified Escape; it must not close a focused View. */
export function isShellChromeEscape(event) {
  if (event.key !== "Escape" || event.isComposing || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return false;
  const path = event.composedPath?.() ?? [];
  return [...path, event.target].some((entry) =>
    entry instanceof Element && entry.closest(".system-bar, .status-bar") !== null);
}

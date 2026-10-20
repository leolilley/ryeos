/** DOM-only modal mechanics. Neither focus nor a dialog frame chooses a
 * subject, route, grant, insertion target or semantic operation. */
export function dialogFocusables(panel: HTMLElement): HTMLElement[] {
  return [...panel.querySelectorAll<HTMLElement>(
    'button:not([disabled]),input:not([disabled]),textarea:not([disabled]),select:not([disabled]),a[href],[tabindex="0"]',
  )].filter((element) => !element.closest('[hidden],[inert],[aria-hidden="true"]')
    && element.getClientRects().length > 0);
}

export function trapDialogTab(panel: HTMLElement, event: KeyboardEvent): void {
  if (event.key !== "Tab" || event.isComposing) return;
  const controls = dialogFocusables(panel);
  event.preventDefault();
  if (!controls.length) {
    panel.focus();
    return;
  }
  const current = controls.indexOf(panel.ownerDocument.activeElement as HTMLElement);
  const next = event.shiftKey
    ? (current <= 0 ? controls.length - 1 : current - 1)
    : (current < 0 || current === controls.length - 1 ? 0 : current + 1);
  (controls[next] ?? panel).focus();
}

export function restoreDialogFocus(panel: HTMLElement, opener: HTMLElement | null): void {
  const active = panel.ownerDocument.activeElement;
  // A semantic View action may already have focused its destination. Do not
  // undo that action merely because its former frame is being removed.
  if (opener?.isConnected && (active === panel.ownerDocument.body
    || active === null || panel.contains(active))) opener.focus();
}

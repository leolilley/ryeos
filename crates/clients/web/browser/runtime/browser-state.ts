interface FocusSnapshot {
  readonly key: string;
  readonly selectionStart: number | null;
  readonly selectionEnd: number | null;
}

interface ScrollSnapshot {
  readonly top: number;
  readonly left: number;
  readonly atTail: boolean;
}

const modalReturnFocus = new WeakMap<Element, FocusSnapshot>();

export interface BrowserPresentationSnapshot {
  readonly focus: FocusSnapshot | null;
  readonly scroll: ReadonlyMap<string, ScrollSnapshot>;
}

export function captureBrowserPresentation(root: Element): BrowserPresentationSnapshot {
  const active = document.activeElement;
  const key = active instanceof HTMLElement && root.contains(active)
    ? active.dataset.focusKey ?? null : null;
  const focus = key === null ? null : {
    key,
    selectionStart: selection(active, "selectionStart"),
    selectionEnd: selection(active, "selectionEnd"),
  };
  const scroll = new Map<string, ScrollSnapshot>();
  for (const node of root.querySelectorAll<HTMLElement>("[data-scroll-key]")) {
    const scrollKey = node.dataset.scrollKey;
    if (!scrollKey) continue;
    scroll.set(scrollKey, {
      top: node.scrollTop,
      left: node.scrollLeft,
      atTail: node.scrollHeight - node.scrollTop - node.clientHeight <= 24,
    });
  }
  return { focus, scroll };
}

export function restoreBrowserPresentation(root: Element, snapshot: BrowserPresentationSnapshot): void {
  for (const node of root.querySelectorAll<HTMLElement>("[data-scroll-key]")) {
    const state = snapshot.scroll.get(node.dataset.scrollKey ?? "");
    if (!state) continue;
    node.scrollTop = state.atTail ? node.scrollHeight : state.top;
    node.scrollLeft = state.left;
  }
  if (root.querySelector<HTMLElement>('[role="dialog"][aria-modal="true"]')) {
    if (snapshot.focus && !snapshot.focus.key.startsWith("overlay:")) {
      modalReturnFocus.set(root, snapshot.focus);
    }
    return;
  }
  let focus = snapshot.focus ?? modalReturnFocus.get(root) ?? null;
  if (!focus) return;
  let target = root.querySelector<HTMLElement>(`[data-focus-key="${cssEscape(focus.key)}"]`);
  if (!target) {
    focus = modalReturnFocus.get(root) ?? null;
    if (!focus) return;
    target = root.querySelector<HTMLElement>(`[data-focus-key="${cssEscape(focus.key)}"]`);
  }
  target?.focus({ preventScroll: true });
  if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
    const start = focus.selectionStart;
    if (start !== null) target.setSelectionRange(start, focus.selectionEnd ?? start);
  }
  if (target) modalReturnFocus.delete(root);
}

function selection(element: Element | null, field: "selectionStart" | "selectionEnd"): number | null {
  if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) return element[field];
  return null;
}

function cssEscape(value: string): string {
  return CSS.escape(value);
}

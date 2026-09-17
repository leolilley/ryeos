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
  if (!snapshot.focus) return;
  const target = root.querySelector<HTMLElement>(`[data-focus-key="${cssEscape(snapshot.focus.key)}"]`);
  target?.focus({ preventScroll: true });
  if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
    const start = snapshot.focus.selectionStart;
    if (start !== null) target.setSelectionRange(start, snapshot.focus.selectionEnd ?? start);
  }
}

function selection(element: Element | null, field: "selectionStart" | "selectionEnd"): number | null {
  if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) return element[field];
  return null;
}

function cssEscape(value: string): string {
  return CSS.escape(value);
}

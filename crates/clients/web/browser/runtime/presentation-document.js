// Mechanical browser handoff only. Node-owned preparation grants no producer.
export function preparedDocumentUrl(current, presentationId) {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(presentationId)) {
    throw new Error("prepared presentation identity is invalid");
  }
  const next = new URL("/ui/", current);
  next.searchParams.set("presentation_id", presentationId);
  return next;
}

/** One platform-effect frame. Retry repeats the exact supplied request.
 * Dismissal removes framing only, never the retained seat or its obligations. */
export function createPresentationDocumentHandoff(document, parent, current, prepare, onDismiss = () => {}) {
  const frame = document.createElement("section");
  frame.className = "ryeos-document-handoff";
  frame.setAttribute("role", "status");
  frame.setAttribute("aria-label", "Prepared presentation");
  const status = document.createElement("p");
  const link = document.createElement("a");
  link.textContent = "Open Surface";
  link.target = "_blank";
  link.rel = "noopener noreferrer";
  link.hidden = true;
  const retry = document.createElement("button");
  retry.type = "button";
  retry.textContent = "Retry exact preparation";
  retry.hidden = true;
  const dismiss = document.createElement("button");
  dismiss.type = "button";
  dismiss.textContent = "Dismiss link";
  frame.append(status, link, retry, dismiss);
  parent.append(frame);
  let disposed = false;
  let inFlight = null;
  function start() {
    if (disposed || inFlight) return inFlight ?? Promise.resolve();
    retry.hidden = true;
    status.textContent = "Preparing an independent presentation. No producer is claimed.";
    const running = Promise.resolve().then(prepare).then((presentationId) => {
      const url = preparedDocumentUrl(current, presentationId);
      if (disposed) return; // Node retains a late preparation independently.
      link.href = url.href;
      link.hidden = false;
      status.textContent = "Ready. Open the prepared Surface when you choose. The link remains available if opening is blocked.";
    }).catch((error) => {
      if (disposed) return;
      status.textContent = `Preparation is not confirmed: ${error instanceof Error ? error.message : String(error)}. Retry reconciles the same request.`;
      retry.hidden = false;
    });
    inFlight = running;
    void running.finally(() => { if (inFlight === running) inFlight = null; });
    return running;
  }
  const retryClick = () => { void start(); };
  const dismissClick = () => { dispose(); onDismiss(); };
  retry.addEventListener("click", retryClick);
  dismiss.addEventListener("click", dismissClick);
  function dispose() {
    if (disposed) return;
    disposed = true;
    retry.removeEventListener("click", retryClick);
    dismiss.removeEventListener("click", dismissClick);
    frame.remove();
  }
  return { ready: start(), dispose, settle: () => inFlight ?? Promise.resolve() };
}

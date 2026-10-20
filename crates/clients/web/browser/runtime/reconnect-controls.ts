import type { UiPresentationProducerObservation } from "../generated";
import type { PresentationReconnectChoice, SessionRuntime } from "./session";

/** Mechanical choices from the authenticated owner; never diagnostic-text inference. */
export function replacementChoices(observation: UiPresentationProducerObservation): PresentationReconnectChoice[] {
  const producer = observation.producer;
  if (producer.kind === "relinquished") return [
    { kind: "resume", expected_producer_incarnation: producer.producer_incarnation, predecessor: { ...observation.control_predecessor } },
    { kind: "takeover", expected_producer_incarnation: producer.producer_incarnation, predecessor: { ...observation.control_predecessor } },
  ];
  if (producer.kind === "active") return [
    { kind: "takeover", expected_producer_incarnation: producer.producer_incarnation, predecessor: { ...observation.control_predecessor } },
  ];
  return [];
}

/**
 * Reconcile the retained unknown append before observing its owner. This does
 * not await newly queued suffix writes: observation is read-only and may
 * coexist with sync. An explicit reconnect still refuses until that queue
 * settles; these choices are not evidence that replacement is already safe.
 */
export async function observeReconnectChoices(runtime: SessionRuntime): Promise<UiPresentationProducerObservation> {
  await runtime.reconcilePendingAppend();
  return runtime.observeProducer();
}

/** One failed bootstrap's mechanical decision frame, disposed with its document. */
export function awaitBootRecovery(root: Element, runtime: SessionRuntime, failure: unknown, signal?: AbortSignal,
  options: { readonly allowExactRetry?: boolean } = {}): Promise<SessionRuntime> {
  return new Promise((resolve, reject) => {
    let settled = false;
    let busy = false;
    let inFlightAction: Promise<void> | null = null;
    const listeners: Array<() => void> = [];
    const main = document.createElement("main");
    main.className = "ryeos-boot ryeos-boot-failed";
    main.setAttribute("role", "alert");
    const title = document.createElement("h1");
    title.textContent = "Presentation needs your decision";
    const diagnostic = document.createElement("pre");
    diagnostic.className = "ryeos-boot-error-detail";
    diagnostic.textContent = failure instanceof Error ? failure.message : String(failure);
    const guidance = document.createElement("p");
    guidance.textContent = options.allowExactRetry === false
      ? "This live presentation is fenced. Reconcile retained receipts, inspect the writer, then explicitly choose a replacement. No writer death is inferred."
      : "Retry keeps this document’s exact request. Reconcile checks retained receipts before showing explicit writer choices.";
    main.append(title, diagnostic, guidance);

    const cleanup = () => { listeners.splice(0).forEach((remove) => remove()); signal?.removeEventListener("abort", abort); };
    const finish = (next: SessionRuntime) => {
      if (settled) { if (next !== runtime) next.close(); return; }
      settled = true; cleanup(); resolve(next);
    };
    const abort = () => {
      if (settled) return;
      settled = true;
      cleanup();
      const reason = signal?.reason ?? new Error("document disposed");
      // A clicked replacement may still be reconciling an exact request.
      // Reject the decision only after that browser-owned action has stopped;
      // its late result is closed by `finish`, never attached to this document.
      void (inFlightAction ?? Promise.resolve()).then(
        () => reject(reason),
        () => reject(reason),
      );
    };
    const button = (label: string, action: () => Promise<void>) => {
      const control = document.createElement("button");
      control.type = "button";
      control.className = "ryeos-boot-retry";
      control.textContent = label;
      const click = () => {
        if (busy || settled) return;
        busy = true;
        const running = Promise.resolve().then(() => {
          if (settled) return;
          return action();
        }).catch((error: unknown) => {
          if (!settled) diagnostic.textContent = error instanceof Error ? error.message : String(error);
        }).finally(() => { busy = false; });
        inFlightAction = running;
        void running.then(
          () => { if (inFlightAction === running) inFlightAction = null; },
          () => { if (inFlightAction === running) inFlightAction = null; },
        );
        return running;
      };
      control.addEventListener("click", click);
      listeners.push(() => control.removeEventListener("click", click));
      main.append(control);
      return control;
    };
    if (options.allowExactRetry !== false) button("Retry exact request", async () => { finish(runtime); });
    if (runtime.preparedPresentation()) {
      button("Reconcile and inspect writer", async () => {
        // A failed observation must not leave old choices looking current.
        main.querySelectorAll("[data-replacement-choice]").forEach((node) => node.remove());
        const observation = await observeReconnectChoices(runtime);
        if (settled) return;
        // Replace previous observation choices; they are not retained authority.
        guidance.textContent = `Observed writer: ${observation.producer.kind}. Replacement requires an explicit choice; the node checks the predecessor again.`;
        for (const choice of replacementChoices(observation)) {
          const control = button(choice.kind === "resume" ? "Resume relinquished presentation" : "Take over this presentation", async () => {
            finish(await runtime.reconnect(choice));
          });
          control.dataset.replacementChoice = "true";
        }
      });
      button("Open independent presentation", async () => {
        if (!window.confirm("Open a distinct presentation? This does not resume or take over the existing writer. Unresolved local writes still must reconcile.")) return;
        finish(await runtime.reconnect({ kind: "independent" }));
      });
    }
    root.replaceChildren(main);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
  });
}

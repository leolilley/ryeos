/** Mechanical ownership of one browser entry evaluation, not application state. */
export interface DocumentRuntime {
  close(): Promise<void>;
}

export function createDocumentLifetime(
  start: (signal: AbortSignal) => Promise<DocumentRuntime>,
): { ready: Promise<void>; settled: Promise<void>; dispose(): Promise<void> } {
  const abort = new AbortController();
  let running: DocumentRuntime | null = null;
  let disposal: Promise<void> | null = null;
  let markDisposalRequested: () => void = () => {};
  const disposalRequested = new Promise<void>((resolve) => { markDisposalRequested = resolve; });
  const ready = Promise.resolve().then(async () => {
    if (abort.signal.aborted) return;
    const runtime = await start(abort.signal);
    if (abort.signal.aborted) {
      await runtime.close();
    } else {
      running = runtime;
    }
  });
  // Prompt mechanical disposal is not proof that an in-flight boot has
  // returned and closed its late runtime. A successor must await this separate
  // barrier before attempting its own seat attachment.
  const settled = (async () => {
    await disposalRequested;
    // Wait for both branches even if either fails. A rejected barrier fences
    // the successor, but must not strand another cleanup still in flight.
    const outcomes = await Promise.allSettled([ready, disposal ?? Promise.resolve()]);
    for (const outcome of outcomes) {
      if (outcome.status === "rejected") throw outcome.reason;
    }
  })();
  return {
    ready,
    settled,
    dispose() {
      abort.abort();
      if (!disposal) {
        // A pending boot observes the signal and cleans up its own partial
        // resources. Disposal must not wait indefinitely for a network reply.
        const runtime = running;
        running = null;
        // A misbehaving Promise-returning close may throw synchronously.
        // Turn it into a rejected cleanup promise so the barrier still settles.
        disposal = runtime ? Promise.resolve().then(() => runtime.close()) : Promise.resolve();
        markDisposalRequested();
      }
      return disposal;
    },
  };
}

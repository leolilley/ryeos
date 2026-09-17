export type EffectIdentity = bigint | number | string;

export interface IdentifiedEffect {
  readonly id: EffectIdentity;
}

export interface EnvelopeWithEffects<Effect extends IdentifiedEffect> {
  readonly effects: readonly Effect[];
}

export interface CommitRuntimeOptions<
  Event,
  Effect extends IdentifiedEffect,
  Result,
  Envelope extends EnvelopeWithEffects<Effect>,
> {
  readonly reduce: (event: Event) => Envelope;
  readonly applyEffectResult: (result: Result) => Envelope;
  readonly startEffect: (effect: Effect) => Promise<Result>;
  readonly failedEffectResult: (effect: Effect, error: unknown) => Result;
  readonly render: (envelope: Envelope) => void | Promise<void>;
  readonly scheduleRender?: (render: () => void) => void;
}

export interface CommitRuntime<Event, Envelope> {
  enqueueEvent(event: Event): void;
  enqueueEnvelope(envelope: Envelope): void;
  /** Test/teardown fence for all work accepted before this call. */
  idle(): Promise<void>;
}

type Mutation<Envelope> = () => Envelope;

/**
 * Serialize all Rust mutations while allowing visual generations to coalesce.
 *
 * Effect registration is deliberately synchronous with envelope acceptance.
 * Svelte flush timing therefore cannot drop, merge, repeat or delay discovery
 * of an effect emitted by an intermediate Rust generation.
 */
export function createCommitRuntime<
  Event,
  Effect extends IdentifiedEffect,
  Result,
  Envelope extends EnvelopeWithEffects<Effect>,
>(options: CommitRuntimeOptions<Event, Effect, Result, Envelope>): CommitRuntime<Event, Envelope> {
  const mutationQueue: Array<Mutation<Envelope>> = [];
  const startedEffects = new Set<EffectIdentity>();
  const scheduleRender = options.scheduleRender ?? ((render) => queueMicrotask(render));
  let draining = false;
  let renderScheduled = false;
  let renderRunning = false;
  let pendingEffects = 0;
  let latestRenderable: Envelope | null = null;
  let acceptedSequence = 0;
  let settledSequence = 0;
  const idleWaiters: Array<{ sequence: number; resolve: () => void }> = [];

  function enqueueMutation(mutation: Mutation<Envelope>): void {
    mutationQueue.push(mutation);
    acceptedSequence += 1;
    void drainMutations();
  }

  async function drainMutations(): Promise<void> {
    if (draining) return;
    draining = true;
    try {
      while (mutationQueue.length > 0) {
        const mutation = mutationQueue.shift();
        if (!mutation) continue;
        acceptEnvelope(mutation());
        settledSequence += 1;
      }
    } finally {
      draining = false;
      resolveIdleWaiters();
      if (mutationQueue.length > 0) void drainMutations();
    }
  }

  function acceptEnvelope(envelope: Envelope): void {
    for (const effect of envelope.effects) {
      if (startedEffects.has(effect.id)) continue;
      startedEffects.add(effect.id);
      pendingEffects += 1;
      let delivered = false;
      options.startEffect(effect).then(
        (result) => deliverEffectResult(result),
        (error: unknown) => deliverEffectResult(options.failedEffectResult(effect, error)),
      );

      function deliverEffectResult(result: Result): void {
        if (delivered) return;
        delivered = true;
        pendingEffects -= 1;
        enqueueMutation(() => options.applyEffectResult(result));
      }
    }
    latestRenderable = envelope;
    requestRender();
  }

  function requestRender(): void {
    if (renderScheduled || renderRunning) return;
    renderScheduled = true;
    scheduleRender(() => { void flushRender(); });
  }

  async function flushRender(): Promise<void> {
    if (renderRunning) return;
    renderScheduled = false;
    renderRunning = true;
    try {
      // Capture once. A newer envelope arriving during the renderer flush is
      // scheduled separately; intermediate visual generations may coalesce.
      const envelope = latestRenderable;
      latestRenderable = null;
      if (envelope) await options.render(envelope);
    } finally {
      renderRunning = false;
      if (latestRenderable) requestRender();
      resolveIdleWaiters();
    }
  }

  function resolveIdleWaiters(): void {
    if (draining || mutationQueue.length > 0 || pendingEffects > 0
      || renderScheduled || renderRunning || latestRenderable) return;
    for (let index = idleWaiters.length - 1; index >= 0; index -= 1) {
      const waiter = idleWaiters[index];
      if (waiter && settledSequence >= waiter.sequence) {
        idleWaiters.splice(index, 1);
        waiter.resolve();
      }
    }
  }

  return {
    enqueueEvent(event) {
      enqueueMutation(() => options.reduce(event));
    },
    enqueueEnvelope(envelope) {
      enqueueMutation(() => envelope);
    },
    idle() {
      const sequence = acceptedSequence;
      if (!draining && mutationQueue.length === 0 && pendingEffects === 0
        && !renderScheduled && !renderRunning && !latestRenderable
        && settledSequence >= sequence) {
        return Promise.resolve();
      }
      return new Promise((resolve) => idleWaiters.push({ sequence, resolve }));
    },
  };
}

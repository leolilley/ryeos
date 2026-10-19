export interface LayoutPreferencePersistence {
  observeAcceptedState(): boolean;
  flush(): boolean;
}

interface Options {
  readonly key: string;
  readonly persisted: string | null;
  readonly readCurrent: () => string;
  readonly storage: Storage;
  readonly reportError: (error: unknown) => void;
  readonly delayMs?: number;
}

/** Persist one opaque Rust-owned presentation arrangement. */
export function createLayoutPreferencePersistence(options: Options): LayoutPreferencePersistence {
  const delay = options.delayMs ?? 400;
  let lastObserved = options.readCurrent();
  let lastPersisted = options.persisted;
  let pending: string | null = null;
  let timer: number | null = null;

  function clearPending(): void {
    if (timer !== null) window.clearTimeout(timer);
    timer = null;
    pending = null;
  }

  function writeCaptured(value: string): boolean {
    if (pending !== value) return false;
    timer = null;
    pending = null;
    try {
      options.storage.setItem(options.key, value);
      lastPersisted = value;
      return true;
    } catch (error) {
      pending = value;
      options.reportError(error);
      return false;
    }
  }

  function queue(value: string): void {
    if (timer !== null) window.clearTimeout(timer);
    pending = value;
    timer = window.setTimeout(() => writeCaptured(value), delay);
  }

  if (options.persisted !== null && lastObserved !== options.persisted) queue(lastObserved);

  return {
    observeAcceptedState() {
      let current: string;
      try { current = options.readCurrent(); }
      catch (error) { options.reportError(error); return false; }
      if (current === lastObserved) return false;
      lastObserved = current;
      if (current === lastPersisted) { clearPending(); return false; }
      queue(current);
      return true;
    },
    flush() {
      if (pending === null) return false;
      if (timer !== null) window.clearTimeout(timer);
      timer = null;
      return writeCaptured(pending);
    },
  };
}

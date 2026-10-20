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

interface ScopedOptions {
  readonly readKey: () => string | null | undefined;
  readonly readCurrent: () => string;
  readonly storage: Storage;
  readonly reportError: (error: unknown) => void;
  readonly delayMs?: number;
}

/** Adapter persistence follows the opaque scope projected by Rust. It never
 * reconstructs a Surface identity or writes a new scope under an old key. */
export function createScopedLayoutPreferencePersistence(options: ScopedOptions): LayoutPreferencePersistence {
  let key: string | null = null;
  let active: LayoutPreferencePersistence | null = null;
  let switching = false;

  return {
    observeAcceptedState() {
      if (switching) return false;
      try {
        const nextKey = options.readKey() ?? null;
        if (nextKey !== key || (nextKey !== null && active === null)) {
          // Flush captured predecessor bytes; do not call its exporter after
          // Rust has selected another Surface or closed the last one.
          active?.flush();
          active = null;
          key = nextKey;
          if (key === null) return false;
          switching = true;
          try {
            const saved = options.storage.getItem(key);
            // Cached structure is a future explicit restore candidate. It
            // cannot replace admitted/replayed mount identities at bootstrap
            // or when another Surface becomes active.
            const capturedKey = key;
            active = createLayoutPreferencePersistence({
              ...options, key: capturedKey, persisted: saved,
              readCurrent: () => {
                if (options.readKey() !== capturedKey) throw new Error("layout preference scope changed");
                return options.readCurrent();
              },
            });
          } finally { switching = false; }
        }
        return active?.observeAcceptedState() ?? false;
      } catch (error) {
        options.reportError(error);
        return false;
      }
    },
    flush() { return active?.flush() ?? false; },
  };
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

  // A differing admitted baseline is not an accepted edit. Preserve a cached
  // restore candidate until a subsequent observed change explicitly replaces it.

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

// Browser persistence for Rust-owned presentation arrangements.
//
// The encoded value is deliberately opaque here. Rust owns its schema,
// authenticated scope, validation, and restoration. The browser only notices
// when the canonical encoded value changes and persists that exact value.

export function createLayoutPreferencePersistence({
  key,
  persisted,
  readCurrent,
  storage,
  reportError,
  delayMs = 400,
  schedule = (callback, delay) => setTimeout(callback, delay),
  cancel = (timer) => clearTimeout(timer),
}) {
  let lastObserved = readCurrent();
  let lastPersisted = persisted;
  let pending = null;
  let timer = null;

  function clearPending() {
    if (timer !== null) cancel(timer);
    timer = null;
    pending = null;
  }

  function writeCaptured(value) {
    if (pending !== value) return false;
    timer = null;
    pending = null;
    try {
      storage.setItem(key, value);
      lastPersisted = value;
      return true;
    } catch (error) {
      // Keep the exact failed value available for a pagehide retry. Never
      // re-export potentially newer Rust state under an older scheduled write.
      pending = value;
      reportError(error);
      return false;
    }
  }

  function queue(value) {
    if (timer !== null) cancel(timer);
    pending = value;
    timer = schedule(() => writeCaptured(value), delayMs);
  }

  // A restored predecessor representation may normalize to a new canonical
  // encoding. Replace it only when Rust actually returned a different value.
  if (persisted !== null && lastObserved !== persisted) queue(lastObserved);

  return {
    observeAcceptedState() {
      let current;
      try {
        current = readCurrent();
      } catch (error) {
        reportError(error);
        return false;
      }
      if (current === lastObserved) return false;
      lastObserved = current;
      if (current === lastPersisted) {
        clearPending();
        return false;
      }
      queue(current);
      return true;
    },

    flush() {
      if (pending === null) return false;
      if (timer !== null) cancel(timer);
      timer = null;
      return writeCaptured(pending);
    },
  };
}

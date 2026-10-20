/** A chain stream is wider than the currently focused route thread. */
export function focusedThreadTailFrame(
  frame: unknown,
  chainRootId: string | null,
  threadId: string | null,
): { eventType: string; payload: unknown } | null {
  if (!chainRootId || !threadId || frame === null || typeof frame !== "object" || Array.isArray(frame)) return null;
  // The browser route adapts the chain SSE record to a message envelope:
  // { event_type, payload: PersistedEventRecord }. The coordinates live on
  // that inner record, unlike the terminal's direct chain stream.
  const envelope = frame as Record<string, unknown>;
  if (typeof envelope.event_type !== "string" || envelope.payload === null
    || typeof envelope.payload !== "object" || Array.isArray(envelope.payload)) return null;
  const record = envelope.payload as Record<string, unknown>;
  if (record.chain_root_id !== chainRootId || record.thread_id !== threadId
    || record.event_type !== envelope.event_type) return null;
  return { eventType: envelope.event_type, payload: record };
}

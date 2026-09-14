export async function runEffect(effect) {
  const kind = effect.kind;
  switch (kind.type) {
    case "fetch_source": {
      const resp = await dispatchBinding(kind.request, kind.request_bounds);
      return result(effect, "source_data", resp?.result?.result ?? resp?.result ?? resp);
    }
    case "invoke_binding": {
      const resp = await dispatchBinding(kind.request, kind.request_bounds);
      const inner = resp?.result?.result ?? resp?.result ?? resp;
      return result(effect, "binding_invoked", inner);
    }
    case "set_location_hash":
      location.hash = kind.hash;
      return result(effect, "browser_only", null);
    case "copy_to_clipboard":
      await navigator.clipboard.writeText(kind.text);
      return result(effect, "browser_only", null);
    case "open_url":
      window.open(kind.url, "_blank", "noopener,noreferrer");
      return result(effect, "browser_only", null);
    case "replace_session":
      // A replacement is a same-origin, one-shot RyeOS launch path. Never let
      // service result data turn this transport effect into open navigation.
      {
        const launch = new URL(kind.launch_url, location.href);
        if (launch.origin !== location.origin || !launch.pathname.startsWith("/ui/launch/")) {
          throw new Error("replacement UI session has an invalid launch origin or path");
        }
        location.assign(`${launch.pathname}${launch.search}${launch.hash}`);
      }
      return result(effect, "browser_only", null);
    default:
      // Degradation discipline: unknown effects fail soft. Throwing here
      // would let one new effect kind take down the whole renderer.
      return failedResultFor(effect, new Error(`unhandled effect: ${kind.type}`));
  }
}

export function failedResultFor(effect, error) {
  return {
    id: effect.id,
    ok: false,
    kind: resultKindFor(effect),
    error: {
      code: "platform_effect_failed",
      error: error?.message || String(error),
      retryable: false,
      outcome: error?.outcome === "unknown" ? "unknown" : "refused",
    },
  };
}

async function dispatchBinding(request, bounds) {
  if (!request || typeof request !== "object") throw new Error("missing UI binding request");
  const maximum = Number(bounds?.max_request_bytes || 0);
  const inputMaximum = Number(bounds?.max_input_bytes || 0);
  if (!Number.isSafeInteger(maximum) || maximum <= 0 || !Number.isSafeInteger(inputMaximum) || inputMaximum <= 0) {
    throw new Error("invalid UI binding request bounds");
  }
  const encoded = JSON.stringify(request);
  if (new TextEncoder().encode(encoded).byteLength > maximum) {
    throw new Error("UI binding request exceeds the session bound");
  }
  if (request.payload?.kind === "input" && new TextEncoder().encode(request.payload.value || "").byteLength > inputMaximum) {
    throw new Error("UI binding input exceeds the session bound");
  }
  // The request is already the closed renderer-neutral wire DTO. Do not add
  // item refs, capability claims, read-only flags, or project context here.
  return postJson("/ui/api/invocations/dispatch", request);
}

function result(effect, kind, data) {
  return { id: effect.id, ok: true, kind, data };
}

function resultKindFor(effect) {
  const type = effect?.kind?.type;
  if (type === "fetch_source") return "source_data";
  if (type === "invoke_binding") return "binding_invoked";
  return "browser_only";
}

async function postJson(url, body) {
  let response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body || {}),
    });
  } catch (cause) {
    const error = new Error(`${url}: delivery outcome is unknown: ${cause?.message || String(cause)}`);
    error.outcome = "unknown";
    throw error;
  }
  if (!response.ok) throw new Error(`${url}: ${response.status} ${await response.text()}`);
  try {
    return await response.json();
  } catch (cause) {
    const error = new Error(`${url}: response outcome is unknown: ${cause?.message || String(cause)}`);
    error.outcome = "unknown";
    throw error;
  }
}

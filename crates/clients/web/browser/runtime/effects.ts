import type {
  RyeOsEffect,
  RyeOsEffectResult,
  RyeOsEffectResultKind,
  UiBindingRequest,
  UiBindingRequestBounds,
} from "../generated";
import { errorMessage, postJson, UnknownDeliveryError } from "./transport";

export async function runEffect(effect: RyeOsEffect): Promise<RyeOsEffectResult> {
  const kind = effect.kind;
  switch (kind.type) {
    case "fetch_source": {
      const response = await dispatchBinding(kind.request, kind.request_bounds);
      return success(effect, "source_data", unwrapResult(response));
    }
    case "invoke_binding": {
      const response = await dispatchBinding(kind.request, kind.request_bounds);
      return success(effect, "binding_invoked", unwrapResult(response));
    }
    case "set_location_hash":
      location.hash = kind.hash;
      return success(effect, "browser_only", null);
    case "copy_to_clipboard":
      await navigator.clipboard.writeText(kind.text);
      return success(effect, "browser_only", null);
    case "open_url":
      window.open(kind.url, "_blank", "noopener,noreferrer");
      return success(effect, "browser_only", null);
    case "replace_session": {
      const launch = new URL(kind.launch_url, location.href);
      if (launch.origin !== location.origin || !launch.pathname.startsWith("/ui/launch/")) {
        throw new Error("replacement UI session has an invalid launch origin or path");
      }
      location.assign(`${launch.pathname}${launch.search}${launch.hash}`);
      return success(effect, "browser_only", null);
    }
  }
}

export function failedEffectResult(effect: RyeOsEffect, error: unknown): RyeOsEffectResult {
  return {
    id: effect.id,
    ok: false,
    kind: resultKind(effect),
    data: null,
    error: {
      code: "platform_effect_failed",
      error: errorMessage(error),
      retryable: false,
      outcome: error instanceof UnknownDeliveryError ? "unknown" : "refused",
    },
  };
}

async function dispatchBinding(request: UiBindingRequest, bounds: UiBindingRequestBounds): Promise<unknown> {
  const maximum = safeBound(bounds.max_request_bytes, "max_request_bytes");
  const inputMaximum = safeBound(bounds.max_input_bytes, "max_input_bytes");
  const encoded = JSON.stringify(request);
  if (new TextEncoder().encode(encoded).byteLength > maximum) {
    throw new Error("UI binding request exceeds the session bound");
  }
  if (request.payload.kind === "input"
    && new TextEncoder().encode(request.payload.value).byteLength > inputMaximum) {
    throw new Error("UI binding input exceeds the session bound");
  }
  return postJson("/ui/api/invocations/dispatch", request);
}

function safeBound(value: bigint, name: string): number {
  if (value <= 0n || value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(`invalid UI binding ${name}`);
  }
  return Number(value);
}

function success(effect: RyeOsEffect, kind: RyeOsEffectResultKind, data: unknown): RyeOsEffectResult {
  return { id: effect.id, ok: true, kind, data, error: null };
}

function resultKind(effect: RyeOsEffect): RyeOsEffectResultKind {
  if (effect.kind.type === "fetch_source") return "source_data";
  if (effect.kind.type === "invoke_binding") return "binding_invoked";
  return "browser_only";
}

function unwrapResult(value: unknown): unknown {
  if (!isRecord(value)) return value;
  const result = value.result;
  if (!isRecord(result)) return result ?? value;
  return result.result ?? result;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

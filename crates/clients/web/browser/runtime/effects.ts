import type {
  RyeOsEffect,
  RyeOsEffectResult,
  RyeOsEffectResultKind,
  UiBindingRequest,
  UiBindingRequestBounds,
  UiInvocationRequest,
  UiPresentationProducer,
} from "../generated";
import { encodeJsonBody, errorMessage, postEncodedJson, UnknownDeliveryError } from "./transport";

export async function runEffect(
  effect: RyeOsEffect,
  producer: UiPresentationProducer | null,
): Promise<RyeOsEffectResult> {
  const kind = effect.kind;
  switch (kind.type) {
    case "open_new_presentation_document":
    case "show_prepared_presentation_document":
      throw new Error("presentation document requires its prepare-first document host");
    case "observe_transfer_destination": {
      if (!producer || producer.presentation_id !== kind.source_producer.presentation_id
        || producer.thread_id !== kind.source_producer.thread_id
        || producer.producer_incarnation !== kind.source_producer.producer_incarnation) {
        throw new Error("transfer observation has another current source producer");
      }
      const encoded = encodeJsonBody({ action: "observe",
        presentation_id: kind.destination_presentation_id,
        thread_id: kind.destination_thread_id,
        chain_root_id: kind.destination_chain_root_id });
      if (new TextEncoder().encode(encoded).byteLength > safeBound(kind.request_bounds.max_request_bytes, "max_request_bytes")) {
        throw new Error("transfer observation exceeds admitted request bound");
      }
      const response = await postEncodedJson("/ui/api/session/seat/open", encoded);
      return success(effect, "transfer_destination_observed", {
        operation_id: crypto.randomUUID(), observation: unwrapResult(response),
      });
    }
    case "contact_transfer": {
      if (!producer) throw new Error("transfer contact requires an acknowledged active presentation producer");
      const request = kind.request;
      const actor = request.action === "commit" || request.action === "abort" || request.action === "cancel_freeze"
        ? request.transfer.source : request.transfer.destination;
      if (producer.presentation_id !== actor.presentation_id
        || producer.thread_id !== actor.thread_id) {
        throw new Error("transfer contact has another active producer");
      }
      // Transfer incarnation is original provenance, not the current lease.
      // The seat owner separately authenticates the current producer/control.
      const encoded = encodeJsonBody(request);
      if (new TextEncoder().encode(encoded).byteLength > safeBound(kind.request_bounds.max_request_bytes, "max_request_bytes")) {
        throw new Error("transfer contact exceeds admitted request bound");
      }
      const response = await postEncodedJson("/ui/api/session/seat/transfer", encoded);
      // The response is observation only. Authenticated replay owns phases.
      return success(effect, "binding_invoked", unwrapResult(response));
    }
    case "prepare_transfer": {
      if (!producer) throw new Error("transfer preparation requires an acknowledged active presentation producer");
      if (producer.presentation_id !== kind.transfer.source.presentation_id
        || producer.thread_id !== kind.transfer.source.thread_id
        || producer.producer_incarnation !== kind.transfer.source.producer_incarnation) {
        throw new Error("transfer preparation has another active producer");
      }
      const encoded = encodeJsonBody({ action: "prepare", transfer: kind.transfer, export: kind.export, freeze: kind.freeze });
      if (new TextEncoder().encode(encoded).byteLength > safeBound(kind.request_bounds.max_request_bytes, "max_request_bytes")) {
        throw new Error("transfer preparation exceeds admitted request bound");
      }
      const response = await postEncodedJson("/ui/api/session/seat/transfer", encoded);
      // Observation only: the Core result owner cannot release or activate.
      return success(effect, "binding_invoked", unwrapResult(response));
    }
    case "fetch_source": {
      const response = await dispatchBinding(kind.request, kind.request_bounds, producer);
      return success(effect, "source_data", unwrapResult(response));
    }
    case "invoke_binding": {
      const response = await dispatchBinding(kind.request, kind.request_bounds, producer);
      return success(effect, "binding_invoked", unwrapResult(response));
    }
    case "invoke_presentation_operation": {
      const operation = kind.operation;
      if (operation.body.kind !== "admission") throw new Error("composition operations are not executable invocations");
      const response = await dispatchBinding(operation.body.request, operation.request_bounds, producer, operation.operation_id);
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
      // noopener may return null even when dispatch succeeds. This is only
      // testimony that the host call returned, never observed window opening.
      return success(effect, "browser_only", {
        action: "open_url", dispatch: "returned", opened: "unknown",
      });
    case "replace_session": {
      const launch = new URL(kind.launch_url, location.href);
      if (launch.origin !== location.origin || !launch.pathname.startsWith("/ui/launch/")) {
        throw new Error("replacement UI session has an invalid launch origin or path");
      }
      location.assign(`${launch.pathname}${launch.search}${launch.hash}`);
      return success(effect, "browser_only", null);
    }
    case "release_binding_attachment": {
      if (!producer) throw new Error("attachment release requires an acknowledged active presentation producer");
      const response = await postEncodedJson(
        "/ui/api/session/attachments/detach",
        encodeJsonBody({
          producer,
          binding_attachment_id: kind.binding_attachment_id,
          binding_generation: kind.binding_generation,
          binding_digest: kind.binding_digest,
        }),
      );
      return success(effect, "browser_only", unwrapResult(response));
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

async function dispatchBinding(
  request: UiBindingRequest,
  bounds: UiBindingRequestBounds,
  producer: UiPresentationProducer | null,
  presentationOperationId?: string,
): Promise<unknown> {
  if (!producer) throw new Error("UI dispatch requires an acknowledged active presentation producer");
  const maximum = safeBound(bounds.max_request_bytes, "max_request_bytes");
  const inputMaximum = safeBound(bounds.max_input_bytes, "max_input_bytes");
  const invocation: UiInvocationRequest = { producer, binding: request };
  if (presentationOperationId !== undefined) invocation.presentation_operation_id = presentationOperationId;
  const encoded = encodeJsonBody(invocation);
  if (new TextEncoder().encode(encoded).byteLength > maximum) {
    throw new Error("UI binding request exceeds the session bound");
  }
  if (request.payload.kind === "input"
    && new TextEncoder().encode(request.payload.value).byteLength > inputMaximum) {
    throw new Error("UI binding input exceeds the session bound");
  }
  return postEncodedJson("/ui/api/invocations/dispatch", encoded);
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
  if (effect.kind.type === "observe_transfer_destination") return "transfer_destination_observed";
  if (effect.kind.type === "fetch_source") return "source_data";
  if (effect.kind.type === "invoke_binding" || effect.kind.type === "invoke_presentation_operation" || effect.kind.type === "prepare_transfer" || effect.kind.type === "contact_transfer") return "binding_invoked";
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

<script lang="ts">
  import type { RyeOsInputVm } from "../generated";
  import { dispatchUi } from "../runtime/context";
  interface Props { model: RyeOsInputVm }
  let { model }: Props = $props();
  const dispatch = dispatchUi();
  let composing = false;

  function cursorBytes(text: string, utf16Offset: number): bigint {
    return BigInt(new TextEncoder().encode(text.slice(0, utf16Offset)).length);
  }

  function emitInput(target: HTMLTextAreaElement): void {
    if (composing) return;
    dispatch({ type: "input_at", address: model.address, action: {
      type: "set_text", text: target.value, cursor: cursorBytes(target.value, target.selectionStart ?? target.value.length),
    } });
  }
</script>

<section class="composer" aria-label={model.route_label}>
  <div class="composer-route"><span class="route-state">●</span><span>{model.route_label}</span><span class="draft-state">DRAFT</span></div>
  <textarea
    value={model.text}
    placeholder={model.placeholder}
    aria-label={model.route_label}
    onfocus={() => dispatch({ type: "input_at", address: model.address, action: { type: "focus" } })}
    oncompositionstart={() => composing = true}
    oncompositionend={(event) => { composing = false; emitInput(event.currentTarget); }}
    oninput={(event) => emitInput(event.currentTarget)}
    onkeydown={(event) => {
      if (event.key === "Enter" && !event.shiftKey && !composing && model.submit_enabled) {
        event.preventDefault();
        dispatch({ type: "input_at", address: model.address, action: { type: "submit", interrupt: event.altKey } });
      }
    }}
  ></textarea>
  <div class="composer-actions"><span>{model.hint}</span><button disabled={!model.submit_enabled} onclick={() => dispatch({ type: "input_at", address: model.address, action: { type: "submit", interrupt: false } })}>↑</button></div>
</section>

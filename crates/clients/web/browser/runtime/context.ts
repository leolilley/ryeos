import { getContext, setContext } from "svelte";
import type { RyeOsUiEvent } from "../generated";

const DISPATCH_CONTEXT = Symbol("ryeos-ui-dispatch");

export type DispatchUi = (event: RyeOsUiEvent) => void;

export function provideDispatchUi(dispatch: DispatchUi): void {
  setContext(DISPATCH_CONTEXT, dispatch);
}

export function dispatchUi(): DispatchUi {
  return getContext<DispatchUi>(DISPATCH_CONTEXT);
}

import type { RyeOsFieldVm, RyeOsViewInstanceKey } from "../generated";
import type { DispatchUi } from "../runtime/context";

export class FieldCanvasController {
  constructor(canvas: HTMLCanvasElement, dispatchUi: DispatchUi, instanceKey: RyeOsViewInstanceKey);
  update(vm: RyeOsFieldVm): void;
  resize(): void;
  unmount(): void;
}

export function canCompareEntity(vm: RyeOsFieldVm, entityId: string): boolean;

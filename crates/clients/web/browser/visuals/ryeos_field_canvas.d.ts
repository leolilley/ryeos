import type { RyeOsFieldVm } from "../generated";

export interface FieldCanvasInteractions {
  entity(hit: { entityId: string; compare: boolean; activate: boolean }): void;
  group(hit: { groupId: string; collapsed: boolean }): void;
}

export class FieldCanvasController {
  constructor(canvas: HTMLCanvasElement, interactions: FieldCanvasInteractions);
  update(vm: RyeOsFieldVm): void;
  resize(): void;
  unmount(): void;
}

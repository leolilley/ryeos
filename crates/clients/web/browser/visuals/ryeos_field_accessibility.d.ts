import type { RyeOsFieldVm, RyeOsUiIntent } from "../generated";

export interface FieldAccessibilityItem {
  id: string;
  domId: string;
  label: string;
  selected: boolean;
  position: number;
  size: number;
  level: number;
  groupId: string | null;
  groupLabel: string | null;
  expanded: boolean | null;
  neighbors: string;
  selectIntent: RyeOsUiIntent | null;
  activateIntent: RyeOsUiIntent | null;
  compare: boolean;
}

export function fieldAccessibilityModel(vm: RyeOsFieldVm): FieldAccessibilityItem[];

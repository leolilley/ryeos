import type { RyeOsFieldVm } from "../generated";

export interface FieldEmptyState {
  tone: "error" | "pending" | "bounded" | "empty" | "unknown";
  title: string;
  detail: string;
}

export function fieldEmptyState(field: RyeOsFieldVm): FieldEmptyState | null;

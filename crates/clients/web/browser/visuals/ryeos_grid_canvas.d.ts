import type { RyeOsArtifactPreviewVm } from "../generated";

export function drawIndexedGrid(canvas: HTMLCanvasElement, preview: RyeOsArtifactPreviewVm, options?: { scale?: number }): void;
export function indexedGridAccessibilityLabel(preview: RyeOsArtifactPreviewVm, fallback: string): string;
export function indexedGridTranscriptPage(preview: RyeOsArtifactPreviewVm, requestedRow?: number, pageSize?: number): {
  rows: Array<{ number: number; cells: string[] }>;
  start: number;
  end: number;
  height: number;
} | null;

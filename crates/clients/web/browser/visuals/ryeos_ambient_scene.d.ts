import type { RyeOsAmbientAtlasStyleVm, RyeOsAmbientModeVm, RyeOsSceneModel } from "../generated";

export interface AmbientController {
  update(scene: RyeOsSceneModel, options: AmbientOptions): void;
  dispose(): void;
}

export interface AmbientOptions {
  mode: RyeOsAmbientModeVm;
  atlasStyle?: RyeOsAmbientAtlasStyleVm;
}

export function mountRyeOsAmbientScene(
  canvas: HTMLCanvasElement,
  scene: RyeOsSceneModel,
  options: AmbientOptions,
): AmbientController;

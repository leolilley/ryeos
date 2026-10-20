import type {
  RyeOsArtifactPreviewVm,
  RyeOsFieldEntityVm,
  RyeOsFieldVm,
} from "../generated";

export interface ComparedFieldPreviewGroup {
  entity: RyeOsFieldEntityVm;
  previews: RyeOsArtifactPreviewVm[];
}

export function fieldPreviewGroups(field: RyeOsFieldVm): {
  selectedPreviews: RyeOsArtifactPreviewVm[];
  compared: ComparedFieldPreviewGroup[];
};

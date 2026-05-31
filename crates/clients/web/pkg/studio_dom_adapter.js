import { launcherDialog, notices } from "/ui/assets/studio_components_chrome.js";
import { ambientHome, opticFrame, statusLine } from "/ui/assets/studio_components_home.js";
import { studioWorkspace, tileIdsForNode } from "/ui/assets/studio_components_workspace.js";
import { applyWorkspaceMotion, captureWorkspaceMotion } from "/ui/assets/studio_motion.js";
import { applyPresentationState, presentationState } from "/ui/assets/studio_presentation_state.js";

export function renderDom(root, vm, scene, dispatchUi, shell = {}) {
  root.className = "studio-app studio-os";
  const presentation = presentationState(vm, scene);
  applyPresentationState(root, presentation);
  const motionSnapshot = captureWorkspaceMotion(root);
  const currentTileIds = new Set(vm.workspace?.is_home ? [] : tileIdsForNode(vm.workspace?.root));
  root.replaceChildren(
    ambientHome(vm, scene, shell),
    opticFrame(vm.presentation?.frame),
    notices(vm.notices || []),
    studioWorkspace(vm.workspace, presentation.motion, dispatchUi),
    statusLine(vm, shell),
    launcherDialog(vm.launcher || {}, shell),
  );
  applyWorkspaceMotion(root, motionSnapshot, currentTileIds, presentation.motion);
}

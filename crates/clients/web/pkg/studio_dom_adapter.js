import { launcherDialog, notices } from "/ui/assets/studio_components_chrome.js";
import { opticFrame, statusLine, studioHome, topStatusLine } from "/ui/assets/studio_components_home.js";
import { studioWorkspace, tileIdsForNode } from "/ui/assets/studio_components_workspace.js";
import { applyWorkspaceMotion, captureWorkspaceMotion } from "/ui/assets/studio_motion.js";
import { applyPresentationState, presentationState } from "/ui/assets/studio_presentation_state.js";

export function renderDom(root, vm, scene, dispatchUi, shell = {}) {
  root.className = "studio-app studio-os";
  const chromeShell = { ...shell, dispatchUi };
  const topBar = vm.presentation?.chrome?.top_bar;
  const statusBar = vm.presentation?.chrome?.status_bar;
  const tabChanged = (vm.presentation?.motion || []).some((motion) => motion.type === "tab_changed");
  root.classList.toggle("topbar-visible", !!topBar?.visible);
  root.classList.toggle("topbar-transient", !topBar?.visible && tabChanged);
  root.classList.toggle("bottombar-hidden", statusBar?.visible === false);
  const presentation = presentationState(vm, scene);
  applyPresentationState(root, presentation);
  const motionSnapshot = captureWorkspaceMotion(root);
  const currentTileIds = new Set(vm.workspace?.is_home ? [] : tileIdsForNode(vm.workspace?.root));
  const home = studioHome(vm, scene, chromeShell);
  const layers = [
    opticFrame(vm.presentation?.frame),
    notices(vm.notices || []),
    topStatusLine(vm, chromeShell),
    studioWorkspace(vm.workspace, presentation.motion, dispatchUi),
    statusLine(vm, chromeShell),
    launcherDialog(vm.launcher || {}, chromeShell),
  ];
  if (root.firstChild !== home) root.prepend(home);
  while (home.nextSibling) home.nextSibling.remove();
  root.append(...layers);
  applyWorkspaceMotion(root, motionSnapshot, currentTileIds, presentation.currentMotion);
}

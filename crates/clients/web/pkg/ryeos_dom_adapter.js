import { overlayDialog } from "/ui/assets/ryeos_components_chrome.js";
import { opticFrame, statusLine, ryeosHome, topStatusLine } from "/ui/assets/ryeos_components_home.js";
import { ryeosWorkspace, tileIdsForNode } from "/ui/assets/ryeos_components_workspace.js";
import { applyWorkspaceMotion, captureWorkspaceMotion } from "/ui/assets/ryeos_motion.js";
import { applyPresentationState, presentationState } from "/ui/assets/ryeos_presentation_state.js";

export function renderDom(root, vm, scene, dispatchUi, shell = {}) {
  root.className = "ryeos-app ryeos-os";
  // Surface-declared border treatment (thick | thin | hidden | none);
  // CSS maps it onto tiles, dock tiles, and panels.
  root.dataset.border = vm.presentation?.chrome?.border || "thin";
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
  const currentTileIds = new Set(vm.workspace?.center_is_empty ? [] : tileIdsForNode(vm.workspace?.root));
  const home = ryeosHome(vm, scene, chromeShell);
  const layers = [
    opticFrame(vm.presentation?.frame),
    topStatusLine(vm, chromeShell),
    ryeosWorkspace(vm.workspace, vm.session?.ambient, presentation.motion, dispatchUi),
    statusLine(vm, chromeShell),
    overlayDialog(activeOverlayState(vm) || {}, chromeShell),
  ];
  if (root.firstChild !== home) root.prepend(home);
  while (home.nextSibling) home.nextSibling.remove();
  root.append(...layers);
  applyWorkspaceMotion(root, motionSnapshot, currentTileIds, presentation.currentMotion);
}

function activeOverlayState(vm) {
  const overlay = vm.overlays?.[0];
  if (!overlay) return null;
  return {
    open: true,
    title: overlay.title,
    query: overlay.query || "",
    selected: overlay.selected || 0,
    hint: overlay.hint || "",
    items: (overlay.items || []).map((item) => ({
      label: item.primary || item.category || "",
      hint: item.secondary || item.meta || item.category || "",
      enabled: item.enabled !== false,
      intent: item.intent,
      secondary_intent: item.secondary_intent,
    })),
  };
}

use serde::{Deserialize, Serialize};

use super::effect::{RyeOsEffectResult, RyeOsUiError};
use super::model::{BrowserSession, BrowserViewport, RyeOsDockEdge};
use crate::atlas::{AtlasItemKind, AtlasLensVm, AtlasProjectionVm};
use crate::ids::RyeOsViewInstanceKey;
use crate::view_set::{FieldCursorState, FocusDirection, ViewSpec};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsFilterField {
    ItemsQuery,
    ItemsKind,
    ServicesQuery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsUiIntent {
    Refresh,
    /// Run a content-declared affordance against a projected row: the
    /// ONE generic row interaction. The engine resolves the binding's
    /// affordance, substitutes row fields, and applies its plane (ui
    /// facet write or rye token dispatch). No product verbs in code.
    InvokeAffordance {
        instance_key: RyeOsViewInstanceKey,
        view_ref: String,
        affordance_id: String,
        record: serde_json::Value,
    },
    OpenView {
        view: ViewSpec,
    },
    OpenNewView {
        view: ViewSpec,
    },
    OpenOverlay {
        overlay_id: String,
    },
    /// Fold or unfold one launcher group. The overlay stays open — this
    /// mutates presentation state, never launches anything.
    ToggleOverlayGroup {
        group: String,
    },
    CloseFocused,
    CloseTile {
        tile_id: String,
    },
    ToggleTileMaximized {
        tile_id: String,
    },
    ToggleFocusedMaster,
    MoveFocusedTile {
        direction: RyeOsStackMoveDirection,
    },
    /// Pure placement edit between already mounted instances. No view ref,
    /// binding or executable authority is introduced by this operation.
    MoveTileBeside {
        layout_guard: String,
        tile_id: String,
        target_tile_id: String,
        edge: FocusDirection,
    },
    CycleTab {
        direction: RyeOsStackMoveDirection,
    },
    MoveTileToGroup {
        layout_guard: String,
        tile_id: String,
        target_tile_id: String,
        index: usize,
    },
    CycleViewTab {
        direction: RyeOsStackMoveDirection,
    },
    SwitchTab {
        index: usize,
    },
    NewViewSet,
    SelectViewSet {
        view_set_id: crate::ids::ViewSetId,
    },
    RenameViewSet {
        view_set_id: crate::ids::ViewSetId,
        title: String,
    },
    DuplicateViewSet {
        view_set_id: crate::ids::ViewSetId,
    },
    CloseViewSet {
        view_set_id: crate::ids::ViewSetId,
    },
    MoveTileToViewSet {
        layout_guard: String,
        tile_id: String,
        view_set_id: crate::ids::ViewSetId,
    },
    ResizeSplit {
        layout_guard: String,
        path: Vec<crate::layout::SplitBranch>,
        ratio: f32,
    },
    ToggleTopStatusBar,
    ToggleBottomStatusBar,
    ToggleBackdropBreak,
    ToggleDock {
        edge: RyeOsDockEdge,
    },
    ResizeFocused {
        direction: FocusDirection,
    },
    SelectDimension,
    InspectItem {
        canonical_ref: String,
    },
    InspectThread {
        thread_id: String,
    },
    InspectSummary {
        title: String,
        detail: serde_json::Value,
    },
    ReadFile {
        root: String,
        path: String,
    },
    CopyText {
        text: String,
    },
    OpenExternal {
        url: String,
    },
    /// Steer the route's head thread via `service:commands/submit`
    /// (`cancel` / `interrupt` / `continue` / `kill`). The reducer reads the
    /// head thread at dispatch time. This is the same authority the CLI's
    /// `ryeos commands submit` uses — no new bypass; see the daemon authz
    /// note at `command_service.submit` / `.tmp/thread-authorization-review.md`.
    SubmitThreadCommand {
        command: super::dto::ThreadControlCommand,
    },
    /// Aim the input route at a thread — the feed re-projects to its braid.
    /// Activating a forked-subthread feed entry "enters" that subthread.
    AimThread {
        thread_id: String,
    },
    /// Step INTO a child execution from a feed entry — the debugger step-in
    /// across the dispatch edge. Unlike `AimThread` (a bare route retarget),
    /// this pushes a return frame so Backspace walks back to the parent braid,
    /// and sets BOTH route coordinates: a spawned child is a fresh root, so its
    /// `chain_root_id` equals its own thread id. The lens stays the braid
    /// timeline and re-projects onto the child via the route facet — no view
    /// ref named in code.
    DrillThread {
        thread_id: String,
        chain_root_id: String,
        /// Human label for the level stepped into (the graph node, e.g.
        /// `study`), for the breadcrumb tail. `None` falls back to the child
        /// thread id.
        #[serde(default)]
        label: Option<String>,
    },
    /// Pre-fill the routed foot input to retry a failed turn: retarget the
    /// route at the SELECTED failed thread and stage that turn's original
    /// stimulus for the operator to review and resubmit. The resubmit is a
    /// continuation (a fresh successor), NOT a re-run of the terminal thread.
    /// Deliberately not one-click — the submit goes through the normal
    /// `threads/input` path, where the daemon enforces ownership and
    /// continuation eligibility.
    PrefillRetryTurn {
        thread_id: String,
        chain_root_id: String,
        input: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsUiEvent {
    /// Pointer-originated input names its exact mounted input; it must never
    /// silently edit whichever other input currently owns keyboard focus.
    InputAt {
        address: super::model::RyeOsInputAddress,
        action: RyeOsInputAction,
    },
    Activate {
        intent: RyeOsUiIntent,
    },
    SetFilter {
        tile_id: String,
        field: RyeOsFilterField,
        value: String,
    },
    SetFilesRoot {
        tile_id: String,
        root: String,
    },
    SetFilesPath {
        tile_id: String,
        path: String,
    },
    SetAtlasLayerVisible {
        /// Target tile; `None` (default) targets the ambient backdrop atlas.
        #[serde(default)]
        tile_id: Option<String>,
        kind: AtlasItemKind,
        visible: bool,
    },
    SetAtlasLens {
        #[serde(default)]
        tile_id: Option<String>,
        lens: AtlasLensVm,
    },
    SetAtlasProjection {
        #[serde(default)]
        tile_id: Option<String>,
        projection: AtlasProjectionVm,
        #[serde(default)]
        root: Option<String>,
    },
    SetAtlasFileSpacePath {
        #[serde(default)]
        tile_id: Option<String>,
        root: String,
        path: String,
    },
    SetFieldSelection {
        instance_key: RyeOsViewInstanceKey,
        entity_id: Option<String>,
    },
    MoveFieldSelection {
        instance_key: RyeOsViewInstanceKey,
        delta: i32,
    },
    SetFieldGroupCollapsed {
        instance_key: RyeOsViewInstanceKey,
        group_id: String,
        collapsed: bool,
    },
    SetFieldLayerVisible {
        instance_key: RyeOsViewInstanceKey,
        layer_id: String,
        visible: bool,
    },
    SetFieldCursor {
        instance_key: RyeOsViewInstanceKey,
        cursor: FieldCursorState,
    },
    StepFieldCursor {
        instance_key: RyeOsViewInstanceKey,
        direction: FieldStepDirection,
    },
    SetFieldPlayback {
        instance_key: RyeOsViewInstanceKey,
        playing: bool,
    },
    SetFieldQuery {
        instance_key: RyeOsViewInstanceKey,
        query: String,
    },
    MoveFieldSearchMatch {
        instance_key: RyeOsViewInstanceKey,
        delta: i32,
    },
    ToggleFieldCompare {
        instance_key: RyeOsViewInstanceKey,
        entity_id: String,
    },
    RequestFieldExpansion {
        instance_key: RyeOsViewInstanceKey,
        source: String,
        root_id: String,
    },
    ContinueFieldExpansion {
        instance_key: RyeOsViewInstanceKey,
        source: String,
        root_id: String,
    },
    ClearFieldExpansion {
        instance_key: RyeOsViewInstanceKey,
        source: String,
        root_id: String,
    },
    FocusChanged {
        target: Option<String>,
    },
    FocusDock {
        edge: RyeOsDockEdge,
    },
    FocusDirection {
        direction: FocusDirection,
    },
    OpenOverlay {
        overlay_id: String,
    },
    CloseOverlay,
    SetOverlayQuery {
        query: String,
    },
    FocusInput,
    BlurInput,
    InsertInputChar {
        ch: char,
    },
    DeleteInputChar,
    SetInputText {
        text: String,
        cursor: usize,
    },
    CompleteInput,
    /// Cycle the input's submit target through `[new conversation, …open
    /// chains]`: a directive with no `thread` (spawns a new chain) or an
    /// existing chain's head (a follow-up braids onto it). `forward` walks
    /// toward more-recent chains; the reverse walks back. The chosen target
    /// is written to the seat route (head + chain_root) and shown in the
    /// input border.
    CycleInputTarget {
        forward: bool,
    },
    /// Cycle a live-filter box to its next (forward) / previous target field —
    /// e.g. status → kind → source. The buffer clears (the prior field's text
    /// doesn't apply to the new one) and the list refetches on the new field.
    CycleFilterField {
        forward: bool,
    },
    /// Cancel the running head thread (esc while it works) — terminates it
    /// through `service:commands/submit { command_type: cancel }`, the single
    /// ryeos cancel path. No-op when the head isn't running.
    /// (Named `InterruptHead` for the esc-terminate control; the text-bearing
    /// "interrupt" is `SubmitInputInterrupt`, a redirect, not a kill.)
    InterruptHead,
    /// Submit the focused input as a cooperative STEER — a running-thread target
    /// folds it at the next turn boundary.
    SubmitInput,
    /// Submit the focused input as a forceful INTERRUPT — a running-thread target
    /// cuts the in-flight cognition and redirects. Falls back to steer semantics
    /// on non-running targets. Bound to Alt+Enter.
    SubmitInputInterrupt,
    MoveOverlaySelection {
        delta: i32,
    },
    /// Pointer selection names the exact projected overlay row; it must not
    /// derive a delta or trust a mutable index from a stale renderer frame.
    SetOverlaySelection {
        item_id: String,
    },
    ChooseOverlay {
        secondary: bool,
    },
    /// Atomically select and choose one exact projected overlay row.
    ChooseOverlayAt {
        item_id: String,
        secondary: bool,
    },
    /// Fold (`expand: false`) or unfold the launcher group under the
    /// overlay selection. Folding from a leaf lands the selection on the
    /// group's header. Inert while a search query is live — matches
    /// present under force-expanded headers.
    FoldOverlayGroup {
        expand: bool,
    },
    SetTileCursor {
        tile_id: String,
        index: usize,
    },
    /// Select a projected list point by its exact mounted view identity. This
    /// is the pointer-facing form: unlike `SetTileCursor`, it also addresses
    /// views mounted in a dock slot without inventing a tile id.
    SetViewCursor {
        instance_key: RyeOsViewInstanceKey,
        index: usize,
    },
    /// Atomically select one exact projected view item and, when requested,
    /// activate the intent attached to that same item in the reducer's
    /// current projection. A stale browser frame can therefore never select
    /// or invoke whichever record later occupied the old numeric index.
    ChooseViewItem {
        instance_key: RyeOsViewInstanceKey,
        item_id: String,
        activate: bool,
    },
    /// Dismiss one exact transient notice. Unknown/already-dismissed ids are
    /// idempotent no-ops so stale renderer frames cannot remove another one.
    DismissNotice {
        id: String,
    },
    /// Atomically toggle one exact authored section in the current mounted
    /// projection. The reducer resolves its current numeric position; stale
    /// section indices from a replaced/reordered binding are never trusted.
    ToggleViewSection {
        instance_key: RyeOsViewInstanceKey,
        section_id: String,
    },
    /// Fold (`collapsed: true`) or unfold a turn-section of a feed lens.
    SetFold {
        tile_id: String,
        section: usize,
        collapsed: bool,
    },
    /// Fold a section in an exactly addressed tile or dock view instance.
    SetViewFold {
        instance_key: RyeOsViewInstanceKey,
        section: usize,
        collapsed: bool,
    },
    /// Expand/collapse the selected row or timeline entry in the focused lens.
    /// The reducer resolves the selected record/event to a stable key; keymaps
    /// never carry row identity.
    ExpandSelectedRow {
        expand: bool,
    },
    /// Fold/unfold the selected hierarchy row's descendants. The reducer
    /// resolves the stable authored hierarchy id; renderers never send it.
    SetTreeRowCollapsed {
        collapsed: bool,
    },
    ActivateFocused,
    /// Step back up the execution-drill stack: restore the view a step-in left
    /// and the facet context it read. The "return" half of the debugger drill;
    /// no-op at the top of the tree.
    PopLens,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RyeOsInputAction {
    Focus,
    SetText { text: String, cursor: usize },
    Complete,
    Submit { interrupt: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsStackMoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldStepDirection {
    Previous,
    Next,
    Live,
}

/// Independent renderer transports whose continuity has different evidence.
/// The lossy hint bus can reconnect without proving a focused durable tail is
/// current, and neither condition should be inferred from an HTTP request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsTransportChannel {
    Session,
    Hints,
    FocusedTail,
}

/// Explicit freshness of one transport channel. A renderer reports observed
/// facts; it never collapses disconnect into execution failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsTransportFreshness {
    #[default]
    Connecting,
    Current,
    Reconnecting,
    GapResnapshotRequired,
    ExpiredOrRevoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsEvent {
    Start {
        session: BrowserSession,
        viewport: BrowserViewport,
        now_ms: u64,
    },
    Ui {
        event: RyeOsUiEvent,
    },
    EffectResult {
        result: RyeOsEffectResult,
    },
    DaemonEvent {
        payload: serde_json::Value,
    },
    /// Immediate lossy presentation patch. It never emits a fetch.
    HintReceived {
        kind: String,
        payload: serde_json::Value,
    },
    /// Coalesced reconciliation for every dirty hint kind in one client
    /// window. Reconciliation remains single-flight per resolved source key.
    HintFlushBatch {
        kinds: Vec<String>,
    },
    /// Update one transport's explicit freshness. Entering
    /// `gap_resnapshot_required` rebuilds read projections exactly once;
    /// reconnecting/current transitions never repeat a mutation.
    TransportStateChanged {
        channel: RyeOsTransportChannel,
        freshness: RyeOsTransportFreshness,
        #[serde(default)]
        observed_at_ms: Option<u64>,
        #[serde(default)]
        error: Option<RyeOsUiError>,
    },
    /// One frame from the head thread's live SSE tail. The reducer applies
    /// ryeos event semantics so both clients reach them through `dispatch`:
    /// cognition deltas accumulate into the live buffer; durable milestones
    /// supersede it and refetch the braid snapshot. Clients only open the
    /// stream and forward each frame's `(event_type, payload)`.
    ThreadTail {
        thread_id: String,
        event_type: String,
        payload: serde_json::Value,
    },
    Tick {
        now_ms: u64,
    },
    Resize {
        viewport: BrowserViewport,
    },
    RouteChanged {
        route: String,
    },
}

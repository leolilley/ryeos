# Web and terminal UI parity ledger

The browser and terminal are transport and rendering adapters over the same
`RyeOsCore`. Rust owns state, reduction, effects, layout, semantic view models,
scene projection and key commands. A checked row means both adapters were
audited; it does not claim byte-identical presentation.

`tests/parity_matrix.test.js` compares these checked inventories with the Rust
enums, rejects duplicates and unchecked rows, and requires a web and TUI
decision. Adding or removing a variant therefore requires an explicit parity
decision here.

Legend: `native` means adapter-owned mechanics; `shared` means behavior reached
through the shared core/keymap; `presentation` means a platform rendering of
the same projection; `neutral` means a documented no-op result; `gap` is
shared-core behavior the adapter cannot currently originate.

## Effects

<!-- parity:RyeOsEffectKind:start -->
| Checked Rust variant | Web | TUI | Contract |
| --- | --- | --- | --- |
| [x] `FetchSource` | native | native | Same closed binding request. |
| [x] `InvokeBinding` | native | native | Same bounded compiled binding. |
| [x] `InvokePresentationOperation` | native | native | Execute an acknowledged prepared admission operation with its retained identity. |
| [x] `OpenNewPresentationDocument` | native | gap | Browser prepares exact retained membership before offering an explicit same-origin link; no popup reservation or parent-side producer claim. Terminal returns explicit unsupported-native-effect error. |
| [x] `ShowPreparedPresentationDocument` | native | gap | Completion ACK exposes the already-prepared nonbearer presentation link; no second preparation, producer claim or parent mount publication. Terminal reports unsupported independent-document hosting. |
| [x] `ObserveTransferDestination` | native | native | Bounded destination observation under the exact current source producer; no attachment or grant follows from observation. |
| [x] `PrepareTransfer` | native | native | Existing transfer ingress carries exact producer/anchors and retained export/freeze evidence; completion returns to Core. |
| [x] `ContactTransfer` | native | native | Existing transfer contact/reconciliation transport; no adapter-owned transfer state machine. |
| [x] `SetLocationHash` | native | gap | Terminal returns an explicit unsupported-native-effect error. |
| [x] `CopyToClipboard` | native | gap | Terminal returns an explicit unsupported-native-effect error. |
| [x] `OpenUrl` | native | gap | Terminal returns an explicit unsupported-native-effect error. |
| [x] `ReplaceSession` | native | native | Both redeem the one-shot successor session. |
| [x] `ReleaseBindingAttachment` | native | native | Same exact session-bound attachment triple; explicit release only. |
<!-- parity:RyeOsEffectKind:end -->

## Root events

<!-- parity:RyeOsEvent:start -->
| Checked Rust variant | Web | TUI | Contract |
| --- | --- | --- | --- |
| [x] `Start` | native | native | Initialize the same core. |
| [x] `Ui` | native | native | Translate native input to shared UI events. |
| [x] `EffectResult` | native | native | Return every effect to the reducer. |
| [x] `PresentationProducerReady` | native | native | Report authenticated producer and next engine sequence after claim or replay. |
| [x] `DaemonEvent` | native | native | Forward observation payloads unchanged. |
| [x] `HintReceived` | native | native | Forward lossy hints unchanged. |
| [x] `HintFlushBatch` | native | native | Reconcile coalesced dirty kinds. |
| [x] `TransportStateChanged` | native | gap | TUI reconnects but does not report explicit channel freshness. |
| [x] `ThreadTail` | native | native | Forward SSE type and payload unchanged. |
| [x] `Tick` | native | native | Advance presentation time. |
| [x] `Resize` | native | native | Project native viewport dimensions. |
| [x] `RouteChanged` | native | gap | Browser hash is wired; TUI has no route adapter. |
<!-- parity:RyeOsEvent:end -->

Those two `gap` rows are the only recorded root-event gaps. Svelte migration
must not hide or widen them.

Seat acknowledgement is not a serialized root event. The closed event router
rejects `seat_append_acknowledged`; the existing authenticated receipt owner
alone confirms exact retained prefixes. Prepared operations wait for that ACK;
neither replayed semantic JSON nor effect completion can manufacture it. Cold
replay remains provisional until its fixed-anchor completion fence. Browser
disposal returns promptly while its separate settlement promise fences successor
attachment; disposal is not proof that remote execution has settled.

## UI intents

All intents are reducer-owned. Browser buttons and pointer gestures are native
producers of the same intents, not a second command vocabulary.

<!-- parity:RyeOsUiIntent:start -->
| Checked Rust variant | Web | TUI | Reachability |
| --- | --- | --- | --- |
| [x] `Refresh` | shared | shared | shared command |
| [x] `InvokeAffordance` | shared | shared | content-declared affordance |
| [x] `ActivateLocalProjection` | shared | shared | guarded Rust-projected local row activation |
| [x] `OpenView` | shared | shared | compiled view activation |
| [x] `OpenNewView` | shared | shared | compiled view activation |
| [x] `OpenViewInNewTab` | shared | shared | Launcher row captures exact insertion tab/binding; existing retained InsertTab publishes only after ACK, with fresh required subjects. |
| [x] `OpenCompositionInNewSurface` | shared | shared | Exact source tab/binding is recaptured portably; ordinary shared catalogue dialog invokes its signed composed-opening affordance. Admission remains ACK-gated. |
| [x] `OpenSurfaceInNewPresentation` | shared | gap | Captures the exact selected Surface and each portable tab origin under ordinary signed admission. Only completed ACK reveals a prepared independent-document link; native document hosting remains unsupported. |
| [x] `OpenLauncher` | shared | shared | shared launcher command |
| [x] `OpenNewPresentationDocument` | shared | shared | Shared launcher choice; browser hosts the new document, terminal reports unsupported hosting instead of success. |
| [x] `BeginTransfer` | shared | shared | Shared projected destination choice starts observation/capture through Core. |
| [x] `ContactTransfer` | shared | shared | Shared projected transfer reconciliation action. |
| [x] `PrepareTransfer` | gap | gap | Retained reducer ingress has no independent ordinary control producer; normal transfer preparation uses Core capture/freeze flow. |
| [x] `RenameSurface` | shared | shared | Shared projected Surface rename action. |
| [x] `MoveTileToSurfaceViewSet` | shared | shared | Shared projected exact destination action; Core owns the composition edit. |
| [x] `SelectSurface` | shared | shared | Web strip and shared launcher use the same projected exact-attachment action; terminal reaches it through the launcher. |
| [x] `CloseSurface` | shared | shared | Web strip and shared launcher use the same guarded exact-attachment close; terminal reaches it through the launcher. |
| [x] `OpenDialog` | shared | shared | projected dialog action |
| [x] `CloseDialog` | shared | shared | exact mounted dialog close |
| [x] `CloseFocused` | shared | shared | shared close command |
| [x] `CloseTile` | shared | shared | exact tile close |
| [x] `ToggleTileMaximized` | shared | shared | render-only focused tile projection |
| [x] `ToggleFocusedMaster` | shared | shared | layout edit |
| [x] `PromoteTileToMaster` | native | gap | exact guarded pointer promotion; TUI uses focused master promotion |
| [x] `MoveFocusedTile` | shared | shared | layout edit |
| [x] `MoveTileBeside` | shared | shared | guarded layout edit |
| [x] `CycleTab` | shared | shared | group tab command |
| [x] `MoveTileToGroup` | shared | shared | guarded layout edit |
| [x] `CycleViewTab` | shared | shared | view-tab command |
| [x] `SwitchTab` | shared | shared | exact tab selection |
| [x] `NewViewSet` | shared | shared | view-set edit |
| [x] `SelectViewSet` | shared | shared | view-set edit |
| [x] `RenameViewSet` | shared | shared | view-set edit |
| [x] `DuplicateViewSet` | shared | shared | composition-only view-set duplication |
| [x] `CloseViewSet` | shared | shared | view-set edit |
| [x] `MoveTileToViewSet` | shared | shared | guarded view-set edit |
| [x] `PinViewSelection` | shared | shared | exact mounted-view selection snapshot; shared command overlay and web tile menu |
| [x] `OpenPinnedViewAlongside` | shared | shared | fresh mount pinned to the originating view's captured selection |
| [x] `FollowViewSetSelection` | shared | shared | explicit retained selection owner; shared command overlay and web tile menu |
| [x] `SupplyRequiredSubject` | shared | shared | Browser acts inline; terminal reaches the exact focused-mount compatible-selection action through the shared launcher. |
| [x] `ReleaseBindingAttachment` | shared | shared | explicit command for a locally unused non-surface attachment; session-wide effect |
| [x] `ResizeSplit` | shared | shared | guarded ratio edit |
| [x] `ToggleTopStatusBar` | shared | shared | surface presentation |
| [x] `ToggleBottomStatusBar` | shared | shared | surface presentation |
| [x] `ToggleBackdropBreak` | shared | shared | surface presentation |
| [x] `ToggleDock` | shared | shared | surface presentation |
| [x] `ResizeFocused` | shared | shared | keyboard layout edit |
| [x] `SelectDimension` | shared | shared | semantic selection |
| [x] `InspectItem` | shared | shared | derived inspector |
| [x] `InspectThread` | shared | shared | derived inspector |
| [x] `InspectSummary` | shared | shared | inert summary |
| [x] `ReadFile` | shared | shared | bound file read |
| [x] `CopyText` | shared | shared | platform effect |
| [x] `OpenExternal` | shared | shared | platform effect |
| [x] `SubmitThreadCommand` | shared | shared | bound command service |
| [x] `AimThread` | shared | shared | route shared lens |
| [x] `DrillThread` | shared | shared | push drill stack |
| [x] `PrefillRetryTurn` | shared | shared | stage text for review |
<!-- parity:RyeOsUiIntent:end -->

## UI events and exact input actions

`shared` means native controls or shared key commands can originate the event
without duplicating its reducer transition. Browser native text and IME use
exact `InputAt` addresses; character-level events remain shared keymap paths.

<!-- parity:RyeOsUiEvent:start -->
| Checked Rust variant | Web | TUI | Reachability |
| --- | --- | --- | --- |
| [x] `InputAt` | native | gap | TUI uses the focused-input key path, not addressed native input |
| [x] `Activate` | native | shared | generic intent |
| [x] `SetFilter` | gap | gap | retained reducer event; current live filters use addressed input |
| [x] `SetFilesRoot` | gap | gap | retained reducer event with no current producer |
| [x] `SetFilesPath` | gap | gap | retained reducer event with no current producer |
| [x] `SetAtlasLayerVisible` | native | gap | browser scene control only |
| [x] `SetAtlasLens` | native | gap | browser scene control only |
| [x] `SetAtlasProjection` | gap | gap | retained reducer event with no current producer |
| [x] `SetAtlasFileSpacePath` | native | gap | browser scene/file control only |
| [x] `SetFieldSelection` | native | shared | field state |
| [x] `MoveFieldSelection` | native | shared | field state |
| [x] `SetFieldGroupCollapsed` | native | shared | field state |
| [x] `SetFieldLayerVisible` | native | gap | browser field control only |
| [x] `SetFieldCursor` | native | gap | browser direct cursor control only |
| [x] `StepFieldCursor` | native | shared | field state |
| [x] `SetFieldPlayback` | native | shared | field state |
| [x] `SetFieldQuery` | native | shared | field search |
| [x] `MoveFieldSearchMatch` | native | shared | field search |
| [x] `ToggleFieldCompare` | native | shared | field compare |
| [x] `RequestFieldExpansion` | native | shared | shared effect path |
| [x] `ContinueFieldExpansion` | native | shared | shared effect path |
| [x] `ClearFieldExpansion` | native | gap | browser expansion control only |
| [x] `FocusChanged` | native | gap | TUI reaches focus through directional shared commands |
| [x] `FocusDock` | native | shared | dock focus |
| [x] `FocusDirection` | native | shared | directional focus |
| [x] `OpenLauncher` | native | shared | launcher button or shared key command |
| [x] `OpenDialog` | shared | shared | projected dialog action |
| [x] `CloseDialog` | native | shared | mounted dialog close |
| [x] `FocusInput` | native | shared | input focus |
| [x] `BlurInput` | native | shared | input blur |
| [x] `InsertInputChar` | shared | native | character key path |
| [x] `DeleteInputChar` | shared | native | character key path |
| [x] `SetInputText` | shared | gap | web reaches it through addressed `SetText` conversion |
| [x] `CompleteInput` | shared | native | completion |
| [x] `CycleInputTarget` | shared | shared | shared keymap |
| [x] `CycleFilterField` | shared | shared | shared keymap |
| [x] `InterruptHead` | shared | shared | shared keymap |
| [x] `SubmitInput` | shared | shared | shared keymap |
| [x] `SubmitInputInterrupt` | shared | shared | shared keymap |
| [x] `SetTileCursor` | native | shared | pointer/key selection |
| [x] `SetViewCursor` | native | gap | exact tile-or-dock instance pointer selection |
| [x] `ChooseViewItem` | native | gap | atomic semantic-identity selection and current-intent activation |
| [x] `ToggleViewItemExpansion` | native | gap | exact mounted instance and semantic item identity disclosure |
| [x] `DismissNotice` | native | gap | exact idempotent browser notice dismissal |
| [x] `ToggleViewSection` | native | gap | semantic section identity resolved atomically in current projection |
| [x] `SetFold` | native | native | click/point fold |
| [x] `SetViewFold` | native | gap | exact tile-or-dock instance fold |
| [x] `ExpandSelectedRow` | shared | shared | shared keymap |
| [x] `SetTreeRowCollapsed` | shared | shared | shared keymap |
| [x] `SetViewTreeItemCollapsed` | native | gap | Browser hierarchy button addresses exact visible mount and stable row identity; terminal uses focused tree-fold keys. |
| [x] `ActivateFocused` | shared | shared | shared keymap |
| [x] `PopLens` | shared | shared | drill return |
<!-- parity:RyeOsUiEvent:end -->

<!-- parity:RyeOsInputAction:start -->
| Checked Rust variant | Web | TUI | Reachability |
| --- | --- | --- | --- |
| [x] `Focus` | native | gap | TUI uses focused-input events |
| [x] `SetText` | native | gap | Rust validates browser byte cursor |
| [x] `Complete` | native | gap | TUI uses `CompleteInput` |
| [x] `Submit` | native | gap | TUI uses shared submit events |
| [x] `NavigateSelection` | native | gap | addressed browser live-filter selection; TUI uses focused keys |
| [x] `ActivateSelection` | native | gap | addressed browser live-filter activation; TUI uses focused keys |
<!-- parity:RyeOsInputAction:end -->

## Layout and views

<!-- parity:RyeOsLayoutNodeVm:start -->
| Checked Rust variant | Web | TUI | Contract |
| --- | --- | --- | --- |
| [x] `Split` | presentation | presentation | Rust-owned axis, ratio and children. |
| [x] `Tile` | presentation | presentation | Same group, tabs, view, input and chrome facts. |
<!-- parity:RyeOsLayoutNodeVm:end -->

<!-- parity:RyeOsViewVm:start -->
| Checked Rust variant | Web | TUI | Contract |
| --- | --- | --- | --- |
| [x] `Field` | presentation | presentation | Same field model. |
| [x] `Text` | presentation | presentation | Same lines, tones and position. |
| [x] `Document` | presentation | presentation | Same bounded text, path and provenance. |
| [x] `Rows` | presentation | presentation | Same rows and affordances. |
| [x] `Timeline` | presentation | presentation | Same entries, folds and details. |
| [x] `Map` | presentation | presentation | Same scene model. |
| [x] `Atlas` | presentation | presentation | Same scene model. |
| [x] `Sections` | presentation | presentation | Same foldable sections. |
| [x] `Table` | presentation | presentation | Same columns, rows and affordances. |
| [x] `Placeholder` | presentation | presentation | Same title and message. |
| [x] `RequiredSubject` | presentation | presentation | Both display unresolved input and required facets; web acts inline, terminal through the shared launcher. |
<!-- parity:RyeOsViewVm:end -->

## Explicit platform exceptions

1. Browser hash, clipboard, URL navigation, native selection, IME, pointer,
   drag and WebGL have no byte-identical TUI equivalent. They translate
   mechanics into the shared variants above.
2. Terminal fold keys use the cell-grid point before shared directional focus;
   web folding is click/activation based. Both emit `SetFold`.
3. Scenes use different drawing primitives but the exact same scene model.
4. `TransportStateChanged` and `RouteChanged` are root-event TUI gaps. Exact
   addressed input, direct scene/field controls and several retained reducer
   setters have additional row-level gaps above. They stay visible until
   implemented or explicitly retired from the contract.

## Migration rule

Svelte may replace browser presentation only. It must retain every checked row,
keep effects/events FIFO through WASM/core, and preserve the exceptions above.
A browser-only store, command model, authority mode or product-specific route
is a competing application model and fails this gate.

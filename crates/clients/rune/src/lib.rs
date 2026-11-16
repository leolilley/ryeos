//! RyeOS in Rune: the browser (and later native) renderer over the shared
//! client runtime.
//!
//! One state and one loop. The core owns meaning and the runtime owns the
//! seat; this crate only projects the core's view model to Rune elements and
//! turns presses and keys back into core events. Async completions land in
//! the runtime's inbox and wake Rune, whose `poll` folds them in order.

mod boot;
mod field;
mod icons;
#[cfg(test)]
mod probe;
mod recovery;
mod shell;
mod theme;
mod views;

use std::collections::HashMap;
use std::time::Duration;

use rune::{AppSpec, Cx, El, KeyEvent, TextEdit, Tx};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::model::RyeOsInputAddress;
use ryeos_client_base::ui::view_model::{RyeOsInputVm, RyeOsLayoutNodeVm, RyeOsViewModel};
use ryeos_client_base::ui::{RyeOsEvent, RyeOsUiEvent};
use ryeos_client_runtime::documents::DocumentOffer;

pub use boot::Boot;

/// The core's frame clock: field playback and seat cadence, as in the
/// terminal. Rendering itself is driven by changes, not by this tick.
const TICK: Duration = Duration::from_millis(250);

/// Everything Rune holds between frames.
pub struct App {
    boot: Boot,
    phase: Phase,
    local: Local,
    /// The view model of the core as it is now, built on first use after
    /// anything that can change the core, and shared by every reader until
    /// the next such change.
    model: std::cell::RefCell<Option<std::rc::Rc<RyeOsViewModel>>>,
}

/// State only this renderer keeps: the text fields, and which tile's
/// actions menu is open. None of it is presentation state.
#[derive(Default)]
pub struct Local {
    pub(crate) edits: Edits,
    /// The `views::menu_key` of the open actions menu.
    pub(crate) menu: Option<String>,
    /// An unfinished name being typed; committing it goes through the core.
    pub(crate) rename: Option<Rename>,
    /// Whether the recovery screen is asking to confirm an independent
    /// presentation.
    pub(crate) confirming_independent: bool,
    scratch: TextEdit,
    /// Each split's main-axis length, by `shell::split_key`, as last laid out.
    pub(crate) split_extent: HashMap<String, f32>,
    /// The ratio a divider drag shows before it commits through the core.
    pub(crate) split_preview: Option<(String, f32)>,
    /// Each mounted field's search text, by `field::query_key`.
    field_queries: HashMap<String, TextEdit>,
    /// The `field-evidence:` keys of fields whose Evidence panel is open.
    pub(crate) evidence_open: std::collections::BTreeSet<String>,
    /// Open grid transcripts by `field` transcript key, at their first row.
    pub(crate) grid_transcripts: HashMap<String, u32>,
    /// The selected rows' element keys as last revealed, so only a newly
    /// selected row scrolls into view.
    revealed: std::collections::BTreeSet<String>,
    /// The shell's size in points, as last laid out.
    pub(crate) viewport: Option<(f32, f32)>,
    /// Device pixels per point, as the last frame was drawn.
    scale: std::cell::Cell<f32>,
}

/// At or under this width the shell shows only the focused tile.
pub(crate) const NARROW: f32 = 760.0;
/// At or under this width the shell tightens the gap between tiles.
pub(crate) const COMPACT: f32 = 980.0;

impl Local {
    pub(crate) fn width(&self) -> f32 {
        self.viewport.map_or(f32::MAX, |(width, _)| width)
    }

    pub(crate) fn narrow(&self) -> bool {
        self.width() <= NARROW
    }

    /// A field's search buffer, or the core's query before one exists.
    pub(crate) fn field_query(&self, key: &str, query: &str) -> TextEdit {
        self.field_queries
            .get(key)
            .cloned()
            .unwrap_or_else(|| TextEdit::new(query))
    }

    /// Forget an open menu or an unfinished rename whose subject the core no
    /// longer shows: a menu whose tile lost its actions, a view-set rename
    /// once that tab is not active, a Surface rename once it cannot be renamed.
    fn drop_stale(&mut self, vm: &RyeOsViewModel) {
        if let Some(key) = &self.menu {
            let root = vm.view_set.as_ref().and_then(|set| set.root.as_ref());
            if root
                .and_then(|root| shell::menu_intents(root, key))
                .is_none()
            {
                self.menu = None;
            }
        }
        let live = match self.rename.as_ref().map(|rename| &rename.target) {
            None => true,
            Some(RenameTarget::ViewSet(id)) => vm
                .presentation
                .chrome
                .top_bar
                .tabs
                .iter()
                .any(|tab| tab.active && tab.view_set_id == *id),
            Some(RenameTarget::Surface(ryeos_client_base::ui::RyeOsUiIntent::RenameSurface {
                surface_instance_id,
                ..
            })) => vm.session.surfaces.iter().any(|surface| {
                surface.surface_instance_id == *surface_instance_id
                    && surface.rename_intent.is_some()
            }),
            Some(RenameTarget::Surface(_)) => false,
        };
        if !live {
            self.rename = None;
        }
    }

    /// Match every mounted field's search buffer to the core's query.
    fn sync_field_queries(&mut self, vm: &RyeOsViewModel) {
        let mut mounted = Vec::new();
        collect_fields(vm, &mut mounted);
        let keys: Vec<String> = mounted
            .iter()
            .map(|(key, _)| field::query_key(key))
            .collect();
        self.field_queries.retain(|key, _| keys.contains(key));
        for (key, (_, query)) in keys.into_iter().zip(mounted) {
            let edit = self
                .field_queries
                .entry(key)
                .or_insert_with(|| TextEdit::new(""));
            if edit.text != query {
                edit.replace_text(query.to_owned());
                edit.set_caret(query.len());
            }
        }
    }
}

/// What an unfinished rename will rename.
pub(crate) enum RenameTarget {
    ViewSet(ryeos_client_base::ids::ViewSetId),
    /// The Surface's projected rename intent; its title is replaced.
    Surface(ryeos_client_base::ui::RyeOsUiIntent),
}

pub(crate) struct Rename {
    pub(crate) target: RenameTarget,
    pub(crate) edit: TextEdit,
}

/// One Rune text field per mounted input, keyed by the input's exact address.
/// A field is never rebuilt per frame (that would lose undo, selection and
/// IME preedit); it is rewritten only when the core's buffer differs.
#[derive(Default)]
pub struct Edits(HashMap<String, TextEdit>);

impl Edits {
    pub(crate) fn edit(&self, key: &str) -> TextEdit {
        self.0
            .get(key)
            .cloned()
            .unwrap_or_else(|| TextEdit::new(""))
    }

    /// Match every mounted input's field to the core, dropping fields whose
    /// input is gone.
    fn sync(&mut self, vm: &RyeOsViewModel) {
        let mut mounted = Vec::new();
        collect_inputs(vm, &mut mounted);
        let keys: Vec<String> = mounted
            .iter()
            .map(|input| input_key(&input.address))
            .collect();
        self.0.retain(|key, _| keys.contains(key));
        // A dialog's field takes focus when the dialog opens, as in the
        // browser shell; other fields wait for a press.
        let dialog_inputs: Vec<String> = vm
            .dialogs
            .iter()
            .filter_map(|dialog| dialog.input.as_ref())
            .map(|input| input_key(&input.address))
            .collect();
        for (key, input) in keys.into_iter().zip(mounted) {
            let opening = !self.0.contains_key(&key) && dialog_inputs.contains(&key);
            let edit = self.0.entry(key).or_insert_with(|| TextEdit::new(""));
            if opening {
                edit.focused = true;
            }
            if edit.text != input.text {
                edit.replace_text(input.text.clone());
                edit.set_caret(input.cursor.min(input.text.len()));
            }
        }
    }
}

/// The mounted view an event acts on; dialogs are not frames and focus
/// nothing.
fn addressed_instance(event: &RyeOsUiEvent) -> Option<&RyeOsViewInstanceKey> {
    use RyeOsUiEvent as E;
    match event {
        E::SetFieldSelection { instance_key, .. }
        | E::MoveFieldSelection { instance_key, .. }
        | E::SetFieldGroupCollapsed { instance_key, .. }
        | E::SetFieldLayerVisible { instance_key, .. }
        | E::SetFieldCursor { instance_key, .. }
        | E::StepFieldCursor { instance_key, .. }
        | E::SetFieldPlayback { instance_key, .. }
        | E::SetFieldQuery { instance_key, .. }
        | E::MoveFieldSearchMatch { instance_key, .. }
        | E::ToggleFieldCompare { instance_key, .. }
        | E::RequestFieldExpansion { instance_key, .. }
        | E::ContinueFieldExpansion { instance_key, .. }
        | E::ClearFieldExpansion { instance_key, .. }
        | E::SetViewCursor { instance_key, .. }
        | E::ChooseViewItem { instance_key, .. }
        | E::ToggleViewItemExpansion { instance_key, .. }
        | E::SetViewTreeItemCollapsed { instance_key, .. }
        | E::ToggleViewSection { instance_key, .. }
        | E::SetViewFold { instance_key, .. } => Some(instance_key),
        _ => None,
    }
}

/// The stable key of an input address.
pub(crate) fn input_key(address: &RyeOsInputAddress) -> String {
    serde_json::to_string(address).unwrap_or_default()
}

fn collect_inputs<'a>(vm: &'a RyeOsViewModel, out: &mut Vec<&'a RyeOsInputVm>) {
    out.extend(vm.dialogs.iter().filter_map(|dialog| dialog.input.as_ref()));
    if let Some(root) = vm.view_set.as_ref().and_then(|set| set.root.as_ref()) {
        collect_tile_inputs(root, out);
    }
}

fn collect_tile_inputs<'a>(node: &'a RyeOsLayoutNodeVm, out: &mut Vec<&'a RyeOsInputVm>) {
    match node {
        RyeOsLayoutNodeVm::Split { first, second, .. } => {
            collect_tile_inputs(first, out);
            collect_tile_inputs(second, out);
        }
        RyeOsLayoutNodeVm::Tile { input, .. } => out.extend(input.as_ref()),
    }
}

/// Every view the presentation mounts: dialogs and the tiles of the
/// active view set, each with its instance key.
fn mounted_views(
    vm: &RyeOsViewModel,
) -> Vec<(
    &ryeos_client_base::ids::RyeOsViewInstanceKey,
    &ryeos_client_base::ui::view_model::RyeOsViewVm,
)> {
    fn walk<'a>(
        node: &'a RyeOsLayoutNodeVm,
        out: &mut Vec<(
            &'a ryeos_client_base::ids::RyeOsViewInstanceKey,
            &'a ryeos_client_base::ui::view_model::RyeOsViewVm,
        )>,
    ) {
        match node {
            RyeOsLayoutNodeVm::Split { first, second, .. } => {
                walk(first, out);
                walk(second, out);
            }
            RyeOsLayoutNodeVm::Tile {
                instance_key, view, ..
            } => out.push((instance_key, view)),
        }
    }
    let mut out: Vec<_> = Vec::new();
    out.extend(
        vm.dialogs
            .iter()
            .map(|dialog| (&dialog.instance_key, &dialog.view)),
    );
    if let Some(root) = vm.view_set.as_ref().and_then(|set| set.root.as_ref()) {
        walk(root, &mut out);
    }
    out
}

fn collect_fields<'a>(
    vm: &'a RyeOsViewModel,
    out: &mut Vec<(&'a ryeos_client_base::ids::RyeOsViewInstanceKey, &'a str)>,
) {
    use ryeos_client_base::ui::view_model::RyeOsViewVm;
    for (key, view) in mounted_views(vm) {
        if let RyeOsViewVm::Field { field } = view {
            out.push((key, field.search.query.as_str()));
        }
    }
}

/// The element keys of every selected row, entry and section header, as the
/// views key them.
fn selected_keys(vm: &RyeOsViewModel) -> std::collections::BTreeSet<String> {
    use ryeos_client_base::ui::view_model::RyeOsViewVm;
    let mut keys = std::collections::BTreeSet::new();
    for (instance, view) in mounted_views(vm) {
        match view {
            RyeOsViewVm::Rows { rows, .. } => keys.extend(
                rows.iter()
                    .filter(|row| row.selected)
                    .map(|row| format!("item:{instance:?}:{}", row.id)),
            ),
            RyeOsViewVm::Table { rows, .. } => keys.extend(
                rows.iter()
                    .filter(|row| row.selected)
                    .map(|row| format!("item:{instance:?}:{}", row.id)),
            ),
            RyeOsViewVm::Sections { sections, .. } => {
                for section in sections {
                    if section.header_selected {
                        keys.insert(format!("section:{instance:?}:{}", section.id));
                    }
                    keys.extend(
                        section
                            .rows
                            .iter()
                            .filter(|row| row.selected)
                            .map(|row| format!("item:{instance:?}:{}", row.id)),
                    );
                }
            }
            RyeOsViewVm::Timeline {
                selected: Some(index),
                entry_ids,
                ..
            } => {
                let id = entry_ids
                    .get(*index)
                    .cloned()
                    .unwrap_or_else(|| index.to_string());
                keys.insert(format!("entry:{instance:?}:{id}"));
            }
            _ => {}
        }
    }
    keys
}

enum Phase {
    /// Establishing the session and claiming the seat.
    Starting,
    Running(Box<boot::Running>),
    /// The seat cannot be driven; the operator chooses what happens next.
    Recovering(Box<boot::Recovery>),
    /// A recovery decision is being carried out.
    Working,
    /// The page cannot continue; the message says why.
    Failed(String),
    /// A native drawing check over an in-memory core: no node, effects dropped.
    #[cfg(not(target_arch = "wasm32"))]
    Fixture(Box<ryeos_client_base::ui::RyeOsCore>),
}

impl App {
    fn new() -> App {
        App {
            boot: Boot::default(),
            phase: Phase::Starting,
            local: Local::default(),
            model: Default::default(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn fixture(core: Result<ryeos_client_base::ui::RyeOsCore, String>) -> App {
        App {
            boot: Boot::default(),
            phase: match core {
                Ok(core) => Phase::Fixture(Box::new(core)),
                Err(message) => Phase::Failed(message),
            },
            local: Local::default(),
            model: Default::default(),
        }
    }

    /// Apply `f` to the view a mounted instance shows in the current model.
    pub(crate) fn with_view<R>(
        &self,
        instance: &ryeos_client_base::ids::RyeOsViewInstanceKey,
        f: impl FnOnce(&ryeos_client_base::ui::view_model::RyeOsViewVm) -> R,
    ) -> Option<R> {
        let vm = self.view_model()?;
        mounted_views(&vm)
            .into_iter()
            .find(|(key, _)| *key == instance)
            .map(|(_, view)| f(view))
    }

    /// The view model being drawn, once there is a core.
    pub fn view_model(&self) -> Option<std::rc::Rc<RyeOsViewModel>> {
        if let Some(model) = &*self.model.borrow() {
            return Some(model.clone());
        }
        let model = std::rc::Rc::new(self.core()?.envelope(Vec::new()).view_model);
        *self.model.borrow_mut() = Some(model.clone());
        Some(model)
    }

    /// The core may have changed: the next reader builds the model again.
    fn invalidate_model(&self) {
        self.model.borrow_mut().take();
    }

    /// The core being drawn, once there is one.
    fn core(&self) -> Option<&ryeos_client_base::ui::RyeOsCore> {
        match &self.phase {
            Phase::Running(running) => Some(running.core()),
            #[cfg(not(target_arch = "wasm32"))]
            Phase::Fixture(core) => Some(core),
            Phase::Starting | Phase::Recovering(_) | Phase::Working | Phase::Failed(_) => None,
        }
    }

    /// The presentation documents offered as links.
    fn document_offers(&self) -> Vec<(DocumentOffer, Option<String>)> {
        match &self.phase {
            Phase::Running(running) => running.document_offers(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn retry_document(&mut self, id: u64) {
        if let Phase::Running(running) = &mut self.phase {
            running.retry_document(id);
        }
    }

    pub(crate) fn dismiss_document(&mut self, id: u64) {
        if let Phase::Running(running) = &self.phase {
            running.dismiss_document(id);
        }
    }

    /// Fold a core event and hand its effects to the runtime.
    pub(crate) fn dispatch(&mut self, event: RyeOsEvent) {
        match &mut self.phase {
            Phase::Running(running) => running.dispatch(event),
            #[cfg(not(target_arch = "wasm32"))]
            Phase::Fixture(core) => {
                core.dispatch(event);
            }
            Phase::Starting | Phase::Recovering(_) | Phase::Working | Phase::Failed(_) => {}
        }
        self.invalidate_model();
        self.sync_edits();
    }

    fn sync_edits(&mut self) {
        if let Some(vm) = self.view_model() {
            self.local.edits.sync(&vm);
            self.local.sync_field_queries(&vm);
            self.local.drop_stale(&vm);
        }
    }

    /// Tell the core when a field took keyboard focus, as the browser
    /// shell's focus handler does. Rune records focus on the field's buffer
    /// and has no focus callback, so the change is read here.
    fn sync_input_focus(&mut self) -> bool {
        let Some(vm) = self.view_model() else {
            return false;
        };
        let mut mounted = Vec::new();
        collect_inputs(&vm, &mut mounted);
        let Some(address) = mounted
            .into_iter()
            .find(|input| {
                !input.focused
                    && self
                        .local
                        .edits
                        .0
                        .get(&input_key(&input.address))
                        .is_some_and(|edit| edit.focused)
            })
            .map(|input| input.address.clone())
        else {
            return false;
        };
        self.ui(RyeOsUiEvent::InputAt {
            address,
            action: ryeos_client_base::ui::event::RyeOsInputAction::Focus,
        });
        true
    }

    pub(crate) fn edit_mut(&mut self, key: &str) -> &mut TextEdit {
        self.local
            .edits
            .0
            .entry(key.to_owned())
            .or_insert_with(|| TextEdit::new(""))
    }

    /// A field's text and caret (a byte offset, as the core's cursor is).
    pub(crate) fn edit_state(&self, key: &str) -> Option<(String, usize)> {
        self.local
            .edits
            .0
            .get(key)
            .map(|edit| (edit.text.clone(), edit.focus))
    }

    /// Send a UI event. An event addressed to a mounted view first focuses its
    /// tile, as a pointer press anywhere inside a frame does in the
    /// browser shell; Rune does not pass a press on a child to its frame.
    pub(crate) fn ui(&mut self, event: RyeOsUiEvent) {
        if let Some(focus) = addressed_instance(&event).and_then(|key| self.focus_for(key)) {
            self.dispatch(RyeOsEvent::Ui { event: focus });
        }
        self.dispatch(RyeOsEvent::Ui { event });
    }

    /// The event that focuses `instance`'s tile, when it is not focused.
    fn focus_for(&self, instance: &RyeOsViewInstanceKey) -> Option<RyeOsUiEvent> {
        let vm = self.view_model()?;
        fn tile(
            node: &RyeOsLayoutNodeVm,
            instance: &RyeOsViewInstanceKey,
        ) -> Option<(String, bool)> {
            match node {
                RyeOsLayoutNodeVm::Split { first, second, .. } => {
                    tile(first, instance).or_else(|| tile(second, instance))
                }
                RyeOsLayoutNodeVm::Tile {
                    instance_key,
                    tile_id,
                    focused,
                    ..
                } => (instance_key == instance).then(|| (tile_id.clone(), *focused)),
            }
        }
        let root = vm.view_set.as_ref()?.root.as_ref()?;
        let (tile_id, focused) = tile(root, instance)?;
        (!focused).then_some(RyeOsUiEvent::FocusChanged {
            target: Some(tile_id),
        })
    }

    /// Open or close one tile's actions menu.
    pub(crate) fn toggle_menu(&mut self, key: String) {
        self.local.menu = match self.local.menu.take() {
            Some(open) if open == key => None,
            _ => Some(key),
        };
    }

    pub(crate) fn close_menu(&mut self) {
        self.local.menu = None;
    }

    pub(crate) fn begin_rename(&mut self, target: RenameTarget, title: &str) {
        // Focused with the whole name selected, ready to type over.
        let mut edit = TextEdit::new(title);
        edit.anchor = 0;
        edit.focus = title.len();
        edit.focused = true;
        self.local.rename = Some(Rename { target, edit });
    }

    /// The rename field's buffer. A stray edit after the rename ended lands in
    /// a scratch buffer that is never committed.
    pub(crate) fn rename_edit(&mut self) -> &mut TextEdit {
        match &mut self.local.rename {
            Some(rename) => &mut rename.edit,
            None => &mut self.local.scratch,
        }
    }

    pub(crate) fn cancel_rename(&mut self) {
        self.local.rename = None;
    }

    /// Commit a non-empty trimmed name through the core.
    pub(crate) fn finish_rename(&mut self) {
        let Some(rename) = self.local.rename.take() else {
            return;
        };
        let title = rename.edit.text.trim().to_owned();
        if title.is_empty() {
            return;
        }
        use ryeos_client_base::ui::RyeOsUiIntent;
        let intent = match rename.target {
            RenameTarget::ViewSet(view_set_id) => {
                RyeOsUiIntent::RenameViewSet { view_set_id, title }
            }
            RenameTarget::Surface(RyeOsUiIntent::RenameSurface {
                surface_instance_id,
                binding,
                ..
            }) => RyeOsUiIntent::RenameSurface {
                surface_instance_id,
                binding,
                title,
            },
            RenameTarget::Surface(_) => return,
        };
        self.ui(RyeOsUiEvent::Activate { intent });
    }

    pub(crate) fn field_query_mut(&mut self, key: &str) -> &mut TextEdit {
        self.local
            .field_queries
            .entry(key.to_owned())
            .or_insert_with(|| TextEdit::new(""))
    }

    pub(crate) fn field_query_text(&self, key: &str) -> String {
        self.local
            .field_queries
            .get(key)
            .map(|edit| edit.text.clone())
            .unwrap_or_default()
    }

    pub(crate) fn toggle_grid_transcript(&mut self, key: String) {
        if self.local.grid_transcripts.remove(&key).is_none() {
            self.local.grid_transcripts.insert(key, 0);
        }
    }

    pub(crate) fn set_grid_transcript(&mut self, key: String, row: u32) {
        if let Some(open) = self.local.grid_transcripts.get_mut(&key) {
            *open = row;
        }
    }

    pub(crate) fn toggle_evidence(&mut self, key: String) {
        if !self.local.evidence_open.remove(&key) {
            self.local.evidence_open.insert(key);
        }
    }

    /// Record the shell's laid-out size and tell the core, as the browser
    /// shell does on a window resize.
    pub(crate) fn set_viewport(&mut self, width: f32, height: f32) {
        let scale = self.local.scale.get();
        let same_ratio = self
            .core()
            .is_some_and(|core| core.runtime.viewport.device_pixel_ratio == scale);
        if self.local.viewport == Some((width, height)) && (same_ratio || scale == 0.0) {
            return;
        }
        self.local.viewport = Some((width, height));
        let Some(core) = self.core() else {
            return;
        };
        let viewport = ryeos_client_base::ui::model::BrowserViewport {
            width: width.round() as u32,
            height: height.round() as u32,
            device_pixel_ratio: if scale > 0.0 {
                scale
            } else {
                core.runtime.viewport.device_pixel_ratio
            },
        };
        self.dispatch(RyeOsEvent::Resize { viewport });
    }

    pub(crate) fn set_split_extent(&mut self, key: String, extent: f32) {
        if self.local.split_extent.get(&key) != Some(&extent) {
            self.local.split_extent.insert(key, extent);
        }
    }

    pub(crate) fn split_extent(&self, key: &str) -> Option<f32> {
        self.local.split_extent.get(key).copied()
    }

    pub(crate) fn set_split_preview(&mut self, preview: Option<(String, f32)>) {
        self.local.split_preview = preview;
    }
}

fn view(app: &App, cx: &mut Cx<App>) -> El {
    app.local.scale.set(cx.env().scale);
    match (&app.phase, app.view_model()) {
        (Phase::Failed(message), _) => shell::boot_screen(Some(message)),
        (Phase::Recovering(recovery), _) => {
            recovery::screen(recovery, app.local.confirming_independent)
        }
        (Phase::Working, _) => recovery::working(),
        (_, Some(vm)) => shell::shell(&vm, &app.local, &app.document_offers()),
        (_, None) => shell::boot_screen(None),
    }
}

fn setup(app: &mut App, waker: rune::runtime::Waker) {
    app.boot.start(waker);
}

fn poll(app: &mut App, seconds: f64, tx: &mut Tx) -> bool {
    // A display change alters the pixel ratio without a new layout size.
    if let Some((width, height)) = app.local.viewport {
        app.set_viewport(width, height);
    }
    let changed = app.sync_input_focus() | advance(app, seconds, tx);
    if changed {
        app.sync_edits();
    }
    app.reveal_selection(tx);
    changed
}

fn advance(app: &mut App, _seconds: f64, tx: &mut Tx) -> bool {
    tx.wake_after(TICK);
    // The runtime folds what its tasks produced into the core here.
    app.invalidate_model();
    match &mut app.phase {
        Phase::Starting | Phase::Working => match app.boot.take() {
            Some(outcome) => {
                app.local.confirming_independent = false;
                app.phase = match outcome {
                    boot::Outcome::Running(running) => Phase::Running(running),
                    boot::Outcome::Recovery(recovery) => Phase::Recovering(recovery),
                    boot::Outcome::Failed(message) => Phase::Failed(message),
                };
                true
            }
            None => false,
        },
        Phase::Running(running) => match running.poll() {
            Ok(changed) => changed,
            Err(message) => {
                let Phase::Running(running) = std::mem::replace(&mut app.phase, Phase::Working)
                else {
                    return false;
                };
                app.phase = Phase::Recovering(boot::Recovery::lost(running, message));
                true
            }
        },
        Phase::Recovering(_) | Phase::Failed(_) => false,
        #[cfg(not(target_arch = "wasm32"))]
        Phase::Fixture(_) => false,
    }
}

impl App {
    /// Scroll a newly selected row, entry or section header into view, as
    /// keyboard selection moves it: the nearest scrolling list moves just far
    /// enough, and a row that is already visible stays put.
    fn reveal_selection(&mut self, tx: &mut Tx) {
        let Some(vm) = self.view_model() else {
            return;
        };
        let selected = selected_keys(&vm);
        for key in selected.difference(&self.local.revealed) {
            tx.scroll_into_view(key.clone());
        }
        self.local.revealed = selected;
    }

    /// Carry out a recovery decision. Only one runs at a time: the screen
    /// shows that it is working until the outcome arrives.
    pub(crate) fn recover(&mut self, action: boot::RecoveryAction) {
        if !matches!(self.phase, Phase::Recovering(_)) {
            return;
        }
        let Phase::Recovering(recovery) = std::mem::replace(&mut self.phase, Phase::Working) else {
            return;
        };
        self.invalidate_model();
        self.boot.recover(recovery, action);
    }
}

fn on_key(app: &mut App, key: &KeyEvent, tx: &mut Tx) -> bool {
    let taken = app.core_key(key);
    if taken {
        app.reveal_selection(tx);
    }
    taken
}

impl App {
    /// Run a key through the core's keymap. False when the core leaves it
    /// alone, so Rune may use it.
    pub(crate) fn core_key(&mut self, key: &KeyEvent) -> bool {
        let Some(event) = views::key_event(key) else {
            return false;
        };
        let taken = match &mut self.phase {
            Phase::Running(running) => running.apply_key(event),
            #[cfg(not(target_arch = "wasm32"))]
            Phase::Fixture(core) => {
                use ryeos_client_base::ui::{RyeOsKeyCommand, ryeos_key_command};
                let command = ryeos_key_command(event, core.key_context());
                if matches!(command, RyeOsKeyCommand::Ignore | RyeOsKeyCommand::Quit) {
                    return false;
                }
                core.apply_key_command(command);
                true
            }
            Phase::Starting | Phase::Recovering(_) | Phase::Working | Phase::Failed(_) => false,
        };
        if taken {
            self.invalidate_model();
            self.sync_edits();
        }
        taken
    }
}

/// The app. `fonts` are the coverage faces the page fetched beside the
/// renderer (CJK and emoji); Rune keeps its embedded Inter and IBM Plex Mono
/// first and falls back to these.
pub fn spec(fonts: &'static [&'static [u8]]) -> AppSpec<App> {
    AppSpec {
        fonts,
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        families: ("Inter", "IBM Plex Mono"),
        ..AppSpec::new("RyeOS", (1280.0, 800.0), App::new, view)
    }
}

/// The shell over an in-memory core, for drawing checks without a node:
/// events fold into the core and effects that would reach a node are
/// dropped. Rune's own arguments apply (`--snap out.png --size 1280x800`
/// renders headlessly).
///
/// `fonts` adds font files: native Rune bundles Inter but not the data face.
#[cfg(not(target_arch = "wasm32"))]
pub fn start_fixture(
    core: Result<ryeos_client_base::ui::RyeOsCore, String>,
    fonts: &'static [&'static [u8]],
) {
    fixture_app(core, fonts).run();
}

/// The fixture shell as a windowless runtime at `size`, for tests that press,
/// type and read the element tree.
#[cfg(not(target_arch = "wasm32"))]
pub fn fixture_runtime(
    core: Result<ryeos_client_base::ui::RyeOsCore, String>,
    size: (f32, f32),
) -> rune::Runtime<App> {
    fixture_app(core, &[]).headless_runtime(size, rune::Env::new(true, 1.0))
}

#[cfg(not(target_arch = "wasm32"))]
fn fixture_app(
    core: Result<ryeos_client_base::ui::RyeOsCore, String>,
    fonts: &'static [&'static [u8]],
) -> rune::App<App> {
    rune::App::new(AppSpec {
        fonts,
        poll: Some(poll),
        on_key: Some(on_key),
        families: ("Inter", "IBM Plex Mono"),
        ..AppSpec::new("RyeOS fixture", (1280.0, 800.0), App::new, view)
    })
    .init(move || App::fixture(core))
}

/// Font files the page hands over before `start`.
#[cfg(target_arch = "wasm32")]
static PENDING_FONTS: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());

/// Hand the renderer one font file. The page calls this for each coverage
/// face it fetched, then `start`; a face that failed to load is just absent.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn add_font(bytes: Vec<u8>) {
    if let Ok(mut pending) = PENDING_FONTS.lock() {
        pending.push(bytes);
    }
}

/// The page's entry point, after its fonts.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub fn start() {
    // Rune borrows its fonts for the life of the page.
    static FILES: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    static FONTS: std::sync::OnceLock<Vec<&'static [u8]>> = std::sync::OnceLock::new();
    #[cfg(target_arch = "wasm32")]
    let handed = PENDING_FONTS
        .lock()
        .map(|mut pending| std::mem::take(&mut *pending))
        .unwrap_or_default();
    #[cfg(not(target_arch = "wasm32"))]
    let handed = Vec::new();
    let files = FILES.get_or_init(|| handed);
    let fonts = FONTS.get_or_init(|| files.iter().map(Vec::as_slice).collect());
    rune::run(spec(fonts));
}

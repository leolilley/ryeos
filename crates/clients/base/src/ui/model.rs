use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use super::dto::{
    RyeOsDimensionDto, RyeOsFileSpaceDto, RyeOsFilesDto, RyeOsItemsDto, RyeOsProjectsDto,
    RyeOsThreadsDto, RyeOsTopologyDto,
};
use super::effect::{RyeOsEffect, RyeOsEffectKind, RyeOsUiError};
use super::event::{RyeOsTransportChannel, RyeOsTransportFreshness};
use super::scene_model::RyeOsSceneModel;
use super::view_model::{RyeOsMotionEventVm, RyeOsNoticeVm, RyeOsTone, RyeOsViewModel};
use crate::atlas::AtlasUiStateVm;
use crate::ids::RyeOsViewInstanceKey;
use crate::surface::{SurfaceSpec, SurfaceStyleSpec};
use crate::view_set::{FieldCursorScopeState, ViewLocalState, ViewSet};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowserSession {
    #[serde(default)]
    pub ui_binding_contract_revision: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub user_principal_id: Option<String>,
    /// All daemon-retained binding authorities admitted into this browser
    /// session. Mounts select one exact attachment; list order has no meaning.
    #[serde(default)]
    pub binding_attachments: Vec<super::binding::UiBindingAttachment>,
    /// Attachment whose signed root authored the launch composition. This
    /// is immutable provenance, not a mutable current/default authority.
    #[serde(default)]
    pub root_attachment_id: String,
    /// Exact session-retained presentation memberships. Membership does not
    /// imply an active producer; a document must explicitly claim its seat.
    pub presentations: Vec<super::binding::UiPresentationMembership>,
    #[serde(default)]
    pub events_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub device_pixel_ratio: f32,
}

impl Default for BrowserViewport {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            device_pixel_ratio: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEnvelope {
    pub schema_version: String,
    pub generation: u64,
    pub view_model: RyeOsViewModel,
    pub scene_model: RyeOsSceneModel,
    pub effects: Vec<RyeOsEffect>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFilters {
    pub items_query: String,
    pub items_kind: String,
    pub services_query: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFilesState {
    pub root: String,
    pub path: String,
}

impl Default for RyeOsFilesState {
    fn default() -> Self {
        Self {
            root: "project_ai".to_string(),
            path: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsNotice {
    pub id: String,
    pub message: String,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RyeOsFocusTarget {
    MountedView { instance_key: RyeOsViewInstanceKey },
    ViewSetTile { tile_id: String },
    Dock { edge: RyeOsDockEdge },
}

/// Buffer state only — ephemera, never braided. Where text LANDS is the
/// `input.route` facet on the seat braid (`ui::seat`), not a field
/// here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RyeOsInputState {
    pub text: String,
    pub cursor: usize,
    /// For a cyclable live filter: which declared field the buffer currently
    /// targets (index into `feeds.fields`). 0 for single-field inputs.
    #[serde(default)]
    pub filter_field: usize,
}

impl RyeOsInputState {
    pub(crate) fn validate_retained_binding(
        &self,
        key: &InputBufferKey,
        descriptor: &super::binding::UiBindingAttachment,
    ) -> Result<(), String> {
        let limit = descriptor.binding_request_bounds.max_input_bytes;
        if limit == 0
            || u64::try_from(self.text.len()).map_err(|_| "draft byte length overflow")? > limit
            || self.cursor > self.text.len()
            || !self.text.is_char_boundary(self.cursor)
        {
            return Err(
                "retained draft exceeds destination input bound or has invalid cursor".into(),
            );
        }
        let binding = super::binding_context::RetainedUiBindingAttachment::from_descriptor(
            descriptor.clone(),
        )
        .map_err(|_| "retained draft has invalid destination descriptor")?;
        if binding
            .views
            .get(&key.view_ref)
            .and_then(|view| view.input.as_ref())
            .is_none_or(|input| input.id != key.input_id)
        {
            return Err("retained draft has no exact destination input declaration".into());
        }
        Ok(())
    }

    pub fn insert_char(&mut self, ch: char) {
        let cursor = clamp_to_char_boundary(&self.text, self.cursor);
        self.text.insert(cursor, ch);
        self.cursor = cursor + ch.len_utf8();
    }

    pub fn delete_before_cursor(&mut self) {
        let cursor = clamp_to_char_boundary(&self.text, self.cursor);
        if cursor == 0 {
            return;
        }
        let prev = self.text[..cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.drain(prev..cursor);
        self.cursor = prev;
    }

    pub fn set_text(&mut self, text: String, cursor: usize) {
        self.text = text;
        self.cursor = clamp_to_char_boundary(&self.text, cursor);
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

fn clamp_to_char_boundary(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while cursor > 0 && !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn patch_thread_record(
    record: &mut serde_json::Value,
    thread_id: &str,
    status: &str,
    updated_at: &str,
) -> bool {
    let serde_json::Value::Object(map) = record else {
        return false;
    };
    if map.get("thread_id").and_then(serde_json::Value::as_str) != Some(thread_id) {
        return false;
    }
    let current = map.get("updated_at").and_then(serde_json::Value::as_str);
    if current.is_some_and(|current| current > updated_at) {
        return false;
    }
    let unchanged = map.get("status").and_then(serde_json::Value::as_str) == Some(status)
        && current == Some(updated_at);
    if unchanged {
        return false;
    }
    map.insert(
        "status".to_string(),
        serde_json::Value::String(status.to_string()),
    );
    map.insert(
        "updated_at".to_string(),
        serde_json::Value::String(updated_at.to_string()),
    );
    true
}

fn patch_thread_collection(
    response: &mut serde_json::Value,
    collection_path: &str,
    thread_id: &str,
    status: &str,
    updated_at: &str,
) -> bool {
    let mut collection = response;
    for part in collection_path.split('.') {
        let Some(next) = collection.get_mut(part) else {
            return false;
        };
        collection = next;
    }
    let Some(records) = collection.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for record in records {
        changed |= patch_thread_record(record, thread_id, status, updated_at);
    }
    changed
}

fn is_thread_collection(path: Option<&str>) -> bool {
    path.is_some_and(|path| path.rsplit('.').next() == Some("threads"))
}

/// Layout-neutral key for a transient input buffer. The buffer belongs to
/// a view instance, not to a placement: the same `view:` rendered twice
/// (two tiles, a tile and a slot) has independent buffer state. The
/// `view_instance_key` is a typed layout address, not a semantic category.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InputBufferKey {
    pub view_instance_key: RyeOsViewInstanceKey,
    pub binding_scope: String,
    pub view_ref: String,
    pub input_id: String,
    /// Route identity for a conversation composer. This is absent for filters
    /// and ordinary form inputs. A chain scope deliberately excludes its
    /// moving head thread, so the same draft follows that logical work across
    /// continuation ratchets without leaking to another work item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_scope: Option<String>,
}

/// Exact presentation origin. This fences a renderer event, not a capability:
/// submission still goes through the existing compiled binding and seat route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RyeOsInputAddress {
    pub session_id: String,
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
    pub owner: RyeOsInputOwner,
    pub buffer: InputBufferKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RyeOsInputOwner {
    Shared {
        presentation_id: crate::ids::PresentationId,
    },
    Surface {
        presentation_id: crate::ids::PresentationId,
        surface_instance_id: crate::ids::SurfaceInstanceId,
        view_set_id: crate::ids::ViewSetId,
    },
    SurfaceDialog {
        presentation_id: crate::ids::PresentationId,
        surface_instance_id: crate::ids::SurfaceInstanceId,
    },
}

impl InputBufferKey {
    pub fn new(
        view_instance_key: RyeOsViewInstanceKey,
        view_ref: impl Into<String>,
        input_id: impl Into<String>,
        descriptor: &super::binding::UiBindingAttachment,
    ) -> Self {
        Self::from_binding_coordinate(
            view_instance_key,
            view_ref,
            input_id,
            &super::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: descriptor.binding_attachment_id.clone(),
                binding_generation: descriptor.binding_generation,
                binding_digest: descriptor.binding_digest.clone(),
            },
        )
    }

    /// Construct data from an exact coordinate; this does not admit it.
    /// Address validation still compares against the mounted binding owner.
    pub fn from_binding_coordinate(
        view_instance_key: RyeOsViewInstanceKey,
        view_ref: impl Into<String>,
        input_id: impl Into<String>,
        binding: &super::binding::UiBindingAttachmentCoordinate,
    ) -> Self {
        Self {
            view_instance_key,
            binding_scope: Self::binding_scope(binding),
            view_ref: view_ref.into(),
            input_id: input_id.into(),
            target_scope: None,
        }
    }

    pub fn scoped_for_input(
        mut self,
        input: &super::content::InputBlock,
        route: &super::seat::InputRoute,
    ) -> Self {
        if input.submits_to_route() {
            self.target_scope = Some(route_draft_scope(route));
        }
        self
    }

    pub fn route_facet_key(&self) -> String {
        super::seat::input_route_facet_key(&self.view_instance_key)
    }

    /// Stable string key for the buffer map (JSON map keys must be
    /// strings). Components are separated by the ASCII unit separator.
    pub fn storage_key(&self) -> String {
        let base = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.view_instance_key, self.binding_scope, self.view_ref, self.input_id
        );
        match &self.target_scope {
            Some(scope) => format!("{base}\u{1f}{scope}"),
            None => base,
        }
    }

    /// Closed inverse of the existing draft storage coordinate. Parsing does
    /// not admit the binding or route; callers compare those to the mount owner.
    pub(crate) fn from_storage_key(value: &str) -> Option<Self> {
        let parts = value.split('\u{1f}').collect::<Vec<_>>();
        if !(parts.len() == 4 || parts.len() == 5)
            || parts.iter().any(|part| part.is_empty())
            || parts[1].len() != 64
            || !parts[1]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        super::composition_operation::view(parts[2]).ok()?;
        let key = Self {
            view_instance_key: RyeOsViewInstanceKey::from_canonical(parts[0])?,
            binding_scope: parts[1].to_owned(),
            view_ref: parts[2].to_owned(),
            input_id: parts[3].to_owned(),
            target_scope: parts.get(4).map(|part| (*part).to_owned()),
        };
        (key.storage_key() == value).then_some(key)
    }

    /// Inspect ownership without re-deriving the current route: a mounted
    /// composer can retain drafts for previously selected subjects as well.
    pub fn storage_key_belongs_to(key: &str, instance: &RyeOsViewInstanceKey) -> bool {
        key.split_once('\u{1f}')
            .is_some_and(|(owner, _)| owner == instance.as_str())
    }

    pub(crate) fn binding_scope(binding: &super::binding::UiBindingAttachmentCoordinate) -> String {
        super::seat_digest::seat_payload_digest(
            &serde_json::to_value(binding).expect("binding coordinate serializes"),
        )
        .expect("closed binding coordinate has canonical digest")
    }

    pub fn storage_key_matches_view_binding(
        key: &str,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
        binding: &super::binding::UiBindingAttachmentCoordinate,
    ) -> bool {
        let mut components = key.split('\u{1f}');
        components.next() == Some(instance.as_str())
            && components.next() == Some(Self::binding_scope(binding).as_str())
            && components.next() == Some(view_ref)
    }
}

fn route_draft_scope(route: &super::seat::InputRoute) -> String {
    if let Some(chain_root) = route.chain_root.as_deref() {
        return format!("chain:{chain_root}");
    }
    if let Some(thread) = route.thread.as_deref() {
        return format!("thread:{thread}");
    }
    // Before a chain exists, the complete signed-route projection is the
    // target. Serialization cannot fail for InputRoute's closed value shape;
    // keeping it readable also makes accidental target drift reviewable.
    format!(
        "fresh:{}",
        serde_json::to_string(route).expect("InputRoute serializes")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsDockEdge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsUiState {
    pub filters: RyeOsFilters,
    pub files: RyeOsFilesState,
    /// Ambient/backdrop atlas state — the empty-center `namespace_atlas`
    /// background. Surface-level, not a tile. Per-tile Atlas tiles keep
    /// their own arrangement in [`Self::tile_atlas`].
    #[serde(default)]
    pub atlas: AtlasUiStateVm,
    /// Per-tile atlas arrangements, keyed by tile id (string). Atlas/graph
    /// were never meant to show one fixed thing — a surface can host
    /// several independent atlas tiles, each with its own layers, lens,
    /// and projection. A tile with no entry falls back to [`Self::atlas`].
    #[serde(default)]
    pub tile_atlas: BTreeMap<String, AtlasUiStateVm>,
    pub motion: Vec<RyeOsMotionEventVm>,
    pub loading: BTreeMap<String, bool>,
    pub notices: Vec<RyeOsNotice>,
    /// Bounded structured failures keyed by the exact effect coordinate. A
    /// mutation whose outcome is unknown stays distinguishable from a refused
    /// request after its transient notice is gone.
    #[serde(default)]
    pub effect_failures: BTreeMap<u64, RyeOsUiError>,
    pub route: Option<String>,
    #[serde(default)]
    pub top_status_visible: bool,
    #[serde(default = "default_true")]
    pub bottom_status_visible: bool,
    #[serde(default)]
    pub backdrop_break_amount: f32,
    #[serde(default)]
    pub backdrop_break_target: f32,
}

impl Default for RyeOsUiState {
    fn default() -> Self {
        Self {
            filters: RyeOsFilters::default(),
            files: RyeOsFilesState::default(),
            atlas: AtlasUiStateVm::default(),
            tile_atlas: BTreeMap::new(),
            motion: Vec::new(),
            loading: BTreeMap::new(),
            notices: Vec::new(),
            effect_failures: BTreeMap::new(),
            route: None,
            // Both status bars start hidden — their content was incoherent
            // and we have nothing settled to put there yet. Toggle-on still
            // works if we decide on content later.
            top_status_visible: false,
            bottom_status_visible: true,
            backdrop_break_amount: 0.0,
            backdrop_break_target: 0.0,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RyeOsDataState {
    pub session: Option<BrowserSession>,
    pub dimension: Option<RyeOsDimensionDto>,
    pub topology: Option<RyeOsTopologyDto>,
    pub projects: Option<RyeOsProjectsDto>,
    pub threads: Option<RyeOsThreadsDto>,
    pub items: Option<RyeOsItemsDto>,
    pub tile_items: HashMap<String, RyeOsItemsDto>,
    pub files: Option<RyeOsFilesDto>,
    pub file_space: Option<RyeOsFileSpaceDto>,
    pub tile_files: HashMap<String, RyeOsFilesDto>,
    /// Per-tile file-space, keyed by tile id, for atlas tiles whose
    /// `body.scope` declares a file-space root/path. Absent tile → the
    /// shared `file_space` (ambient / scopeless tiles).
    pub tile_file_space: HashMap<String, RyeOsFileSpaceDto>,
    /// Bound-view source responses, keyed by the canonical source-instance
    /// identity (mounted view host plus channel). The values remain open JSON
    /// projected through view bindings.
    #[serde(default)]
    pub sources: HashMap<String, serde_json::Value>,
    /// Latest failed source fetch per bound view instance. Errors are keyed
    /// exactly like `sources`, cleared when a retry launches or newer data
    /// lands, and rendered only when no usable response exists.
    #[serde(default)]
    pub source_errors: HashMap<String, RyeOsUiError>,
    /// Transient projected timeline cache, keyed by the same source key as
    /// `sources`. Rebuilt only when source data lands so scroll keys do not
    /// re-project long transcripts on every frame.
    #[serde(default, skip)]
    pub(crate) timeline_sources: HashMap<String, RyeOsTimelineSourceCache>,
    /// Parsed, bounded field documents keyed by source-instance identity.
    /// Rebuilt only when a response is accepted, so local selection/replay
    /// envelopes never deserialize multi-megabyte fact documents again.
    #[serde(default, skip)]
    pub(crate) field_sources: HashMap<String, Result<super::field::FieldFactsDocument, String>>,
    /// Expensive renderer-neutral field projections, keyed by view instance.
    /// Local controls are applied to a cloned base VM and therefore never
    /// rerun joins/rules over the complete accepted fact set.
    #[serde(default, skip)]
    pub(crate) field_projections:
        RefCell<HashMap<RyeOsViewInstanceKey, super::field::FieldProjectionCacheEntry>>,
    /// The newest fetch effect id issued for each source key (the current
    /// revalidation target; informational once a floor exists).
    #[serde(default)]
    pub source_epoch: HashMap<String, u64>,
    /// The result id currently STORED per source key. Responses land
    /// MONOTONICALLY against this — a superseded-but-first response still
    /// renders (a refetch cadence faster than the query latency must not
    /// starve the view into a permanent "loading"), while an out-of-order
    /// older response never overwrites a newer one.
    #[serde(default)]
    pub source_stored_epoch: HashMap<String, u64>,
    /// Minimum acceptable result id per source key, raised when the key's
    /// SUBJECT changes (lens swap, drill return, selection facet write):
    /// a straggler for the previous subject must never land under the new
    /// one, no matter what is or isn't stored.
    #[serde(default)]
    pub source_floor: HashMap<String, u64>,
    /// Hash of the resolved source ref + params currently assigned to each
    /// source instance. This is the subject identity, independent of whatever
    /// response happens to be cached under it.
    #[serde(default)]
    pub source_subject_fingerprint: HashMap<String, String>,
    /// Exact result of resolving required authored source parameters for each
    /// mounted source key. Kept beside the source subject so renderers do not
    /// repeat facet resolution or infer availability from missing data.
    #[serde(default)]
    pub source_unresolved: HashMap<String, bool>,
    /// Transient live cognition stream for the tailed head thread —
    /// ephemeral deltas accumulated between durable snapshots. Not truth;
    /// the braid snapshot is. Cleared once a fresh snapshot supersedes it.
    #[serde(default)]
    pub live_delta: Option<RyeOsLiveDelta>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RyeOsTimelineSourceCache {
    pub entries: Vec<super::timeline::RyeOsTimelineEntryVm>,
    pub indents: Vec<u8>,
    pub sources: Vec<Option<super::timeline::TimelineEntrySource>>,
    pub arrivals: Vec<Option<u64>>,
    /// Section (turn) index per entry, parallel to `entries`. Depends only on
    /// `entries`, so it is derived once here rather than rescanned by every
    /// windowed render frame (`timeline::fold_timeline_window`).
    pub sections: Vec<usize>,
    /// Sections headed by a real `Separator` — the ones a fold key can
    /// collapse. Derived alongside `sections`.
    pub collapsible: std::collections::BTreeSet<usize>,
}

// The live cognition buffer type + its accumulation logic live with the rest
// of the thread-execution-stream concern in `super::timeline`.
pub use super::timeline::RyeOsLiveDelta;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsRuntimeState {
    pub viewport: BrowserViewport,
    pub now_ms: u64,
    #[serde(default)]
    pub last_tick_ms: u64,
    #[serde(default)]
    pub activity_pulse: f32,
    #[serde(default)]
    pub attention_until_ms: u64,
    /// Scene animation frame counter, driven by `advance_scene_frame`.
    /// While `scene_frame_anchor_ms` is 0 the clock is undriven and
    /// `RyeOsCore::scene_frame` falls back to wall-clock quantization.
    #[serde(default)]
    pub scene_frame: u64,
    #[serde(default)]
    pub scene_frame_anchor_ms: u64,
    /// Renderer-neutral continuity state. Channels are recorded separately so
    /// a lossy hint reconnect cannot masquerade as durable-tail freshness.
    #[serde(default)]
    pub transport: RyeOsTransportState,
}

impl Default for RyeOsRuntimeState {
    fn default() -> Self {
        Self {
            viewport: BrowserViewport::default(),
            now_ms: 0,
            last_tick_ms: 0,
            activity_pulse: 0.0,
            attention_until_ms: 0,
            scene_frame: 0,
            scene_frame_anchor_ms: 0,
            transport: RyeOsTransportState::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTransportChannelState {
    pub freshness: RyeOsTransportFreshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RyeOsUiError>,
}

impl Default for RyeOsTransportChannelState {
    fn default() -> Self {
        Self {
            freshness: RyeOsTransportFreshness::Connecting,
            last_observed_at_ms: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTransportState {
    #[serde(default)]
    pub channels: BTreeMap<RyeOsTransportChannel, RyeOsTransportChannelState>,
}

impl Default for RyeOsTransportState {
    fn default() -> Self {
        Self {
            channels: BTreeMap::from([(
                RyeOsTransportChannel::Session,
                // `RyeOsCore` is constructed only after the exact session
                // bootstrap succeeds. Pre-bootstrap connecting state belongs
                // to the launcher adapter; once a core exists its session
                // channel starts current.
                RyeOsTransportChannelState {
                    freshness: RyeOsTransportFreshness::Current,
                    ..RyeOsTransportChannelState::default()
                },
            )]),
        }
    }
}

impl RyeOsTransportState {
    pub fn overall_freshness(&self) -> RyeOsTransportFreshness {
        let session = self
            .channels
            .get(&RyeOsTransportChannel::Session)
            .map(|state| state.freshness)
            .unwrap_or(RyeOsTransportFreshness::Connecting);
        if session == RyeOsTransportFreshness::ExpiredOrRevoked {
            return session;
        }
        if self
            .channels
            .values()
            .any(|state| state.freshness == RyeOsTransportFreshness::GapResnapshotRequired)
        {
            return RyeOsTransportFreshness::GapResnapshotRequired;
        }
        if session != RyeOsTransportFreshness::Current {
            return session;
        }
        match self.channels.get(&RyeOsTransportChannel::FocusedTail) {
            Some(state) if state.freshness != RyeOsTransportFreshness::Current => state.freshness,
            _ => RyeOsTransportFreshness::Current,
        }
    }

    pub fn last_observed_at_ms(&self) -> Option<u64> {
        self.channels
            .values()
            .filter_map(|state| state.last_observed_at_ms)
            .max()
    }
}

#[derive(Debug, Clone, Default)]
pub struct RyeOsCore {
    /// Adapter implementation fact only; never an admission or authority grant.
    pub supports_independent_documents: bool,
    pub data: RyeOsDataState,
    /// Parsed, retained binding tables keyed by daemon-issued attachment id.
    pub binding_attachments: BTreeMap<String, super::binding_context::RetainedUiBindingAttachment>,
    /// Context used only when inserting a new mount into a view set.
    pub view_set_insertion_attachments: BTreeMap<crate::ids::ViewSetId, String>,
    /// Runtime provenance for compositions opened from the personal library.
    /// This maps fresh set identities back to stable template identities so a
    /// later saved set can name cross-set follow relationships without ever
    /// serializing runtime ids.
    pub view_set_template_ids: BTreeMap<crate::ids::ViewSetId, String>,
    /// Immutable launch-composition provenance.
    pub root_attachment_id: String,
    pub ui: RyeOsUiState,
    /// Seat braid (engine-local log while the engine holds append
    /// authority; see `ui::seat`). Seat truth — route, selection —
    /// folds from here, never from renderer state.
    pub seat: super::seat::SeatLog,
    pub(crate) presentation_operations: super::admission_operation::PresentationOperationState,
    /// Checked derivative of the exact acknowledged seat prefix, never an authority source.
    pub(crate) acknowledged_composition_cache:
        Option<(usize, super::composition_replay::UiCompositionReplay)>,
    /// Only completed authenticated cold-cut frames, never transport
    /// responses or user-supplied phase hashes. Historical evidence is not a
    /// current writer or fresh pair authorization.
    pub(crate) authenticated_transfer_proofs: BTreeMap<
        String,
        (
            super::transfer_proof_frame::UiTransferProofFrame,
            ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        ),
    >,
    pub(crate) authenticated_cancellation_proofs: BTreeMap<
        String,
        super::transfer_cancellation_proof_frame::UiMoveFreezeCancellationProofFrame,
    >,
    /// Set only after a private paired cold candidate reaches its authenticated
    /// canonical tip. Presence of frames alone is not a completed replay.
    pub(crate) authenticated_reproof_tip: Option<(String, u64)>,
    /// One local contact per operation/action until a fresh authenticated
    /// bootstrap. A response cannot reset this uncertainty fence.
    pub(crate) contacted_transfer_actions: BTreeSet<(String, String)>,
    /// A definite refusal may be retried only after a new completed paired
    /// observation; an unknown contact never enters this set.
    pub(crate) refused_transfer_contacts: BTreeSet<(String, String)>,
    /// Runtime-only mounted-view selection relationships. Reusable layout
    /// preferences intentionally never serialize this map.
    pub selection_attachments:
        BTreeMap<RyeOsViewInstanceKey, super::attachment::SelectionAttachment>,
    /// Immutable admitted fallback, rebuilt from the acknowledged seat prefix.
    /// Explicit durable selection (including null) always takes precedence.
    pub(crate) retained_work_selections: BTreeMap<crate::ids::ViewSetId, serde_json::Value>,
    /// Derived only from the acknowledged causal composition history.
    pub(crate) retained_lens_frames:
        BTreeMap<RyeOsViewInstanceKey, Vec<super::composition_lens::RetainedLensFrame>>,
    pub(crate) retained_input_routes:
        BTreeMap<RyeOsViewInstanceKey, (u64, Option<serde_json::Value>)>,
    pub(crate) retained_lens_labels: BTreeMap<RyeOsViewInstanceKey, Option<String>>,
    /// Surface-declared chrome style (border treatment).
    pub style: SurfaceStyleSpec,
    pub presentation: Option<super::presentation_state::PresentationComposition>,
    /// Draft writes stay inside Core's exact-instance mutation guards. Platform
    /// adapters use the focused input accessors, not a mutable draft registry.
    pub(crate) input_buffers: BTreeMap<String, RyeOsInputState>,
    pub focused_input: Option<RyeOsViewInstanceKey>,
    pub(crate) dialog_return_focus: Option<RyeOsViewInstanceKey>,
    pub field_query_editing: Option<RyeOsViewInstanceKey>,
    pub runtime: RyeOsRuntimeState,
    pub pending_effects: BTreeMap<u64, RyeOsEffectKind>,
    /// Surface-instance-scoped replay state declared by signed field views.
    /// The storage key combines the mounted surface instance and scope id.
    pub field_cursor_scopes: BTreeMap<String, FieldCursorScopeState>,
    /// Latest hint-driven reconciliation requested while the same source key
    /// already had a fetch in flight. Transient client backpressure: at most
    /// one request runs and one trailing request waits per source.
    pub(crate) deferred_source_fetches: BTreeMap<String, DeferredSourceFetch>,
    pub generation: u64,
    pub next_effect_id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct DeferredSourceFetch {
    pub view_ref: String,
    pub channel: String,
    pub params: serde_json::Value,
}

impl RyeOsCore {
    pub fn active_surface(&self) -> Option<&super::presentation_state::SurfaceComposition> {
        self.presentation.as_ref()?.active_surface()
    }
    pub fn active_surface_mut(
        &mut self,
    ) -> Option<&mut super::presentation_state::SurfaceComposition> {
        self.presentation.as_mut()?.active_surface_mut()
    }
    pub fn active_surface_view_sets(&self) -> Option<&[ViewSet]> {
        Some(self.active_surface()?.view_sets())
    }
    pub fn active_view_set(&self) -> Option<&ViewSet> {
        self.presentation.as_ref()?.active_view_set()
    }
    pub fn active_view_set_mut(&mut self) -> Option<&mut ViewSet> {
        self.presentation.as_mut()?.active_view_set_mut()
    }
    pub fn surface_view_sets(
        &self,
    ) -> impl Iterator<Item = (&crate::ids::SurfaceInstanceId, &ViewSet)> {
        self.presentation
            .iter()
            .flat_map(|presentation| presentation.view_sets())
    }
    pub fn view_sets(&self) -> impl Iterator<Item = &ViewSet> {
        self.surface_view_sets().map(|(_, set)| set)
    }
    pub fn view_set(&self, id: crate::ids::ViewSetId) -> Option<&ViewSet> {
        self.view_sets().find(|set| set.id == id)
    }
    pub fn view_set_mut(&mut self, id: crate::ids::ViewSetId) -> Option<&mut ViewSet> {
        let surface = self
            .surface_view_sets()
            .find(|(_, set)| set.id == id)?
            .0
            .clone();
        self.presentation.as_mut()?.view_set_mut(&surface, id)
    }
    pub fn view_set_for_instance(&self, instance: &RyeOsViewInstanceKey) -> Option<&ViewSet> {
        Some(
            self.presentation
                .as_ref()?
                .view_set_for_instance(instance)?
                .1,
        )
    }
    pub fn mounted_local_state(&self, instance: &RyeOsViewInstanceKey) -> Option<&ViewLocalState> {
        Some(self.presentation.as_ref()?.mount(instance)?.local())
    }
    pub fn mounted_local_state_mut(
        &mut self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<&mut ViewLocalState> {
        if self.move_freeze_restricts_instance(instance) {
            return None;
        }
        self.presentation.as_mut()?.mount_local_mut(instance)
    }
    pub fn shared_dock_instance_key(&self, edge: RyeOsDockEdge) -> Option<RyeOsViewInstanceKey> {
        let presentation = self.presentation.as_ref()?;
        let edge = match edge {
            RyeOsDockEdge::Top => "top",
            RyeOsDockEdge::Left => "left",
            RyeOsDockEdge::Right => "right",
            RyeOsDockEdge::Bottom => "bottom",
        };
        Some(
            presentation
                .docks()
                .get(edge)?
                .view()
                .instance_key()
                .clone(),
        )
    }
    pub fn input_owner(&self, instance: &RyeOsViewInstanceKey) -> Option<RyeOsInputOwner> {
        let composition = self.presentation.as_ref()?;
        match composition.mount(instance)? {
            super::presentation_state::MountRef::SurfaceTile {
                surface_id,
                view_set_id,
                ..
            } => Some(RyeOsInputOwner::Surface {
                presentation_id: composition.id().clone(),
                surface_instance_id: surface_id.clone(),
                view_set_id,
            }),
            super::presentation_state::MountRef::SurfaceDialog { surface_id, .. } => {
                Some(RyeOsInputOwner::SurfaceDialog {
                    presentation_id: composition.id().clone(),
                    surface_instance_id: surface_id.clone(),
                })
            }
            _ => Some(RyeOsInputOwner::Shared {
                presentation_id: composition.id().clone(),
            }),
        }
    }
    pub fn mount_owner(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<crate::ids::UiViewMountOwner> {
        self.presentation.as_ref()?.mount(instance)?;
        instance.mount_owner()
    }
    pub fn mounted_instances(&self) -> impl Iterator<Item = RyeOsViewInstanceKey> + '_ {
        self.presentation.iter().flat_map(|presentation| {
            presentation
                .docks()
                .values()
                .map(|mount| mount.view().instance_key().clone())
                .chain(
                    presentation
                        .dialogs()
                        .values()
                        .map(|mount| mount.view().instance_key().clone()),
                )
                .chain(
                    presentation.view_sets().flat_map(|(_, set)| {
                        set.tiles.values().map(|tile| tile.instance_key.clone())
                    }),
                )
        })
    }
    pub fn same_surface(
        &self,
        instance: &RyeOsViewInstanceKey,
        set_id: crate::ids::ViewSetId,
    ) -> bool {
        let Some(presentation) = self.presentation.as_ref() else {
            return false;
        };
        let Some(crate::ids::UiViewMountOwner::Surface {
            presentation_id,
            surface_instance_id,
        }) = self.mount_owner(instance)
        else {
            return false;
        };
        presentation_id == *presentation.id()
            && presentation
                .view_set(&surface_instance_id, set_id)
                .is_some()
    }

    pub(crate) fn select_surface_instance(
        &mut self,
        id: &crate::ids::SurfaceInstanceId,
        binding: &super::binding::UiBindingAttachmentCoordinate,
    ) -> Vec<RyeOsEffect> {
        if !self.composition_write_available() {
            return Vec::new();
        }
        let Some(producer) = self.presentation_operations.producer.as_ref() else {
            return Vec::new();
        };
        let selection = super::seat::UiSurfaceSelection {
            surface_instance_id: id.clone(),
            binding: binding.clone(),
        };
        let payload = serde_json::json!({"key":super::seat::KEY_ACTIVE_SURFACE, "value":selection});
        let Some(root) = self
            .presentation
            .as_ref()
            .and_then(|p| self.binding_attachment(&p.root_binding().binding_attachment_id))
        else {
            return Vec::new();
        };
        if !self.producer_matches_membership(producer)
            || !super::admission_operation::single_seat_append_fits(
                producer,
                "seat.facet",
                &payload,
                root.binding_request_bounds.max_seat_append_bytes,
            )
        {
            return Vec::new();
        }
        if self.presentation_operations.replay_blocked
            || !self
                .presentation
                .as_ref()
                .and_then(|p| p.surfaces().get(id))
                .is_some_and(|surface| surface.binding() == binding)
            || !self
                .binding_attachment(&binding.binding_attachment_id)
                .is_some_and(|attachment| {
                    attachment.binding_generation == binding.binding_generation
                        && attachment.binding_digest == binding.binding_digest
                })
        {
            return Vec::new();
        }
        self.seat.append_facet(
            super::seat::KEY_ACTIVE_SURFACE,
            serde_json::to_value(selection).expect("Surface selection serializes"),
        );
        self.bump_generation();
        Vec::new()
    }

    pub(crate) fn input_key_for(
        &self,
        view_instance_key: RyeOsViewInstanceKey,
        view_ref: impl Into<String>,
        input: &super::content::InputBlock,
    ) -> Option<InputBufferKey> {
        let descriptor = self.binding_attachment_for_instance(&view_instance_key)?;
        let key = InputBufferKey::new(view_instance_key, view_ref, input.id.clone(), descriptor);
        let route = self.input_route_for(&key);
        Some(key.scoped_for_input(input, &route))
    }

    pub(crate) fn input_route_for(&self, key: &InputBufferKey) -> super::seat::InputRoute {
        self.route_for_instance(&key.view_instance_key)
    }

    pub(crate) fn route_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> super::seat::InputRoute {
        // Local sequences and canonical chain positions cannot be compared.
        let facet = super::seat::input_route_facet_key(instance);
        let pending = self
            .seat
            .events()
            .get(self.presentation_operations.acknowledged_prefix_len..)
            .unwrap_or_default()
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                super::seat::SeatEventKind::Facet { key, value } if key == &facet => Some(value),
                _ => None,
            });
        let explicit = pending.map(Some).or_else(|| {
            self.retained_input_routes
                .get(instance)
                .map(|(_, value)| value.as_ref())
        });
        match explicit {
            // Explicit clearing and malformed local intent cannot resurrect
            // an authored invocation target. Replay rejects malformed writes.
            Some(value) => value
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default(),
            None => self
                .binding_context_for_instance(instance)
                .map(|attachment| attachment.initial_input_route.clone())
                .unwrap_or_default(),
        }
    }

    pub(crate) fn route_seq_for_instance(&self, instance: &RyeOsViewInstanceKey) -> Option<u64> {
        let prefix = self.presentation_operations.acknowledged_prefix_len;
        let facet = super::seat::input_route_facet_key(instance);
        self.seat
            .events()
            .iter()
            .enumerate()
            .skip(prefix)
            .rev()
            .find_map(|(index, event)| {
                matches!(&event.kind,
                super::seat::SeatEventKind::Facet { key, .. } if key == &facet)
                .then(|| u64::try_from(index + 1).ok())
                .flatten()
            })
            .or_else(|| {
                let (event_seq, _) = self.retained_input_routes.get(instance)?;
                // The replay timestamp identifies its source event in this
                // exact prefix; publish its stable vector ordinal, not the
                // growing prefix length or a cross-domain numeric comparison.
                self.seat
                    .events()
                    .get(..prefix)?
                    .iter()
                    .position(|event| event.seq == *event_seq)
                    .and_then(|index| u64::try_from(index + 1).ok())
            })
    }

    pub(crate) fn set_input_route(
        &mut self,
        key: &InputBufferKey,
        route: &super::seat::InputRoute,
    ) -> Option<u64> {
        if self.route_write_fenced_by_composition(&key.view_instance_key) {
            return None;
        }
        Some(self.seat.append_facet(
            key.route_facet_key(),
            serde_json::to_value(route).expect("InputRoute serializes"),
        ))
    }

    pub(crate) fn input_address_for(&self, key: InputBufferKey) -> Option<RyeOsInputAddress> {
        let session = self.data.session.as_ref()?;
        let attachment = self.binding_attachment_for_instance(&key.view_instance_key)?;
        Some(RyeOsInputAddress {
            session_id: session.session_id.clone(),
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
            owner: self.input_owner(&key.view_instance_key)?,
            buffer: key,
        })
    }

    pub(crate) fn input_address_is_live(&self, address: &RyeOsInputAddress) -> bool {
        let Some(session) = self.data.session.as_ref() else {
            return false;
        };
        if address.session_id != session.session_id
            || self.input_owner(&address.buffer.view_instance_key).as_ref() != Some(&address.owner)
        {
            return false;
        }
        let Some(mount) = self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(&address.buffer.view_instance_key))
        else {
            return false;
        };
        if !mount.visible()
            || mount.view_ref() != address.buffer.view_ref
            || mount.binding().binding_attachment_id != address.binding_attachment_id
            || mount.binding().binding_generation != address.binding_generation
            || mount.binding().binding_digest != address.binding_digest
        {
            return false;
        }
        let Some(input) = self
            .binding_for_instance(&address.buffer.view_instance_key, &address.buffer.view_ref)
            .and_then(|view| view.input.as_ref())
        else {
            return false;
        };
        self.input_key_for(
            address.buffer.view_instance_key.clone(),
            address.buffer.view_ref.clone(),
            input,
        )
        .as_ref()
            == Some(&address.buffer)
    }

    pub fn new(
        session: BrowserSession,
        presentation_id: &str,
        viewport: BrowserViewport,
        now_ms: u64,
    ) -> Self {
        let mut core = Self::default();
        core.runtime.viewport = viewport;
        core.runtime.now_ms = now_ms;
        core.runtime.last_tick_ms = now_ms;
        let prepared = (|| -> Result<_, String> {
            if session.ui_binding_contract_revision != crate::UI_BINDING_CONTRACT_REVISION {
                return Err("UI binding contract is not current".into());
            }
            let mut retained = BTreeMap::new();
            for descriptor in &session.binding_attachments {
                let attachment =
                    super::binding_context::RetainedUiBindingAttachment::from_descriptor(
                        descriptor.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                if retained
                    .insert(descriptor.binding_attachment_id.clone(), attachment)
                    .is_some()
                {
                    return Err("duplicate binding attachment identity".into());
                }
            }
            let memberships = session
                .presentations
                .iter()
                .filter(|membership| membership.presentation_id == presentation_id)
                .collect::<Vec<_>>();
            if memberships.len() != 1 {
                return Err("selected presentation membership is absent or duplicated".into());
            }
            let membership = memberships[0];
            let root = retained
                .get(&membership.root_binding.binding_attachment_id)
                .ok_or("presentation root attachment is absent")?;
            let composition = super::presentation_state::PresentationComposition::from_membership(
                membership,
                &root.descriptor,
            )?;
            // Current membership is a dispatch boundary, not a structural
            // replay base. The immutable initialization and causal seat prefix
            // open all Surfaces before adapters enable effects.
            Ok((
                retained,
                composition,
                membership.root_binding.binding_attachment_id.clone(),
            ))
        })();
        core.data.session = Some(session);
        match prepared {
            Ok((retained, composition, root_attachment_id)) => {
                core.binding_attachments = retained;
                core.root_attachment_id = root_attachment_id;
                core.presentation = Some(composition);
            }
            Err(reason) => core.notice(reason, RyeOsTone::Danger),
        }
        core
    }

    pub(crate) fn emit(&mut self, kind: RyeOsEffectKind) -> RyeOsEffect {
        self.next_effect_id += 1;
        self.pending_effects
            .insert(self.next_effect_id, kind.clone());
        RyeOsEffect {
            id: self.next_effect_id,
            kind,
        }
    }

    /// Exact prefix accepted by the shared reducer, not merely by an adapter
    /// transport acknowledgement. Native shutdown and later batches must not
    /// advance past a receipt the reducer refused.
    pub fn acknowledged_seat_prefix_len(&self) -> usize {
        self.presentation_operations.acknowledged_prefix_len
    }

    /// Resolve the one retained seat named by this Core's authenticated
    /// session membership. A copied event coordinate cannot select another
    /// presentation or silently use the first of duplicate memberships.
    pub(crate) fn exact_retained_seat_coordinate(&self) -> Result<(String, String), String> {
        let presentation = self.presentation.as_ref().ok_or("presentation absent")?;
        let session = self.data.session.as_ref().ok_or("session absent")?;
        let memberships = session
            .presentations
            .iter()
            .filter(|membership| membership.presentation_id == presentation.id().as_str())
            .collect::<Vec<_>>();
        let [membership] = memberships.as_slice() else {
            return Err("presentation membership is absent or duplicated".into());
        };
        Ok((
            membership.thread_id.clone().ok_or("seat thread absent")?,
            membership
                .chain_root_id
                .clone()
                .ok_or("seat chain root absent")?,
        ))
    }

    pub(crate) fn compiled_binding_operation_for_attachment(
        &self,
        attachment_id: &str,
        coordinate: super::binding::UiBindingCoordinate,
        payload: super::binding::UiBindingPayload,
    ) -> Option<(
        super::binding::UiBindingRequest,
        super::binding::UiBindingRequestBounds,
    )> {
        let attachment = self.binding_attachment(attachment_id)?;
        Some((
            super::binding::UiBindingRequest {
                binding_attachment_id: attachment.binding_attachment_id.clone(),
                binding_generation: attachment.binding_generation,
                binding_digest: attachment.binding_digest.clone(),
                coordinate,
                payload,
            },
            attachment.binding_request_bounds,
        ))
    }

    pub(crate) fn compiled_binding_operation(
        &self,
        instance: &RyeOsViewInstanceKey,
        coordinate: super::binding::UiBindingCoordinate,
        payload: super::binding::UiBindingPayload,
    ) -> Option<(
        super::binding::UiBindingRequest,
        super::binding::UiBindingRequestBounds,
    )> {
        // RequiredSubject is an engine execution fence. Keep this check at the
        // common mounted-operation boundary so a new or forged reducer event
        // cannot bypass the more specific source/input/affordance guards.
        if self.instance_has_unresolved_required_subject(instance) {
            return None;
        }
        let retained = self.binding_context_for_instance(instance)?;
        match &coordinate {
            super::binding::UiBindingCoordinate::Source { view_ref, .. }
            | super::binding::UiBindingCoordinate::Affordance { view_ref, .. } => {
                if self.mounted_view_ref(instance) != Some(view_ref.as_str()) {
                    return None;
                }
            }
            super::binding::UiBindingCoordinate::SurfaceRoute => {
                // Shared mount authority never comes from the active Surface.
                retained.descriptor.presentation.definition.surface()?;
            }
        }
        let attachment_id = &retained.descriptor.binding_attachment_id;
        self.compiled_binding_operation_for_attachment(attachment_id, coordinate, payload)
    }

    /// Resolve command grammar from the signed input block currently holding
    /// focus. The reducer never names a view or affordance ID.
    pub(crate) fn focused_command_submit_coordinate(
        &self,
    ) -> Option<super::binding::UiBindingCoordinate> {
        let (instance, view_ref) = self.focused_input_instance()?;
        let affordance_id = self
            .binding_for_instance(&instance.view_instance_key, &view_ref)?
            .input
            .as_ref()?
            .command_submit
            .as_ref()?
            .clone();
        Some(super::binding::UiBindingCoordinate::Affordance {
            view_ref,
            affordance_id,
        })
    }

    /// Resolve thread-control grammar from the exact mounted input origin.
    /// Another view in the same attachment is not authority for this mount.
    pub(crate) fn thread_control_coordinate(&self) -> Option<super::binding::UiBindingCoordinate> {
        let (input, view_ref) = self.focused_input_instance()?;
        let affordance_id = self
            .binding_for_instance(&input.view_instance_key, &view_ref)?
            .input
            .as_ref()?
            .thread_control
            .clone()?;
        Some(super::binding::UiBindingCoordinate::Affordance {
            view_ref,
            affordance_id,
        })
    }

    pub fn has_project_bound(&self) -> bool {
        self.active_surface_attachment()
            .and_then(|attachment| attachment.project_path.as_deref())
            .is_some_and(|path| !path.is_empty())
    }

    pub(crate) fn instance_has_project_bound(&self, instance: &RyeOsViewInstanceKey) -> bool {
        self.binding_attachment_for_instance(instance)
            .and_then(|attachment| attachment.project_path.as_deref())
            .is_some_and(|path| !path.is_empty())
    }

    pub(crate) fn atlas_target_has_project_bound(&self, tile_id: Option<&str>) -> bool {
        if self.active_view_set().is_none() {
            return false;
        }

        let Some(tile_id) = tile_id else {
            return self.has_project_bound();
        };
        tile_id
            .parse::<u64>()
            .ok()
            .map(crate::ids::TileId::new)
            .and_then(|id| {
                self.active_view_set()
                    .expect("real Surface tab checked")
                    .tiles
                    .get(&id)
            })
            .is_some_and(|tile| self.instance_has_project_bound(&tile.instance_key))
    }

    pub(crate) fn surface_navigation(&self) -> Vec<crate::surface::SurfaceNavigationSpec> {
        self.data
            .session
            .as_ref()
            .and_then(|_| self.active_surface_attachment())
            .and_then(|attachment| attachment.presentation.surface_value().ok().flatten())
            .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
            .map(|surface| {
                surface
                    .navigation
                    .into_iter()
                    .filter(|entry| {
                        entry.view.starts_with("view:")
                            && self
                                .active_surface_attachment()
                                .and_then(|descriptor| {
                                    self.binding_attachments
                                        .get(&descriptor.binding_attachment_id)
                                })
                                .is_some_and(|attachment| {
                                    attachment.views.contains_key(&entry.view)
                                })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn initial_effects(&mut self) -> Vec<RyeOsEffect> {
        self.initial_effects_excluding_subject_refresh(&BTreeSet::new())
    }

    pub(crate) fn initial_effects_excluding_subject_refresh(
        &mut self,
        refreshed_subjects: &BTreeSet<RyeOsViewInstanceKey>,
    ) -> Vec<RyeOsEffect> {
        if self.data.session.as_ref().is_some_and(|session| {
            !session.session_id.is_empty()
                && session.ui_binding_contract_revision != crate::UI_BINDING_CONTRACT_REVISION
        }) {
            return Vec::new();
        }
        self.normalize_mounted_local_states();
        let visible_input_instances = self.visible_mounted_views();
        let mut effects = Vec::new();
        let roots = self
            .presentation
            .iter()
            .flat_map(|presentation| {
                presentation.ordered_surfaces().map(move |surface| {
                    (
                        presentation.id().clone(),
                        surface.id().clone(),
                        surface.binding().clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        for (presentation_id, surface_id, coordinate) in roots {
            let Some(attachment) = self
                .binding_attachments
                .get(&coordinate.binding_attachment_id)
            else {
                continue;
            };
            if attachment.descriptor.binding_generation != coordinate.binding_generation
                || attachment.descriptor.binding_digest != coordinate.binding_digest
            {
                continue;
            }
            let surface_ref = attachment.descriptor.root_ref.clone();
            let sources = attachment
                .surface_sources
                .iter()
                .filter(|(_, source)| {
                    source.activation == super::content::SourceActivation::Initial
                        && (!source.requires_project
                            || attachment.descriptor.project_path.is_some())
                })
                .map(|(channel, source)| (channel.clone(), source.params.clone()))
                .collect::<Vec<_>>();
            for (channel, params) in sources {
                if let Some((request, request_bounds)) = self
                    .compiled_binding_operation_for_attachment(
                        &coordinate.binding_attachment_id,
                        super::binding::UiBindingCoordinate::Source {
                            view_ref: surface_ref.clone(),
                            channel: channel.clone(),
                        },
                        super::binding::UiBindingPayload::SourceParameters { params },
                    )
                {
                    effects.push(
                        self.emit(RyeOsEffectKind::FetchSource {
                            tile_id: super::source_key::RyeOsSurfaceSourceKey {
                                presentation_id: presentation_id.clone(),
                                surface_instance_id: surface_id.clone(),
                                channel,
                            }
                            .encode(),
                            request,
                            request_bounds,
                        }),
                    );
                }
            }
        }
        for (instance, view_ref) in &visible_input_instances {
            if !refreshed_subjects.contains(instance) {
                effects.extend(self.emit_fetch_source_for_instance(instance.clone(), view_ref));
            }
        }
        // @-mention sources: fetch the refs each input declares, keyed so the
        // reader (key_context / CompleteInput) reads them back. A generic
        // FetchSource, so clients need no bespoke handling.
        let mention_fetches: Vec<(String, String, String)> = visible_input_instances
            .iter()
            .filter_map(|(instance_key, view_ref)| {
                if self.move_freeze_restricts_instance(instance_key)
                    || self.instance_has_unresolved_required_subject(instance_key)
                {
                    return None;
                }
                let binding = self.binding_for_instance(instance_key, view_ref)?;
                let input = binding.input.as_ref()?;
                input.mentions.as_ref()?;
                Some((
                    super::source_key::RyeOsSourceInstanceKey::mention(
                        instance_key.clone(),
                        &input.id,
                    )
                    .encode(),
                    view_ref.clone(),
                    super::binding::input_mentions_channel(&input.id),
                ))
            })
            .collect();
        for (key, view_ref, channel) in mention_fetches {
            let Some(source_instance) = super::source_key::RyeOsSourceInstanceKey::decode(&key)
                .map(|key| key.view_instance)
            else {
                continue;
            };
            let Some((request, request_bounds)) = self.compiled_binding_operation(
                &source_instance,
                super::binding::UiBindingCoordinate::Source { view_ref, channel },
                super::binding::UiBindingPayload::SourceParameters {
                    params: serde_json::json!({}),
                },
            ) else {
                continue;
            };
            effects.push(self.emit(RyeOsEffectKind::FetchSource {
                tile_id: key,
                request,
                request_bounds,
            }));
        }
        // `completion` sources (the line-start `/` grammar): fetched through the
        // same generic keyed FetchSource as mentions, read back by the
        // slash-completion projectors. No bespoke commands effect.
        let completion_fetches: Vec<(String, String, String)> = visible_input_instances
            .iter()
            .filter_map(|(instance_key, view_ref)| {
                if self.move_freeze_restricts_instance(instance_key)
                    || self.instance_has_unresolved_required_subject(instance_key)
                {
                    return None;
                }
                let binding = self.binding_for_instance(instance_key, view_ref)?;
                let input = binding.input.as_ref()?;
                input.completion.as_ref()?;
                Some((
                    super::source_key::RyeOsSourceInstanceKey::completion(
                        instance_key.clone(),
                        &input.id,
                    )
                    .encode(),
                    view_ref.clone(),
                    super::binding::input_completion_channel(&input.id),
                ))
            })
            .collect();
        for (key, view_ref, channel) in completion_fetches {
            let Some(source_instance) = super::source_key::RyeOsSourceInstanceKey::decode(&key)
                .map(|key| key.view_instance)
            else {
                continue;
            };
            let Some((request, request_bounds)) = self.compiled_binding_operation(
                &source_instance,
                super::binding::UiBindingCoordinate::Source { view_ref, channel },
                super::binding::UiBindingPayload::SourceParameters {
                    params: serde_json::json!({}),
                },
            ) else {
                continue;
            };
            effects.push(self.emit(RyeOsEffectKind::FetchSource {
                tile_id: key,
                request,
                request_bounds,
            }));
        }
        effects
    }

    /// Initialize renderer-local state only from the exact admitted mounted
    /// widget. Shared mounts do not pass through tile construction. Existing
    /// list/field state survives visibility changes and repeated projection.
    pub(crate) fn normalize_mounted_local_states(&mut self) {
        let instances = self
            .mounted_instances()
            .filter_map(|instance| {
                let view = self.mounted_view_ref(&instance)?;
                let binding = self.binding_for_instance(&instance, view)?;
                match binding.widget.as_str() {
                    "field" => Some((instance, ViewLocalState::Field(Default::default()))),
                    "rows" | "table" | "sections" | "timeline" => {
                        let initial = crate::view_set::ViewSpec::bound(view).initial_local_state();
                        Some((instance, initial))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        for (instance, initial) in instances {
            if let Some(local) = self.mounted_local_state_mut(&instance)
                && (matches!(local, ViewLocalState::None)
                    || (matches!(&initial, ViewLocalState::Field(_))
                        && !matches!(local, ViewLocalState::Field(_))))
            {
                *local = initial;
            }
        }
        self.normalize_field_cursor_scopes();
    }

    /// Hint arrival: semantic hook for transient "look" notices. Visual pulse
    /// state is layered on this entry point; refetches are content-bound via
    /// `refresh.on_hint`.
    pub fn note_hint_received(&mut self, kind: &str, payload: &serde_json::Value) {
        let attention_changed = kind == "thread"
            && payload
                .get("event_type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|event| {
                    matches!(
                        event,
                        "thread_failed" | "thread_killed" | "thread_timed_out"
                    )
                });
        if attention_changed {
            self.runtime.attention_until_ms = self.runtime.now_ms.saturating_add(3_000);
        }
        let patched = kind == "thread" && self.patch_thread_hint(payload);
        if attention_changed || patched {
            self.bump_generation();
        }
    }

    fn patch_thread_hint(&mut self, payload: &serde_json::Value) -> bool {
        let Some(thread_id) = payload.get("thread_id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(status) = payload.get("status").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(updated_at) = payload
            .get("updated_at")
            .and_then(serde_json::Value::as_str)
        else {
            return false;
        };

        // Resolve only live view instances whose authored source selects a
        // thread collection. Transcript/event sources also contain thread_id,
        // but are not lifecycle rows and must remain byte-for-byte untouched.
        let instances = self.visible_mounted_views();
        let mut targets = Vec::new();
        for (instance_key, view_ref) in instances {
            let Some(binding) = self.binding_for_instance(&instance_key, &view_ref) else {
                continue;
            };
            if binding.widget == "sections" {
                for section in &binding.sections {
                    if is_thread_collection(section.collection.as_deref()) {
                        targets.push((
                            super::source_key::RyeOsSourceInstanceKey::named(
                                instance_key.clone(),
                                &section.source_channel,
                            )
                            .encode(),
                            section.collection.clone().unwrap_or_default(),
                        ));
                    }
                }
            } else if let Some((channel, source)) = binding.primary_source()
                && is_thread_collection(source.collection.as_deref())
            {
                targets.push((
                    super::source_key::RyeOsSourceInstanceKey::named(instance_key, channel)
                        .encode(),
                    source.collection.clone().unwrap_or_default(),
                ));
            }
        }

        let mut changed_sources = Vec::new();
        for (source_key, collection) in targets {
            let Some(source) = self.data.sources.get_mut(&source_key) else {
                continue;
            };
            let old = source.clone();
            if patch_thread_collection(source, &collection, thread_id, status, updated_at) {
                changed_sources.push((source_key, old, source.clone()));
            }
        }
        let mut changed = !changed_sources.is_empty();
        if let Some(threads) = &mut self.data.threads {
            for row in &mut threads.threads {
                changed |= patch_thread_record(row, thread_id, status, updated_at);
            }
        }
        for (source_key, old, new) in changed_sources {
            self.note_source_row_changes(&source_key, Some(&old), &new);
        }
        changed
    }

    pub(crate) fn bump_activity_pulse(&mut self, amount: f32) {
        self.runtime.activity_pulse = (self.runtime.activity_pulse + amount).min(1.0);
    }

    pub fn wants_fast_ticks(&self) -> bool {
        self.active_view_set()
            .is_none_or(|set| set.center_is_empty())
            || (self.surface_uses_backdrop_underlay() && self.view_set_has_transparent_view())
            || self.runtime.activity_pulse > 0.02
    }

    /// Hint arrival: refetch every bound tile or visible slot whose binding
    /// declares `refresh.on_hint: <kind>` or includes it in an array. Content
    /// decides its own liveness.
    pub fn effects_for_hint(&mut self, kind: &str) -> Vec<RyeOsEffect> {
        self.effects_for_hints(&[kind.to_string()])
    }

    /// Reconcile one client coalescing window. Targets are unioned across hint
    /// kinds before any effects are emitted, then each source key goes through
    /// the hint single-flight gate. This prevents `[thread, activity]` views
    /// from issuing duplicate reads for one burst.
    pub fn effects_for_hints(&mut self, kinds: &[String]) -> Vec<RyeOsEffect> {
        let matching_channels = |binding: &super::content::ViewBinding| {
            binding
                .sources
                .iter()
                .filter(|(_, source)| {
                    // Absent/null inherits the view policy; an explicit empty
                    // object is a declaration of no per-source liveness.
                    let refresh = if source.refresh.is_null() {
                        &binding.refresh
                    } else {
                        &source.refresh
                    };
                    kinds
                        .iter()
                        .any(|kind| refresh_matches_hint(refresh.get("on_hint"), kind))
                })
                .map(|(channel, _)| channel.clone())
                .collect::<Vec<_>>()
        };
        let mut targets = std::collections::BTreeSet::new();
        for (instance_key, view_ref) in self.visible_mounted_views() {
            let Some(binding) = self.binding_for_instance(&instance_key, &view_ref) else {
                continue;
            };
            for channel in matching_channels(binding) {
                targets.insert((instance_key.clone(), view_ref.clone(), channel));
            }
        }
        targets
            .into_iter()
            .flat_map(|(instance_key, view_ref, channel)| {
                self.emit_fetch_source_for_instance_policy(
                    instance_key,
                    &view_ref,
                    true,
                    Some(&channel),
                )
            })
            .collect()
    }

    /// Emit the source fetch(es) for a bound view tile, resolving `@facet:`
    /// params against the seat fold (explicit references only). One effect for
    /// a single-source widget; one per section for a `sections` widget.
    pub fn emit_fetch_source(
        &mut self,
        tile_id: crate::ids::TileId,
        view_ref: &str,
    ) -> Vec<RyeOsEffect> {
        if self.active_view_set().is_none() {
            return Vec::new();
        }

        let Some(instance_key) = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)
            .map(|tile| tile.instance_key.clone())
        else {
            return Vec::new();
        };
        self.emit_fetch_source_for_instance(instance_key, view_ref)
    }

    /// Instance-keyed variant: slots and other non-tile hosts subscribe with
    /// stable typed keys. The same instance key addresses the
    /// instance's transient input buffer: a view declaring `input.feeds`
    /// injects its buffer text into the named source param before fetch.
    ///
    /// Every declared named channel is fetched independently. Sections only
    /// reference those channels and never create hidden fetch identities.
    pub fn emit_fetch_source_for_instance(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        view_ref: &str,
    ) -> Vec<RyeOsEffect> {
        self.emit_fetch_source_for_instance_policy(instance_key, view_ref, false, None)
    }

    pub(crate) fn refresh_source_channel(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        view_ref: &str,
        channel: &str,
    ) -> Vec<RyeOsEffect> {
        let source_key =
            super::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), channel)
                .encode();
        // Fence the old subject even when the newly resolved params are null
        // and therefore emit no replacement fetch.
        self.data
            .source_floor
            .insert(source_key.clone(), self.next_effect_id.saturating_add(1));
        self.data.sources.remove(&source_key);
        self.data.source_errors.remove(&source_key);
        self.data.source_stored_epoch.remove(&source_key);
        self.data.timeline_sources.remove(&source_key);
        self.data.field_sources.remove(&source_key);
        self.data
            .field_projections
            .borrow_mut()
            .remove(&instance_key);
        self.deferred_source_fetches.remove(&source_key);
        self.emit_fetch_source_for_instance_policy(instance_key, view_ref, false, Some(channel))
    }

    fn emit_fetch_source_for_instance_policy(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        view_ref: &str,
        hint_single_flight: bool,
        only_channel: Option<&str>,
    ) -> Vec<RyeOsEffect> {
        if self.move_freeze_restricts_instance(&instance_key) {
            return Vec::new();
        }
        if self.instance_has_unresolved_required_subject(&instance_key) {
            return Vec::new();
        }
        let Some(attachment_id) = self
            .binding_attachment_for_instance(&instance_key)
            .map(|descriptor| descriptor.binding_attachment_id.clone())
        else {
            return Vec::new();
        };
        let Some(binding) = self.binding_for_instance(&instance_key, view_ref) else {
            return Vec::new();
        };
        if binding.sources.is_empty() {
            return Vec::new();
        }
        let sources = binding.sources.clone();
        let feeds = binding
            .input
            .as_ref()
            .and_then(|input| input.feeds.as_ref())
            .cloned();
        let input_id = binding.input.as_ref().map(|input| input.id.clone());
        let single_channel = (sources.len() == 1).then_some(());
        let resolved = sources
            .into_iter()
            .filter(|(channel, _)| only_channel.is_none_or(|only| channel == only))
            .filter(|(_, source)| {
                (!source.requires_project || self.instance_has_project_bound(&instance_key))
                    && (only_channel.is_some()
                        || source.activation == super::content::SourceActivation::Initial)
            })
            .filter_map(|(channel, source)| {
                let route = serde_json::to_value(self.route_for_instance(&instance_key))
                    .expect("InputRoute serializes");
                let mut params = super::content::resolve_params(&source.params, |key| {
                    if key == super::seat::KEY_INPUT_ROUTE {
                        Some(route.clone())
                    } else {
                        self.facet_value_for_instance(&instance_key, key)
                    }
                });
                if binding.widget == "field" {
                    params = resolve_field_params(
                        &params,
                        field_local_state_for_instance(self, &instance_key),
                        &channel,
                    );
                }
                // A live-filter buffer belongs to a singular source-backed
                // input. Multi-channel views must express their own explicit
                // field params instead of leaking one buffer across channels.
                if single_channel.is_some()
                    && let (Some(feeds), Some(input_id)) = (&feeds, &input_id)
                {
                    let descriptor = self.binding_attachment_for_instance(&instance_key)?;
                    let key = InputBufferKey::new(
                        instance_key.clone(),
                        view_ref,
                        input_id.clone(),
                        descriptor,
                    );
                    let buffer = self.input_buffers.get(&key.storage_key());
                    let text = buffer.map(|buffer| buffer.text.clone()).unwrap_or_default();
                    let field = buffer.map(|buffer| buffer.filter_field).unwrap_or(0);
                    let param = feeds.active_param(field).to_string();
                    if let Some(object) = params.as_object_mut() {
                        object.insert(param, serde_json::Value::String(text));
                    } else {
                        params = serde_json::json!({ param: text });
                    }
                }
                Some((
                    super::source_key::RyeOsSourceInstanceKey::named(
                        instance_key.clone(),
                        &channel,
                    )
                    .encode(),
                    channel,
                    source,
                    params,
                ))
            })
            .collect::<Vec<_>>();
        resolved
            .into_iter()
            .filter_map(|(key, channel, source, params)| {
                self.build_fetch_source(
                    &attachment_id,
                    key,
                    view_ref,
                    &channel,
                    &source,
                    params,
                    hint_single_flight,
                )
            })
            .collect()
    }

    /// Emit one `FetchSource` for a resolved (key, source, params) triple, or
    /// skip when a param references an unset facet (e.g. the inspector's
    /// `@facet:selection.item` before anything is selected): that resolves to
    /// null — nothing to fetch — and dispatching the null arg the op rejects
    /// is a 500, not an empty view.
    /// Raise the acceptance floor to this batch's fetch ids and evict the
    /// keys' stored payloads: the SUBJECT behind these keys changed, so a
    /// response older than this batch must never land under it. Ordinary
    /// refetches skip this — they keep prior data rendering while the
    /// fresh response is in flight.
    pub(crate) fn floor_source_fetches(&mut self, effects: &[RyeOsEffect], evict: bool) {
        for effect in effects {
            if let RyeOsEffectKind::FetchSource { tile_id, .. } = &effect.kind {
                self.data.source_floor.insert(tile_id.clone(), effect.id);
                if evict {
                    self.data.sources.remove(tile_id);
                    self.data.source_errors.remove(tile_id);
                    self.data.source_stored_epoch.remove(tile_id);
                    self.data.timeline_sources.remove(tile_id);
                    self.data.field_sources.remove(tile_id);
                    if let Some(key) = super::source_key::RyeOsSourceInstanceKey::decode(tile_id) {
                        self.data
                            .field_projections
                            .borrow_mut()
                            .remove(&key.view_instance);
                    }
                }
            }
        }
    }

    /// Drop every cached and in-flight source instance owned by one bound
    /// view host. Subject transitions call this before resolving the new
    /// parameters, so an unresolved new subject stays empty and responses for
    /// the prior subject are ignored rather than landing under the new route.
    pub(crate) fn invalidate_view_sources(&mut self, instance_key: &RyeOsViewInstanceKey) {
        if let Some(tile) = instance_key.surface_tile_id() {
            let key = tile.0.to_string();
            self.data.tile_items.remove(&key);
            self.data.tile_files.remove(&key);
            self.data.tile_file_space.remove(&key);
        }
        let belongs = |encoded: &str| {
            super::source_key::RyeOsSourceInstanceKey::decode(encoded)
                .is_some_and(|key| key.belongs_to(instance_key))
        };
        self.pending_effects.retain(|_, kind| {
            !matches!(kind, RyeOsEffectKind::FetchSource { tile_id, .. } if belongs(tile_id))
        });
        self.data.sources.retain(|key, _| !belongs(key));
        self.data.source_errors.retain(|key, _| !belongs(key));
        self.data.source_epoch.retain(|key, _| !belongs(key));
        self.data.source_stored_epoch.retain(|key, _| !belongs(key));
        self.data.source_floor.retain(|key, _| !belongs(key));
        self.data
            .source_subject_fingerprint
            .retain(|key, _| !belongs(key));
        self.data.source_unresolved.retain(|key, _| !belongs(key));
        self.data.timeline_sources.retain(|key, _| !belongs(key));
        self.data.field_sources.retain(|key, _| !belongs(key));
        self.data
            .field_projections
            .borrow_mut()
            .remove(instance_key);
        self.deferred_source_fetches.retain(|key, _| !belongs(key));
    }

    fn build_fetch_source(
        &mut self,
        attachment_id: &str,
        source_key: String,
        view_ref: &str,
        channel: &str,
        source: &super::content::SourceBinding,
        params: serde_json::Value,
        hint_single_flight: bool,
    ) -> Option<RyeOsEffect> {
        let subject_fingerprint =
            super::field::source_subject_fingerprint(&source.item_ref, &params);
        let prior_subject = self
            .data
            .source_subject_fingerprint
            .insert(source_key.clone(), subject_fingerprint.clone());
        let subject_changed = prior_subject.is_some_and(|prior| prior != subject_fingerprint);
        if subject_changed {
            self.data
                .source_floor
                .insert(source_key.clone(), self.next_effect_id.saturating_add(1));
            self.data.sources.remove(&source_key);
            self.data.source_errors.remove(&source_key);
            self.data.source_stored_epoch.remove(&source_key);
            self.data.timeline_sources.remove(&source_key);
            self.data.field_sources.remove(&source_key);
            if let Some(key) = super::source_key::RyeOsSourceInstanceKey::decode(&source_key) {
                self.data
                    .field_projections
                    .borrow_mut()
                    .remove(&key.view_instance);
            }
            self.deferred_source_fetches.remove(&source_key);
        }
        let unresolved = scoped_param_unresolved(&source.params, &params);
        self.data
            .source_unresolved
            .insert(source_key.clone(), unresolved);
        if unresolved {
            return None;
        }
        if hint_single_flight
            && self.pending_effects.values().any(|kind| {
                matches!(kind, RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == &source_key)
            })
        {
            self.deferred_source_fetches.insert(
                source_key,
                DeferredSourceFetch {
                    view_ref: view_ref.to_string(),
                    channel: channel.to_string(),
                    params,
                },
            );
            return None;
        }
        // A fetch that can launch now supersedes any parked request for this
        // key. This is especially important for explicit subject changes
        // (lens/filter/project): an older deferred hint must never refetch the
        // previous subject after the new request settles.
        self.deferred_source_fetches.remove(&source_key);
        self.data.source_errors.remove(&source_key);
        let (request, request_bounds) = self.compiled_binding_operation_for_attachment(
            attachment_id,
            super::binding::UiBindingCoordinate::Source {
                view_ref: view_ref.to_string(),
                channel: channel.to_string(),
            },
            super::binding::UiBindingPayload::SourceParameters { params },
        )?;
        let effect = self.emit(RyeOsEffectKind::FetchSource {
            tile_id: source_key.clone(),
            request,
            request_bounds,
        });
        // Mark this as the newest request for the key; an older in-flight fetch
        // for the same key that resolves later is then dropped on arrival.
        self.data.source_epoch.insert(source_key, effect.id);
        Some(effect)
    }

    /// Fetch a presentation-role source for one exact mounted view. Roles are
    /// signed view metadata; this lookup never chooses an executable target.
    pub(crate) fn fetch_view_source_role(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        view_ref: &str,
        role: &str,
        dynamic_params: serde_json::Value,
    ) -> Option<RyeOsEffect> {
        if self.instance_has_unresolved_required_subject(&instance_key) {
            return None;
        }
        let attachment_id = self
            .binding_attachment_for_instance(&instance_key)?
            .binding_attachment_id
            .clone();
        let (channel, source) = self
            .binding_for_instance(&instance_key, view_ref)?
            .sources
            .iter()
            .find(|(_, source)| source.role.as_deref() == Some(role))?;
        let channel = channel.clone();
        let source = source.clone();
        let source_key =
            super::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), &channel)
                .encode();
        self.build_fetch_source(
            &attachment_id,
            source_key,
            view_ref,
            &channel,
            &source,
            dynamic_params,
            false,
        )
    }

    pub(crate) fn fetch_surface_source_role(
        &mut self,
        role: &str,
        dynamic_params: serde_json::Value,
    ) -> Option<RyeOsEffect> {
        let composition = self.presentation.as_ref()?;
        let surface = composition.active_surface()?;
        let presentation_id = composition.id().clone();
        let surface_instance_id = surface.id().clone();
        let attachment = self
            .binding_attachments
            .get(&surface.binding().binding_attachment_id)?;
        if attachment.descriptor.binding_generation != surface.binding().binding_generation
            || attachment.descriptor.binding_digest != surface.binding().binding_digest
        {
            return None;
        }
        attachment.descriptor.presentation.definition.surface()?;
        let (channel, source) = attachment
            .surface_sources
            .iter()
            .find(|(_, source)| source.role.as_deref() == Some(role))?;
        let channel = channel.clone();
        let source = source.clone();
        let surface_ref = attachment.descriptor.root_ref.clone();
        let attachment_id = attachment.descriptor.binding_attachment_id.clone();
        self.build_fetch_source(
            &attachment_id,
            super::source_key::RyeOsSurfaceSourceKey {
                presentation_id,
                surface_instance_id,
                channel: channel.clone(),
            }
            .encode(),
            &surface_ref,
            &channel,
            &source,
            dynamic_params,
            false,
        )
    }

    pub(crate) fn fetch_atlas_source_role(
        &mut self,
        tile_id: Option<&str>,
        role: &str,
        dynamic_params: serde_json::Value,
    ) -> Option<RyeOsEffect> {
        self.active_view_set()?;

        let Some(tile_id) = tile_id else {
            return self.fetch_surface_source_role(role, dynamic_params);
        };
        let tile = tile_id
            .parse::<u64>()
            .ok()
            .map(crate::ids::TileId::new)
            .and_then(|id| {
                self.active_view_set()
                    .expect("real Surface tab checked")
                    .tiles
                    .get(&id)
                    .cloned()
            })?;
        self.fetch_view_source_role(tile.instance_key, &tile.view.view_ref, role, dynamic_params)
    }

    /// Release one trailing hint reconciliation after the prior request for
    /// the same source key settles. If another (non-hint) request is still in
    /// flight, leave the trailing request parked until the final one settles.
    pub(crate) fn release_deferred_source_fetch(
        &mut self,
        source_key: &str,
    ) -> Option<RyeOsEffect> {
        let instance = super::source_key::RyeOsSourceInstanceKey::decode(source_key)?.view_instance;
        // Keep the trailing observation parked while its exact source is
        // frozen. Neither its cache/error state nor its request epoch may
        // advance before authoritative transfer reconciliation.
        if self.move_freeze_restricts_instance(&instance) {
            return None;
        }
        if self.pending_effects.values().any(|kind| {
            matches!(kind, RyeOsEffectKind::FetchSource { tile_id, .. } if tile_id == source_key)
        }) {
            return None;
        }
        if !self.source_key_is_live(source_key) {
            self.deferred_source_fetches.remove(source_key);
            return None;
        }
        let deferred = self.deferred_source_fetches.remove(source_key)?;
        self.data.source_errors.remove(source_key);
        if self.instance_has_unresolved_required_subject(&instance) {
            return None;
        }
        let (request, request_bounds) = self.compiled_binding_operation(
            &instance,
            super::binding::UiBindingCoordinate::Source {
                view_ref: deferred.view_ref,
                channel: deferred.channel,
            },
            super::binding::UiBindingPayload::SourceParameters {
                params: deferred.params,
            },
        )?;
        let effect = self.emit(RyeOsEffectKind::FetchSource {
            tile_id: source_key.to_string(),
            request,
            request_bounds,
        });
        self.data
            .source_epoch
            .insert(source_key.to_string(), effect.id);
        Some(effect)
    }

    fn source_key_is_live(&self, source_key: &str) -> bool {
        super::source_key::RyeOsSourceInstanceKey::decode(source_key).is_some_and(|key| {
            self.presentation
                .as_ref()
                .and_then(|presentation| presentation.mount(&key.view_instance))
                .is_some_and(|mount| mount.visible())
        })
    }

    pub fn visible_mounted_views(&self) -> Vec<(RyeOsViewInstanceKey, String)> {
        self.mounted_instances()
            .filter_map(|instance| {
                let mount = self.presentation.as_ref()?.mount(&instance)?;
                mount
                    .visible()
                    .then(|| (instance.clone(), mount.view_ref().to_string()))
            })
            .collect()
    }

    pub fn visible_dock_views(&self) -> Vec<(RyeOsViewInstanceKey, String)> {
        self.presentation
            .iter()
            .flat_map(|presentation| presentation.docks().values())
            .filter(|mount| mount.visible)
            .map(|mount| {
                (
                    mount.view().instance_key().clone(),
                    mount.view().view_ref().into(),
                )
            })
            .collect()
    }

    /// Does the instance addressed by `instance_id`/`view_ref` declare an
    /// input buffer? (Layout-neutral — slots and tiles answer the same.)
    pub fn instance_declares_input(&self, instance: &RyeOsViewInstanceKey, view_ref: &str) -> bool {
        self.binding_for_instance(instance, view_ref)
            .is_some_and(|binding| binding.input.is_some())
    }

    fn surface_uses_backdrop_underlay(&self) -> bool {
        self.data
            .session
            .as_ref()
            .and_then(|_| self.binding_attachment(&self.root_attachment_id))
            .and_then(|attachment| attachment.presentation.surface_value().ok().flatten())
            .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
            .is_some_and(|surface| {
                surface.backdrop.is_some()
                    && surface.ambient.is_some_and(|ambient| {
                        ambient.show_background.unwrap_or(true)
                            && ambient
                                .opacity
                                .is_some_and(|opacity| opacity > 0.0 && opacity < 1.0)
                    })
            })
    }

    fn view_set_has_transparent_view(&self) -> bool {
        if self.active_view_set().is_none() {
            return false;
        }

        self.active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .values()
            .any(|tile| {
                self.binding_for_instance(&tile.instance_key, &tile.view.view_ref)
                    .and_then(|binding| binding.presentation.background)
                    .is_some_and(|background| {
                        matches!(
                            background,
                            super::content::ViewBackgroundPresentation::Transparent
                        )
                    })
            })
    }

    pub fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// The scene animation frame. Scene motion samples this — never
    /// `generation`, which counts events — so animation speed is uniform
    /// regardless of event traffic or the client's tick rate.
    ///
    /// Driven clients (the terminal loop calls `advance_scene_frame`
    /// each tick) read the stepped counter; undriven clients (web paints
    /// far above the scene cadence) fall back to wall-clock quantization,
    /// where oversampling keeps the steps uniform.
    pub fn scene_frame(&self) -> u64 {
        if self.runtime.scene_frame_anchor_ms != 0 {
            return self.runtime.scene_frame;
        }
        self.runtime.now_ms / super::scene_model::SCENE_FRAME_MS
    }

    /// Advance the scene clock for one painted frame. A client whose tick
    /// period EQUALS the scene cadence cannot re-floor the wall clock each
    /// frame (`now_ms / SCENE_FRAME_MS` sampled every SCENE_FRAME_MS
    /// aliases against the bucket boundaries: a held frame, then a double
    /// step, whenever the tick phase drifts near an edge). Instead the
    /// counter steps by the rounded elapsed frames, clamped to at least
    /// one — exactly one step per on-time tick, catch-up on a stall or a
    /// slow tick mode — so wall time still governs the average rate while
    /// every painted frame moves.
    pub fn advance_scene_frame(&mut self, now_ms: u64) {
        let frame_ms = super::scene_model::SCENE_FRAME_MS;
        if self.runtime.scene_frame_anchor_ms == 0 {
            // First driven frame: adopt the wall-clock frame so the
            // counter continues exactly where the fallback left off.
            self.runtime.scene_frame = now_ms / frame_ms;
            self.runtime.scene_frame_anchor_ms = now_ms;
        } else {
            let elapsed = now_ms.saturating_sub(self.runtime.scene_frame_anchor_ms);
            // Whole frames only — the sub-frame remainder stays in the
            // anchor, so a cadence that isn't a multiple of the frame
            // (the 250ms slow tick) still averages the wall-clock rate.
            // `.max(1)` keeps every painted tick moving; it advances the
            // anchor ahead of the wall clock, which later ticks repay
            // with fewer steps, so the average still holds.
            let steps = (elapsed / frame_ms).max(1);
            self.runtime.scene_frame = self.runtime.scene_frame.wrapping_add(steps);
            self.runtime.scene_frame_anchor_ms = self
                .runtime
                .scene_frame_anchor_ms
                .saturating_add(steps * frame_ms);
        }
    }

    /// Atlas arrangement for a specific tile, falling back to the ambient
    /// backdrop state when the tile has no per-tile entry yet.
    pub(crate) fn tile_atlas_state(&self, tile_id: crate::ids::TileId) -> &AtlasUiStateVm {
        self.ui
            .tile_atlas
            .get(&tile_id.0.to_string())
            .unwrap_or(&self.ui.atlas)
    }

    pub fn notice(&mut self, message: impl Into<String>, tone: RyeOsTone) {
        let id = format!("notice:{}", self.generation.saturating_add(1));
        self.ui.notices.push(RyeOsNotice {
            id,
            message: message.into(),
            tone,
        });
        const MAX_NOTICES: usize = 8;
        if self.ui.notices.len() > MAX_NOTICES {
            let excess = self.ui.notices.len() - MAX_NOTICES;
            self.ui.notices.drain(0..excess);
        }
        self.bump_generation();
    }

    pub fn notice_replacing_prefix(
        &mut self,
        prefix: &str,
        message: impl Into<String>,
        tone: RyeOsTone,
    ) {
        self.ui
            .notices
            .retain(|notice| !notice.message.starts_with(prefix));
        self.notice(message, tone);
    }

    /// Like `notice`, but skips when the most recent notice carries the same
    /// message — for repeatable intents (e.g. Tab on an untargetable route)
    /// that must surface the reason once without spamming it on every press.
    pub fn notice_deduped(&mut self, message: impl Into<String>, tone: RyeOsTone) {
        let message = message.into();
        if self
            .ui
            .notices
            .last()
            .is_some_and(|last| last.message == message)
        {
            return;
        }
        self.notice(message, tone);
    }

    pub(crate) fn rebuild_timeline_source_cache(&mut self, source_key: &str) {
        let previous = self.data.timeline_sources.remove(source_key);
        let Some(instance) = super::source_key::RyeOsSourceInstanceKey::decode(source_key)
            .map(|key| key.view_instance)
        else {
            return;
        };
        let Some(view_ref) = self.mounted_view_ref(&instance) else {
            return;
        };
        let Some(binding) = self.binding_for_instance(&instance, view_ref) else {
            return;
        };
        if binding.widget != "timeline" {
            return;
        }
        let Some(response) = self.data.sources.get(source_key) else {
            return;
        };
        let (mut entries, mut indents, mut sources) = super::timeline::timeline_entries_indented(
            super::content::project_records(binding, response),
        );
        if let Some(summary) = super::view_model::timeline_summary_entry(response) {
            entries.insert(0, summary);
            indents.insert(0, 0);
            sources.insert(0, None);
        }
        let (sections, collapsible) = super::timeline::timeline_section_index(&entries);
        let previous_by_key: HashMap<String, (String, Option<u64>)> = previous
            .as_ref()
            .map(|cache| {
                cache
                    .sources
                    .iter()
                    .zip(cache.entries.iter())
                    .zip(cache.arrivals.iter())
                    .filter_map(|((source, entry), arrived)| {
                        Some((
                            source.as_ref()?.key.clone(),
                            (serde_json::to_string(entry).ok()?, *arrived),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let baseline = previous.is_none();
        let arrivals = sources
            .iter()
            .zip(entries.iter())
            .map(|(source, entry)| {
                let source = source.as_ref()?;
                if baseline {
                    return None;
                }
                let signature = serde_json::to_string(entry).ok()?;
                match previous_by_key.get(&source.key) {
                    Some((old_signature, arrived)) if old_signature == &signature => {
                        arrived.filter(|at| self.runtime.now_ms.saturating_sub(*at) <= 2_000)
                    }
                    _ => Some(self.runtime.now_ms),
                }
            })
            .collect();
        self.data.timeline_sources.insert(
            source_key.to_string(),
            RyeOsTimelineSourceCache {
                entries,
                indents,
                sources,
                arrivals,
                sections,
                collapsible,
            },
        );
    }

    pub(crate) fn rebuild_field_source_cache(&mut self, source_key: &str) {
        let is_field_instance = super::source_key::RyeOsSourceInstanceKey::decode(source_key)
            .is_some_and(|key| {
                matches!(
                    self.mounted_local_state(&key.view_instance),
                    Some(ViewLocalState::Field(_))
                )
            });
        if !is_field_instance {
            self.data.field_sources.remove(source_key);
            return;
        }
        if let Some(key) = super::source_key::RyeOsSourceInstanceKey::decode(source_key) {
            self.data
                .field_projections
                .borrow_mut()
                .remove(&key.view_instance);
        }
        let parsed = self
            .data
            .sources
            .get(source_key)
            .map(super::field::parse_field_document);
        match parsed {
            Some(parsed) => {
                self.data
                    .field_sources
                    .insert(source_key.to_string(), parsed);
            }
            None => {
                self.data.field_sources.remove(source_key);
            }
        }
    }

    pub fn envelope(&self, effects: Vec<RyeOsEffect>) -> RyeOsEnvelope {
        RyeOsEnvelope {
            schema_version: "ryeos.ui.envelope.v1".to_string(),
            generation: self.generation,
            view_model: super::view_model::build_view_model(self),
            scene_model: super::scene_model::build_scene_model(self, &self.ui.atlas, None, None),
            effects,
        }
    }

    pub fn notices_vm(&self) -> Vec<RyeOsNoticeVm> {
        self.ui
            .notices
            .iter()
            .map(|notice| RyeOsNoticeVm {
                id: notice.id.clone(),
                message: notice.message.clone(),
                tone: notice.tone,
            })
            .collect()
    }

    /// The currently selected UI target. `None` means the view set's
    /// focused tile remains selected.
    pub(crate) fn open_dialog(&mut self, dialog_id: &str) -> Vec<RyeOsEffect> {
        let predecessor = self.focused_view_instance_key();
        let Some((dialog_id, key)) = self
            .presentation
            .as_ref()
            .and_then(|p| p.resolve_dialog(dialog_id, predecessor.as_ref()))
            .map(|(id, dialog)| (id.to_owned(), dialog.view().instance_key().clone()))
        else {
            return Vec::new();
        };
        if self.binding_attachment_for_instance(&key).is_none() {
            return Vec::new();
        }
        let previous_dialog_open = self
            .presentation
            .as_ref()
            .is_some_and(|p| p.dialogs().values().any(|dialog| dialog.is_open()));
        let destination = self.active_surface().and_then(|surface| {
            surface.active_view_set().map(|set| {
                super::presentation_state::PresentationInsertionDestination {
                    surface_instance_id: surface.id().clone(),
                    view_set_id: set.id,
                }
            })
        });
        let context_origin = predecessor.as_ref().and_then(|key| {
            self.presentation
                .as_ref()?
                .dialogs()
                .values()
                .find(|dialog| dialog.view().instance_key() == key)
                .map(|dialog| dialog.context_origin().cloned())
                .unwrap_or_else(|| Some(key.clone()))
        });
        let Some(presentation) = self.presentation.as_mut() else {
            return Vec::new();
        };
        if presentation
            .open_dialog_with_context(&dialog_id, destination, context_origin)
            .is_err()
        {
            return Vec::new();
        }
        if !previous_dialog_open {
            self.dialog_return_focus = predecessor;
        }
        self.focused_input = Some(key);
        self.field_query_editing = None;
        self.bump_generation();
        self.initial_effects()
    }

    pub(crate) fn close_dialog(&mut self, instance_key: &RyeOsViewInstanceKey) -> Vec<RyeOsEffect> {
        if !self
            .presentation
            .as_mut()
            .is_some_and(|p| p.close_dialog(instance_key))
        {
            return Vec::new();
        }
        if self.focused_input.as_ref() == Some(instance_key) {
            let predecessor = self.dialog_return_focus.take();
            self.focused_input = predecessor.filter(|key| {
                self.presentation
                    .as_ref()
                    .and_then(|p| p.mount(key))
                    .is_some_and(|mount| mount.visible())
            });
        } else {
            self.dialog_return_focus = None;
        }
        if self.field_query_editing.as_ref() == Some(instance_key) {
            self.field_query_editing = None;
        }
        self.bump_generation();
        Vec::new()
    }

    pub fn focus_target(&self) -> Option<RyeOsFocusTarget> {
        if let Some(instance) = self.focused_input.as_ref().filter(|instance| {
            self.presentation
                .as_ref()
                .and_then(|presentation| presentation.mount(instance))
                .is_some_and(|mount| mount.visible())
        }) {
            return Some(RyeOsFocusTarget::MountedView {
                instance_key: instance.clone(),
            });
        }
        self.focus_target_without_input_focus()
    }

    fn focus_target_without_input_focus(&self) -> Option<RyeOsFocusTarget> {
        let set = self.active_view_set()?;
        Some(
            set.focus_target
                .clone()
                .unwrap_or_else(|| RyeOsFocusTarget::ViewSetTile {
                    tile_id: set.focused_tile.0.to_string(),
                }),
        )
    }

    pub fn focused_input_instance(&self) -> Option<(InputBufferKey, String)> {
        let instance = self.focused_view_instance_key()?;
        let mount = self.presentation.as_ref()?.mount(&instance)?;
        if !mount.visible() {
            return None;
        }
        let view_ref = mount.view_ref().to_string();
        let input = self
            .binding_for_instance(&instance, &view_ref)?
            .input
            .as_ref()?;
        Some((
            self.input_key_for(instance, view_ref.clone(), input)?,
            view_ref,
        ))
    }

    pub fn focused_input_route(&self) -> super::seat::InputRoute {
        self.focused_input_instance()
            .map(|(key, _)| self.input_route_for(&key))
            .unwrap_or_default()
    }

    pub(crate) fn focused_view_instance_key(&self) -> Option<RyeOsViewInstanceKey> {
        self.view_instance_key_for_focus_target(self.focus_target()?)
    }

    pub(crate) fn focused_view_instance_key_without_input_focus(
        &self,
    ) -> Option<RyeOsViewInstanceKey> {
        self.view_instance_key_for_focus_target(self.focus_target_without_input_focus()?)
    }

    fn view_instance_key_for_focus_target(
        &self,
        target: RyeOsFocusTarget,
    ) -> Option<RyeOsViewInstanceKey> {
        match target {
            RyeOsFocusTarget::MountedView { instance_key } => Some(instance_key),
            RyeOsFocusTarget::Dock { edge } => self.shared_dock_instance_key(edge),
            RyeOsFocusTarget::ViewSetTile { tile_id } => {
                let tile_id = tile_id.parse::<u64>().ok().map(crate::ids::TileId::new)?;
                Some(
                    self.active_view_set()?
                        .tiles
                        .get(&tile_id)?
                        .instance_key
                        .clone(),
                )
            }
        }
    }

    pub(crate) fn mounted_view_ref(&self, instance: &RyeOsViewInstanceKey) -> Option<&str> {
        Some(self.presentation.as_ref()?.mount(instance)?.view_ref())
    }

    pub(crate) fn focus_default_input(&mut self) -> bool {
        let instance = self.mounted_instances().find(|instance| {
            self.presentation
                .as_ref()
                .and_then(|presentation| presentation.mount(instance))
                .is_some_and(|mount| mount.visible())
                && self
                    .mounted_view_ref(instance)
                    .is_some_and(|view| self.instance_declares_input(instance, view))
        });
        self.focused_input = instance;
        self.focused_input.is_some()
    }

    pub fn default_input_edge(&self) -> Option<RyeOsDockEdge> {
        [
            RyeOsDockEdge::Top,
            RyeOsDockEdge::Left,
            RyeOsDockEdge::Right,
            RyeOsDockEdge::Bottom,
        ]
        .into_iter()
        .find(|edge| {
            self.shared_dock_instance_key(*edge)
                .is_some_and(|instance| {
                    self.presentation
                        .as_ref()
                        .and_then(|presentation| presentation.mount(&instance))
                        .is_some_and(|mount| {
                            mount.visible()
                                && self.instance_declares_input(&instance, mount.view_ref())
                        })
                })
        })
    }

    pub fn has_focused_input(&self) -> bool {
        self.focused_input_instance().is_some()
    }
    pub fn focused_input_buffer(&self) -> Option<&RyeOsInputState> {
        let (key, _) = self.focused_input_instance()?;
        self.input_buffers.get(&key.storage_key())
    }
    pub fn focused_input_buffer_mut(&mut self) -> Option<&mut RyeOsInputState> {
        let (key, _) = self.focused_input_instance()?;
        if self.move_freeze_restricts_instance(&key.view_instance_key) {
            return None;
        }
        Some(self.input_buffers.entry(key.storage_key()).or_default())
    }

    /// Local drafts use the exact mounted binding's admitted input budget too.
    /// Refuse whole edits: clipping would silently change the operator's text.
    pub(crate) fn admit_focused_input_length(&mut self, bytes: Option<usize>) -> bool {
        let Some((key, _)) = self.focused_input_instance() else {
            return false;
        };
        if self.move_freeze_restricts_instance(&key.view_instance_key) {
            return false;
        }
        let limit = self
            .binding_attachment_for_instance(&key.view_instance_key)
            .map(|attachment| attachment.binding_request_bounds.max_input_bytes);
        if let (Some(bytes), Some(limit)) = (bytes, limit)
            && limit > 0
            && u64::try_from(bytes).is_ok_and(|bytes| bytes <= limit)
        {
            return true;
        }
        self.notice("Input edit exceeds the admitted binding budget or has no valid budget; draft unchanged.", RyeOsTone::Warn);
        false
    }

    pub(crate) fn set_focused_input_text_bounded(&mut self, text: String, cursor: usize) -> bool {
        if !self.admit_focused_input_length(Some(text.len())) {
            return false;
        }
        let Some(buffer) = self.focused_input_buffer_mut() else {
            return false;
        };
        buffer.set_text(text, cursor);
        true
    }

    /// Resolve the focused-context capabilities the shared keymap needs.
    /// One base helper so terminal and web build the same context and don't
    /// drift — physical key mapping lives in `ryeos_key_command`, capability
    /// resolution lives here.
    pub fn key_context(&self) -> super::keymap::RyeOsKeyContext {
        let focused = self.focused_input_instance();
        let (text, cursor) = focused
            .as_ref()
            .and_then(|(key, _)| self.input_buffers.get(&key.storage_key()))
            .map(|buf| (buf.text.clone(), buf.cursor))
            .unwrap_or_default();
        let input = focused
            .as_ref()
            .and_then(|(key, view_ref)| self.binding_for_instance(&key.view_instance_key, view_ref))
            .and_then(|binding| binding.input.as_ref());

        // Completion can accept now iff the focused input declares the
        // commands completion AND `accept_slash_completion` would produce a
        // result (cursor at end, leading single `/`, a matching record) —
        // the same predicate `CompleteInput` acts on, so Tab never dispatches
        // a no-op completion when it could cycle the target instead.
        let slash_can_accept = focused
            .as_ref()
            .and_then(|(key, view_ref)| {
                let completion = self
                    .binding_for_instance(&key.view_instance_key, view_ref)?
                    .input
                    .as_ref()?
                    .completion
                    .as_ref()?;
                let response = self.data.sources.get(
                    &super::source_key::RyeOsSourceInstanceKey::completion(
                        key.view_instance_key.clone(),
                        &key.input_id,
                    )
                    .encode(),
                )?;
                let records = super::content::completion_records(completion, response);
                super::tokenize::accept_slash_completion(records, &text, cursor).map(|_| ())
            })
            .is_some();

        // A mention can accept now iff the cursor sits in an `@`-token and the
        // declared mentions source has a matching ref — the same predicate
        // `CompleteInput` acts on, so Tab completes a mention rather than
        // cycling the target or no-op-ing.
        let mention_can_accept = focused
            .as_ref()
            .and_then(|(key, view_ref)| {
                let mentions = self
                    .binding_for_instance(&key.view_instance_key, view_ref)?
                    .input
                    .as_ref()?
                    .mentions
                    .as_ref()?;
                let (_, partial) = super::tokenize::active_mention(&text, cursor)?;
                let response = self.data.sources.get(
                    &super::source_key::RyeOsSourceInstanceKey::mention(
                        key.view_instance_key.clone(),
                        &key.input_id,
                    )
                    .encode(),
                )?;
                let records = super::content::project_mentions(mentions, response);
                (!super::tokenize::mention_completion(&records, partial).is_empty()).then_some(())
            })
            .is_some();
        let input_can_accept_completion = slash_can_accept || mention_can_accept;
        let (focused_row_expandable, focused_row_expanded) =
            self.focused_row_expand_state().unwrap_or((false, false));
        let (focused_tree_collapsible, focused_tree_collapsed) =
            self.focused_tree_row_fold_state().unwrap_or((false, false));

        super::keymap::RyeOsKeyContext {
            dialog_open: self.focused_view_instance_key().is_some_and(|key| {
                self.presentation.as_ref().is_some_and(|presentation| {
                    presentation
                        .dialogs()
                        .values()
                        .any(|dialog| dialog.is_open() && dialog.view().instance_key() == &key)
                })
            }),
            input_visible: focused.is_some() || self.default_input_edge().is_some(),
            input_focused: focused.is_some(),
            input_blurrable: matches!(self.focus_target(), Some(RyeOsFocusTarget::Dock { .. })),
            input_has_text: !text.is_empty(),
            input_is_live_filter: input.is_some_and(|i| i.is_live_filter()),
            input_filter_fields: input
                .filter(|i| i.is_live_filter())
                .and_then(|i| i.feeds.as_ref())
                .is_some_and(|f| f.field_count() > 1),
            input_has_completion: input
                .is_some_and(|i| i.completion.is_some() || i.mentions.is_some()),
            input_can_accept_completion,
            // Targeting retargets the route, so it's only exposed for a
            // route-submit input (defense-in-depth; content validation also
            // degrades a target on a non-route input).
            input_target_cycle: input
                .filter(|i| i.submits_to_route())
                .and_then(|i| i.target.as_ref())
                .map(|t| t.cycle),
            // The head thread is mid-execution → esc interrupts it.
            head_thread_running: self
                .focused_input_instance()
                .map(|(key, _)| self.input_route_for(&key))
                .unwrap_or_default()
                .thread
                .as_deref()
                .is_some_and(|head| self.head_thread_running(head)),
            focused_row_expandable,
            focused_row_expanded,
            focused_tree_collapsible,
            focused_tree_collapsed,
            field: super::keymap::focused_field_key_context(self),
        }
    }

    /// Whether the given thread is still executing per the fetched thread
    /// projections (a non-terminal status). Drives the feed's working
    /// indicator and the esc-interrupt binding.
    pub(crate) fn head_thread_running(&self, head: &str) -> bool {
        self.data.threads.as_ref().is_some_and(|threads| {
            threads.threads.iter().any(|row| {
                row.get("thread_id").and_then(serde_json::Value::as_str) == Some(head)
                    && row
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| {
                            matches!(s, "running" | "created" | "accepted" | "pending")
                        })
            })
        })
    }

    pub(crate) fn note_source_row_changes(
        &mut self,
        source_key: &str,
        old: Option<&serde_json::Value>,
        new: &serde_json::Value,
    ) {
        let Some(instance) = super::source_key::RyeOsSourceInstanceKey::decode(source_key)
            .map(|key| key.view_instance)
        else {
            return;
        };
        let Some(view_ref) = self.mounted_view_ref(&instance) else {
            return;
        };
        let Some(binding) = self.binding_for_instance(&instance, view_ref) else {
            return;
        };
        let cursor = match self.mounted_local_state(&instance) {
            Some(ViewLocalState::GenericList { cursor, .. }) => *cursor,
            _ => return,
        };
        let total = super::content::source_collection(binding, new).len();
        let (start, end) = row_signature_window(total, cursor);
        let new_rows = projected_row_signatures(binding, new, start, end);
        if new_rows.is_empty() {
            return;
        }
        let old_rows = old
            .map(|value| projected_row_signatures(binding, value, start, end))
            .unwrap_or_default();
        let now_ms = self.runtime.now_ms;
        let Some(ViewLocalState::GenericList { changed_rows, .. }) =
            self.mounted_local_state_mut(&instance)
        else {
            return;
        };
        let mut live = std::collections::BTreeSet::new();
        let mut changed = false;
        for (key, (signature, tone)) in new_rows {
            live.insert(key.clone());
            let flash = match old_rows.get(&key) {
                Some((old_signature, _)) if *old_signature == signature => continue,
                // A tone transition (created→running, →completed, →failed)
                // flashes in the NEW tone; a same-tone content change
                // flashes generic (None → the renderer's accent). A newly
                // arrived row announces itself in its own tone.
                Some((_, old_tone)) => (*old_tone != tone).then_some(tone).flatten(),
                None => tone,
            };
            changed_rows.insert(
                key,
                crate::view_set::RowFlash {
                    at_ms: now_ms,
                    tone: flash,
                },
            );
            changed = true;
        }
        changed_rows
            .retain(|key, flash| live.contains(key) && now_ms.saturating_sub(flash.at_ms) <= 2_000);
        if changed {
            self.bump_activity_pulse(0.35);
        }
    }

    pub(crate) fn focused_row_expand_state(&self) -> Option<(bool, bool)> {
        self.active_view_set()?;

        let tile_id = self
            .active_view_set()
            .expect("real Surface tab checked")
            .focused_tile;
        let tile = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)?;
        let binding = self.binding_for_instance(&tile.instance_key, &tile.view.view_ref)?;
        let (key, _, fields) = self.focused_expandable_row(tile_id, binding)?;
        if fields.is_empty() {
            return None;
        }
        let expanded = match &tile.local {
            ViewLocalState::GenericList { expanded_rows, .. } => expanded_rows.contains(&key),
            ViewLocalState::None | ViewLocalState::Field(_) => false,
        };
        Some((true, expanded))
    }

    pub(crate) fn set_focused_row_expanded(&mut self, expand: bool) -> bool {
        if self.active_view_set().is_none() {
            return false;
        }

        let tile_id = self
            .active_view_set()
            .expect("real Surface tab checked")
            .focused_tile;
        let Some(instance) = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)
            .map(|tile| tile.instance_key.clone())
        else {
            return false;
        };
        self.set_view_row_expanded(&instance, expand)
    }

    pub(crate) fn set_view_row_expanded(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        expand: bool,
    ) -> bool {
        if self.active_view_set().is_none() {
            return false;
        }

        let Some(tile_id) = instance.surface_tile_id() else {
            return false;
        };
        let Some(tile) = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)
        else {
            return false;
        };
        if &tile.instance_key != instance {
            return false;
        }
        let Some(binding) = self.binding_for_instance(&tile.instance_key, &tile.view.view_ref)
        else {
            return false;
        };
        let Some((key, _, fields)) = self.focused_expandable_row(tile_id, binding) else {
            return false;
        };
        if fields.is_empty() {
            return false;
        }
        let Some(tile) = self
            .active_view_set_mut()
            .expect("real Surface tab checked")
            .tiles
            .get_mut(&tile_id)
        else {
            return false;
        };
        let ViewLocalState::GenericList { expanded_rows, .. } = &mut tile.local else {
            return false;
        };
        if expand {
            expanded_rows.insert(key)
        } else {
            expanded_rows.remove(&key)
        }
    }

    pub(crate) fn focused_tree_row_fold_state(&self) -> Option<(bool, bool)> {
        self.active_view_set()?;

        let tile_id = self
            .active_view_set()
            .expect("real Surface tab checked")
            .focused_tile;
        let tile = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)?;
        let binding = self.binding_for_instance(&tile.instance_key, &tile.view.view_ref)?;
        super::content::table_hierarchy(binding)?;
        let (channel, _) = binding.primary_source()?;
        let source_key =
            super::source_key::RyeOsSourceInstanceKey::named(tile.instance_key.clone(), channel)
                .encode();
        let response = self.data.sources.get(&source_key)?;
        let records = super::content::source_collection(binding, response);
        let (cursor, collapsed_tree_rows) = match &tile.local {
            ViewLocalState::GenericList {
                cursor,
                collapsed_tree_rows,
                ..
            } => (*cursor, collapsed_tree_rows),
            ViewLocalState::None | ViewLocalState::Field(_) => return None,
        };
        let rows = super::content::table_hierarchy_rows(binding, records, collapsed_tree_rows)?;
        let row = rows.get(cursor.min(rows.len().saturating_sub(1)))?;
        Some((row.has_children, row.collapsed))
    }

    pub(crate) fn set_focused_tree_row_collapsed(&mut self, collapsed: bool) -> bool {
        if self.active_view_set().is_none() {
            return false;
        }

        let tile_id = self
            .active_view_set()
            .expect("real Surface tab checked")
            .focused_tile;
        let Some(tile) = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)
        else {
            return false;
        };
        let Some(binding) = self.binding_for_instance(&tile.instance_key, &tile.view.view_ref)
        else {
            return false;
        };
        if super::content::table_hierarchy(binding).is_none() {
            return false;
        }
        let Some((key, _)) = self.focused_row_key_and_record(tile_id, binding) else {
            return false;
        };
        let Some(tile) = self
            .active_view_set_mut()
            .expect("real Surface tab checked")
            .tiles
            .get_mut(&tile_id)
        else {
            return false;
        };
        let ViewLocalState::GenericList {
            collapsed_tree_rows,
            ..
        } = &mut tile.local
        else {
            return false;
        };
        if collapsed {
            collapsed_tree_rows.insert(key)
        } else {
            collapsed_tree_rows.remove(&key)
        }
    }

    pub(crate) fn set_view_tree_row_collapsed_key(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        key: String,
        collapsed: bool,
    ) -> bool {
        if !self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(instance))
            .is_some_and(|mount| mount.visible())
        {
            return false;
        }
        let Some(ViewLocalState::GenericList {
            collapsed_tree_rows,
            ..
        }) = self.mounted_local_state_mut(instance)
        else {
            return false;
        };
        if collapsed {
            collapsed_tree_rows.insert(key)
        } else {
            collapsed_tree_rows.remove(&key)
        }
    }

    fn focused_row_key_and_record(
        &self,
        tile_id: crate::ids::TileId,
        binding: &super::content::ViewBinding,
    ) -> Option<(String, serde_json::Value)> {
        self.active_view_set()?;

        let tile = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)?;
        let cursor = match &tile.local {
            ViewLocalState::GenericList { cursor, .. } => *cursor,
            ViewLocalState::None | ViewLocalState::Field(_) => 0,
        };
        let (channel, _) = binding.primary_source()?;
        let source_key =
            super::source_key::RyeOsSourceInstanceKey::named(tile.instance_key.clone(), channel)
                .encode();
        let response = self.data.sources.get(&source_key)?;
        match binding.widget.as_str() {
            "rows" => super::content::source_collection(binding, response)
                .get(cursor)
                .map(|record| {
                    (
                        projected_row_key(record, cursor, &binding.projections),
                        record.clone(),
                    )
                }),
            "table" => {
                let records = super::content::source_collection(binding, response);
                let collapsed_tree_rows = match &tile.local {
                    ViewLocalState::GenericList {
                        collapsed_tree_rows,
                        ..
                    } => collapsed_tree_rows,
                    ViewLocalState::None | ViewLocalState::Field(_) => return None,
                };
                match super::content::table_hierarchy_rows(binding, records, collapsed_tree_rows) {
                    Some(rows) => {
                        rows.get(cursor.min(rows.len().saturating_sub(1)))
                            .and_then(|row| {
                                records
                                    .get(row.source_index)
                                    .map(|record| (row.key.clone(), record.clone()))
                            })
                    }
                    None => records.get(cursor).map(|record| {
                        (
                            projected_row_key(record, cursor, &binding.projections),
                            record.clone(),
                        )
                    }),
                }
            }
            "timeline" => {
                let (mut entries, mut indents, mut sources) = self
                    .data
                    .timeline_sources
                    .get(&source_key)
                    .map(|cache| {
                        (
                            cache.entries.clone(),
                            cache.indents.clone(),
                            cache.sources.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        let (mut entries, mut indents, mut sources) =
                            super::timeline::timeline_entries_indented(
                                super::content::project_records(binding, response),
                            );
                        if let Some(summary) = super::view_model::timeline_summary_entry(response) {
                            entries.insert(0, summary);
                            indents.insert(0, 0);
                            sources.insert(0, None);
                        }
                        (entries, indents, sources)
                    });
                // `append_live_delta` can add non-expandable transient entries.
                super::timeline::append_live_delta(self, &mut entries);
                indents.resize(entries.len(), 0);
                sources.resize(entries.len(), None);
                let collapsed = match &tile.local {
                    ViewLocalState::GenericList { collapsed, .. } => collapsed,
                    ViewLocalState::None | ViewLocalState::Field(_) => return None,
                };
                let folded = super::timeline::fold_timeline(entries, indents, sources, collapsed);
                let selected = folded.entries.len().checked_sub(1).map(|last| {
                    let distance = cursor.min(last);
                    last - distance
                })?;
                folded
                    .sources
                    .get(selected)
                    .cloned()
                    .flatten()
                    .map(|source| (source.key, source.raw))
            }
            _ => None,
        }
    }

    fn focused_expandable_row(
        &self,
        tile_id: crate::ids::TileId,
        binding: &super::content::ViewBinding,
    ) -> Option<(String, serde_json::Value, Vec<String>)> {
        self.active_view_set()?;

        if binding.widget != "sections" {
            let (key, record) = self.focused_row_key_and_record(tile_id, binding)?;
            return Some((key, record, super::content::expand_fields(binding)));
        }
        let tile = self
            .active_view_set()
            .expect("real Surface tab checked")
            .tiles
            .get(&tile_id)?;
        let (cursor, collapsed) = match &tile.local {
            ViewLocalState::GenericList {
                cursor, collapsed, ..
            } => (*cursor, collapsed),
            ViewLocalState::None | ViewLocalState::Field(_) => return None,
        };
        let mut flat = 0usize;
        for (section_index, section) in binding.sections.iter().enumerate() {
            if collapsed.contains(&section_index) {
                if cursor == flat {
                    return None;
                }
                flat += 1;
                continue;
            }
            let source_key = super::source_key::RyeOsSourceInstanceKey::named(
                tile.instance_key.clone(),
                &section.source_channel,
            )
            .encode();
            let response = self.data.sources.get(&source_key)?;
            let records = super::content::source_collection_for_section(section, response);
            if cursor < flat.saturating_add(records.len()) {
                let index = cursor - flat;
                let record = (*records.get(index)?).clone();
                return Some((
                    format!(
                        "{section_index}:{}",
                        projected_row_key(&record, index, &section.projection)
                    ),
                    record,
                    super::content::expand_fields_from_projection(&section.projection),
                ));
            }
            flat += records.len();
        }
        None
    }
}

pub(crate) fn row_key(record: &serde_json::Value, index: usize) -> String {
    for field in ["id", "thread_id", "ref"] {
        if let Some(value) = super::content::field_path(record, field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return format!("{field}:{value}");
        }
    }
    let encoded = serde_json::to_vec(record).unwrap_or_else(|_| index.to_string().into_bytes());
    format!("value:{:x}", Sha256::digest(encoded))
}

/// Resolve a view-authored occurrence identity before using the generic record
/// fallback. Collections such as event streams legitimately contain several
/// records for one thread, so the renderer cannot infer their identity from a
/// convenient shared field such as `thread_id`.
pub(crate) fn projected_row_key(
    record: &serde_json::Value,
    index: usize,
    projection: &serde_json::Value,
) -> String {
    if let Some(path) = projection
        .get("identity")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        && let Some(value) =
            super::content::field_text(record, path).filter(|value| !value.is_empty())
    {
        return format!("identity:{path}:{value}");
    }
    row_key(record, index)
}

/// Per-row `(signature, projected tone)` — the signature detects change,
/// the tone names the transition so the flash can speak it.
fn projected_row_signatures(
    binding: &super::content::ViewBinding,
    response: &serde_json::Value,
    start: usize,
    end: usize,
) -> std::collections::BTreeMap<String, (String, Option<String>)> {
    let records = super::content::source_collection(binding, response);
    let start = start.min(records.len());
    let end = end.min(records.len()).max(start);
    match binding.widget.as_str() {
        "rows" => records[start..end]
            .iter()
            .enumerate()
            .map(|(offset, raw)| {
                let index = start + offset;
                let record = super::content::project_record_for_binding(binding, raw);
                let signature = serde_json::json!({
                    "primary": record.primary,
                    "meta": record.meta,
                    "tone": record.tone,
                })
                .to_string();
                (
                    projected_row_key(&record.raw, index, &binding.projections),
                    (signature, record.tone.clone()),
                )
            })
            .collect(),
        "table" => {
            let columns = super::content::table_columns(binding);
            records[start..end]
                .iter()
                .enumerate()
                .map(|(offset, raw)| {
                    let index = start + offset;
                    let record = super::content::project_table_record(binding, raw, &columns);
                    let signature = serde_json::json!({
                        "cells": record.cells,
                        "cell_tones": record.cell_tones,
                        "tone": record.tone,
                    })
                    .to_string();
                    (
                        projected_row_key(&record.raw, index, &binding.projections),
                        (signature, record.tone.clone()),
                    )
                })
                .collect()
        }
        _ => std::collections::BTreeMap::new(),
    }
}

fn row_signature_window(total: usize, cursor: usize) -> (usize, usize) {
    const ROW_WINDOW: usize = 96;
    if total <= ROW_WINDOW {
        return (0, total);
    }
    let cursor = cursor.min(total.saturating_sub(1));
    let start = cursor
        .saturating_sub(ROW_WINDOW / 2)
        .min(total.saturating_sub(ROW_WINDOW));
    (start, start + ROW_WINDOW)
}

fn refresh_matches_hint(value: Option<&serde_json::Value>, kind: &str) -> bool {
    match value {
        Some(serde_json::Value::String(s)) => s == kind,
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .any(|item| item.as_str().is_some_and(|s| s == kind)),
        _ => false,
    }
}

/// True when a scoped source reference resolved to null. Traverse the same
/// object/array structure as parameter resolution; no absent subject is fetched.
fn scoped_param_unresolved(original: &serde_json::Value, resolved: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (original, resolved) {
        (Value::Object(orig), res) => orig
            .iter()
            .any(|(k, ov)| scoped_param_unresolved(ov, res.get(k).unwrap_or(&Value::Null))),
        (Value::Array(orig), res) => orig
            .iter()
            .enumerate()
            .any(|(index, ov)| scoped_param_unresolved(ov, res.get(index).unwrap_or(&Value::Null))),
        (Value::String(s), rv) => {
            (s.starts_with("@facet:") || s.starts_with("@field:")) && rv.is_null()
        }
        _ => false,
    }
}

#[cfg(test)]
mod scoped_source_parameter_tests {
    use super::scoped_param_unresolved;
    use serde_json::json;

    #[test]
    fn scoped_references_inside_nested_arrays_remain_unresolved_until_supplied() {
        let authored = json!({"subjects":[{"ids":["literal", "@facet:selection.thread_id"]},
            ["@field:selected.id"]], "optional":null});
        let complete = json!({"subjects":[{"ids":["literal", "T-one"]},
            ["entity:one"]], "optional":null});
        assert!(!scoped_param_unresolved(&authored, &complete));
        for missing in [
            json!({"subjects":[{"ids":["literal",null]},["entity:one"]]}),
            json!({"subjects":[{"ids":["literal","T-one"]},[null]]}),
            json!({"subjects":[{"ids":["literal","T-one"]}]}),
            json!({"subjects":null}),
        ] {
            assert!(scoped_param_unresolved(&authored, &missing));
        }
        assert!(!scoped_param_unresolved(
            &json!([null, "literal"]),
            &json!([null, "literal"])
        ));
    }
}

fn field_local_state_for_instance<'a>(
    core: &'a RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
) -> Option<&'a crate::view_set::FieldLocalState> {
    match core.mounted_local_state(instance_key)? {
        ViewLocalState::Field(state) => Some(state),
        _ => None,
    }
}

fn resolve_field_params(
    value: &serde_json::Value,
    state: Option<&crate::view_set::FieldLocalState>,
    source_channel: &str,
) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(reference) if reference == "@field:cursor" => state
            .map(|state| serde_json::to_value(&state.cursor).unwrap_or(Value::Null))
            .unwrap_or_else(|| serde_json::json!({ "mode": "live" })),
        Value::String(reference) if reference == "@field:expansions" => Value::Array(
            state
                .into_iter()
                .flat_map(|state| state.expansions.iter())
                .filter_map(|(key, expansion)| {
                    let (source, root_id) = key.split_once('\0')?;
                    if source != source_channel {
                        return None;
                    }
                    Some((root_id, expansion))
                })
                .take(16)
                .map(|(root_id, expansion)| {
                    serde_json::json!({
                        "root_id": root_id,
                        "max_depth": expansion.max_depth,
                        "max_entities": expansion.max_entities,
                        "continuation_token": expansion.continuation_token,
                    })
                })
                .collect(),
        ),
        Value::String(reference) if reference.starts_with("@field:") => Value::Null,
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| resolve_field_params(value, state, source_channel))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        resolve_field_params(value, state, source_channel),
                    )
                })
                .collect(),
        ),
        value => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_widget_local_state_comes_from_exact_authored_binding_and_is_not_reset() {
        use super::*;
        use crate::ui::reducer::test_support;
        let mut session = test_support::session();
        let root = session.root_attachment_id.clone();
        let shared = session
            .binding_attachments
            .iter_mut()
            .find(|binding| binding.binding_attachment_id == root)
            .unwrap();
        shared.presentation.views.insert(
            "view:ryeos/launcher".into(),
            serde_json::json!({"widget":"table"}),
        );
        shared.presentation.views.insert(
            "view:ryeos/input".into(),
            serde_json::json!({"widget":"rows"}),
        );
        shared.presentation.views.insert(
            "view:ryeos/item/inspector".into(),
            serde_json::json!({"widget":"unsupported-test-widget"}),
        );
        let mut core = test_support::core(session);
        core.initial_effects();
        let dock = core
            .shared_dock_instance_key(RyeOsDockEdge::Bottom)
            .unwrap();
        let dialog = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        let unsupported = core.shared_dock_instance_key(RyeOsDockEdge::Right).unwrap();
        for instance in [&dock, &dialog] {
            assert!(matches!(
                core.mounted_local_state(instance),
                Some(ViewLocalState::GenericList { cursor: 0, .. })
            ));
        }
        assert!(matches!(
            core.mounted_local_state(&unsupported),
            Some(ViewLocalState::None)
        ));
        let ViewLocalState::GenericList {
            cursor,
            collapsed_tree_rows,
            ..
        } = core.mounted_local_state_mut(&dialog).unwrap()
        else {
            panic!("authored table local state");
        };
        *cursor = 3;
        collapsed_tree_rows.insert("retained-row".into());
        let before = core.mounted_local_state(&dialog).unwrap().clone();
        core.open_dialog("launcher");
        core.close_dialog(&dialog);
        core.open_dialog("launcher");
        core.normalize_mounted_local_states();
        assert_eq!(core.mounted_local_state(&dialog), Some(&before));
        assert!(matches!(
            core.mounted_local_state(&unsupported),
            Some(ViewLocalState::None)
        ));
    }

    use super::*;

    #[test]
    fn unrelated_acknowledged_events_do_not_change_exact_route_token() {
        let mut core =
            crate::ui::reducer::test_support::core(crate::ui::reducer::test_support::session());
        let instance = core.focused_view_instance_key().unwrap();
        let route = serde_json::json!({"thread":"retained-target"});
        let seq = core.seat.append_facet(
            super::super::seat::input_route_facet_key(&instance),
            route.clone(),
        );
        let token = core.route_seq_for_instance(&instance).unwrap();
        core.retained_input_routes
            .insert(instance.clone(), (seq, Some(route)));
        core.presentation_operations.acknowledged_prefix_len = core.seat.events().len();
        assert_eq!(core.route_seq_for_instance(&instance), Some(token));
        core.seat.append_facet("unrelated", serde_json::json!(true));
        core.presentation_operations.acknowledged_prefix_len = core.seat.events().len();
        assert_eq!(core.route_seq_for_instance(&instance), Some(token));
    }

    #[test]
    fn cleared_or_malformed_route_never_resurrects_authored_target() {
        let mut core =
            crate::ui::reducer::test_support::core(crate::ui::reducer::test_support::session());
        let instance = core.focused_view_instance_key().unwrap();
        for binding in core.binding_attachments.values_mut() {
            binding.initial_input_route.thread = Some("authored-target".into());
        }
        assert_eq!(
            core.route_for_instance(&instance).thread.as_deref(),
            Some("authored-target")
        );
        core.retained_input_routes
            .insert(instance.clone(), (999, None));
        assert_eq!(
            core.route_for_instance(&instance),
            super::super::seat::InputRoute::default()
        );
        core.seat.append_facet(
            super::super::seat::input_route_facet_key(&instance),
            serde_json::json!({"thread":"pending-target"}),
        );
        assert_eq!(
            core.route_for_instance(&instance).thread.as_deref(),
            Some("pending-target")
        );
        core.seat.append_facet(
            super::super::seat::input_route_facet_key(&instance),
            serde_json::Value::Null,
        );
        assert_eq!(
            core.route_for_instance(&instance),
            super::super::seat::InputRoute::default()
        );
        core.seat.append_facet(
            super::super::seat::input_route_facet_key(&instance),
            serde_json::json!({"thread":[] }),
        );
        assert_eq!(
            core.route_for_instance(&instance),
            super::super::seat::InputRoute::default()
        );
    }

    #[test]
    fn duplicate_attachment_identity_does_not_choose_first_or_last_authority() {
        let mut session = test_session(serde_json::json!({"name":"test"}));
        let mut conflicting = session.binding_attachments[0].clone();
        conflicting.binding_digest = "different-admission".into();
        conflicting.project_path = Some("/other/project".into());
        conflicting.subject = super::super::binding::UiBindingSubject::UnregisteredProject;
        session.binding_attachments.push(conflicting);
        let mut core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        assert!(core.binding_attachments.is_empty());
        assert!(core.presentation.is_none());
        assert!(core.initial_effects().is_empty());
    }

    #[test]
    fn malformed_additional_attachment_refuses_the_collection() {
        let mut session = test_session(serde_json::json!({"name":"test"}));
        let mut invalid = session.binding_attachments[0].clone();
        invalid.binding_attachment_id = "invalid-additional".into();
        invalid.binding_generation = 0;
        session.binding_attachments.push(invalid);
        let core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        assert!(core.binding_attachments.is_empty());
    }

    #[test]
    fn missing_shared_config_root_cannot_bootstrap_from_remaining_surface() {
        let mut session = crate::ui::reducer::test_support::session();
        let root = session.presentations[0].root_binding.clone();
        let descriptor = session
            .binding_attachments
            .iter()
            .find(|attachment| attachment.binding_attachment_id == root.binding_attachment_id)
            .unwrap();
        assert!(matches!(
            &descriptor.presentation.definition,
            crate::presentation::UiCompositionDefinition::SharedPresentation { .. }
        ));
        session
            .binding_attachments
            .retain(|attachment| attachment.binding_attachment_id != root.binding_attachment_id);
        assert!(
            session
                .binding_attachments
                .iter()
                .any(|attachment| matches!(
                    &attachment.presentation.definition,
                    crate::presentation::UiCompositionDefinition::Surface { .. }
                ))
        );
        let mut candidate = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        assert!(candidate.presentation.is_none());
        assert!(candidate.binding_attachments.is_empty());
        assert!(candidate.initial_effects().is_empty());
        assert!(candidate.open_dialog("launcher").is_empty());
        assert!(candidate.notices_vm().iter().any(|notice| {
            notice
                .message
                .contains("presentation root attachment is absent")
        }));
    }

    #[test]
    fn resumed_surface_cold_bootstrap_uses_exact_tab_binding_without_provisional_writes() {
        use crate::ui::binding::{UiSavedSurfaceComposition, UiSavedSurfaceTab};
        use crate::ui::reducer::test_support::{
            claim_fixture_producer, pending_prefix, settle_pending_prefix,
        };
        let mut session = test_session(serde_json::json!({
            "name":"test", "tiles":["view:test/detail"],
            "views":{"view:test/detail":{"widget":"rows"}}
        }));
        let root_id = session.root_attachment_id.clone();
        session.binding_attachments.iter_mut().find(|entry| entry.binding_attachment_id == root_id)
            .unwrap().presentation.views.insert("view:ryeos/launcher".into(), serde_json::from_value(serde_json::json!({
                "widget":"rows", "affordances":[{"id":"resume","presentation_operation":"resume_particular_surface",
                    "invoke":{"plane":"rye","ref":"service:ui/ryeos-ui/surfaces/resume","args":{
                        "id":"{record.id}","expected_revision":"{record.expected_revision}",
                        "view_set_expected_revision":"{record.view_set_expected_revision}","creation_request_id":"{record.creation_request_id}"}}}]
            })).unwrap());
        let mut core = crate::ui::reducer::test_support::core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.open_dialog("launcher");
        let instance = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .instance_key()
            .clone();
        core.invoke_affordance(
            &instance,
            "view:ryeos/launcher",
            "resume",
            &serde_json::json!({
                "id":"saved-collection","expected_revision":1,"view_set_expected_revision":1
            }),
        );
        let prepared = pending_prefix(&core)
            .into_iter()
            .find_map(|event| match event.kind {
                crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                    Some(operation)
                }
                _ => None,
            })
            .unwrap();
        let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let invocation = effects
            .iter()
            .find(|effect| {
                matches!(
                    effect.kind,
                    crate::ui::effect::RyeOsEffectKind::InvokePresentationOperation { .. }
                )
            })
            .unwrap();
        let origin = core.active_surface_attachment().unwrap().clone();
        let mut membership =
            core.data.session.as_ref().unwrap().presentations[0].surfaces[0].clone();
        membership.surface_instance_id = "55555555-5555-4555-8555-555555555555".into();
        membership.ordinal += 1;
        let mut tab = origin.clone();
        tab.binding_attachment_id = "saved-tab-binding".into();
        let tab_coordinate = crate::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: tab.binding_attachment_id.clone(),
            binding_generation: tab.binding_generation,
            binding_digest: tab.binding_digest.clone(),
        };
        membership.saved_surface = Some(UiSavedSurfaceComposition {
            provenance:crate::ui::binding::UiSurfaceCompositionProvenance::Library { saved_id:"saved-collection".into(),library_revision:1,record_digest:"a".repeat(64) },
            active_view_set_id:"saved-tab".into(),
            body: crate::ui::binding::UiSavedSurfaceCompositionBody::Particular { tabs:vec![UiSavedSurfaceTab {saved_set_id:"saved-tab".into(),binding:tab_coordinate,
                resolved:serde_json::from_value(serde_json::json!({
                    "id":"saved-tab","name":"Retained tab",
                    "composition":{"schema":2,"id":"saved-tab","title":"Retained tab",
                        "root":{"type":"group","views":["view:test/detail"],"active":0}},
                    "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}],
                    "resolved_selection_work":{"thread":"T-restored","chain_root":"T-root"}
                })).unwrap()
            }] },
        });
        let saved = membership.saved_surface.as_ref().unwrap();
        membership.mount_manifest =
            crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
                core.presentation.as_ref().unwrap().id().clone(),
                crate::ids::SurfaceInstanceId::try_new(membership.surface_instance_id.clone())
                    .unwrap(),
                core.acknowledged_composition_replay()
                    .unwrap()
                    .next_mount_id,
                saved.mount_seeds(),
                Some(&saved.active_view_set_id),
            )
            .unwrap();
        let before = core.presentation.as_ref().unwrap().surfaces().len();
        core.apply_effect_result(crate::ui::effect::RyeOsEffectResult {
            id: invocation.id, ok: true, kind: crate::ui::effect::RyeOsEffectResultKind::BindingInvoked,
            data: Some(serde_json::json!({"particular_surface":membership.saved_surface,
                "particular_selection":{"kind":"surface","id":"saved-collection","expected_revision":1},
                "ui_transition":{"kind":"resume_particular_surface","presentation_id":producer.presentation_id,
                    "creation_request_id":prepared.operation_id,"surface":membership,"attachment":origin,"tab_attachments":[tab]}})), error: None,
        });
        assert_eq!(core.presentation.as_ref().unwrap().surfaces().len(), before);
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces().len(),
            before + 1
        );
        let events = core.seat.events().to_vec();
        assert_eq!(events.len(), core.acknowledged_seat_prefix_len());
        let mut core = RyeOsCore::new(
            core.data.session.clone().unwrap(),
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        core.replay_seat_events(events.clone());
        core.require_replayed_initialization().unwrap();
        let set_id = core
            .active_view_set()
            .expect("saved collection is mounted")
            .id;
        assert_eq!(core.active_view_set().unwrap().title, "Retained tab");
        assert_eq!(
            core.insertion_attachment_id(set_id),
            Some("saved-tab-binding")
        );
        assert!(
            core.seat.events() == events.as_slice(),
            "cold bootstrap must retain only the acknowledged history, without provisional writes"
        );
        assert_eq!(
            core.selection_value_in_view_set(set_id, "selection.work"),
            Some(serde_json::json!({"thread":"T-restored","chain_root":"T-root"}))
        );
        let key = crate::ui::seat::selection_storage_key(set_id, "selection.work").unwrap();
        core.seat.append_facet(key.clone(), serde_json::Value::Null);
        assert_eq!(
            core.selection_value_in_view_set(set_id, "selection.work"),
            Some(serde_json::Value::Null)
        );
        core.seat.append_facet(
            key,
            serde_json::json!({"thread":"T-selected","chain_root":"T-other"}),
        );
        assert_eq!(
            core.selection_value_in_view_set(set_id, "selection.work"),
            Some(serde_json::json!({"thread":"T-selected","chain_root":"T-other"}))
        );
    }

    fn test_session(effective_surface: serde_json::Value) -> BrowserSession {
        crate::ui::reducer::test_support::session_with_surface_and_docks(
            effective_surface,
            serde_json::json!({}),
        )
    }

    #[test]
    fn authored_projection_identity_distinguishes_occurrences_with_one_subject() {
        let projection = serde_json::json!({ "identity": "event_hash" });
        let first = serde_json::json!({ "thread_id": "T-one", "event_hash": "event-a" });
        let second = serde_json::json!({ "thread_id": "T-one", "event_hash": "event-b" });

        assert_eq!(
            projected_row_key(&first, 0, &projection),
            "identity:event_hash:event-a"
        );
        assert_eq!(
            projected_row_key(&second, 1, &projection),
            "identity:event_hash:event-b"
        );
    }

    #[test]
    fn binding_contract_mismatch_decodes_no_views_and_emits_no_effects() {
        let session = BrowserSession {
            ui_binding_contract_revision: "ryeos.ui.binding.future".to_string(),
            session_id: "session:mismatched".to_string(),
            root_attachment_id: "mismatch".into(),
            binding_attachments: vec![super::super::binding::UiBindingAttachment {
                binding_attachment_id: "mismatch".into(),
                binding_generation: 1,
                binding_digest: "mismatch".into(),
                root_ref: "surface:test/mismatch".into(),
                root_generation: "mismatch".into(),
                presentation:
                    crate::presentation::UiCompositionPresentation::from_effective_surface(
                        serde_json::json!({
                        "name": "must-not-activate",
                        "tiles": ["view:test/source"],
                        "views": {
                            "view:test/source": {
                                "widget": "list",
                                "sources": {
                                    "default": { "ref": "service:must/not/fetch" }
                                }
                            }
                        }
                        }),
                    )
                    .unwrap(),
                project_path: None,
                subject: super::super::binding::UiBindingSubject::Projectless,
                posture: Default::default(),
                binding_request_bounds: Default::default(),
            }],
            ..Default::default()
        };

        let mut core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        assert!(core.binding_attachments.is_empty());
        assert!(core.initial_effects().is_empty());
    }

    #[test]
    fn ryeos_input_handles_non_boundary_cursor() {
        let mut input = RyeOsInputState {
            text: "é".to_string(),
            cursor: 1,
            ..Default::default()
        };
        input.insert_char('x');
        assert_eq!(input.text, "xé");
        assert_eq!(input.cursor, 1);

        input.set_text("aé".to_string(), 2);
        assert_eq!(input.cursor, 1);
        input.delete_before_cursor();
        assert_eq!(input.text, "é");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn sections_view_emits_one_fetch_per_section_under_distinct_keys() {
        let session = test_session(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/ryeos/status"],
            "views": {
                "view:ryeos/ryeos/status": {
                    "widget": "sections",
                    "sources": {
                        "threads": { "ref": "service:ui/ryeos-ui/threads/list" },
                        "bundles": { "ref": "service:ui/ryeos-ui/items/list" }
                    },
                    "sections": [
                        { "title": "Threads", "source_channel": "threads", "collection": "rows", "projection": { "primary": "thread_id" } },
                        { "title": "Bundles", "source_channel": "bundles", "collection": "rows", "projection": { "primary": "name" } }
                    ]
                }
            }
        }));
        let mut core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        core = crate::ui::reducer::test_support::core(core.data.session.clone().unwrap());
        let fetches: Vec<(String, String)> = core
            .initial_effects()
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource {
                    tile_id,
                    request:
                        crate::ui::binding::UiBindingRequest {
                            coordinate:
                                crate::ui::binding::UiBindingCoordinate::Source { channel, .. },
                            ..
                        },
                    ..
                } => Some((tile_id.clone(), channel.clone())),
                _ => None,
            })
            .collect();
        // One fetch per authored channel; channel identity, not declaration
        // iteration order, associates it with its service.
        assert_eq!(fetches.len(), 2, "one fetch per section: {fetches:?}");
        let by_channel = fetches
            .iter()
            .map(|(key, binding_channel)| {
                let decoded = crate::ui::source_key::RyeOsSourceInstanceKey::decode(key)
                    .expect("typed source key");
                let crate::ui::source_key::RyeOsSourceChannel::Named(channel) = decoded.channel
                else {
                    panic!("view source must use a named channel");
                };
                (channel, binding_channel.clone())
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            by_channel.get("threads").map(String::as_str),
            Some("threads")
        );
        assert_eq!(
            by_channel.get("bundles").map(String::as_str),
            Some("bundles")
        );
        assert_ne!(fetches[0].0, fetches[1].0);
    }

    #[test]
    fn bound_view_tiles_emit_generic_source_fetch() {
        let session = test_session(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/list"],
            "views": {
                "view:ryeos/threads/list": {
                    "widget": "rows",
                    "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": { "limit": 5 }, "collection": "threads" } },
                    "projections": { "primary": "item_ref" }
                }
            }
        }));
        let mut core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        core = crate::ui::reducer::test_support::core(core.data.session.clone().unwrap());
        let effects = core.initial_effects();
        let fetch = effects.iter().find_map(|effect| match &effect.kind {
            RyeOsEffectKind::FetchSource { request, .. } => match &request.payload {
                crate::ui::binding::UiBindingPayload::SourceParameters { params } => {
                    Some((request.coordinate.clone(), params.clone()))
                }
                _ => None,
            },
            _ => None,
        });
        let (coordinate, params) = fetch.expect("bound tile emits FetchSource");
        assert_eq!(
            coordinate,
            crate::ui::binding::UiBindingCoordinate::Source {
                view_ref: "view:ryeos/threads/list".to_string(),
                channel: "default".to_string(),
            }
        );
        assert_eq!(params["limit"], 5);
    }

    #[test]
    fn hint_refetch_matches_string_and_list_forms() {
        let session = test_session(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/list", "view:ryeos/node/status"],
            "views": {
                "view:ryeos/threads/list": {
                    "widget": "rows",
                    "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "collection": "threads" } },
                    "projections": { "primary": "thread_id" },
                    "refresh": { "on_hint": ["thread", "activity"] }
                },
                "view:ryeos/node/status": {
                    "widget": "text",
                    "sources": { "default": { "ref": "service:node/status" } },
                    "refresh": { "on_hint": "thread" }
                }
            }
        }));
        let mut core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );

        core = crate::ui::reducer::test_support::core(core.data.session.clone().unwrap());
        let hint_fetches: Vec<String> = core
            .effects_for_hints(&["activity".to_string(), "thread".to_string()])
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource { request, .. } => match &request.coordinate {
                    crate::ui::binding::UiBindingCoordinate::Source { view_ref, .. } => {
                        Some(view_ref.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            hint_fetches
                .iter()
                .filter(|view_ref| *view_ref == "view:ryeos/threads/list")
                .count(),
            1,
            "one coalescing window deduplicates views matching multiple hints"
        );
        assert!(hint_fetches.contains(&"view:ryeos/node/status".to_string()));
    }

    #[test]
    fn hint_refetch_preserves_project_requirement_for_visible_sources() {
        let session = test_session(serde_json::json!({
            "name": "t",
            "tiles": ["view:ryeos/project/status"],
            "views": {
                "view:ryeos/project/status": {
                    "widget": "text",
                    "sources": { "default": {
                        "ref": "service:project/status",
                        "requires_project": true,
                        "refresh": { "on_hint": "activity" }
                    }}
                }
            }
        }));
        let mut project_bound = RyeOsCore::new(
            session.clone(),
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        project_bound =
            crate::ui::reducer::test_support::core(project_bound.data.session.clone().unwrap());
        assert!(
            project_bound
                .effects_for_hint("activity")
                .iter()
                .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
        );

        let mut projectless_session = session;
        projectless_session.binding_attachments[0].project_path = None;
        projectless_session.binding_attachments[0].subject =
            super::super::binding::UiBindingSubject::Projectless;
        let mut projectless = RyeOsCore::new(
            projectless_session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        projectless =
            crate::ui::reducer::test_support::core(projectless.data.session.clone().unwrap());
        assert!(
            !projectless
                .initial_effects()
                .iter()
                .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
        );
        assert!(
            !projectless
                .effects_for_hint("activity")
                .iter()
                .any(|effect| matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
        );
    }

    #[test]
    fn hint_refetch_reaches_visible_slot_views() {
        let session = crate::ui::reducer::test_support::session_with_surface_and_docks(
            serde_json::json!({
                "name": "t",
                "tiles": [],
                "views": {
                    "view:ryeos/node/status": {
                        "widget": "text",
                        "sources": { "default": { "ref": "service:node/status" } },
                        "refresh": { "on_hint": "thread" }
                    }
                }
            }),
            serde_json::json!({"top":{"content":"view:ryeos/node/status","open":true,"size":1}}),
        );
        let mut core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let fetches: Vec<(String, crate::ui::binding::UiBindingCoordinate)> = core
            .effects_for_hint("thread")
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource {
                    tile_id, request, ..
                } => Some((tile_id.clone(), request.coordinate.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            fetches,
            vec![(
                crate::ui::source_key::RyeOsSourceInstanceKey::named(
                    core.shared_dock_instance_key(RyeOsDockEdge::Top).unwrap(),
                    "default",
                )
                .encode(),
                crate::ui::binding::UiBindingCoordinate::Source {
                    view_ref: "view:ryeos/node/status".to_string(),
                    channel: "default".to_string(),
                }
            )]
        );
    }

    #[test]
    fn unbound_core_has_no_fabricated_shared_dock_mounts() {
        let core = RyeOsCore::default();
        assert!(core.presentation.is_none());
        let docks = super::super::view_model::build_view_model(&core).docks;
        assert!(docks.top.is_none());
        assert!(docks.bottom.is_none());
        assert!(docks.left.is_none());
        assert!(docks.right.is_none());
    }

    /// Seed a `view:ryeos/input` binding (chat box: route-fold submit).
    fn with_input_view(core: &mut RyeOsCore) {
        let session = crate::ui::reducer::test_support::session_with_surface_and_docks(
            serde_json::json!({"name":"input-fixture", "views":{
                "view:ryeos/input":{"widget":"text","input":{"id":"line","placeholder":"Ask or run a command","submit":"route"}}
            }}),
            serde_json::json!({"bottom":{"content":"view:ryeos/input","open":true,"size":7}}),
        );
        *core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        // Test the explicit-focus transition separately from normal boot focus.
        core.focused_input = None;
    }

    #[test]
    fn input_follows_focused_view_instance() {
        let mut core = RyeOsCore::default();
        // No input view embedded yet: no instance owns input.
        assert!(!core.has_focused_input());
        with_input_view(&mut core);
        // The visible bottom slot is available, but does not own input until
        // focus explicitly moves there.
        assert!(!core.has_focused_input());
        assert_eq!(core.default_input_edge(), Some(RyeOsDockEdge::Bottom));
        core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::FocusInput,
        });
        let (key, view_ref) = core.focused_input_instance().expect("input focused");
        assert_eq!(view_ref, "view:ryeos/input");
        assert_eq!(
            key.view_instance_key,
            core.shared_dock_instance_key(RyeOsDockEdge::Bottom)
                .unwrap()
        );
        assert_eq!(key.input_id, "line");
        // Hiding the slot removes the instance: focus falls through.
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("bottom")
            .unwrap()
            .visible = false;
        assert!(!core.has_focused_input());
    }

    #[test]
    fn draft_namespace_requires_the_exact_admitted_binding_coordinate() {
        let session = crate::ui::reducer::test_support::session();
        let descriptor = &session.binding_attachments[0];
        let instance = RyeOsViewInstanceKey::shared_dock(
            &crate::ids::PresentationId::try_new(
                crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            )
            .unwrap(),
            "bottom",
        )
        .unwrap();
        let original = InputBufferKey::new(instance.clone(), "view:test/chat", "line", descriptor);
        let mut replaced = descriptor.clone();
        replaced.binding_generation += 1;
        let other = InputBufferKey::new(instance.clone(), "view:test/chat", "line", &replaced);
        assert_ne!(original.storage_key(), other.storage_key());
        let coordinate = super::super::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        assert!(InputBufferKey::storage_key_matches_view_binding(
            &original.storage_key(),
            &instance,
            "view:test/chat",
            &coordinate
        ));
        assert!(!InputBufferKey::storage_key_matches_view_binding(
            &other.storage_key(),
            &instance,
            "view:test/chat",
            &coordinate
        ));
        assert!(!InputBufferKey::storage_key_matches_view_binding(
            &original.storage_key(),
            &instance,
            "view:test/replaced",
            &coordinate
        ));
        assert!(InputBufferKey::storage_key_belongs_to(
            &other.storage_key(),
            &instance
        ));
        let mut predecessor = serde_json::to_value(&original).unwrap();
        predecessor.as_object_mut().unwrap().remove("binding_scope");
        assert!(serde_json::from_value::<InputBufferKey>(predecessor).is_err());
    }

    #[test]
    fn conversation_drafts_follow_chain_roots_without_crossing_targets() {
        let session = crate::ui::reducer::test_support::session();
        let descriptor = &session.binding_attachments[0];
        let input: super::super::content::InputBlock = serde_json::from_value(serde_json::json!({
            "id": "line",
            "submit": "route"
        }))
        .unwrap();
        let base = || {
            InputBufferKey::new(
                RyeOsViewInstanceKey::shared_dock(
                    &crate::ids::PresentationId::try_new(
                        crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
                    )
                    .unwrap(),
                    "bottom",
                )
                .unwrap(),
                "view:ryeos/input",
                "line",
                descriptor,
            )
        };
        let first_head = super::super::seat::InputRoute {
            thread: Some("T-head-1".to_string()),
            chain_root: Some("T-root-1".to_string()),
            ..Default::default()
        };
        let next_head = super::super::seat::InputRoute {
            thread: Some("T-head-2".to_string()),
            chain_root: Some("T-root-1".to_string()),
            ..Default::default()
        };
        let other_work = super::super::seat::InputRoute {
            thread: Some("T-head-3".to_string()),
            chain_root: Some("T-root-2".to_string()),
            ..Default::default()
        };

        let first = base().scoped_for_input(&input, &first_head).storage_key();
        let continued = base().scoped_for_input(&input, &next_head).storage_key();
        let other = base().scoped_for_input(&input, &other_work).storage_key();
        assert_eq!(first, continued, "a head ratchet retains the work draft");
        assert_ne!(first, other, "another chain cannot inherit that draft");
    }

    #[test]
    fn ryeos_core_initializes_docks_only_from_admitted_shared_config() {
        let session = crate::ui::reducer::test_support::session_with_surface_and_docks(
            serde_json::json!({"name":"custom", "style":{"border":"thick"}, "views":{
                "view:custom/list":{"widget":"rows"}, "view:ryeos/input":{"widget":"text","input":{"id":"line","submit":"route"}}
            }}),
            serde_json::json!({"left":{"content":"view:custom/list","open":true,"size":20},
                "bottom":{"content":"view:ryeos/input","open":false,"size":5}}),
        );
        let core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );
        let core = crate::ui::reducer::test_support::core(core.data.session.clone().unwrap());
        let presentation = core.presentation.as_ref().unwrap();
        let left = &presentation.docks()["left"];
        assert!(left.visible);
        assert_eq!(left.size, 20);
        assert_eq!(left.view().view_ref(), "view:custom/list");
        let bottom = &presentation.docks()["bottom"];
        assert!(!bottom.visible);
        assert_eq!(bottom.size, 5);
        assert!(!presentation.docks().contains_key("right"));
        assert!(!presentation.docks().contains_key("top"));
        assert_eq!(
            core.active_surface()
                .unwrap()
                .binding()
                .binding_attachment_id,
            "fixture-attachment"
        );
        let binding = core.binding_attachment("fixture-attachment").unwrap();
        assert_eq!(
            binding
                .presentation
                .definition
                .surface()
                .unwrap()
                .style
                .border,
            crate::surface::BorderStyleSpec::Thick
        );
    }

    #[test]
    fn daemon_session_with_invalid_surface_does_not_gain_preview_content() {
        let session = BrowserSession {
            session_id: "ui-session-1".to_string(),
            ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
            root_attachment_id: "broken".into(),
            binding_attachments: vec![super::super::binding::UiBindingAttachment {
                binding_attachment_id: "broken".into(),
                binding_generation: 1,
                binding_digest: "broken".into(),
                root_ref: "config:test/broken".into(),
                root_generation: "broken".into(),
                presentation:
                    crate::presentation::UiCompositionPresentation::from_effective_surface(
                        serde_json::json!({
                            "name": "broken"
                        }),
                    )
                    .unwrap(),
                project_path: None,
                subject: super::super::binding::UiBindingSubject::Projectless,
                posture: Default::default(),
                binding_request_bounds: super::super::binding::UiBindingRequestBounds {
                    max_request_bytes: 4096,
                    max_input_bytes: 1024,
                    max_seat_append_bytes: 64 * 1024,
                },
            }],
            ..Default::default()
        };
        let core = RyeOsCore::new(
            session,
            crate::ui::reducer::test_support::FIXTURE_PRESENTATION_ID,
            BrowserViewport::default(),
            0,
        );

        assert!(core.binding_attachments.is_empty());
        assert!(core.presentation.is_none());
        assert!(
            super::super::view_model::build_view_model(&core)
                .docks
                .bottom
                .is_none()
        );
        assert!(!core.ui.notices.is_empty());
    }

    #[test]
    fn ryeos_core_vm_exposes_default_input_dock() {
        let mut core = RyeOsCore::default();
        with_input_view(&mut core);
        let vm = super::super::view_model::build_view_model(&core);
        assert!(vm.docks.bottom.is_some());
        assert!(vm.docks.top.is_none());
        assert!(vm.docks.left.is_none());
        assert!(vm.docks.right.is_none());
    }

    #[test]
    fn hidden_input_ignores_stale_input_events() {
        let mut core = RyeOsCore::default();
        with_input_view(&mut core);
        core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::FocusInput,
        });
        let (key, _) = core.focused_input_instance().unwrap();
        let stale_address = core.input_address_for(key).unwrap();
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("bottom")
            .unwrap()
            .visible = false;

        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::InsertInputChar { ch: 'x' },
        });
        assert!(effects.is_empty());
        assert!(core.focused_input_buffer().is_none());

        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::InputAt {
                address: stale_address,
                action: super::super::event::RyeOsInputAction::SetText {
                    text: "must not become another mount's draft".into(),
                    cursor: 0,
                },
            },
        });
        assert!(effects.is_empty());
        assert!(
            core.input_buffers
                .values()
                .all(|input| input.text.is_empty())
        );

        // No focused input instance: submit is a no-op.
        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(core.ui.notices.is_empty());
    }

    #[test]
    fn visible_input_without_a_route_refuses_before_dispatch_and_preserves_draft() {
        let mut core = RyeOsCore::default();
        with_input_view(&mut core);
        // A shared input owns its own route; it must not borrow the active
        // Surface's target. No executable binding is reached without a route.
        core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::FocusInput,
        });
        core.focused_input_buffer_mut()
            .unwrap()
            .set_text("run this".to_string(), 8);
        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert_eq!(core.ui.notices.len(), 1);
        assert_eq!(
            core.ui.notices[0].message,
            "Input has no target — the surface declares no route."
        );
        assert_eq!(core.focused_input_buffer().unwrap().text, "run this");
    }
}

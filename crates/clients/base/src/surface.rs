//! SurfaceSpec — declarative UI contract for the RyeOS Studio.
//!
//! A surface is a non-executable Rye item describing the dynamic-tiling
//! workspace (tiling algorithm + ordered initial tiles), edge slots,
//! chrome style, views, commands, and instruments. The TUI consumes
//! **effective surfaces** — either:
//! - `BuiltinDefault`: internal safe fallback (no explicit request)
//! - `LocalPreview`: from `--surface-file`, explicitly untrusted, dev-only
//! - `RyeResolved`: from Rye item services via `--surface`, trusted and composed
//!
//! The TUI does NOT implement source-space precedence, trust verification,
//! kind-schema loading, signature verification, or extends-chain composition.
//! Those belong in ryeosd / item services.

use crate::workspace::{ViewLocalState, ViewSpec, Workspace};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceProvenance {
    pub root: SurfaceProvenanceNode,
    pub ancestors: Vec<SurfaceProvenanceNode>,
    pub references: Vec<SurfaceProvenanceEdge>,
    pub referenced_items: Vec<SurfaceProvenanceNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceProvenanceNode {
    pub requested_id: String,
    pub resolved_ref: String,
    pub source_path: PathBuf,
    pub trust_class: String,
    pub alias_resolution: Option<serde_json::Value>,
    pub added_by: String,
    pub raw_content_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceProvenanceEdge {
    pub from_ref: String,
    pub from_source_path: PathBuf,
    pub to_ref: String,
    pub to_source_path: PathBuf,
    pub trust_class: String,
    pub added_by: String,
}

// ---------------------------------------------------------------------------
// SurfaceSpec — the declarative UI contract
// ---------------------------------------------------------------------------

/// Top-level surface specification — the composed value the TUI receives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceSpec {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Dynamic-tiling algorithm for the center plane. The layout tree is
    /// COMPUTED from this plus the ordered tile list — never authored.
    #[serde(default)]
    pub tiling: TilingSpec,
    /// Ordered initial center tiles, each a `view:<ref>` (graph/atlas
    /// included — they are ordinary `view:` items).
    #[serde(default)]
    pub tiles: Vec<ViewKindSpec>,
    /// Fixed edge slots; an absent edge has no slot.
    #[serde(default)]
    pub slots: SlotsSpec,
    /// Chrome style (border treatment).
    #[serde(default)]
    pub style: SurfaceStyleSpec,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    /// Resolved `view:` bindings embedded at session/load time, keyed by
    /// ref (views-as-content; populated by the resolver, never authored
    /// inline — surfaces reference views, they do not define them).
    #[serde(default)]
    pub views: Option<serde_json::Value>,
    /// Optional backdrop scene view ref (`view:ryeos/backdrop/<name>`, a
    /// normal `view` with `widget: scene`). The renderer draws this scene
    /// into the center rect when the center is empty — the background is
    /// content, never a renderer enum. Absent = no backdrop, the
    /// background fill stands.
    #[serde(default)]
    pub backdrop: Option<String>,
    /// Launchable view refs (the surface's library): resolved and
    /// embedded alongside pane refs; the launcher derives from these.
    #[serde(default)]
    pub library: Vec<String>,
    #[serde(default)]
    pub ambient: Option<AmbientSpec>,
    #[serde(default)]
    pub affordances: Vec<SurfaceCommandSpec>,
    #[serde(default)]
    pub instruments: Vec<InstrumentSpec>,
    #[serde(default)]
    pub capabilities: Option<SurfaceCapabilitySpec>,
}

/// Surface capability restrictions.
/// Child surfaces can only narrow, never widen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceCapabilitySpec {
    #[serde(default)]
    pub allow_execute: Option<bool>,
    #[serde(default)]
    pub allow_thread_cancel: Option<bool>,
    #[serde(default)]
    pub allow_thread_kill: Option<bool>,
    #[serde(default)]
    pub allow_layout_changes: Option<bool>,
}

// ---------------------------------------------------------------------------
// Tiling — the dynamic layout algorithm (mechanism words only)
// ---------------------------------------------------------------------------

/// Dynamic tiling algorithm. The engine computes the layout tree from
/// this spec and the ordered tile list; surfaces never author trees.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TilingSpec {
    #[serde(default)]
    pub mode: TilingModeSpec,
    #[serde(default)]
    pub master: MasterSpec,
    #[serde(default)]
    pub stack: StackSpec,
    #[serde(default)]
    pub insert: InsertSpec,
}

/// Closed tiling-mode vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TilingModeSpec {
    #[default]
    MasterStack,
    /// One center lens at a time: the center holds exactly one tile and
    /// opening a view REPLACES it rather than splitting. The cell-grid
    /// (TUI) composition — breadth comes from swapping the single lens,
    /// not arranging panes. `compute_layout` already renders one tile as a
    /// full-center monocle; this mode keeps the tile count at one.
    SingleLens,
}

/// Master region: side, internal arrangement, share, and tile count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MasterSpec {
    #[serde(default)]
    pub side: SideSpec,
    #[serde(default = "arrange_vertical")]
    pub arrange: ArrangeSpec,
    #[serde(default = "default_master_ratio", deserialize_with = "clamped_ratio")]
    pub ratio: f32,
    #[serde(default = "default_master_count")]
    pub count: usize,
}

impl Default for MasterSpec {
    fn default() -> Self {
        Self {
            side: SideSpec::Right,
            arrange: ArrangeSpec::Vertical,
            ratio: default_master_ratio(),
            count: default_master_count(),
        }
    }
}

/// Stack region: internal arrangement of the non-master tiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackSpec {
    #[serde(default = "arrange_horizontal")]
    pub arrange: ArrangeSpec,
}

impl Default for StackSpec {
    fn default() -> Self {
        Self {
            arrange: ArrangeSpec::Horizontal,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideSpec {
    Left,
    #[default]
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrangeSpec {
    /// Stacked top-to-bottom.
    Vertical,
    /// Side-by-side left-to-right.
    Horizontal,
}

/// Closed insertion vocabulary: where new tiles enter the ordered list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertSpec {
    /// Append: new tiles land at the bottom of the stack region.
    #[default]
    End,
}

fn arrange_vertical() -> ArrangeSpec {
    ArrangeSpec::Vertical
}

fn arrange_horizontal() -> ArrangeSpec {
    ArrangeSpec::Horizontal
}

fn default_master_ratio() -> f32 {
    0.6
}

fn default_master_count() -> usize {
    1
}

fn clamped_ratio<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let raw = f32::deserialize(deserializer)?;
    Ok(raw.clamp(0.1, 0.9))
}

// ---------------------------------------------------------------------------
// Slots — fixed edge slots
// ---------------------------------------------------------------------------

/// Fixed edge slots. An absent edge has no slot at all (nothing to
/// toggle); a closed slot keeps its content but frees its space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotsSpec {
    #[serde(default)]
    pub top: Option<SlotSpec>,
    #[serde(default)]
    pub bottom: Option<SlotSpec>,
    #[serde(default)]
    pub left: Option<SlotSpec>,
    #[serde(default)]
    pub right: Option<SlotSpec>,
}

impl Default for SlotsSpec {
    fn default() -> Self {
        // No default slots: the engine never names product views. Surfaces
        // declare their own slots; a slots-less surface simply has none. The
        // builtin fallback surface declares its own minimal slot explicitly.
        Self {
            top: None,
            bottom: None,
            left: None,
            right: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotSpec {
    pub content: SlotContentSpec,
    #[serde(default)]
    pub open: bool,
    #[serde(default = "default_slot_size")]
    pub size: u16,
}

fn default_slot_size() -> u16 {
    8
}

/// Slot content: a bound `view:` ref, the one uniform content form.
/// Input is no longer a slot literal — it is a view that declares an
/// `input` block (`view:ryeos/input`). Unknown content errors — fail
/// closed, like view kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotContentSpec {
    View(String),
}

impl Serialize for SlotContentSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            SlotContentSpec::View(view_ref) => serializer.serialize_str(view_ref),
        }
    }
}

impl<'de> Deserialize<'de> for SlotContentSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw.starts_with("view:") {
            return Ok(SlotContentSpec::View(raw));
        }
        Err(serde::de::Error::custom(format!(
            "unknown slot content `{raw}` (view: ref expected; `input` is now a view)"
        )))
    }
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

/// Chrome style declared by the surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceStyleSpec {
    #[serde(default)]
    pub border: BorderStyleSpec,
}

/// Closed border vocabulary. Renderers map names to local glyph/pixel
/// treatments; the engine never draws.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BorderStyleSpec {
    Thick,
    #[default]
    Thin,
    Hidden,
    None,
}

impl BorderStyleSpec {
    pub fn name(self) -> &'static str {
        match self {
            BorderStyleSpec::Thick => "thick",
            BorderStyleSpec::Thin => "thin",
            BorderStyleSpec::Hidden => "hidden",
            BorderStyleSpec::None => "none",
        }
    }
}

// ---------------------------------------------------------------------------
// View kinds
// ---------------------------------------------------------------------------

/// A center tile as authored in a surface: a `view:` item ref. Every
/// tile is views-as-content — graph/atlas are ordinary `view:` items
/// (their `widget:` does the rest), so there are no named structural
/// kinds. Serializes as the bare ref string the surface YAML carries.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewKindSpec(pub String);

impl Serialize for ViewKindSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ViewKindSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw.starts_with("view:") {
            return Ok(ViewKindSpec(raw));
        }
        Err(serde::de::Error::custom(format!(
            "unknown pane view `{raw}` (view: ref expected)"
        )))
    }
}

impl ViewKindSpec {
    /// Convert to a runtime ViewSpec.
    pub fn to_view_spec(&self) -> ViewSpec {
        ViewSpec {
            view_ref: self.0.clone(),
        }
    }

    /// Create the appropriate initial local state for this view kind.
    pub fn initial_local_state(&self) -> ViewLocalState {
        self.to_view_spec().initial_local_state()
    }
}

// ---------------------------------------------------------------------------
// Ambient
// ---------------------------------------------------------------------------

/// Ambient animation configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmbientSpec {
    #[serde(default)]
    pub show_background: Option<bool>,
    #[serde(default)]
    pub opacity: Option<f32>,
    #[serde(default)]
    pub mode: AmbientModeSpec,
    #[serde(default)]
    pub atlas: Option<AmbientAtlasSpec>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbientModeSpec {
    #[default]
    Ambient,
    NamespaceAtlas,
    #[serde(rename = "atlas_2d")]
    Atlas2d,
    #[serde(rename = "atlas_paper_3d")]
    AtlasPaper3d,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbientAtlasSpec {
    #[serde(default)]
    pub style: AmbientAtlasStyleSpec,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmbientAtlasStyleSpec {
    #[default]
    #[serde(rename = "flat_2d")]
    Flat2d,
    #[serde(rename = "paper_3d")]
    Paper3d,
}

impl AmbientSpec {
    pub fn namespace_atlas_style(&self) -> Option<AmbientAtlasStyleSpec> {
        match self.mode {
            AmbientModeSpec::Ambient => None,
            AmbientModeSpec::NamespaceAtlas => Some(
                self.atlas
                    .as_ref()
                    .map(|atlas| atlas.style)
                    .unwrap_or_default(),
            ),
            AmbientModeSpec::Atlas2d => Some(AmbientAtlasStyleSpec::Flat2d),
            AmbientModeSpec::AtlasPaper3d => Some(AmbientAtlasStyleSpec::Paper3d),
        }
    }

    pub fn uses_namespace_atlas(&self) -> bool {
        self.show_background.unwrap_or(true) && self.namespace_atlas_style().is_some()
    }
}

// ---------------------------------------------------------------------------
// Commands (surface-declared operator actions)
// ---------------------------------------------------------------------------

/// A command declared by a surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceCommandSpec {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Invocation spec — open content (planes/grammar), never a typed
    /// client enum. Renderers dispatch it through the shared invocation
    /// system.
    pub invoke: Option<serde_json::Value>,
    #[serde(default)]
    pub requires_capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// Instruments (surface-declared, facet-driven)
// ---------------------------------------------------------------------------

/// An instrument (facet gauge/status) declared by a surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentSpec {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub facets: Vec<String>,
}

// ---------------------------------------------------------------------------
// SurfaceSource — where surfaces come from
// ---------------------------------------------------------------------------

/// Origin of a loaded surface spec.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceSource {
    /// Internal safe fallback. Used when no explicit surface requested
    /// and daemon/item service is unavailable.
    BuiltinDefault,
    /// From `--surface-file`. Explicitly untrusted, dev/preview only.
    LocalPreview(PathBuf),
    /// From `--surface surface:<id>`. Resolved through Rye item services.
    /// Not yet implemented — will call effective item API.
    SurfaceRef { canonical_ref: String },
}

// ---------------------------------------------------------------------------
// SurfaceDiagnostic
// ---------------------------------------------------------------------------

/// A diagnostic produced during surface loading or validation.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceDiagnostic {
    /// A warning about unsupported or ambiguous fields.
    UnsupportedField { field: String, message: String },
    /// A validation error that prevents rendering.
    ValidationError { message: String },
    /// An informational note about provenance or source.
    Info { message: String },
}

// ---------------------------------------------------------------------------
// SurfaceLoadOptions
// ---------------------------------------------------------------------------

/// Options for loading a surface.
#[derive(Debug, Clone)]
pub struct SurfaceLoadOptions {
    /// `--surface-file <path>` — load only this file, never scan local.
    pub explicit_file: Option<PathBuf>,
    /// `--surface <name>` — resolve through Rye item services, never scan local.
    pub surface_name: Option<String>,
}

// ---------------------------------------------------------------------------
// LoadedSurface — the retained effective surface
// ---------------------------------------------------------------------------

/// The loaded effective surface, retained in AppModel for the session lifetime.
#[derive(Debug, Clone)]
pub enum LoadedSurface {
    /// Internal built-in safe surface. Last resort, no explicit request.
    Builtin { spec: SurfaceSpec },
    /// Local file loaded via `--surface-file`. Explicitly untrusted/preview.
    LocalPreview {
        path: PathBuf,
        spec: SurfaceSpec,
        diagnostics: Vec<SurfaceDiagnostic>,
    },
    /// Resolved through Rye item services. Trusted and composed.
    /// Not yet implemented — stub returning builtin with diagnostics.
    RyeResolved {
        requested_ref: String,
        spec: SurfaceSpec,
        trusted: bool,
        provenance: SurfaceProvenance,
        item_diagnostics: Vec<SurfaceDiagnostic>,
        tui_diagnostics: Vec<SurfaceDiagnostic>,
    },
}

impl LoadedSurface {
    /// The effective surface spec.
    pub fn spec(&self) -> &SurfaceSpec {
        match self {
            LoadedSurface::Builtin { spec } => spec,
            LoadedSurface::LocalPreview { spec, .. } => spec,
            LoadedSurface::RyeResolved { spec, .. } => spec,
        }
    }

    /// The surface source type.
    pub fn source(&self) -> SurfaceSource {
        match self {
            LoadedSurface::Builtin { .. } => SurfaceSource::BuiltinDefault,
            LoadedSurface::LocalPreview { path, .. } => SurfaceSource::LocalPreview(path.clone()),
            LoadedSurface::RyeResolved { requested_ref, .. } => SurfaceSource::SurfaceRef {
                canonical_ref: requested_ref.clone(),
            },
        }
    }

    /// Whether this surface is trusted (signed, verified).
    pub fn is_trusted(&self) -> bool {
        matches!(self, LoadedSurface::RyeResolved { trusted: true, .. })
    }

    /// Whether this is a local preview (untrusted).
    pub fn is_local_preview(&self) -> bool {
        matches!(self, LoadedSurface::LocalPreview { .. })
    }

    /// Embed resolved `view:` bindings into the surface. The local-preview
    /// path loads the spec from an untrusted file but still resolves its
    /// views through the trusted daemon, so a layout can be previewed with
    /// real content without a populate/install.
    pub fn set_views(&mut self, views: serde_json::Value) {
        let spec = match self {
            LoadedSurface::Builtin { spec } => spec,
            LoadedSurface::LocalPreview { spec, .. } => spec,
            LoadedSurface::RyeResolved { spec, .. } => spec,
        };
        spec.views = Some(views);
    }

    /// Create a RyeResolved surface from daemon response.
    ///
    /// The `value` is the JSON returned by `items.effective`:
    /// `{ "canonical_ref", "kind", "trusted", "composed_value", "provenance", ... }`.
    /// `provenance` is the engine-owned structured provenance object;
    /// this consumer does not accept the old string-list alias shape.
    pub fn from_daemon(
        requested_ref: &str,
        value: serde_json::Value,
    ) -> Result<Self, SurfaceDiagnostic> {
        let composed = value
            .get("composed_value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let trusted = value
            .get("trusted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let provenance = match value
            .get("provenance")
            .cloned()
            .map(serde_json::from_value::<SurfaceProvenance>)
        {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                return Err(SurfaceDiagnostic::ValidationError {
                    message: format!("daemon returned invalid provenance: {e}"),
                });
            }
            None => {
                return Err(SurfaceDiagnostic::ValidationError {
                    message: "daemon response missing provenance".into(),
                });
            }
        };

        if !trusted {
            return Err(SurfaceDiagnostic::ValidationError {
                message: "surface is not trusted".into(),
            });
        }

        // Parse composed value as SurfaceSpec
        let spec = match serde_json::from_value::<SurfaceSpec>(composed) {
            Ok(s) => s,
            Err(e) => {
                return Err(SurfaceDiagnostic::ValidationError {
                    message: format!("daemon returned invalid surface: {}", e),
                });
            }
        };

        // Daemon-side resolution diagnostics (e.g. a bound view that
        // failed to embed) ride the payload as `{level, message}` entries;
        // fold them into item diagnostics so warnings surface as notices
        // and info stays on stderr.
        let item_diagnostics = value
            .get("diagnostics")
            .and_then(|v| v.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let message = entry.get("message")?.as_str()?.to_string();
                        let level = entry.get("level").and_then(|l| l.as_str()).unwrap_or("info");
                        Some(if level == "info" {
                            SurfaceDiagnostic::Info { message }
                        } else {
                            SurfaceDiagnostic::ValidationError { message }
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(LoadedSurface::RyeResolved {
            requested_ref: requested_ref.to_string(),
            spec,
            trusted,
            provenance,
            item_diagnostics,
            tui_diagnostics: Vec::new(),
        })
    }

    /// All diagnostics combined.
    pub fn all_diagnostics(&self) -> Vec<&SurfaceDiagnostic> {
        match self {
            LoadedSurface::Builtin { .. } => Vec::new(),
            LoadedSurface::LocalPreview { diagnostics, .. } => diagnostics.iter().collect(),
            LoadedSurface::RyeResolved {
                item_diagnostics,
                tui_diagnostics,
                ..
            } => item_diagnostics
                .iter()
                .chain(tui_diagnostics.iter())
                .collect(),
        }
    }

    /// Human-readable source label for status display.
    pub fn source_label(&self) -> &str {
        match self {
            LoadedSurface::Builtin { .. } => "builtin",
            LoadedSurface::LocalPreview { .. } => "local preview (untrusted)",
            LoadedSurface::RyeResolved { trusted, .. } => {
                if *trusted {
                    "trusted"
                } else {
                    "untrusted"
                }
            }
        }
    }

    /// The requested ref (for daemon-resolved surfaces) or None.
    pub fn requested_ref(&self) -> Option<&str> {
        match self {
            LoadedSurface::RyeResolved { requested_ref, .. } => Some(requested_ref),
            _ => None,
        }
    }
}

fn empty_provenance(requested_ref: &str) -> SurfaceProvenance {
    SurfaceProvenance {
        root: SurfaceProvenanceNode {
            requested_id: requested_ref.to_string(),
            resolved_ref: requested_ref.to_string(),
            source_path: PathBuf::new(),
            trust_class: "unsigned".into(),
            alias_resolution: None,
            added_by: "pipeline_init".into(),
            raw_content_digest: String::new(),
        },
        ancestors: Vec::new(),
        references: Vec::new(),
        referenced_items: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Built-in default surface
// ---------------------------------------------------------------------------

/// The built-in default Studio surface — dynamic-tiling workspace.
///
/// Data-equivalent to `surface:ryeos/studio/base`: empty center (the
/// backdrop scene shows on first-run), default master/stack tiling,
/// bottom input slot open, side slots closed, thin borders.
pub fn builtin_default() -> SurfaceSpec {
    SurfaceSpec {
        name: "studio-base".into(),
        version: "1.0.0".into(),
        extends: None,
        description: Some("Default RyeOS Studio — dynamic tiling workspace".into()),
        tiling: TilingSpec::default(),
        tiles: Vec::new(),
        slots: SlotsSpec::default(),
        style: SurfaceStyleSpec::default(),
        input: None,
        views: None,
        backdrop: None,
        library: Vec::new(),
        ambient: None,
        affordances: Vec::new(),
        instruments: Vec::new(),
        capabilities: None,
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load a surface using strict source semantics:
///
/// 1. `explicit_file` → LocalPreview (parse that file only, fail closed)
/// 2. `surface_name` → SurfaceRef (resolve through Rye services, fail closed)
/// 3. No explicit request → Builtin (last resort, acceptable fallback)
pub fn load_surface(opts: &SurfaceLoadOptions) -> LoadedSurface {
    // 1. Explicit file path — parse only that file, fail closed on error
    if let Some(ref path) = opts.explicit_file {
        return load_local_preview(path);
    }

    // 2. Named surface — resolve through Rye item services
    //    Do NOT scan local files for this option.
    if let Some(ref name) = opts.surface_name {
        // Future: call items.effective(ref, "surface") API here
        // For now, return a diagnostic that daemon resolution is not yet available
        return LoadedSurface::RyeResolved {
            requested_ref: name.clone(),
            spec: builtin_default(),
            trusted: false,
            provenance: empty_provenance(name),
            item_diagnostics: vec![SurfaceDiagnostic::Info {
                message: format!(
                    "surface resolution through Rye item services not yet implemented \
                     for '{}', using builtin fallback",
                    name
                ),
            }],
            tui_diagnostics: Vec::new(),
        };
    }

    // 3. No explicit request — safe builtin fallback
    LoadedSurface::Builtin {
        spec: builtin_default(),
    }
}

/// Load a local preview file. Fail closed on parse errors.
fn load_local_preview(path: &std::path::Path) -> LoadedSurface {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return LoadedSurface::LocalPreview {
                path: path.to_path_buf(),
                spec: builtin_default(),
                diagnostics: vec![SurfaceDiagnostic::ValidationError {
                    message: format!("failed to read {}: {}", path.display(), e),
                }],
            };
        }
    };

    let spec = match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => match toml::from_str::<SurfaceSpec>(&content) {
            Ok(s) => s,
            Err(e) => {
                return LoadedSurface::LocalPreview {
                    path: path.to_path_buf(),
                    spec: builtin_default(),
                    diagnostics: vec![SurfaceDiagnostic::ValidationError {
                        message: format!("failed to parse TOML {}: {}", path.display(), e),
                    }],
                };
            }
        },
        Some("yaml") | Some("yml") => match serde_yaml::from_str::<SurfaceSpec>(&content) {
            Ok(s) => s,
            Err(e) => {
                return LoadedSurface::LocalPreview {
                    path: path.to_path_buf(),
                    spec: builtin_default(),
                    diagnostics: vec![SurfaceDiagnostic::ValidationError {
                        message: format!("failed to parse YAML {}: {}", path.display(), e),
                    }],
                };
            }
        },
        _ => {
            return LoadedSurface::LocalPreview {
                path: path.to_path_buf(),
                spec: builtin_default(),
                diagnostics: vec![SurfaceDiagnostic::ValidationError {
                    message: format!(
                        "unsupported file extension for {}, expected .toml, .yaml, or .yml",
                        path.display()
                    ),
                }],
            };
        }
    };

    // Warn about unsupported fields
    let mut diagnostics = Vec::new();
    if spec.capabilities.is_some() {
        diagnostics.push(SurfaceDiagnostic::UnsupportedField {
            field: "capabilities".into(),
            message: "capability enforcement not yet implemented, field accepted but ignored"
                .into(),
        });
    }
    if !spec.instruments.is_empty() {
        diagnostics.push(SurfaceDiagnostic::UnsupportedField {
            field: "instruments".into(),
            message: "instrument rendering not yet implemented, field accepted but ignored".into(),
        });
    }
    if spec.extends.is_some() {
        diagnostics.push(SurfaceDiagnostic::UnsupportedField {
            field: "extends".into(),
            message: "extends composition not yet supported in local preview, \
                       tiling must be fully specified"
                .into(),
        });
    }

    LoadedSurface::LocalPreview {
        path: path.to_path_buf(),
        spec,
        diagnostics,
    }
}

// ---------------------------------------------------------------------------
// SurfaceSpec → Workspace conversion
// ---------------------------------------------------------------------------

impl SurfaceSpec {
    /// Convert this surface spec into a Workspace for rendering.
    ///
    /// The workspace holds the ordered tile list and the tiling spec;
    /// the layout tree is computed, never stored.
    pub fn to_workspace(&self) -> Workspace {
        Workspace::from_tiling(
            self.tiling.clone(),
            self.tiles.iter().map(ViewKindSpec::to_view_spec).collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance_json<const N: usize>(root: &str, ancestors: [&str; N]) -> serde_json::Value {
        serde_json::json!({
            "root": provenance_node(root, "pipeline_init"),
            "ancestors": ancestors
                .into_iter()
                .map(|r| provenance_node(r, "resolve_extends_chain"))
                .collect::<Vec<_>>(),
            "references": [],
            "referenced_items": [],
        })
    }

    fn provenance_node(ref_: &str, added_by: &str) -> serde_json::Value {
        serde_json::json!({
            "requested_id": ref_,
            "resolved_ref": ref_,
            "source_path": format!("/mock/{ref_}"),
            "trust_class": "trusted_bundle",
            "alias_resolution": null,
            "added_by": added_by,
            "raw_content_digest": "0".repeat(64),
        })
    }

    #[test]
    fn builtin_default_names_no_views() {
        // The builtin fallback is an empty shell: it names zero product views
        // (no fire-sword). Real content comes from surface data; with nothing
        // resolved, the fallback is simply empty.
        let spec = builtin_default();
        assert_eq!(spec.name, "studio-base");
        assert!(spec.tiles.is_empty());
        assert_eq!(spec.tiling, TilingSpec::default());
        assert!(spec.slots.bottom.is_none());
        assert!(spec.slots.top.is_none());
        assert!(spec.slots.left.is_none());
        assert!(spec.slots.right.is_none());
        assert!(spec.views.is_none());
        assert!(spec.backdrop.is_none());
        assert!(spec.library.is_empty());
    }

    #[test]
    fn builtin_default_produces_empty_center_workspace() {
        let ws = builtin_default().to_workspace();
        assert!(ws.center_is_empty());
        assert!(ws.tile_ids().is_empty());
        assert!(ws.layout().is_none());
    }

    #[test]
    fn surface_with_home_view_field_is_rejected() {
        // CLEAN CUT: `home_view` (the deleted home-mode field) must fail
        // to parse — the empty-center background is now `backdrop`.
        let bad: Result<SurfaceSpec, _> =
            serde_yaml::from_str("name: x\nhome_view: \"view:ryeos/home/brand\"\n");
        let err = bad.expect_err("`home_view` must be rejected");
        assert!(
            err.to_string().contains("home_view"),
            "error should name the rejected field: {err}"
        );
    }

    #[test]
    fn surface_with_frame_home_field_is_rejected() {
        // CLEAN CUT: there is no `frame` field; `frame: home` must fail.
        let bad: Result<SurfaceSpec, _> = serde_yaml::from_str("name: x\nframe: home\n");
        assert!(bad.is_err(), "`frame: home` must be rejected");
    }

    #[test]
    fn surface_declares_backdrop_scene_view() {
        let spec: SurfaceSpec =
            serde_yaml::from_str("name: x\nbackdrop: \"view:test/backdrop\"\n").unwrap();
        assert_eq!(spec.backdrop.as_deref(), Some("view:test/backdrop"));
    }

    #[test]
    fn legacy_layout_node_tree_is_rejected() {
        // CLEAN CUT: the static node-tree form is gone. A surface that
        // still authors `layout: {root, nodes}` must fail to parse.
        let legacy = r#"
name: legacy
version: "1.0.0"
layout:
  root: main
  nodes:
    main:
      type: pane
      view: "view:ryeos/threads/list"
"#;
        let parsed: Result<SurfaceSpec, _> = serde_yaml::from_str(legacy);
        let err = parsed.expect_err("legacy layout block must be rejected");
        assert!(
            err.to_string().contains("layout"),
            "error should name the rejected field: {err}"
        );
    }

    #[test]
    fn new_schema_parses_fully() {
        let yaml = r#"
name: full
version: "1.0.0"
tiling:
  mode: master_stack
  master: { side: left, arrange: horizontal, ratio: 0.7, count: 2 }
  stack: { arrange: vertical }
  insert: end
tiles:
  - "view:ryeos/chain/timeline"
  - "view:ryeos/atlas"
  - "view:ryeos/graph/topology"
slots:
  bottom: { content: "view:ryeos/input", open: true, size: 7 }
  left: { content: "view:ryeos/threads/list", open: false, size: 32 }
style:
  border: thick
"#;
        let spec: SurfaceSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.tiling.master.side, SideSpec::Left);
        assert_eq!(spec.tiling.master.arrange, ArrangeSpec::Horizontal);
        assert_eq!(spec.tiling.master.count, 2);
        assert!((spec.tiling.master.ratio - 0.7).abs() < f32::EPSILON);
        assert_eq!(spec.tiling.stack.arrange, ArrangeSpec::Vertical);
        assert_eq!(spec.tiles.len(), 3);
        assert_eq!(
            spec.tiles[0],
            ViewKindSpec("view:ryeos/chain/timeline".into())
        );
        assert_eq!(spec.tiles[1], ViewKindSpec("view:ryeos/atlas".into()));
        // Edges not named have no slot.
        assert!(spec.slots.right.is_none());
        assert!(spec.slots.top.is_none());
        assert_eq!(spec.style.border, BorderStyleSpec::Thick);
    }

    #[test]
    fn ratio_is_clamped_at_parse() {
        let spec: SurfaceSpec =
            serde_yaml::from_str("name: clamp\ntiling:\n  master: { ratio: 0.99 }\n").unwrap();
        assert!((spec.tiling.master.ratio - 0.9).abs() < f32::EPSILON);
        let spec: SurfaceSpec =
            serde_yaml::from_str("name: clamp\ntiling:\n  master: { ratio: 0.01 }\n").unwrap();
        assert!((spec.tiling.master.ratio - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn closed_vocab_rejects_unknown_values() {
        for bad in [
            "name: x\ntiling: { mode: spiral }\n",
            "name: x\ntiling: { insert: start }\n",
            "name: x\ntiling:\n  master: { side: top }\n",
            "name: x\ntiling:\n  master: { arrange: diagonal }\n",
            "name: x\ntiling:\n  stack: { arrange: spiral }\n",
            "name: x\nstyle: { border: wavy }\n",
        ] {
            let parsed: Result<SurfaceSpec, _> = serde_yaml::from_str(bad);
            assert!(parsed.is_err(), "must reject: {bad}");
        }
    }

    #[test]
    fn slot_content_parses_view_refs_only() {
        let spec: SurfaceSpec = serde_yaml::from_str(
            "name: x\nslots:\n  bottom: { content: \"view:ryeos/input\" }\n  left: { content: \"view:a/b\" }\n",
        )
        .unwrap();
        assert_eq!(
            spec.slots.bottom.as_ref().map(|s| &s.content),
            Some(&SlotContentSpec::View("view:ryeos/input".into()))
        );
        assert_eq!(
            spec.slots.left.as_ref().map(|s| &s.content),
            Some(&SlotContentSpec::View("view:a/b".into()))
        );
        // Unknown slot content fails closed.
        let bad: Result<SurfaceSpec, _> =
            serde_yaml::from_str("name: x\nslots:\n  bottom: { content: status }\n");
        assert!(bad.is_err(), "unknown slot content must be rejected");
    }

    #[test]
    fn slot_content_input_literal_is_rejected() {
        // CLEAN CUT: `content: input` is gone. The bottom input is a view
        // (`view:ryeos/input`) that declares an `input` block.
        let bad: Result<SurfaceSpec, _> =
            serde_yaml::from_str("name: x\nslots:\n  bottom: { content: input }\n");
        let err = bad.expect_err("`content: input` must be rejected");
        assert!(
            err.to_string().contains("input"),
            "error should mention the rejected literal: {err}"
        );
    }

    #[test]
    fn to_workspace_preserves_tile_order() {
        let spec: SurfaceSpec = serde_yaml::from_str(
            "name: x\ntiles: [\"view:a/b\", \"view:ryeos/graph/topology\", \"view:ryeos/atlas\"]\n",
        )
        .unwrap();
        let ws = spec.to_workspace();
        let ids = ws.tile_ids();
        assert_eq!(ids.len(), 3);
        assert!(matches!(
            ws.tiles.get(&ids[0]).map(|t| &t.view),
            Some(ViewSpec { view_ref }) if view_ref == "view:a/b"
        ));
        assert!(matches!(
            ws.tiles.get(&ids[1]).map(|t| &t.view),
            Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/graph/topology"
        ));
        assert!(matches!(
            ws.tiles.get(&ids[2]).map(|t| &t.view),
            Some(ViewSpec { view_ref }) if view_ref == "view:ryeos/atlas"
        ));
        assert_eq!(ws.focused_tile, ids[0]);
        // Bound tiles carry generic list local state.
        assert!(matches!(
            ws.tiles.get(&ids[0]).map(|t| &t.local),
            Some(ViewLocalState::GenericList { .. })
        ));
    }

    #[test]
    fn load_default_when_no_options() {
        let opts = SurfaceLoadOptions {
            explicit_file: None,
            surface_name: None,
        };
        let loaded = load_surface(&opts);
        assert!(matches!(loaded, LoadedSurface::Builtin { .. }));
        assert_eq!(loaded.spec().name, "studio-base");
        assert!(matches!(loaded.source(), SurfaceSource::BuiltinDefault));
    }

    #[test]
    fn explicit_file_loads_only_that_file() {
        let dir = std::env::temp_dir().join("ryeos_tui_test_explicit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test-surface.toml");
        std::fs::write(
            &path,
            r#"
name = "test"
version = "0.1.0"
tiles = ["view:ryeos/threads/list"]
"#,
        )
        .unwrap();

        let opts = SurfaceLoadOptions {
            explicit_file: Some(path.clone()),
            surface_name: None,
        };
        let loaded = load_surface(&opts);
        assert!(matches!(loaded, LoadedSurface::LocalPreview { .. }));
        assert_eq!(loaded.spec().name, "test");
        assert_eq!(loaded.spec().tiles.len(), 1);
        assert!(matches!(loaded.source(), SurfaceSource::LocalPreview(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_file_fail_closed() {
        let dir = std::env::temp_dir().join("ryeos_tui_test_fail_closed");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "not valid toml {{{{").unwrap();

        let opts = SurfaceLoadOptions {
            explicit_file: Some(path.clone()),
            surface_name: None,
        };
        let loaded = load_surface(&opts);
        // Should still return a surface (builtin fallback) but with error diagnostics
        assert!(matches!(loaded, LoadedSurface::LocalPreview { .. }));
        let has_error = loaded
            .all_diagnostics()
            .iter()
            .any(|d| matches!(d, SurfaceDiagnostic::ValidationError { .. }));
        assert!(has_error, "should have validation error diagnostic");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn surface_name_does_not_scan_local() {
        let opts = SurfaceLoadOptions {
            explicit_file: None,
            surface_name: Some("graph".into()),
        };
        let loaded = load_surface(&opts);
        // Should be RyeResolved (even if stub), not LocalPreview
        assert!(matches!(loaded, LoadedSurface::RyeResolved { .. }));
        assert!(matches!(loaded.source(), SurfaceSource::SurfaceRef { .. }));
        // Should NOT have scanned local files
    }

    #[test]
    fn view_kind_initial_local_state() {
        // Every tile is a bound view and gets list-local state.
        assert!(matches!(
            ViewKindSpec("view:test/x".into()).initial_local_state(),
            ViewLocalState::GenericList { .. }
        ));
        assert!(matches!(
            ViewKindSpec("view:ryeos/atlas".into()).initial_local_state(),
            ViewLocalState::GenericList { .. }
        ));
    }

    #[test]
    fn loaded_surface_source_label() {
        let builtin = LoadedSurface::Builtin {
            spec: builtin_default(),
        };
        assert_eq!(builtin.source_label(), "builtin");

        let local = LoadedSurface::LocalPreview {
            path: PathBuf::from("test.yaml"),
            spec: builtin_default(),
            diagnostics: Vec::new(),
        };
        assert_eq!(local.source_label(), "local preview (untrusted)");

        let resolved = LoadedSurface::RyeResolved {
            requested_ref: "surface:ryeos/studio/graph".into(),
            spec: builtin_default(),
            trusted: true,
            provenance: empty_provenance("surface:ryeos/studio/graph"),
            item_diagnostics: Vec::new(),
            tui_diagnostics: Vec::new(),
        };
        assert_eq!(resolved.source_label(), "trusted");
    }

    #[test]
    fn view_kind_to_view_spec_roundtrip() {
        assert_eq!(
            ViewKindSpec("view:test/x".into()).to_view_spec(),
            ViewSpec::bound("view:test/x")
        );
        assert_eq!(
            ViewKindSpec("view:ryeos/graph/topology".into()).to_view_spec(),
            ViewSpec::bound("view:ryeos/graph/topology")
        );
    }

    #[test]
    fn bundled_base_surface_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("bundles/studio/.ai/surfaces/ryeos/studio/base.yaml");
        assert!(path.exists(), "bundled surface missing at {path:?}");
        let content = std::fs::read_to_string(path).unwrap();
        let spec: SurfaceSpec = serde_yaml::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse bundled base surface: {}", e));
        assert_eq!(spec.name, "studio-base");
        assert!(!spec.affordances.is_empty());
        assert!(spec.tiles.is_empty(), "base starts with an empty center");
        assert!(matches!(
            spec.slots.bottom.as_ref().map(|s| &s.content),
            Some(SlotContentSpec::View(view_ref)) if view_ref == "view:ryeos/input"
        ));
        assert_eq!(spec.style.border, BorderStyleSpec::Thin);
        let ws = spec.to_workspace();
        assert!(ws.center_is_empty());
    }

    #[test]
    fn legacy_product_pane_names_are_rejected() {
        // The named-kind vocabulary is engine-only; product concepts
        // arrive as `view:` refs. Old names must fail loudly, never
        // silently map.
        for legacy in [
            "thread_list",
            "overview",
            "remotes",
            "services",
            "item_inspector",
            "schedules",
            "gc_status",
            "files",
            "projects",
            "space_browser",
            "trust",
            "event_inspector",
            "thread",
        ] {
            let parsed: Result<ViewKindSpec, _> = serde_yaml::from_str(&format!("\"{legacy}\""));
            assert!(
                parsed.is_err(),
                "legacy pane name `{legacy}` must be rejected"
            );
        }
        // Bare named kinds are gone — graph/atlas are `view:` refs now.
        for rejected in ["atlas", "graph"] {
            let parsed: Result<ViewKindSpec, _> = serde_yaml::from_str(&format!("\"{rejected}\""));
            assert!(
                parsed.is_err(),
                "bare named kind `{rejected}` must be rejected"
            );
        }
        for kept in [
            "view:ryeos/threads/list",
            "view:ryeos/atlas",
            "view:ryeos/graph/topology",
        ] {
            let parsed: Result<ViewKindSpec, _> = serde_yaml::from_str(&format!("\"{kept}\""));
            assert!(parsed.is_ok(), "view ref `{kept}` must parse");
        }
    }

    #[test]
    fn bundled_workbench_surface_loads_with_bound_view() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("bundles/studio/.ai/surfaces/ryeos/studio/workbench.yaml");
        assert!(
            path.exists(),
            "bundled workbench surface missing at {path:?}"
        );
        let content = std::fs::read_to_string(path).unwrap();
        let spec: SurfaceSpec = serde_yaml::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse bundled workbench surface: {}", e));
        assert_eq!(spec.name, "studio-workbench");
        // Views-as-content: the workbench binds the threads view by ref
        // in its ordered center tiles.
        assert!(
            spec.tiles
                .iter()
                .any(|tile| matches!(tile, ViewKindSpec(view_ref)
                    if view_ref == "view:ryeos/threads/list")),
            "workbench must bind the threads view by ref"
        );
    }

    #[test]
    fn bundled_atlas_surface_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("bundles/studio/.ai/surfaces/ryeos/studio/atlas.yaml");
        assert!(path.exists(), "bundled atlas surface missing at {path:?}");
        let content = std::fs::read_to_string(path).unwrap();
        let spec: SurfaceSpec = serde_yaml::from_str(&content)
            .unwrap_or_else(|e| panic!("failed to parse bundled atlas surface: {}", e));
        assert_eq!(spec.name, "studio-atlas");
        assert_eq!(
            spec.ambient.as_ref().map(|ambient| ambient.mode),
            Some(AmbientModeSpec::NamespaceAtlas)
        );
        assert_eq!(
            spec.ambient
                .as_ref()
                .and_then(|ambient| ambient.namespace_atlas_style()),
            Some(AmbientAtlasStyleSpec::Flat2d)
        );
    }

    #[test]
    fn yaml_surface_parses() {
        let dir = std::env::temp_dir().join("ryeos_tui_test_yaml");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.yaml");
        std::fs::write(
            &path,
            r#"
name: test-yaml
version: "0.1.0"
tiles:
  - "view:ryeos/threads/list"
"#,
        )
        .unwrap();

        let opts = SurfaceLoadOptions {
            explicit_file: Some(path.clone()),
            surface_name: None,
        };
        let loaded = load_surface(&opts);
        match &loaded {
            LoadedSurface::LocalPreview {
                spec, diagnostics, ..
            } => {
                assert_eq!(spec.name, "test-yaml");
                assert_eq!(spec.tiles.len(), 1);
                assert!(diagnostics.is_empty(), "no diagnostics for clean surface");
            }
            _ => panic!("expected LocalPreview"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_fields_generate_diagnostics() {
        let dir = std::env::temp_dir().join("ryeos_tui_test_unsupported");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("diag.toml");
        std::fs::write(
            &path,
            r#"
name = "test"
version = "0.1.0"
tiles = ["view:ryeos/threads/list"]

[capabilities]
allow_execute = true

[[instruments]]
id = "test"
"#,
        )
        .unwrap();

        let opts = SurfaceLoadOptions {
            explicit_file: Some(path.clone()),
            surface_name: None,
        };
        let loaded = load_surface(&opts);
        if let LoadedSurface::LocalPreview { diagnostics, .. } = &loaded {
            let has_cap_warning = diagnostics.iter().any(|d| {
                matches!(d, SurfaceDiagnostic::UnsupportedField { field, .. } if field == "capabilities")
            });
            let has_instr_warning = diagnostics.iter().any(|d| {
                matches!(d, SurfaceDiagnostic::UnsupportedField { field, .. } if field == "instruments")
            });
            assert!(has_cap_warning, "should warn about capabilities");
            assert!(has_instr_warning, "should warn about instruments");
        } else {
            panic!("expected LocalPreview");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_daemon_signed_surface() {
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/base",
            "canonical_ref": "surface:ryeos/studio/base",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/base.yaml" },
            "provenance": provenance_json("surface:ryeos/studio/base", []),
            "composed_value": {
                "name": "base",
                "tiles": ["view:ryeos/chain/timeline"],
                "affordances": [
                    { "id": "view.thread", "label": "Thread", "category": "View" }
                ]
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let loaded = LoadedSurface::from_daemon("surface:ryeos/studio/base", response).unwrap();

        match &loaded {
            LoadedSurface::RyeResolved {
                requested_ref,
                spec,
                trusted,
                provenance,
                item_diagnostics,
                tui_diagnostics,
            } => {
                assert_eq!(requested_ref, "surface:ryeos/studio/base");
                assert_eq!(spec.name, "base");
                assert_eq!(spec.tiles.len(), 1);
                assert_eq!(spec.affordances.len(), 1);
                assert_eq!(spec.affordances[0].id, "view.thread");
                assert!(*trusted, "signed surface should be trusted");
                assert_eq!(provenance.root.resolved_ref, "surface:ryeos/studio/base");
                assert!(provenance.ancestors.is_empty());
                assert!(
                    item_diagnostics.is_empty(),
                    "signed surface should have no item diagnostics"
                );
                assert!(tui_diagnostics.is_empty());
            }
            other => panic!(
                "expected RyeResolved, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn from_daemon_embedded_views_and_diagnostics() {
        // The daemon embeds bound views into `composed_value.views` and
        // reports per-view failures as warn diagnostics; both must survive
        // into the loaded surface (warn → ValidationError notice, info →
        // Info on stderr).
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/base",
            "canonical_ref": "surface:ryeos/studio/base",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/base.yaml" },
            "provenance": provenance_json("surface:ryeos/studio/base", []),
            "composed_value": {
                "name": "base",
                "tiles": ["view:ryeos/chain/timeline", "view:ryeos/gone"],
                "views": {
                    "view:ryeos/chain/timeline": { "widget": "timeline" },
                    "view:ryeos/gone": { "degraded": "item not found" }
                }
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": [
                { "level": "warn", "message": "view view:ryeos/gone unavailable: item not found" },
                { "level": "info", "message": "extends chain: base" }
            ]
        });

        let loaded = LoadedSurface::from_daemon("surface:ryeos/studio/base", response).unwrap();

        let views = loaded.spec().views.as_ref().expect("views embedded");
        assert_eq!(views["view:ryeos/chain/timeline"]["widget"], "timeline");
        assert_eq!(views["view:ryeos/gone"]["degraded"], "item not found");

        let diags = loaded.all_diagnostics();
        assert!(
            diags.iter().any(|d| matches!(
                d,
                SurfaceDiagnostic::ValidationError { message }
                    if message.contains("view:ryeos/gone unavailable")
            )),
            "warn diagnostic folds into a visible notice: {diags:?}"
        );
        assert!(
            diags.iter().any(|d| matches!(
                d,
                SurfaceDiagnostic::Info { message } if message.contains("extends chain")
            )),
            "info diagnostic stays informational: {diags:?}"
        );
    }

    #[test]
    fn from_daemon_unsigned_surface_fails_closed() {
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/graph",
            "canonical_ref": "surface:ryeos/studio/graph",
            "kind": "surface",
            "trusted": false,
            "trust_class": "unsigned",
            "root_trust_class": "unsigned",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/graph.yaml" },
            "provenance": provenance_json("surface:ryeos/studio/graph", []),
            "composed_value": {
                "name": "graph",
                "tiles": ["view:ryeos/graph/topology"],
                "affordances": []
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let err = LoadedSurface::from_daemon("surface:ryeos/studio/graph", response).unwrap_err();

        match err {
            SurfaceDiagnostic::ValidationError { message } => {
                assert!(message.contains("not trusted"));
            }
            other => panic!("expected ValidationError, got {:?}", other),
        }
    }

    #[test]
    fn from_daemon_invalid_composed_fails_closed() {
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/bad",
            "canonical_ref": "surface:ryeos/studio/bad",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/bad.yaml" },
            "provenance": provenance_json("surface:ryeos/studio/bad", []),
            "composed_value": { "garbage": true },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let err = LoadedSurface::from_daemon("surface:ryeos/studio/bad", response).unwrap_err();
        match err {
            SurfaceDiagnostic::ValidationError { message } => {
                assert!(message.contains("daemon returned invalid surface"));
            }
            other => panic!("expected ValidationError, got {:?}", other),
        }
    }

    #[test]
    fn from_daemon_rejects_legacy_layout_field() {
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/old",
            "canonical_ref": "surface:ryeos/studio/old",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/old.yaml" },
            "provenance": provenance_json("surface:ryeos/studio/old", []),
            "composed_value": {
                "name": "old",
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "view:ryeos/chain/timeline" }
                    }
                }
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let err = LoadedSurface::from_daemon("surface:ryeos/studio/old", response).unwrap_err();
        match err {
            SurfaceDiagnostic::ValidationError { message } => {
                assert!(message.contains("unknown field `layout`"));
            }
            other => panic!("expected ValidationError, got {:?}", other),
        }
    }

    #[test]
    fn from_daemon_rejects_old_commands_field() {
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/old",
            "canonical_ref": "surface:ryeos/studio/old",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/old.yaml" },
            "provenance": provenance_json("surface:ryeos/studio/old", []),
            "composed_value": {
                "name": "old",
                "tiles": ["view:ryeos/chain/timeline"],
                "commands": []
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let err = LoadedSurface::from_daemon("surface:ryeos/studio/old", response).unwrap_err();
        match err {
            SurfaceDiagnostic::ValidationError { message } => {
                assert!(message.contains("unknown field `commands`"));
            }
            other => panic!("expected ValidationError, got {:?}", other),
        }
    }

    #[test]
    fn from_daemon_invalid_provenance_fails_closed() {
        let response = serde_json::json!({
            "requested_ref": "surface:ryeos/studio/bad-provenance",
            "canonical_ref": "surface:ryeos/studio/bad-provenance",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/usr/lib/ryeos/.ai/surfaces/ryeos/studio/bad-provenance.yaml" },
            "provenance": ["old-string-list-is-invalid"],
            "composed_value": {
                "name": "bad-provenance",
                "tiles": ["view:ryeos/chain/timeline"],
                "affordances": []
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let err = LoadedSurface::from_daemon("surface:ryeos/studio/bad-provenance", response)
            .unwrap_err();
        match err {
            SurfaceDiagnostic::ValidationError { message } => {
                assert!(message.contains("invalid provenance"));
            }
            other => panic!("expected ValidationError, got {:?}", other),
        }
    }

    #[test]
    fn from_daemon_uses_engine_provenance() {
        let response = serde_json::json!({
            "requested_ref": "surface:my/custom",
            "canonical_ref": "surface:my/custom",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_project",
            "root_trust_class": "trusted_project",
            "source": { "path": "/home/user/.ai/surfaces/my/custom.yaml" },
            "provenance": provenance_json(
                "surface:my/custom",
                ["surface:ryeos/studio/base"]
            ),
            "composed_value": {
                "name": "custom",
                "tiles": ["view:ryeos/chain/timeline"],
                "affordances": []
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        });

        let loaded = LoadedSurface::from_daemon("surface:my/custom", response).unwrap();

        match &loaded {
            LoadedSurface::RyeResolved { provenance, .. } => {
                assert_eq!(provenance.root.resolved_ref, "surface:my/custom");
                assert_eq!(
                    provenance.ancestors[0].resolved_ref,
                    "surface:ryeos/studio/base"
                );
            }
            other => panic!(
                "expected RyeResolved, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }
}

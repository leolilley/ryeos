//! SurfaceSpec — declarative UI contract for the RyeOS Studio.
//!
//! A surface is a non-executable Rye item describing layout, views, commands,
//! and instruments. The TUI consumes **effective surfaces** — either:
//! - `BuiltinDefault`: internal safe fallback (no explicit request)
//! - `LocalPreview`: from `--surface-file`, explicitly untrusted, dev-only
//! - `RyeResolved`: from Rye item services via `--surface`, trusted and composed
//!
//! The TUI does NOT implement source-space precedence, trust verification,
//! kind-schema loading, signature verification, or extends-chain composition.
//! Those belong in ryeosd / item services.

use crate::ids::TileId;
use crate::layout::{LayoutTree, SplitAxis};
use crate::workspace::{InputBarState, TileState, ViewLocalState, ViewSpec, Workspace};
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
    pub layout: SurfaceLayoutSpec,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    /// Resolved `view:` bindings embedded at session/load time, keyed by
    /// ref (views-as-content; populated by the resolver, never authored
    /// inline — surfaces reference views, they do not define them).
    #[serde(default)]
    pub views: Option<serde_json::Value>,
    /// Optional home-panel view ref (content for the home dimension's
    /// brand/idle state; absent = no panel, renderers degrade).
    #[serde(default)]
    pub home_view: Option<String>,
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
// Layout
// ---------------------------------------------------------------------------

/// Root layout spec — names a root node and defines a map of layout nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceLayoutSpec {
    pub root: String,
    pub nodes: std::collections::HashMap<String, LayoutNodeSpec>,
}

/// A single layout node — either a split or a pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNodeSpec {
    Split {
        axis: SplitAxis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        first: String,
        second: String,
    },
    Pane {
        view: ViewKindSpec,
    },
}

fn default_ratio() -> f32 {
    0.5
}

/// Supported view kinds for surface panes. `Bound` carries a `view:` item
/// ref — views-as-content; the remaining named kinds are sanctioned
/// structural views.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewKindSpec {
    Atlas,
    Graph,
    /// A `view:` item reference (views-as-content).
    Bound(String),
}

const VIEW_KIND_NAMES: &[(&str, fn() -> ViewKindSpec)] = &[
    ("atlas", || ViewKindSpec::Atlas),
    ("graph", || ViewKindSpec::Graph),
];

impl Serialize for ViewKindSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let ViewKindSpec::Bound(view_ref) = self {
            return serializer.serialize_str(view_ref);
        }
        for (name, make) in VIEW_KIND_NAMES {
            if make() == *self {
                return serializer.serialize_str(name);
            }
        }
        unreachable!("every non-Bound view kind has a name")
    }
}

impl<'de> Deserialize<'de> for ViewKindSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        for (name, make) in VIEW_KIND_NAMES {
            if *name == raw {
                return Ok(make());
            }
        }
        if raw.starts_with("view:") {
            return Ok(ViewKindSpec::Bound(raw));
        }
        Err(serde::de::Error::custom(format!(
            "unknown pane view `{raw}` (named kind or view: ref expected)"
        )))
    }
}

impl ViewKindSpec {
    /// Convert to a ViewSpec with no ID binding.
    pub fn to_view_spec(&self) -> ViewSpec {
        match self {
            ViewKindSpec::Atlas => ViewSpec::Atlas,
            ViewKindSpec::Graph => ViewSpec::Graph { graph_id: None },
            ViewKindSpec::Bound(view_ref) => ViewSpec::Bound {
                view_ref: view_ref.clone(),
            },
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

        Ok(LoadedSurface::RyeResolved {
            requested_ref: requested_ref.to_string(),
            spec,
            trusted,
            provenance,
            item_diagnostics: Vec::new(),
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

/// The built-in default Studio surface — mission-control layout.
///
/// Data-equivalent to `surface:ryeos/studio/base`.
/// Three-pane: thread list (left 25%) | chain timeline (right-top 85%) + status (right-bottom).
pub fn builtin_default() -> SurfaceSpec {
    let mut nodes = std::collections::HashMap::new();

    nodes.insert(
        "main".into(),
        LayoutNodeSpec::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.25,
            first: "thread_list".into(),
            second: "right".into(),
        },
    );
    nodes.insert(
        "thread_list".into(),
        LayoutNodeSpec::Pane {
            view: ViewKindSpec::Bound("view:ryeos/threads/list".into()),
        },
    );
    nodes.insert(
        "right".into(),
        LayoutNodeSpec::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.85,
            first: "thread".into(),
            second: "status".into(),
        },
    );
    nodes.insert(
        "thread".into(),
        LayoutNodeSpec::Pane {
            view: ViewKindSpec::Bound("view:ryeos/chain/timeline".into()),
        },
    );
    nodes.insert(
        "status".into(),
        LayoutNodeSpec::Pane {
            view: ViewKindSpec::Bound("view:ryeos/node/status".into()),
        },
    );

    SurfaceSpec {
        name: "studio-base".into(),
        version: "1.0.0".into(),
        extends: None,
        description: Some("Default RyeOS Studio — three-pane mission control".into()),
        layout: SurfaceLayoutSpec {
            root: "main".into(),
            nodes,
        },
        input: None,
        views: None,
        home_view: None,
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

    let (spec, parse_diagnostics) = match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => match toml::from_str::<SurfaceSpec>(&content) {
            Ok(s) => (s, Vec::new()),
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
            Ok(s) => (s, Vec::new()),
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

    // Run TUI semantic validation
    let mut diagnostics = parse_diagnostics;
    let validation_warnings = validate_surface(&spec);
    for w in validation_warnings {
        diagnostics.push(SurfaceDiagnostic::ValidationError { message: w });
    }

    // Warn about unsupported fields
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
                       layout must be fully specified"
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
// Validation
// ---------------------------------------------------------------------------

/// Validate a surface spec for TUI renderability.
///
/// Returns diagnostics. Empty means valid.
/// Invalid specs should still produce a usable workspace (with fallbacks),
/// but the diagnostics must be surfaced to the operator.
pub fn validate_surface(spec: &SurfaceSpec) -> Vec<String> {
    let mut warnings = Vec::new();

    // Root must exist in nodes
    if !spec.layout.nodes.contains_key(&spec.layout.root) {
        warnings.push(format!(
            "layout root '{}' not found in nodes",
            spec.layout.root
        ));
    }

    // Check all node references exist
    for (name, node) in &spec.layout.nodes {
        match node {
            LayoutNodeSpec::Split { first, second, .. } => {
                if !spec.layout.nodes.contains_key(first) {
                    warnings.push(format!(
                        "node '{}' references missing child '{}'",
                        name, first
                    ));
                }
                if !spec.layout.nodes.contains_key(second) {
                    warnings.push(format!(
                        "node '{}' references missing child '{}'",
                        name, second
                    ));
                }
            }
            LayoutNodeSpec::Pane { .. } => {}
        }
    }

    // Check for cycles
    if let Some(cycle) = detect_cycle(&spec.layout) {
        warnings.push(format!("layout contains a cycle: {}", cycle));
    }

    warnings
}

fn detect_cycle(layout: &SurfaceLayoutSpec) -> Option<String> {
    let mut visited = std::collections::HashSet::new();
    let mut path = Vec::new();
    dfs_cycle(layout, &layout.root, &mut visited, &mut path)
}

fn dfs_cycle(
    layout: &SurfaceLayoutSpec,
    node_name: &str,
    visited: &mut std::collections::HashSet<String>,
    path: &mut Vec<String>,
) -> Option<String> {
    if path.contains(&node_name.to_string()) {
        let cycle_start = path.iter().position(|n| n == node_name).unwrap();
        let cycle: Vec<&str> = path[cycle_start..].iter().map(|s| s.as_str()).collect();
        return Some(cycle.join(" -> "));
    }
    if visited.contains(node_name) {
        return None;
    }

    visited.insert(node_name.to_string());
    path.push(node_name.to_string());

    if let Some(node) = layout.nodes.get(node_name) {
        match node {
            LayoutNodeSpec::Split { first, second, .. } => {
                if let Some(cycle) = dfs_cycle(layout, first, visited, path) {
                    return Some(cycle);
                }
                if let Some(cycle) = dfs_cycle(layout, second, visited, path) {
                    return Some(cycle);
                }
            }
            LayoutNodeSpec::Pane { .. } => {}
        }
    }

    path.pop();
    None
}

// ---------------------------------------------------------------------------
// SurfaceSpec → Workspace conversion
// ---------------------------------------------------------------------------

impl SurfaceSpec {
    /// Convert this surface spec into a Workspace for rendering.
    ///
    /// Allocates fresh TileIds, builds the LayoutTree from the node map,
    /// and initializes view-local state based on view kind.
    /// Falls back to `Workspace::default_three_pane()` on structural error.
    pub fn to_workspace(&self) -> Workspace {
        match self.try_to_workspace() {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!(
                    "warn: surface->workspace conversion failed ({}), using default",
                    e
                );
                Workspace::default_three_pane()
            }
        }
    }

    fn try_to_workspace(&self) -> Result<Workspace, String> {
        let mut tile_id_counter = 0u64;
        let mut tile_map: std::collections::HashMap<String, TileId> =
            std::collections::HashMap::new();

        // First pass: allocate TileIds for all pane nodes
        for (name, node) in &self.layout.nodes {
            if matches!(node, LayoutNodeSpec::Pane { .. }) {
                tile_id_counter += 1;
                tile_map.insert(name.clone(), TileId::new(tile_id_counter));
            }
        }

        // Second pass: build LayoutTree recursively
        let layout = self.build_layout_node(&self.layout.root, &self.layout, &tile_map)?;

        // Third pass: build tile states with proper initial local state
        let mut tiles = std::collections::HashMap::new();
        for (name, node) in &self.layout.nodes {
            if let LayoutNodeSpec::Pane { view } = node {
                let tile_id = tile_map[name];
                tiles.insert(
                    tile_id,
                    TileState {
                        view: view.to_view_spec(),
                        local: view.initial_local_state(),
                    },
                );
            }
        }

        // Focused tile: first pane tile in tree traversal order
        let focused_tile = layout
            .tile_ids()
            .into_iter()
            .next()
            .ok_or_else(|| "layout tree has no tiles".to_string())?;

        Ok(Workspace {
            layout,
            tiles,
            focused_tile,
            input_bar: InputBarState::default(),
            master_tiles: vec![focused_tile],
        })
    }

    fn build_layout_node(
        &self,
        name: &str,
        layout: &SurfaceLayoutSpec,
        tile_map: &std::collections::HashMap<String, TileId>,
    ) -> Result<LayoutTree, String> {
        let node = layout
            .nodes
            .get(name)
            .ok_or_else(|| format!("layout node '{}' not found in nodes map", name))?;

        match node {
            LayoutNodeSpec::Pane { .. } => {
                let id = tile_map
                    .get(name)
                    .ok_or_else(|| format!("pane '{}' has no allocated TileId", name))?;
                Ok(LayoutTree::Leaf(*id))
            }
            LayoutNodeSpec::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first_tree = self.build_layout_node(first, layout, tile_map)?;
                let second_tree = self.build_layout_node(second, layout, tile_map)?;
                Ok(LayoutTree::Split {
                    axis: *axis,
                    ratio: ratio.clamp(0.1, 0.9),
                    first: Box::new(first_tree),
                    second: Box::new(second_tree),
                })
            }
        }
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
    fn builtin_default_is_valid() {
        let spec = builtin_default();
        assert_eq!(spec.name, "studio-base");
        let warnings = validate_surface(&spec);
        assert!(
            warnings.is_empty(),
            "builtin should be valid: {:?}",
            warnings
        );
    }

    #[test]
    fn builtin_default_produces_workspace() {
        let spec = builtin_default();
        let ws = spec.to_workspace();
        assert_eq!(ws.tiles.len(), 3, "should have 3 tiles");
        assert!(!ws.layout.tile_ids().is_empty());
    }

    #[test]
    fn builtin_default_matches_three_pane() {
        let spec = builtin_default();
        let ws = spec.to_workspace();
        let default_ws = Workspace::default_three_pane();
        assert_eq!(ws.tiles.len(), default_ws.tiles.len());
    }

    #[test]
    fn workspace_initializes_correct_local_state() {
        let spec = builtin_default();
        let ws = spec.to_workspace();
        // Bound list tile should carry generic list local state
        let thread_list_tile = ws.tiles.values().find(|t| {
            matches!(&t.view, ViewSpec::Bound { view_ref } if view_ref == "view:ryeos/threads/list")
        });
        assert!(thread_list_tile.is_some());
        assert!(matches!(
            thread_list_tile.unwrap().local,
            ViewLocalState::GenericList { .. }
        ));
    }

    #[test]
    fn detect_missing_root() {
        let spec = SurfaceSpec {
            name: "bad".into(),
            version: "1.0.0".into(),
            extends: None,
            description: None,
            layout: SurfaceLayoutSpec {
                root: "nonexistent".into(),
                nodes: std::collections::HashMap::new(),
            },
            input: None,
            views: None,
            home_view: None,
            library: Vec::new(),
            ambient: None,
            affordances: Vec::new(),
            instruments: Vec::new(),
            capabilities: None,
        };
        let warnings = validate_surface(&spec);
        assert!(warnings.iter().any(|w| w.contains("not found")));
    }

    #[test]
    fn detect_cycle() {
        let mut nodes = std::collections::HashMap::new();
        nodes.insert(
            "a".into(),
            LayoutNodeSpec::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                first: "b".into(),
                second: "a".into(),
            },
        );
        nodes.insert(
            "b".into(),
            LayoutNodeSpec::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                first: "a".into(),
                second: "a".into(),
            },
        );
        let spec = SurfaceSpec {
            name: "cyclic".into(),
            version: "1.0.0".into(),
            extends: None,
            description: None,
            layout: SurfaceLayoutSpec {
                root: "a".into(),
                nodes,
            },
            input: None,
            views: None,
            home_view: None,
            library: Vec::new(),
            ambient: None,
            affordances: Vec::new(),
            instruments: Vec::new(),
            capabilities: None,
        };
        let warnings = validate_surface(&spec);
        assert!(
            warnings.iter().any(|w| w.contains("cycle")),
            "should detect cycle: {:?}",
            warnings
        );
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

[layout]
root = "main"

[layout.nodes.main]
type = "pane"
view = "view:ryeos/threads/list"
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
        assert!(matches!(
            ViewKindSpec::Bound("view:test/x".into()).initial_local_state(),
            ViewLocalState::GenericList { .. }
        ));
        assert!(matches!(
            ViewKindSpec::Atlas.initial_local_state(),
            ViewLocalState::None
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
        assert!(matches!(
            ViewKindSpec::Bound("view:test/x".into()).to_view_spec(),
            ViewSpec::Bound { .. }
        ));
        assert!(matches!(
            ViewKindSpec::Graph.to_view_spec(),
            ViewSpec::Graph { .. }
        ));
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
        assert!(spec.layout.nodes.contains_key("home"));
        let warnings = validate_surface(&spec);
        assert!(
            warnings.is_empty(),
            "bundled base should be valid: {:?}",
            warnings
        );
        let ws = spec.to_workspace();
        assert_eq!(ws.tiles.len(), 1);
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
        for kept in ["atlas", "graph", "view:ryeos/threads/list"] {
            let parsed: Result<ViewKindSpec, _> = serde_yaml::from_str(&format!("\"{kept}\""));
            assert!(parsed.is_ok(), "pane name `{kept}` must parse");
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
        // Views-as-content: the side pane references a view item.
        let bound = spec.layout.nodes.values().any(|node| {
            matches!(node, LayoutNodeSpec::Pane { view: ViewKindSpec::Bound(view_ref) }
                if view_ref == "view:ryeos/threads/list")
        });
        assert!(bound, "workbench must bind the threads view by ref");
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
        assert!(validate_surface(&spec).is_empty());
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
layout:
  root: main
  nodes:
    main:
      type: pane
      view: "view:ryeos/threads/list"
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
                assert_eq!(spec.layout.root, "main");
                // Should not have TOML fallback diagnostic
                assert!(!diagnostics.iter().any(|d| matches!(
                    d,
                    SurfaceDiagnostic::Info { message } if message.contains("TOML")
                )));
            }
            _ => panic!("expected LocalPreview"),
        }
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

[layout]
root = "main"

[layout.nodes.main]
type = "pane"
view = "view:ryeos/threads/list"

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
                "layout": {
                    "root": "sidebar",
                    "nodes": {
                        "sidebar": { "type": "pane", "view": "view:ryeos/chain/timeline" }
                    }
                },
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
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "graph" }
                    }
                },
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
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "view:ryeos/chain/timeline" }
                    }
                },
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
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "view:ryeos/chain/timeline" }
                    }
                },
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
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "view:ryeos/chain/timeline" }
                    }
                },
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

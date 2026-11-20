//! Server-side view embedding for effective surfaces.
//!
//! A surface binds views by `view:` ref; it never defines them. Every
//! daemon path that serves an effective surface (`items.effective`,
//! `ui.session.current`) embeds the composed value of each bound view
//! into the surface's `views` map before the payload leaves the daemon,
//! so renderers receive one complete surface instead of resolving each
//! ref in a follow-up round-trip.
//!
//! A view that fails to resolve embeds `{"degraded": <reason>}` under
//! its ref — the surface still ships, and the pane renders the reason
//! instead of the view. Per-view failures never fail the whole surface.
//!
//! A surface also names its view sets by `view_set:` ref. The same pass
//! resolves each one and replaces the ref list with the resolved
//! compositions, in the authored order, before the views they arrange are
//! collected. A set that cannot be used (it does not resolve, is not signed
//! by a trusted signer, fails admission, or repeats an earlier set's id) is
//! left out with a typed [`ViewSetRefusal`]; the surface still ships.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::SubjectResolutionAuthority;
use ryeos_engine::engine::{CheckedEngineGeneration, EffectiveItem, EffectiveItemRequest, Engine};
use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

/// Engine-owned effective identity retained beside one UI item's composed
/// presentation. The digest commits the complete effective resolution; this
/// API never derives identity from the presentation JSON it embeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveUiItemIdentity {
    pub canonical_ref: String,
    pub effective_definition_digest: EffectiveDefinitionDigest,
    /// Effective weakest-link trust result computed by the engine. Binding
    /// admission consumes this explicitly; it must not infer trust from the
    /// definition digest or the presentation payload.
    pub effective_trust_class: TrustClass,
}

impl EffectiveUiItemIdentity {
    /// Whether the item's whole extends chain verified under a signer the
    /// node trusts for it: bundle items and project items signed by a signer
    /// pinned for the project. Everything else is unsigned or untrusted.
    pub fn is_trusted(&self) -> bool {
        matches!(
            self.effective_trust_class,
            TrustClass::TrustedBundle | TrustClass::TrustedProject
        )
    }

    fn from_effective_item(item: &EffectiveItem) -> Self {
        Self {
            canonical_ref: item.canonical_ref.clone(),
            effective_definition_digest: item.effective_definition_digest.clone(),
            effective_trust_class: item.trust_class,
        }
    }
}

/// Key under which a composed surface or view carries the engine's authorship
/// fact for an item with a project-space member.
pub const AUTHORSHIP_KEY: &str = "authorship";

/// The authorship fact for an effective item, or `None` when every member of
/// its extends chain is a verified bundle item. It names the space, the
/// engine's weakest-link trust class, the item's ref and, when the first
/// project-space member is signed, that signer's fingerprint with the label
/// the node's trust registry records for it.
fn authorship_fact(
    item: &EffectiveItem,
    signer_label: impl Fn(&str) -> Option<String>,
) -> Option<Value> {
    if item.trust_class == TrustClass::TrustedBundle {
        return None;
    }
    let mut fact = serde_json::json!({
        "space": "project",
        "trust_class": item.trust_class,
        "ref": item.canonical_ref,
    });
    let signer = std::iter::once(&item.provenance.root)
        .chain(item.provenance.ancestors.iter())
        .find(|member| member.source_space == ryeos_engine::contracts::ItemSpace::Project)
        .and_then(|member| member.signer_fingerprint.clone());
    if let Some(fingerprint) = signer {
        let mut signer = serde_json::json!({ "fingerprint": fingerprint });
        if let Some(name) = signer_label(&fingerprint) {
            signer["name"] = Value::String(name);
        }
        fact["signer"] = signer;
    }
    Some(fact)
}

/// Write the engine's authorship fact over whatever the item authored under
/// that key, so an item file can neither claim an author nor hide one.
fn stamp_authorship(item: &mut EffectiveItem, signer_label: impl Fn(&str) -> Option<String>) {
    let fact = authorship_fact(item, signer_label);
    let Some(fields) = item.composed_value.as_object_mut() else {
        return;
    };
    match fact {
        Some(fact) => {
            fields.insert(AUTHORSHIP_KEY.to_string(), fact);
        }
        None => {
            fields.remove(AUTHORSHIP_KEY);
        }
    }
}

/// Resolution state for one view named by the surface. A failed view remains
/// in both the presentation as a degraded placeholder and this sidecar as a
/// degraded identity record; failure never fabricates definition identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddedViewIdentity {
    Resolved { identity: EffectiveUiItemIdentity },
    Degraded { reason: String },
}

/// Exact surface/view identity closure retained beside the unchanged composed
/// surface presentation. This is input to later binding compilation, not a
/// browser-authored provenance format or an independent source of authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedSurfaceIdentity {
    pub request_engine_generation_identity: String,
    pub surface: EffectiveUiItemIdentity,
    pub views: BTreeMap<String, EmbeddedViewIdentity>,
    /// One entry per `view_set:` ref the surface names, keyed by the ref. A
    /// set embedded in the surface is resolved; a set that was refused is a
    /// degraded entry carrying its typed refusal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub view_sets: BTreeMap<String, EmbeddedViewIdentity>,
}

/// Why a view set a surface names is not part of its effective presentation.
/// Every case leaves the rest of the surface intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewSetRefusal {
    /// The ref is not a canonical item ref.
    InvalidRef { reason: String },
    /// The ref names no `view_set` item the engine can resolve.
    Unresolved { reason: String },
    /// The item resolved but is unsigned or signed by an untrusted signer.
    Untrusted { trust_class: TrustClass },
    /// The item's body is not a valid view-set composition.
    Malformed { reason: String },
    /// Another set earlier in the surface's list already has this id.
    DuplicateId { id: String },
}

impl ViewSetRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRef { .. } => "view_set_invalid_ref",
            Self::Unresolved { .. } => "view_set_unresolved",
            Self::Untrusted { .. } => "view_set_untrusted",
            Self::Malformed { .. } => "view_set_malformed",
            Self::DuplicateId { .. } => "view_set_duplicate_id",
        }
    }
}

impl std::fmt::Display for ViewSetRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ", self.code())?;
        match self {
            Self::InvalidRef { reason }
            | Self::Unresolved { reason }
            | Self::Malformed { reason } => f.write_str(reason),
            Self::Untrusted { trust_class } => write!(
                f,
                "the view set is not signed by a trusted signer (trust class {trust_class:?})"
            ),
            Self::DuplicateId { id } => write!(
                f,
                "an earlier view set in this surface already has the id `{id}`"
            ),
        }
    }
}

impl std::error::Error for ViewSetRefusal {}

/// Admission of one resolved view-set body: the check a body must pass to be
/// embedded. The caller owns the composition grammar; an `Err` refuses that
/// one set as [`ViewSetRefusal::Malformed`].
pub type ViewSetAdmission<'a> = &'a dyn Fn(&Value) -> Result<(), String>;

/// Admission that checks only that a body is a mapping, for callers that
/// report an effective surface and do not compile a binding from it.
pub fn view_set_body_is_mapping(body: &Value) -> Result<(), String> {
    if body.is_object() {
        Ok(())
    } else {
        Err("a view set body must be a mapping".to_string())
    }
}

/// Result of one generation-coherent surface/view embedding operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedSurfaceViews {
    pub identity: EmbeddedSurfaceIdentity,
    pub failures: Vec<(String, String)>,
}

/// Explicit View closure resolved for a composition root, without rewriting
/// the root body. Values are keyed by the declared view ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedUiCompositionViews {
    pub identity: EmbeddedSurfaceIdentity,
    pub values: BTreeMap<String, Value>,
    pub failures: Vec<(String, String)>,
}

/// Collect every `view:`-prefixed ref anywhere in the composed surface
/// value — center `tiles`, edge `slots`, `backdrop`, `library` — skipping
/// the ROOT `views` map only (it holds resolved bindings keyed by ref,
/// not refs to resolve). The skip must not apply to nested `views` keys:
/// each grouped `library` entry is `{ group, views: [ref…] }`, and
/// skipping those lists is exactly how a grouped launcher loses every
/// declared view.
fn collect_view_refs(value: &Value, out: &mut Vec<String>) {
    collect_view_refs_at(value, out, true);
}

fn unique_view_refs(value: &Value) -> Vec<String> {
    let mut view_refs = Vec::new();
    collect_view_refs(value, &mut view_refs);
    view_refs.sort();
    view_refs.dedup();
    view_refs
}

fn collect_view_refs_at(value: &Value, out: &mut Vec<String>, at_root: bool) {
    match value {
        Value::String(s) if s.starts_with("view:") => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_view_refs_at(item, out, false);
            }
        }
        Value::Object(map) => {
            for (key, v) in map {
                if at_root && key == "views" {
                    continue;
                }
                collect_view_refs_at(v, out, false);
            }
        }
        _ => {}
    }
}

/// Embed every bound view into `composed_value.views`, resolving each
/// unique ref through `resolve`. A failed resolution embeds a
/// `{"degraded": <reason>}` entry under the same key. Returns the
/// failures as `(view_ref, reason)` pairs so callers can additionally
/// report them as diagnostics.
pub fn embed_views_with(
    composed_value: &mut Value,
    mut resolve: impl FnMut(&str) -> Result<Value, String>,
) -> Vec<(String, String)> {
    if !composed_value.is_object() {
        return Vec::new();
    }
    let view_refs = unique_view_refs(composed_value);
    if view_refs.is_empty() {
        return Vec::new();
    }

    let resolved = view_refs
        .into_iter()
        .map(|view_ref| {
            let result = resolve(&view_ref);
            (view_ref, result)
        })
        .collect();
    embed_view_results(composed_value, resolved)
}

fn embed_view_results(
    composed_value: &mut Value,
    resolved: Vec<(String, Result<Value, String>)>,
) -> Vec<(String, String)> {
    // `views` is never authored inline; anything non-object there is
    // malformed. Reset it so per-ref insertion below cannot panic.
    if composed_value.get("views").is_some_and(|v| !v.is_object()) {
        composed_value["views"] = Value::Object(serde_json::Map::new());
    }

    let mut failures: Vec<(String, String)> = Vec::new();
    for (view_ref, result) in resolved {
        match result {
            Ok(view_value) => {
                composed_value["views"][&view_ref] = view_value;
            }
            Err(reason) => {
                composed_value["views"][&view_ref] =
                    serde_json::json!({ "degraded": reason.clone() });
                failures.push((view_ref, reason));
            }
        }
    }
    failures
}

fn embed_identity_view_results(
    composed_value: &mut Value,
    request_engine_generation_identity: String,
    surface: EffectiveUiItemIdentity,
    resolved: Vec<(String, Result<(Value, EffectiveUiItemIdentity), String>)>,
) -> EmbeddedSurfaceViews {
    if composed_value.get("views").is_some_and(|v| !v.is_object()) {
        composed_value["views"] = Value::Object(serde_json::Map::new());
    }

    let mut views = BTreeMap::new();
    let mut failures = Vec::new();
    for (view_ref, result) in resolved {
        match result {
            Ok((view_value, identity)) => {
                composed_value["views"][&view_ref] = view_value;
                views.insert(view_ref, EmbeddedViewIdentity::Resolved { identity });
            }
            Err(reason) => {
                composed_value["views"][&view_ref] =
                    serde_json::json!({ "degraded": reason.clone() });
                views.insert(
                    view_ref.clone(),
                    EmbeddedViewIdentity::Degraded {
                        reason: reason.clone(),
                    },
                );
                failures.push((view_ref, reason));
            }
        }
    }

    EmbeddedSurfaceViews {
        identity: EmbeddedSurfaceIdentity {
            request_engine_generation_identity,
            surface,
            views,
            view_sets: BTreeMap::new(),
        },
        failures,
    }
}

/// Resolve each bound `view:` ref through the engine's effective-item
/// pipeline and embed the composed view values into the surface.
pub fn embed_surface_views(
    engine: &Engine,
    project_root: Option<&Path>,
    composed_value: &mut Value,
) -> Vec<(String, String)> {
    embed_views_with(composed_value, |view_ref| {
        let item_ref =
            CanonicalRef::parse(view_ref).map_err(|e| format!("invalid view ref: {e}"))?;
        engine
            .effective_item(EffectiveItemRequest {
                logical_project_root: None,
                item_ref,
                expected_kind: Some("view".to_string()),
                project_root: project_root.map(Path::to_path_buf),
                subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                    project_root,
                ),
            })
            .map(|mut effective| {
                stamp_authorship(&mut effective, |fingerprint| {
                    engine.signer_label(fingerprint)
                });
                effective.composed_value
            })
            .map_err(|e| e.to_string())
    })
}

/// The `view_sets` entries of a composed surface when every one is a ref
/// string, in authored order; `None` when the surface names no view sets or
/// the list holds a non-string (which the composition consumer refuses).
fn view_set_refs(composed_value: &Value) -> Option<Vec<String>> {
    let entries = composed_value.get("view_sets")?.as_array()?;
    entries
        .iter()
        .map(|entry| entry.as_str().map(str::to_owned))
        .collect()
}

type ResolvedViewSets = Vec<(String, Result<(Value, EffectiveUiItemIdentity), String>)>;

/// Replace a composed surface's `view_sets` ref list with the compositions
/// the refs resolve to, in the authored order, and return the identity of
/// every ref with the refusals of those left out. Run before views are
/// collected so the views a set arranges are embedded with the rest.
fn embed_view_set_results(
    composed_value: &mut Value,
    refs: &[String],
    resolved: ResolvedViewSets,
    admit: ViewSetAdmission<'_>,
) -> (
    BTreeMap<String, EmbeddedViewIdentity>,
    Vec<(String, String)>,
) {
    let by_ref: BTreeMap<String, Result<(Value, EffectiveUiItemIdentity), String>> =
        resolved.into_iter().collect();
    let mut identities = BTreeMap::new();
    let mut failures = Vec::new();
    let mut bodies = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for view_set_ref in refs {
        let outcome: Result<Value, ViewSetRefusal> = match by_ref.get(view_set_ref) {
            None => Err(ViewSetRefusal::Unresolved {
                reason: "the ref was not resolved".to_string(),
            }),
            Some(Err(reason)) if CanonicalRef::parse(view_set_ref).is_err() => {
                Err(ViewSetRefusal::InvalidRef {
                    reason: reason.clone(),
                })
            }
            Some(Err(reason)) => Err(ViewSetRefusal::Unresolved {
                reason: reason.clone(),
            }),
            Some(Ok((_, identity))) if !identity.is_trusted() => Err(ViewSetRefusal::Untrusted {
                trust_class: identity.effective_trust_class,
            }),
            Some(Ok((body, _))) => admit(body)
                .map_err(|reason| ViewSetRefusal::Malformed { reason })
                .and_then(|()| {
                    let id = body.get("id").and_then(Value::as_str).unwrap_or_default();
                    if seen_ids.insert(id.to_owned()) {
                        Ok(body.clone())
                    } else {
                        Err(ViewSetRefusal::DuplicateId { id: id.to_owned() })
                    }
                }),
        };
        let entry = match (by_ref.get(view_set_ref), &outcome) {
            (Some(Ok((_, identity))), Ok(_)) => EmbeddedViewIdentity::Resolved {
                identity: identity.clone(),
            },
            (_, Err(refusal)) => EmbeddedViewIdentity::Degraded {
                reason: refusal.to_string(),
            },
            // `outcome` is Ok only for a resolved set.
            (_, Ok(_)) => continue,
        };
        identities.insert(view_set_ref.clone(), entry);
        match outcome {
            Ok(body) => bodies.push(body),
            Err(refusal) => failures.push((view_set_ref.clone(), refusal.to_string())),
        }
    }
    composed_value["view_sets"] = Value::Array(bodies);
    (identities, failures)
}

/// Resolve and embed a surface's view sets inside a checked generation.
fn embed_view_sets_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    composed_value: &mut Value,
    admit: ViewSetAdmission<'_>,
) -> (
    BTreeMap<String, EmbeddedViewIdentity>,
    Vec<(String, String)>,
) {
    let Some(refs) = view_set_refs(composed_value) else {
        return (BTreeMap::new(), Vec::new());
    };
    let unique: Vec<String> = refs
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let resolved = resolve_identity_items_in_generation(
        generation,
        project_root,
        project_content,
        "view_set",
        unique,
    );
    embed_view_set_results(composed_value, &refs, resolved, admit)
}

/// Resolve a surface's bound views inside a caller-owned checked generation
/// batch. This avoids reacquiring and re-verifying the installed bundle
/// generation for every view while preserving the same resolution semantics.
pub fn embed_surface_views_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    composed_value: &mut Value,
    admit_view_set: ViewSetAdmission<'_>,
) -> Vec<(String, String)> {
    if !composed_value.is_object() {
        return Vec::new();
    }
    let (_, mut view_set_failures) = embed_view_sets_in_generation(
        generation,
        project_root,
        None,
        composed_value,
        admit_view_set,
    );
    let resolved = resolve_identity_views_in_generation(
        generation,
        project_root,
        None,
        unique_view_refs(composed_value),
    )
    .into_iter()
    .map(|(view_ref, result)| (view_ref, result.map(|(value, _identity)| value)))
    .collect();
    view_set_failures.extend(embed_view_results(composed_value, resolved));
    view_set_failures
}

/// Embed the views of an already-resolved effective surface while retaining
/// the engine-owned effective identity of the surface and every successfully
/// resolved view. The composed surface and each embedded view keep their
/// existing presentation shape; identity travels only through the typed
/// sidecar returned here.
///
/// The caller must resolve `effective_surface` inside the same checked
/// generation supplied here. Holding the generation across both operations is
/// what makes the returned identity closure coherent.
pub fn embed_effective_surface_views_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    effective_surface: &mut EffectiveItem,
    admit_view_set: ViewSetAdmission<'_>,
) -> EmbeddedSurfaceViews {
    embed_effective_surface_views(
        generation,
        project_root,
        None,
        effective_surface,
        admit_view_set,
    )
}

/// Descriptor-authority variant of
/// [`embed_effective_surface_views_in_generation`]. The typed content owner
/// remains borrowed across resolution of the complete surface/view closure.
pub fn embed_effective_surface_views_in_generation_under_project_authority(
    generation: &CheckedEngineGeneration<'_>,
    project_root: &Path,
    project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
    effective_surface: &mut EffectiveItem,
    admit_view_set: ViewSetAdmission<'_>,
) -> EmbeddedSurfaceViews {
    embed_effective_surface_views(
        generation,
        Some(project_root),
        Some(project_content),
        effective_surface,
        admit_view_set,
    )
}

fn embed_effective_surface_views(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    effective_surface: &mut EffectiveItem,
    admit_view_set: ViewSetAdmission<'_>,
) -> EmbeddedSurfaceViews {
    let surface = EffectiveUiItemIdentity::from_effective_item(effective_surface);
    stamp_authorship(effective_surface, |fingerprint| {
        generation.signer_label(fingerprint)
    });
    if !effective_surface.composed_value.is_object() {
        return EmbeddedSurfaceViews {
            identity: EmbeddedSurfaceIdentity {
                request_engine_generation_identity: generation
                    .request_engine_generation_identity()
                    .to_string(),
                surface,
                views: BTreeMap::new(),
                view_sets: BTreeMap::new(),
            },
            failures: Vec::new(),
        };
    }
    // View sets are embedded first: the views a set arranges are then
    // collected with every other view the surface names.
    let (view_sets, view_set_failures) = embed_view_sets_in_generation(
        generation,
        project_root,
        project_content,
        &mut effective_surface.composed_value,
        admit_view_set,
    );
    let resolved = resolve_identity_views_in_generation(
        generation,
        project_root,
        project_content,
        unique_view_refs(&effective_surface.composed_value),
    );
    let mut embedded = embed_identity_view_results(
        &mut effective_surface.composed_value,
        generation.request_engine_generation_identity().to_string(),
        surface,
        resolved,
    );
    embedded.identity.view_sets = view_sets;
    embedded.failures.splice(0..0, view_set_failures);
    embedded
}

/// Explicit View closure of a composition root whose own body is not rewritten
/// (a shared-presentation Config). The root's identity is read from its
/// effective canonical ref, which must name a Config or a Surface; the view
/// values come back keyed by ref beside the identity closure.
pub fn resolve_effective_ui_view_closure_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    effective_root: &EffectiveItem,
    view_refs: Vec<String>,
) -> Result<EmbeddedUiCompositionViews, String> {
    if project_content.is_some() && project_root.is_none() {
        return Err("UI composition content authority requires its project root".into());
    }
    let root = CanonicalRef::parse(&effective_root.canonical_ref)
        .map_err(|error| format!("invalid composition root ref: {error}"))?;
    if root.kind != "config" && root.kind != "surface" {
        return Err("UI composition root must be config or surface".into());
    }
    let mut view_refs = view_refs;
    view_refs.sort();
    view_refs.dedup();
    let resolved =
        resolve_identity_views_in_generation(generation, project_root, project_content, view_refs);
    let mut body = serde_json::json!({});
    let embedded = embed_identity_view_results(
        &mut body,
        generation.request_engine_generation_identity().to_string(),
        EffectiveUiItemIdentity::from_effective_item(effective_root),
        resolved,
    );
    let values = match body.get("views").and_then(Value::as_object) {
        Some(views) => views
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        None => BTreeMap::new(),
    };
    Ok(EmbeddedUiCompositionViews {
        identity: embedded.identity,
        values,
        failures: embedded.failures,
    })
}

/// The single engine-owned resolution mechanism for a set of `view:` refs.
/// Collection and schema validation belong to the composition consumer; this
/// operation grants no access. Retained project authority is borrowed across
/// the complete batch.
fn resolve_identity_views_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    view_refs: Vec<String>,
) -> Vec<(String, Result<(Value, EffectiveUiItemIdentity), String>)> {
    resolve_identity_items_in_generation(
        generation,
        project_root,
        project_content,
        "view",
        view_refs,
    )
}

/// Resolve a batch of refs that must each name an item of `kind`, with the
/// effective identity of every one that resolves. Results come back sorted
/// by ref.
fn resolve_identity_items_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    kind: &str,
    view_refs: Vec<String>,
) -> Vec<(String, Result<(Value, EffectiveUiItemIdentity), String>)> {
    let mut resolved = Vec::new();
    let mut requests = Vec::new();
    let mut valid_refs = Vec::new();
    for view_ref in view_refs {
        match CanonicalRef::parse(&view_ref) {
            Ok(item_ref) => {
                valid_refs.push(view_ref);
                requests.push(EffectiveItemRequest {
                    logical_project_root: None,
                    item_ref,
                    expected_kind: Some(kind.to_string()),
                    project_root: project_root.map(Path::to_path_buf),
                    subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                        project_root,
                    ),
                });
            }
            Err(error) => resolved.push((view_ref, Err(format!("invalid {kind} ref: {error}")))),
        }
    }
    let effective_items = match project_content {
        Some(authority) => generation.effective_items_under_project_authority(&requests, authority),
        None => generation.effective_items(&requests),
    };
    resolved.extend(
        valid_refs
            .into_iter()
            .zip(effective_items)
            .map(|(view_ref, result)| {
                (
                    view_ref,
                    result
                        .map(|mut effective| {
                            let identity = EffectiveUiItemIdentity::from_effective_item(&effective);
                            stamp_authorship(&mut effective, |fingerprint| {
                                generation.signer_label(fingerprint)
                            });
                            (effective.composed_value, identity)
                        })
                        .map_err(|error| error.to_string()),
                )
            }),
    );
    resolved.sort_by(|(left, _), (right, _)| left.cmp(right));
    resolved
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

    use super::{
        EffectiveUiItemIdentity, EmbeddedSurfaceIdentity, EmbeddedViewIdentity, ViewSetRefusal,
        embed_identity_view_results, embed_view_set_results, embed_views_with,
        view_set_body_is_mapping,
    };

    fn identity(
        canonical_ref: &str,
        digest_byte: char,
        effective_trust_class: TrustClass,
    ) -> EffectiveUiItemIdentity {
        EffectiveUiItemIdentity {
            canonical_ref: canonical_ref.to_string(),
            effective_definition_digest: EffectiveDefinitionDigest::parse(
                digest_byte.to_string().repeat(64),
            )
            .unwrap(),
            effective_trust_class,
        }
    }

    fn surface_binding_three_views() -> serde_json::Value {
        json!({
            "name": "test",
            "tiles": ["view:ryeos/chain/timeline"],
            "slots": {
                "bottom": { "content": "view:ryeos/input", "open": true }
            },
            "library": [
                { "group": "Threads", "views": ["view:ryeos/threads/list", "view:ryeos/chain/timeline"] }
            ]
        })
    }

    #[test]
    fn embeds_every_bound_view_once() {
        // Three unique refs across tiles/slots/library (one duplicated) →
        // three embedded bindings, each resolved exactly once.
        let mut surface = surface_binding_three_views();
        let mut resolved: Vec<String> = Vec::new();
        let failures = embed_views_with(&mut surface, |view_ref| {
            resolved.push(view_ref.to_string());
            Ok(json!({ "widget": "rows", "resolved_from": view_ref }))
        });

        assert!(failures.is_empty());
        assert_eq!(resolved.len(), 3, "duplicate refs resolve once");
        let views = surface["views"].as_object().unwrap();
        assert_eq!(views.len(), 3);
        for view_ref in [
            "view:ryeos/chain/timeline",
            "view:ryeos/input",
            "view:ryeos/threads/list",
        ] {
            assert_eq!(views[view_ref]["resolved_from"], view_ref);
        }
    }

    #[test]
    fn failed_ref_embeds_degraded_entry_others_still_embed() {
        // One failing ref records a per-view error and embeds a degraded
        // placeholder; the rest of the surface's views embed normally —
        // a single bad view never fails the whole surface.
        let mut surface = surface_binding_three_views();
        let failures = embed_views_with(&mut surface, |view_ref| {
            if view_ref == "view:ryeos/input" {
                Err("item not found".to_string())
            } else {
                Ok(json!({ "widget": "rows" }))
            }
        });

        assert_eq!(
            failures,
            vec![("view:ryeos/input".to_string(), "item not found".to_string())]
        );
        let views = surface["views"].as_object().unwrap();
        assert_eq!(views.len(), 3, "the failed ref is still keyed");
        assert_eq!(views["view:ryeos/input"]["degraded"], "item not found");
        assert_eq!(views["view:ryeos/threads/list"]["widget"], "rows");
        assert_eq!(views["view:ryeos/chain/timeline"]["widget"], "rows");
    }

    #[test]
    fn already_embedded_views_map_is_not_rewalked() {
        // Refs inside an existing `views` map are resolved bindings keyed
        // by ref, not refs to resolve — the walker must skip them.
        let mut surface = json!({
            "name": "test",
            "tiles": ["view:a/b"],
            "views": { "view:c/d": { "widget": "rows" } }
        });
        let mut resolved: Vec<String> = Vec::new();
        embed_views_with(&mut surface, |view_ref| {
            resolved.push(view_ref.to_string());
            Ok(json!({ "widget": "text" }))
        });
        assert_eq!(resolved, vec!["view:a/b"]);
        // The pre-existing entry survives alongside the new embedding.
        assert_eq!(surface["views"]["view:c/d"]["widget"], "rows");
        assert_eq!(surface["views"]["view:a/b"]["widget"], "text");
    }

    #[test]
    fn malformed_non_object_views_resets_before_embedding() {
        // `views` is never authored inline; a malformed non-object there
        // must not panic the embedder — it resets to a map and embeds.
        let mut surface = json!({ "name": "x", "tiles": ["view:a/b"], "views": "bogus" });
        let failures = embed_views_with(&mut surface, |_| Ok(json!({ "widget": "rows" })));
        assert!(failures.is_empty());
        assert_eq!(surface["views"]["view:a/b"]["widget"], "rows");
    }

    #[test]
    fn surface_without_view_refs_gains_no_views_map() {
        let mut surface = json!({ "name": "empty" });
        let failures = embed_views_with(&mut surface, |_| unreachable!("no refs to resolve"));
        assert!(failures.is_empty());
        assert!(surface.get("views").is_none());
    }

    #[test]
    fn non_object_composed_value_is_untouched() {
        let mut value = json!("view:not/an/object");
        let failures = embed_views_with(&mut value, |_| unreachable!("nothing to embed into"));
        assert!(failures.is_empty());
        assert_eq!(value, json!("view:not/an/object"));
    }

    #[test]
    fn identity_sidecar_retains_surface_and_resolved_view_identity() {
        let mut surface = json!({ "tiles": ["view:test/one"] });
        let surface_identity = identity("surface:test/base", 'a', TrustClass::TrustedBundle);
        let view_identity = identity("view:test/one", 'b', TrustClass::UntrustedProject);

        let embedded = embed_identity_view_results(
            &mut surface,
            "engine-generation-a".to_string(),
            surface_identity.clone(),
            vec![(
                "view:test/one".to_string(),
                Ok((json!({ "widget": "rows" }), view_identity.clone())),
            )],
        );

        assert!(embedded.failures.is_empty());
        assert_eq!(surface["views"]["view:test/one"]["widget"], "rows");
        assert_eq!(
            view_identity.effective_trust_class,
            TrustClass::UntrustedProject,
            "the binding compiler needs the engine's explicit effective trust result"
        );
        assert_eq!(
            embedded.identity,
            EmbeddedSurfaceIdentity {
                request_engine_generation_identity: "engine-generation-a".to_string(),
                surface: surface_identity,
                views: BTreeMap::from([(
                    "view:test/one".to_string(),
                    EmbeddedViewIdentity::Resolved {
                        identity: view_identity,
                    },
                )]),
                view_sets: BTreeMap::new(),
            }
        );
        assert!(
            surface.get("effective_definition_digest").is_none(),
            "identity must stay out of authored presentation JSON"
        );
    }

    #[test]
    fn identity_sidecar_records_degraded_view_without_fabricating_identity() {
        let mut surface = json!({ "tiles": ["view:test/good", "view:test/bad"] });
        let good_identity = identity("view:test/good", 'b', TrustClass::TrustedProject);

        let embedded = embed_identity_view_results(
            &mut surface,
            "engine-generation-a".to_string(),
            identity("surface:test/base", 'a', TrustClass::TrustedBundle),
            vec![
                (
                    "view:test/bad".to_string(),
                    Err("item not found".to_string()),
                ),
                (
                    "view:test/good".to_string(),
                    Ok((json!({ "widget": "text" }), good_identity.clone())),
                ),
            ],
        );

        assert_eq!(
            embedded.failures,
            vec![("view:test/bad".to_string(), "item not found".to_string())]
        );
        assert_eq!(
            surface["views"]["view:test/bad"]["degraded"],
            "item not found"
        );
        assert_eq!(surface["views"]["view:test/good"]["widget"], "text");
        assert_eq!(
            embedded.identity.views["view:test/bad"],
            EmbeddedViewIdentity::Degraded {
                reason: "item not found".to_string(),
            }
        );
        assert_eq!(
            embedded.identity.views["view:test/good"],
            EmbeddedViewIdentity::Resolved {
                identity: good_identity,
            }
        );
    }

    fn set_body(id: &str) -> serde_json::Value {
        json!({"schema": 2, "id": id, "title": id})
    }

    fn embed_sets(
        refs: &[&str],
        resolved: Vec<(
            &str,
            Result<(serde_json::Value, EffectiveUiItemIdentity), String>,
        )>,
        admit: super::ViewSetAdmission<'_>,
    ) -> (
        serde_json::Value,
        BTreeMap<String, EmbeddedViewIdentity>,
        Vec<(String, String)>,
    ) {
        let mut surface = json!({"view_sets": refs});
        let refs: Vec<String> = refs.iter().map(|r| r.to_string()).collect();
        let resolved = resolved
            .into_iter()
            .map(|(r, result)| (r.to_string(), result))
            .collect();
        let (identities, failures) = embed_view_set_results(&mut surface, &refs, resolved, admit);
        (surface, identities, failures)
    }

    #[test]
    fn view_set_refs_become_their_compositions_in_authored_order() {
        let (surface, identities, failures) = embed_sets(
            &["view_set:t/b", "view_set:t/a"],
            vec![
                (
                    "view_set:t/a",
                    Ok((
                        set_body("a"),
                        identity("view_set:t/a", 'a', TrustClass::TrustedBundle),
                    )),
                ),
                (
                    "view_set:t/b",
                    Ok((
                        set_body("b"),
                        identity("view_set:t/b", 'b', TrustClass::TrustedProject),
                    )),
                ),
            ],
            &view_set_body_is_mapping,
        );
        assert!(failures.is_empty());
        assert_eq!(surface["view_sets"], json!([set_body("b"), set_body("a")]));
        assert_eq!(identities.len(), 2);
        assert!(matches!(
            identities["view_set:t/b"],
            EmbeddedViewIdentity::Resolved { .. }
        ));
    }

    #[test]
    fn a_refused_view_set_is_left_out_with_a_typed_reason_and_the_rest_ship() {
        let (surface, identities, failures) = embed_sets(
            &[
                "view_set:t/missing",
                "view_set:t/unsigned",
                "view_set:t/bad",
                "view_set:t/good",
                "view_set:t/twin",
                "not a ref",
            ],
            vec![
                ("view_set:t/missing", Err("item not found".to_string())),
                (
                    "view_set:t/unsigned",
                    Ok((
                        set_body("unsigned"),
                        identity("view_set:t/unsigned", 'c', TrustClass::UntrustedProject),
                    )),
                ),
                (
                    "view_set:t/bad",
                    Ok((
                        json!("not a mapping"),
                        identity("view_set:t/bad", 'd', TrustClass::TrustedBundle),
                    )),
                ),
                (
                    "view_set:t/good",
                    Ok((
                        set_body("good"),
                        identity("view_set:t/good", 'e', TrustClass::TrustedBundle),
                    )),
                ),
                (
                    "view_set:t/twin",
                    Ok((
                        set_body("good"),
                        identity("view_set:t/twin", 'f', TrustClass::TrustedBundle),
                    )),
                ),
                ("not a ref", Err("invalid view_set ref: x".to_string())),
            ],
            &view_set_body_is_mapping,
        );
        assert_eq!(surface["view_sets"], json!([set_body("good")]));
        let reasons: BTreeMap<_, _> = failures.into_iter().collect();
        assert!(reasons["view_set:t/missing"].starts_with("view_set_unresolved: "));
        assert!(reasons["view_set:t/unsigned"].starts_with("view_set_untrusted: "));
        assert!(reasons["view_set:t/bad"].starts_with("view_set_malformed: "));
        assert!(reasons["view_set:t/twin"].starts_with("view_set_duplicate_id: "));
        assert!(reasons["not a ref"].starts_with("view_set_invalid_ref: "));
        assert_eq!(reasons.len(), 5);
        assert!(matches!(
            identities["view_set:t/missing"],
            EmbeddedViewIdentity::Degraded { ref reason } if reason.starts_with("view_set_unresolved")
        ));
        assert!(matches!(
            identities["view_set:t/unsigned"],
            EmbeddedViewIdentity::Degraded { ref reason } if reason.starts_with("view_set_untrusted")
        ));
        assert!(matches!(
            identities["view_set:t/good"],
            EmbeddedViewIdentity::Resolved { .. }
        ));
        assert_eq!(
            ViewSetRefusal::Untrusted {
                trust_class: TrustClass::Unsigned
            }
            .code(),
            "view_set_untrusted"
        );
    }
}

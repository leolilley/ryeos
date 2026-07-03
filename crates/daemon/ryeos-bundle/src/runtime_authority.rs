//! Manifest runtime-authority policy.
//!
//! "Runtime authority" is the set of daemon callback capabilities a **signed
//! bundle manifest** may declare for the bundle's own running code — today
//! bundle events, runtime vault, and daemon-authored project item writes (see
//! `ryeos/future/tool-runtime-authority`). Per that contract, this authority is
//! *always minted by the daemon* from the signed manifest and is **never**
//! grantable through a composed `permissions:` block.
//!
//! This module is the single source of truth for that vocabulary:
//!
//! - the cap `kind` segments (`bundle-events`, `vault`, and ordinary item kinds),
//! - the typed cap constructors the manifest declarations and the daemon
//!   callback services both use to build/require caps, and
//! - the classifier that rejects a user-composed grant which would overlap the
//!   manifest runtime-authority namespace (including wildcard overlaps).
//!
//! Keeping minting, service authorization, and rejection on one definition is
//! the point: they cannot drift.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use ryeos_runtime::authorizer::{canonical_cap, cap_matches, validate_scope_pattern};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{
    BundleEventDecl, BundleEventOperation, ItemAuthorDecl, RuntimeAuthorityDecls, RuntimeVaultDecl,
    RuntimeVaultOperation,
};

/// Capability `kind` segment for bundle-event authority.
pub const CAP_KIND_BUNDLE_EVENTS: &str = "bundle-events";
/// Capability `kind` segment for runtime-vault authority.
pub const CAP_KIND_RUNTIME_VAULT: &str = "vault";

/// The `(verb, kind)` surfaces a signed manifest can mint into, derived once from
/// the runtime-authority families themselves so this classification cannot drift
/// from what the minter produces. A composed grant that could satisfy any of
/// these is rejected (see [`composed_grant_overlaps_manifest_runtime_authority`]).
fn authority_surfaces() -> &'static [(&'static str, &'static str)] {
    static SURFACES: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    SURFACES.get_or_init(|| {
        let mut surfaces: Vec<(&'static str, &'static str)> = Vec::new();
        for op in BundleEventOperation::ALL {
            surfaces.push((op.cap_verb(), CAP_KIND_BUNDLE_EVENTS));
        }
        for op in RuntimeVaultOperation::ALL {
            surfaces.push((op.cap_verb(), CAP_KIND_RUNTIME_VAULT));
        }
        // `author` intentionally reserves every item kind: the capability shape
        // is `ryeos.author.<kind>.<bare-id>`, so no composed grant may self-mint
        // it.
        surfaces.push(("author", "*"));
        surfaces
    })
}

impl BundleEventOperation {
    /// Every variant, so cap construction and reserved-surface derivation stay
    /// exhaustive as the enum grows.
    pub const ALL: &'static [BundleEventOperation] =
        &[BundleEventOperation::Append, BundleEventOperation::Scan];

    /// The capability `verb` this operation authorizes.
    pub fn cap_verb(&self) -> &'static str {
        match self {
            BundleEventOperation::Append => "append",
            BundleEventOperation::Scan => "scan",
        }
    }
}

impl RuntimeVaultOperation {
    /// Every variant, so cap construction and reserved-surface derivation stay
    /// exhaustive as the enum grows.
    pub const ALL: &'static [RuntimeVaultOperation] = &[
        RuntimeVaultOperation::Put,
        RuntimeVaultOperation::Get,
        RuntimeVaultOperation::Delete,
        RuntimeVaultOperation::List,
    ];

    /// The capability `verb` this operation authorizes.
    pub fn cap_verb(&self) -> &'static str {
        match self {
            RuntimeVaultOperation::Put => "put",
            RuntimeVaultOperation::Get => "get",
            RuntimeVaultOperation::Delete => "delete",
            RuntimeVaultOperation::List => "list",
        }
    }
}

/// Build the bundle-event capability for `op` on `bundle_id`/`event_kind`.
pub fn bundle_event_cap(op: &BundleEventOperation, bundle_id: &str, event_kind: &str) -> String {
    canonical_cap(
        CAP_KIND_BUNDLE_EVENTS,
        &format!("{bundle_id}/{event_kind}"),
        op.cap_verb(),
    )
}

/// Build the runtime-vault capability for `op` on `bundle_id`/`namespace`.
pub fn runtime_vault_cap(op: &RuntimeVaultOperation, bundle_id: &str, namespace: &str) -> String {
    canonical_cap(
        CAP_KIND_RUNTIME_VAULT,
        &format!("{bundle_id}/{namespace}"),
        op.cap_verb(),
    )
}

/// Build the daemon-mediated item-authoring capability for `kind:namespace`.
/// The authoring service checks this against the exact target bare-id, so
/// wildcard support comes only from the unified authorizer.
pub fn item_author_cap(kind: &str, namespace: &str) -> String {
    canonical_cap(kind, namespace, "author")
}

impl BundleEventDecl {
    /// The exact caps this declaration grants the bundle `bundle_id`.
    pub fn runtime_authority_caps<'a>(
        &'a self,
        bundle_id: &'a str,
    ) -> impl Iterator<Item = String> + 'a {
        self.operations
            .iter()
            .map(move |op| bundle_event_cap(op, bundle_id, &self.event_kind))
    }
}

impl RuntimeVaultDecl {
    /// The exact caps this declaration grants the bundle `bundle_id`.
    pub fn runtime_authority_caps<'a>(
        &'a self,
        bundle_id: &'a str,
    ) -> impl Iterator<Item = String> + 'a {
        self.operations
            .iter()
            .map(move |op| runtime_vault_cap(op, bundle_id, &self.namespace))
    }
}

impl ItemAuthorDecl {
    /// The exact cap this declaration grants.
    pub fn runtime_authority_caps(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(item_author_cap(&self.kind, &self.namespace))
    }
}

impl RuntimeAuthorityDecls {
    /// True when the manifest declares no runtime authority in any family.
    pub fn is_empty(&self) -> bool {
        self.bundle_events.is_empty()
            && self.runtime_vault.is_empty()
            && self.item_authoring.is_empty()
    }

    /// The full set of caps this manifest grants `bundle_id` as an authority
    /// *upper bound* — the union of every family's declarations. The minter
    /// checks requested caps against this set; nothing here is granted unless an
    /// item requests it.
    pub fn declared_caps(&self, bundle_id: &str) -> BTreeSet<String> {
        let mut caps = BTreeSet::new();
        for decl in &self.bundle_events {
            caps.extend(decl.runtime_authority_caps(bundle_id));
        }
        for decl in &self.runtime_vault {
            caps.extend(decl.runtime_authority_caps(bundle_id));
        }
        for decl in &self.item_authoring {
            caps.extend(decl.runtime_authority_caps());
        }
        caps
    }

    /// Validate the structural rules a signed manifest's declarations must obey
    /// beyond serde shape, for *every* family: non-empty resource ids, non-empty
    /// operation lists, and no glob metacharacters in the non-pattern families
    /// (a wildcard `event_kind`/`namespace` would let the manifest declare a cap
    /// that globs over many concrete requested names). Item-authoring keeps its
    /// pattern grammar, where `*`/`?` are intentional.
    pub fn validate(&self) -> Result<(), String> {
        for decl in &self.bundle_events {
            if decl.event_kind.trim().is_empty() {
                return Err("bundle_events declaration has an empty `event_kind`".to_string());
            }
            if decl.operations.is_empty() {
                return Err(format!(
                    "bundle_events declaration for '{}' must list at least one operation",
                    decl.event_kind
                ));
            }
            reject_authority_wildcards("bundle_events event_kind", &decl.event_kind)?;
        }
        for decl in &self.runtime_vault {
            if decl.namespace.trim().is_empty() {
                return Err("runtime_vault declaration has an empty `namespace`".to_string());
            }
            if decl.operations.is_empty() {
                return Err(format!(
                    "runtime_vault declaration for '{}' must list at least one operation",
                    decl.namespace
                ));
            }
            reject_authority_wildcards("runtime_vault namespace", &decl.namespace)?;
        }
        for decl in &self.item_authoring {
            validate_item_author_pattern(&decl.kind, &decl.namespace)?;
        }
        Ok(())
    }
}

/// Reject `*`/`?` glob metacharacters in a resource identifier that has no
/// wildcard semantics (bundle-event kinds, vault namespaces). Only item-authoring
/// namespaces are patterns; a wildcard in these families would let a signed
/// manifest declare a cap that globs over many concrete requested names.
fn reject_authority_wildcards(label: &str, value: &str) -> Result<(), String> {
    if value.contains('*') || value.contains('?') {
        return Err(format!(
            "{label} must not contain '*' or '?' wildcards: {value:?}"
        ));
    }
    Ok(())
}

/// True when a `*`/`?` glob metacharacter appears anywhere in a cap string.
fn cap_has_wildcard(cap: &str) -> bool {
    cap.contains('*') || cap.contains('?')
}

/// Whether `requested_cap` is safely backed by a signed manifest's
/// `manifest_caps` upper bound.
///
/// A **concrete** request (no `*`/`?`) is backed when some manifest cap
/// glob-matches it — so a manifest pattern `runtime-authored/*` backs a request
/// `runtime-authored/foo`.
///
/// A request that itself carries a `*`/`?` wildcard is backed **only** by an
/// identical manifest declaration. Glob-vs-glob matching is unsafe here: the
/// authorizer treats the manifest string as a glob over the request string, so a
/// manifest pattern `runtime-authored/foo?` would "match" a request
/// `runtime-authored/foo*` even though the request authorizes names (`foo-long`,
/// …) the manifest never granted. Requiring exact equality fails closed and
/// preserves the "signed manifest is the upper bound" invariant.
pub fn manifest_backs_requested_cap(manifest_caps: &BTreeSet<String>, requested_cap: &str) -> bool {
    if cap_has_wildcard(requested_cap) {
        manifest_caps.contains(requested_cap)
    } else {
        manifest_caps
            .iter()
            .any(|manifest_cap| cap_matches(manifest_cap, requested_cap))
    }
}

// ── Item-level runtime capability requirements ───────────────────────
//
// An item declares everything it needs under one `requires.capabilities` tree,
// split by who is the authority:
//
//     requires:
//       capabilities:
//         declared:                       # the signed item is the authority
//           - ryeos.execute.tool.echo     # → composed into effective_caps
//         manifest:                       # the signed bundle manifest is the authority
//           runtime_authority:            # → minted only as the manifest backs it
//             bundle_events:
//               - event_kind: arc_pattern_event
//                 operations: [append]
//             runtime_vault:
//               - namespace: oauth
//                 operations: [get]
//             item_authoring:
//               - kind: knowledge
//                 namespace: runtime-authored/*
//
// `declared` is honored because the launcher refuses to spawn an unsigned
// effective item — a signed item may assert its own execution authority.
// `manifest` is a *requirement contract*, not a grant: the daemon mints only
// the requested caps the signed manifest actually backs. Absent requirements
// mint nothing — runtime authority is never granted merely because the manifest
// declares it.

/// The `requires:` block of an item, as authored. The whole tree is modelled
/// with `deny_unknown_fields` at every level so a typo (`capabilites:`,
/// `manifst:`, an unknown verb) fails loudly rather than silently dropping
/// authority.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequires {
    #[serde(default)]
    pub capabilities: RuntimeCapabilityRequirements,
}

/// The `requires.capabilities` sub-tree, split by authority source:
/// `declared` (self-asserted action authority) and `manifest` (runtime
/// authority backed by the signed bundle manifest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapabilityRequirements {
    /// Self-asserted action authority — a flat list of canonical cap strings
    /// the signed item declares for itself (the cap encodes its own verb, e.g.
    /// `ryeos.execute.tool.echo`). Composed into `effective_caps`; never minted
    /// from the manifest.
    #[serde(default)]
    pub declared: Vec<String>,
    #[serde(default)]
    pub manifest: ManifestCapabilityRequirements,
}

/// The `manifest` authority source: everything an item requires that only the
/// signed bundle manifest can grant. Today that is exactly one surface —
/// [`RuntimeAuthorityRequirements`] — kept behind a named node so the split
/// between authority sources (`declared` vs `manifest`) stays explicit and the
/// manifest source can grow non-runtime-authority grants later without
/// reshaping requirement authoring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCapabilityRequirements {
    #[serde(default)]
    pub runtime_authority: RuntimeAuthorityRequirements,
}

/// The manifest-backed runtime authority an item requests: a requested *subset*
/// of the signed manifest's [`RuntimeAuthorityDecls`], one field per family. The
/// daemon mints exactly these caps into the callback token, and only where the
/// signed manifest actually backs them. Mirrors the manifest declaration shape
/// so the two cannot drift; kept a distinct type because declarations are signed
/// upper bounds and requirements are requested subsets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuthorityRequirements {
    #[serde(default)]
    pub bundle_events: Vec<BundleEventRequirement>,
    #[serde(default)]
    pub runtime_vault: Vec<RuntimeVaultRequirement>,
    #[serde(default)]
    pub item_authoring: Vec<ItemAuthorRequirement>,
}

/// One bundle-event requirement: an event kind plus the operations the item
/// needs on it. Operation names reuse [`BundleEventOperation`] so requirement
/// and manifest declarations cannot drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventRequirement {
    pub event_kind: String,
    pub operations: Vec<BundleEventOperation>,
}

/// One runtime-vault requirement: a namespace plus the operations the item
/// needs on it. Operation names reuse [`RuntimeVaultOperation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVaultRequirement {
    pub namespace: String,
    pub operations: Vec<RuntimeVaultOperation>,
}

/// One daemon-authored item requirement: a kind plus a bare-id namespace/pattern
/// the runtime needs authority to create or update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemAuthorRequirement {
    pub kind: String,
    pub namespace: String,
}

impl BundleEventRequirement {
    /// The exact caps this requirement requests for `bundle_id`.
    pub fn requested_caps<'a>(&'a self, bundle_id: &'a str) -> impl Iterator<Item = String> + 'a {
        self.operations
            .iter()
            .map(move |op| bundle_event_cap(op, bundle_id, &self.event_kind))
    }
}

impl RuntimeVaultRequirement {
    /// The exact caps this requirement requests for `bundle_id`.
    pub fn requested_caps<'a>(&'a self, bundle_id: &'a str) -> impl Iterator<Item = String> + 'a {
        self.operations
            .iter()
            .map(move |op| runtime_vault_cap(op, bundle_id, &self.namespace))
    }
}

impl ItemAuthorRequirement {
    /// The exact cap this requirement requests.
    pub fn requested_caps(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(item_author_cap(&self.kind, &self.namespace))
    }
}

impl RuntimeAuthorityRequirements {
    /// True when the item requests no runtime authority in any family.
    pub fn is_empty(&self) -> bool {
        self.bundle_events.is_empty()
            && self.runtime_vault.is_empty()
            && self.item_authoring.is_empty()
    }

    /// Whether the item declares any manifest-backed runtime authority at all —
    /// the generic question publish/doctor ask without naming each family.
    pub fn declares_runtime_authority(&self) -> bool {
        !self.is_empty()
    }

    /// The exact caps this requirement requests for `bundle_id` — the union
    /// across every family. `declared` caps are not part of this set.
    pub fn requested_caps(&self, bundle_id: &str) -> BTreeSet<String> {
        let mut caps = BTreeSet::new();
        for req in &self.bundle_events {
            caps.extend(req.requested_caps(bundle_id));
        }
        for req in &self.runtime_vault {
            caps.extend(req.requested_caps(bundle_id));
        }
        for req in &self.item_authoring {
            caps.extend(req.requested_caps());
        }
        caps
    }

    /// Structural rules a type alone cannot enforce: non-empty event
    /// kinds/namespaces, non-empty operation arrays, and the item-authoring
    /// kind/namespace grammar. Shared by every requirement parser so all reject
    /// the same malformed requirements.
    pub fn validate(&self) -> Result<(), String> {
        for req in &self.bundle_events {
            if req.event_kind.trim().is_empty() {
                return Err("bundle_events entry has an empty `event_kind`".to_string());
            }
            if req.operations.is_empty() {
                return Err(format!(
                    "bundle_events entry for '{}' must list at least one operation",
                    req.event_kind
                ));
            }
        }
        for req in &self.runtime_vault {
            if req.namespace.trim().is_empty() {
                return Err("runtime_vault entry has an empty `namespace`".to_string());
            }
            if req.operations.is_empty() {
                return Err(format!(
                    "runtime_vault entry for '{}' must list at least one operation",
                    req.namespace
                ));
            }
        }
        for req in &self.item_authoring {
            validate_item_author_pattern(&req.kind, &req.namespace)?;
        }
        Ok(())
    }
}

/// The exact set of manifest-backed runtime caps `reqs` requests for
/// `bundle_id` (the `manifest` sub-tree; `declared` caps are not minted here).
pub fn requested_runtime_caps(
    reqs: &RuntimeCapabilityRequirements,
    bundle_id: &str,
) -> BTreeSet<String> {
    reqs.manifest.runtime_authority.requested_caps(bundle_id)
}

/// Static, manifest-independent validation of an item's `requires:` block.
///
/// Parses the authored value into the typed shape (rejecting unknown keys,
/// unknown operations, and raw `ryeos.*` strings via serde) and then enforces
/// the structural rules a type alone cannot:
///
/// - non-empty `event_kind` / `namespace`,
/// - non-empty `operations` arrays.
///
/// Deeper, trust-store-dependent checks (manifest subset, bundle-id segment
/// grammar) belong to the launch path, not here.
pub fn parse_runtime_requires(value: &Value) -> Result<RuntimeCapabilityRequirements, String> {
    let requires: RuntimeRequires = serde_json::from_value(value.clone())
        .map_err(|e| format!("malformed `requires` block: {e}"))?;
    validate_runtime_capability_requirements(&requires.capabilities)?;
    Ok(requires.capabilities)
}

/// Enforce the structural rules a type alone cannot: non-empty event
/// kinds/namespaces and non-empty operation arrays. Shared by the launch
/// parser and the graph/directive runtime parsers so both reject the same
/// malformed requirements.
pub fn validate_runtime_capability_requirements(
    caps: &RuntimeCapabilityRequirements,
) -> Result<(), String> {
    caps.manifest.runtime_authority.validate()
}

pub fn validate_item_author_pattern(kind: &str, namespace: &str) -> Result<(), String> {
    validate_item_kind(kind)?;
    validate_bare_id_pattern("item_authoring namespace", namespace)
}

pub fn validate_item_kind(kind: &str) -> Result<(), String> {
    if kind.is_empty()
        || !kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "item_authoring kind must be a canonical kind segment: {kind:?}"
        ));
    }
    Ok(())
}

pub fn validate_bare_id_pattern(label: &str, pattern: &str) -> Result<(), String> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} is empty"));
    }
    if trimmed != pattern {
        return Err(format!(
            "{label} must not contain leading/trailing whitespace: {pattern:?}"
        ));
    }
    if pattern.starts_with('/') || pattern.ends_with('/') {
        return Err(format!(
            "{label} must be a relative bare-id pattern: {pattern:?}"
        ));
    }
    if pattern.contains('\\') {
        return Err(format!(
            "{label} must use '/' separators, not backslashes: {pattern:?}"
        ));
    }
    for segment in pattern.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!(
                "{label} contains an invalid path segment: {pattern:?}"
            ));
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '*' | '?'))
        {
            return Err(format!("{label} contains invalid characters: {pattern:?}"));
        }
    }
    Ok(())
}

/// True when a user-composed grant could satisfy *any* capability the manifest
/// runtime-authority minter can produce — i.e. it overlaps a `(verb, kind)`
/// surface from [`authority_surfaces`], including wildcard forms (`*`, `ryeos.*`,
/// `ryeos.put.*`, `ryeos.*.vault.*`, …). Matched on parsed segments, so
/// unrelated grants like `ryeos.execute.tool.echo` or
/// `ryeos.execute.service.vault/list` are *not* flagged.
pub fn composed_grant_overlaps_manifest_runtime_authority(grant: &str) -> bool {
    if grant == "*" {
        return true;
    }
    let parts: Vec<&str> = grant.splitn(4, '.').collect();
    if parts.first() != Some(&"ryeos") {
        return false;
    }
    match parts.as_slice() {
        // `ryeos.*` — overlaps every surface.
        ["ryeos", "*"] => true,
        // `ryeos.<verb>.*` (kind wildcard) or `ryeos.<verb>.<kind>` (implicit
        // subject wildcard).
        ["ryeos", verb, kind] => overlaps_surface(verb, kind),
        // `ryeos.<verb>.<kind>.<subject>`.
        ["ryeos", verb, kind, _subject] => overlaps_surface(verb, kind),
        _ => false,
    }
}

fn overlaps_surface(grant_verb: &str, grant_kind: &str) -> bool {
    authority_surfaces().iter().any(|(verb, kind)| {
        (grant_verb == "*" || grant_verb == *verb)
            && (grant_kind == "*" || *kind == "*" || grant_kind == *kind)
    })
}

/// Why a composed-permission grant was refused at the cap-assembly boundary.
#[derive(Debug)]
pub enum ComposedGrantError {
    /// Not a canonical `ryeos.<verb>.<kind>.<subject>` scope — e.g. a partial
    /// wildcard like `ryeos.p*.vault.*` or a `?` glob. The authorizer's matcher
    /// honors `*`/`?` anywhere, but the scope grammar permits `*` only as a
    /// whole segment; admitting a partial-wildcard grant would let it slip past
    /// the whole-segment overlap classification yet still satisfy a
    /// runtime-authority requirement. Refused outright.
    Malformed { grant: String, reason: String },
    /// A well-formed grant that overlaps the manifest runtime-authority
    /// namespace (bundle events / vault / item authoring).
    Reserved { grant: String },
}

impl std::fmt::Display for ComposedGrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComposedGrantError::Malformed { grant, reason } => write!(
                f,
                "composed permission '{grant}' is not a canonical capability and cannot be \
                 admitted to a callback token: {reason}"
            ),
            ComposedGrantError::Reserved { grant } => write!(
                f,
                "capability '{grant}' is reserved: bundle-event, runtime-vault, and item-authoring capabilities are \
                 manifest-backed runtime authority. Declare them under \
                 `requires.capabilities.manifest.runtime_authority`, not `requires.capabilities.declared` — the \
                 signed manifest is the authority upper bound and the item selects the subset it \
                 needs"
            ),
        }
    }
}

impl std::error::Error for ComposedGrantError {}

/// Screen composed-permission grants before they are unioned with the
/// daemon-minted caps. Two refusals, both surfaced as `CapabilityRejected`:
///
/// 1. **Malformed** — the grant is not a canonical scope. Enforcing the scope
///    grammar here (where it was previously unchecked) is the security-relevant
///    step: it removes partial-wildcard / `?` forms that whole-segment overlap
///    classification would otherwise miss while the authorizer's matcher would
///    still honor them.
/// 2. **Reserved** — a well-formed grant that overlaps the manifest
///    runtime-authority namespace, which only a signed manifest may mint.
pub fn reject_disallowed_composed_grants(grants: &[String]) -> Result<(), ComposedGrantError> {
    for grant in grants {
        if let Err(reason) = validate_scope_pattern(grant) {
            return Err(ComposedGrantError::Malformed {
                grant: grant.clone(),
                reason,
            });
        }
        if composed_grant_overlaps_manifest_runtime_authority(grant) {
            return Err(ComposedGrantError::Reserved {
                grant: grant.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_match_canonical_wire_form() {
        assert_eq!(
            bundle_event_cap(
                &BundleEventOperation::Append,
                "example-bundle",
                "example_event"
            ),
            "ryeos.append.bundle-events.example-bundle/example_event"
        );
        assert_eq!(
            runtime_vault_cap(&RuntimeVaultOperation::Get, "example-bundle", "oauth"),
            "ryeos.get.vault.example-bundle/oauth"
        );
        assert_eq!(
            item_author_cap("knowledge", "runtime-authored/*"),
            "ryeos.author.knowledge.runtime-authored/*"
        );
    }

    #[test]
    fn decl_caps_cover_all_operations() {
        let ev = BundleEventDecl {
            event_kind: "example_event".into(),
            operations: vec![BundleEventOperation::Append, BundleEventOperation::Scan],
        };
        let caps: Vec<String> = ev.runtime_authority_caps("example-bundle").collect();
        assert_eq!(
            caps,
            vec![
                "ryeos.append.bundle-events.example-bundle/example_event",
                "ryeos.scan.bundle-events.example-bundle/example_event",
            ]
        );
    }

    #[test]
    fn rejects_runtime_authority_grants_exact_and_wildcard() {
        for grant in [
            "ryeos.put.vault.example-bundle/oauth",
            "ryeos.scan.bundle-events.example-bundle/example_event",
            "ryeos.author.knowledge.runtime-authored/*",
            "ryeos.author.tool.ryeos/*",
            "ryeos.scan.bundle-events.*",
            "ryeos.put.*",
            "ryeos.author.*",
            "ryeos.*.vault.*",
            "ryeos.put.vault", // implicit subject wildcard
            "ryeos.*",
            "*",
        ] {
            assert!(
                composed_grant_overlaps_manifest_runtime_authority(grant),
                "should reject runtime-authority grant: {grant}"
            );
        }
    }

    #[test]
    fn allows_ordinary_execute_grants() {
        for grant in [
            "ryeos.execute.tool.echo",
            "ryeos.execute.tool.*",
            "ryeos.execute.*",
            // node-vault is a *service* (kind=service, subject vault/list), not
            // runtime-vault authority — must not be flagged.
            "ryeos.execute.service.vault/list",
            "not-a-ryeos-cap",
        ] {
            assert!(
                !composed_grant_overlaps_manifest_runtime_authority(grant),
                "should allow ordinary grant: {grant}"
            );
        }
    }

    #[test]
    fn screens_reserved_grant_and_names_the_offender() {
        let err = reject_disallowed_composed_grants(&[
            "ryeos.execute.tool.echo".into(),
            "ryeos.put.vault.b/ns".into(),
        ])
        .unwrap_err();
        assert!(
            matches!(&err, ComposedGrantError::Reserved { grant } if grant == "ryeos.put.vault.b/ns"),
            "got {err:?}"
        );
        assert!(reject_disallowed_composed_grants(&["ryeos.execute.tool.echo".into()]).is_ok());
    }

    #[test]
    fn screens_out_partial_wildcard_grants_as_malformed() {
        // The authorizer's matcher would honor these against a runtime-authority
        // requirement, but they are not canonical scopes (`*`/`?` only as a
        // whole `*` segment) — they must be refused before classification.
        for grant in [
            "ryeos.p*.vault.*",
            "ryeos.put.vau*.*",
            "ryeos.s?an.bundle-events.*",
        ] {
            let err = reject_disallowed_composed_grants(&[grant.into()]).unwrap_err();
            assert!(
                matches!(err, ComposedGrantError::Malformed { .. }),
                "expected malformed for {grant}, got {err:?}"
            );
        }
    }

    #[test]
    fn screens_out_wellformed_wildcard_intrusions_as_reserved() {
        for grant in [
            "ryeos.scan.bundle-events.*",
            "ryeos.*.vault.*",
            "ryeos.author.knowledge.*",
            "ryeos.put.*",
            "ryeos.*",
        ] {
            let err = reject_disallowed_composed_grants(&[grant.into()]).unwrap_err();
            assert!(
                matches!(err, ComposedGrantError::Reserved { .. }),
                "expected reserved for {grant}, got {err:?}"
            );
        }
    }

    // ── requirement model ────────────────────────────────────────────

    use serde_json::json;

    #[test]
    fn requirement_caps_match_canonical_wire_form() {
        let reqs = parse_runtime_requires(&json!({
            "capabilities": {
                "manifest": {
                    "runtime_authority": {
                        "bundle_events": [
                            { "event_kind": "arc_pattern_event", "operations": ["append", "scan"] }
                        ],
                        "runtime_vault": [
                            { "namespace": "oauth", "operations": ["get"] }
                        ],
                        "item_authoring": [
                            { "kind": "knowledge", "namespace": "runtime-authored/*" }
                        ]
                    }
                }
            }
        }))
        .unwrap();
        let caps = requested_runtime_caps(&reqs, "arc");
        assert_eq!(
            caps.into_iter().collect::<Vec<_>>(),
            vec![
                "ryeos.append.bundle-events.arc/arc_pattern_event".to_string(),
                "ryeos.author.knowledge.runtime-authored/*".to_string(),
                "ryeos.get.vault.arc/oauth".to_string(),
                "ryeos.scan.bundle-events.arc/arc_pattern_event".to_string(),
            ]
        );
    }

    #[test]
    fn absent_capabilities_request_nothing() {
        // `requires:` with no capabilities sub-tree is valid and mints nothing.
        let reqs = parse_runtime_requires(&json!({})).unwrap();
        assert!(requested_runtime_caps(&reqs, "arc").is_empty());
    }

    #[test]
    fn unknown_keys_fail_static_validation() {
        for value in [
            json!({ "capabilites": {} }),                   // capabilities typo
            json!({ "capabilities": { "manfest": {} } }),   // manifest typo
            json!({ "capabilities": { "callbacks": {} } }), // dropped legacy key
            json!({ "capabilities": { "manifest": { "runtime_authority": {
                "bundle_events": [ { "event_kind": "e", "operations": ["append"], "extra": 1 } ]
            } } } }), // unknown entry field
            json!({ "capabilities": { "declared": { "execute": [] } } }), // declared must be a list, not a map
            json!({ "capabilities": { "declared": [1] } }),               // non-string cap
        ] {
            assert!(
                parse_runtime_requires(&value).is_err(),
                "expected error for {value}"
            );
        }
    }

    #[test]
    fn declared_caps_parse_but_are_not_minted_as_runtime_caps() {
        // `declared` is self-asserted action authority — it parses and validates,
        // but `requested_runtime_caps` only mints the manifest-backed sub-tree.
        let reqs = parse_runtime_requires(&json!({
            "capabilities": { "declared": ["ryeos.execute.tool.echo"] }
        }))
        .unwrap();
        assert_eq!(reqs.declared, vec!["ryeos.execute.tool.echo".to_string()]);
        assert!(requested_runtime_caps(&reqs, "arc").is_empty());
    }

    #[test]
    fn unknown_operation_fails_static_validation() {
        let value = json!({ "capabilities": { "manifest": { "runtime_authority": {
            "bundle_events": [ { "event_kind": "e", "operations": ["frobnicate"] } ]
        } } } });
        assert!(parse_runtime_requires(&value).is_err());
    }

    #[test]
    fn empty_operations_fail_static_validation() {
        let value = json!({ "capabilities": { "manifest": { "runtime_authority": {
            "bundle_events": [ { "event_kind": "e", "operations": [] } ]
        } } } });
        let err = parse_runtime_requires(&value).unwrap_err();
        assert!(err.contains("at least one operation"), "got: {err}");

        let value = json!({ "capabilities": { "manifest": { "runtime_authority": {
            "runtime_vault": [ { "namespace": "oauth", "operations": [] } ]
        } } } });
        let err = parse_runtime_requires(&value).unwrap_err();
        assert!(err.contains("at least one operation"), "got: {err}");
    }

    #[test]
    fn raw_cap_strings_fail_static_validation() {
        // Authors must not paste `ryeos.*` strings under requires.
        let value = json!({ "capabilities": { "manifest": { "runtime_authority": {
            "bundle_events": ["ryeos.append.bundle-events.arc/arc_pattern_event"]
        } } } });
        assert!(parse_runtime_requires(&value).is_err());
    }

    #[test]
    fn empty_event_kind_or_namespace_fails() {
        let value = json!({ "capabilities": { "manifest": { "runtime_authority": {
            "bundle_events": [ { "event_kind": "", "operations": ["append"] } ]
        } } } });
        assert!(parse_runtime_requires(&value).is_err());

        let value = json!({ "capabilities": { "manifest": { "runtime_authority": {
            "runtime_vault": [ { "namespace": "  ", "operations": ["get"] } ]
        } } } });
        assert!(parse_runtime_requires(&value).is_err());
    }

    // ── manifest declaration validation ──────────────────────────────

    #[test]
    fn manifest_validate_rejects_empty_operations_and_ids() {
        let empty_ops = RuntimeAuthorityDecls {
            bundle_events: vec![BundleEventDecl {
                event_kind: "ev".into(),
                operations: vec![],
            }],
            ..Default::default()
        };
        assert!(empty_ops.validate().unwrap_err().contains("at least one"));

        let empty_ns = RuntimeAuthorityDecls {
            runtime_vault: vec![RuntimeVaultDecl {
                namespace: "  ".into(),
                operations: vec![RuntimeVaultOperation::Get],
            }],
            ..Default::default()
        };
        assert!(empty_ns.validate().unwrap_err().contains("empty `namespace`"));
    }

    #[test]
    fn manifest_validate_rejects_wildcards_in_non_pattern_families() {
        // A wildcard event_kind/namespace would let a signed manifest declare a
        // cap that globs over many concrete requested names — rejected. Only
        // item_authoring namespaces are patterns.
        let wild_event = RuntimeAuthorityDecls {
            bundle_events: vec![BundleEventDecl {
                event_kind: "ev_*".into(),
                operations: vec![BundleEventOperation::Append],
            }],
            ..Default::default()
        };
        assert!(wild_event.validate().unwrap_err().contains("wildcards"));

        let wild_vault = RuntimeAuthorityDecls {
            runtime_vault: vec![RuntimeVaultDecl {
                namespace: "oauth?".into(),
                operations: vec![RuntimeVaultOperation::Get],
            }],
            ..Default::default()
        };
        assert!(wild_vault.validate().unwrap_err().contains("wildcards"));

        // item_authoring keeps its pattern grammar — `*` is intentional there.
        let author = RuntimeAuthorityDecls {
            item_authoring: vec![ItemAuthorDecl {
                kind: "knowledge".into(),
                namespace: "runtime-authored/*".into(),
            }],
            ..Default::default()
        };
        assert!(author.validate().is_ok());
    }

    // ── wildcard-safe subset check (the "signed manifest is the upper
    //    bound" invariant) ─────────────────────────────────────────────

    #[test]
    fn concrete_request_is_backed_by_manifest_wildcard() {
        let manifest: BTreeSet<String> =
            ["ryeos.author.knowledge.runtime-authored/*".to_string()]
                .into_iter()
                .collect();
        assert!(manifest_backs_requested_cap(
            &manifest,
            "ryeos.author.knowledge.runtime-authored/foo"
        ));
    }

    #[test]
    fn wildcard_request_requires_exact_manifest_declaration() {
        let manifest: BTreeSet<String> =
            ["ryeos.author.knowledge.runtime-authored/foo?".to_string()]
                .into_iter()
                .collect();
        // `foo?` would glob-"match" the literal `foo*`, but `foo*` authorizes
        // names `foo?` never grants — the wildcard request must be declared
        // verbatim, so this fails closed.
        assert!(!manifest_backs_requested_cap(
            &manifest,
            "ryeos.author.knowledge.runtime-authored/foo*"
        ));
        // An identical wildcard request IS backed.
        let manifest: BTreeSet<String> =
            ["ryeos.author.knowledge.runtime-authored/*".to_string()]
                .into_iter()
                .collect();
        assert!(manifest_backs_requested_cap(
            &manifest,
            "ryeos.author.knowledge.runtime-authored/*"
        ));
    }
}

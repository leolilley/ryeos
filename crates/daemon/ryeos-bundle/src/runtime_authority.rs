//! Manifest runtime-authority policy.
//!
//! "Runtime authority" is the set of daemon callback capabilities a **signed
//! bundle manifest** may declare for the bundle's own running code — today
//! bundle events and runtime vault (see
//! `ryeos/future/tool-runtime-authority`). Per that contract, this authority is
//! *always minted by the daemon* from the signed manifest and is **never**
//! grantable through a composed `permissions:` block.
//!
//! This module is the single source of truth for that vocabulary:
//!
//! - the cap `kind` segments (`bundle-events`, `vault`),
//! - the typed cap constructors the manifest declarations and the daemon
//!   callback services both use to build/require caps, and
//! - the classifier that rejects a user-composed grant which would overlap the
//!   manifest runtime-authority namespace (including wildcard overlaps).
//!
//! Keeping minting, service authorization, and rejection on one definition is
//! the point: they cannot drift.

use std::collections::BTreeSet;

use ryeos_runtime::authorizer::{canonical_cap, validate_scope_pattern};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{
    BundleEventDecl, BundleEventOperation, RuntimeVaultDecl, RuntimeVaultOperation,
};

/// Capability `kind` segment for bundle-event authority.
pub const CAP_KIND_BUNDLE_EVENTS: &str = "bundle-events";
/// Capability `kind` segment for runtime-vault authority.
pub const CAP_KIND_RUNTIME_VAULT: &str = "vault";

/// The `(verb, kind)` surfaces a signed manifest can mint into. A composed
/// grant that could satisfy any of these is rejected (see
/// [`composed_grant_overlaps_manifest_runtime_authority`]). Derived from the
/// operation enums below; kept here as the one classification surface.
const AUTHORITY_SURFACES: &[(&str, &str)] = &[
    ("append", CAP_KIND_BUNDLE_EVENTS),
    ("scan", CAP_KIND_BUNDLE_EVENTS),
    ("put", CAP_KIND_RUNTIME_VAULT),
    ("get", CAP_KIND_RUNTIME_VAULT),
    ("delete", CAP_KIND_RUNTIME_VAULT),
    ("list", CAP_KIND_RUNTIME_VAULT),
];

impl BundleEventOperation {
    /// The capability `verb` this operation authorizes.
    pub fn cap_verb(&self) -> &'static str {
        match self {
            BundleEventOperation::Append => "append",
            BundleEventOperation::Scan => "scan",
        }
    }
}

impl RuntimeVaultOperation {
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

// ── Item-level runtime capability requirements ───────────────────────
//
// A signed manifest declares the bundle's *upper bound* runtime authority
// (see `BundleManifest::bundle_events` / `runtime_vault`). An individual item
// declares the exact subset it needs under:
//
//     requires:
//       capabilities:
//         callbacks:
//           bundle_events:
//             - event_kind: arc_pattern_event
//               operations: [append]
//           runtime_vault:
//             - namespace: oauth
//               operations: [get]
//
// This is a *requirement contract*, not a grant: the daemon mints into the
// callback token only the requested caps that the signed manifest actually
// backs. Absent requirements mint nothing — runtime authority is never granted
// merely because the manifest declares it.

/// The `requires:` block of an item, as authored. Only the
/// `capabilities` sub-tree is modelled; unknown keys are rejected so a typo
/// (`capabilites:`) fails loudly rather than silently minting nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRequires {
    #[serde(default)]
    pub capabilities: RuntimeCapabilityRequirements,
}

/// The `requires.capabilities` sub-tree. Today only `callbacks` exists;
/// future surfaces (`actions`, `network`, …) are added here, keeping the
/// callback-token authority distinct from other requirement classes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapabilityRequirements {
    #[serde(default)]
    pub callbacks: CallbackCapabilityRequirements,
}

/// Runtime callback-token authority an item requires: bundle-event and
/// runtime-vault operations the daemon must mint into the callback token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackCapabilityRequirements {
    #[serde(default)]
    pub bundle_events: Vec<BundleEventRequirement>,
    #[serde(default)]
    pub runtime_vault: Vec<RuntimeVaultRequirement>,
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

impl BundleEventRequirement {
    /// The exact caps this requirement requests for `bundle_id`.
    pub fn requested_caps<'a>(
        &'a self,
        bundle_id: &'a str,
    ) -> impl Iterator<Item = String> + 'a {
        self.operations
            .iter()
            .map(move |op| bundle_event_cap(op, bundle_id, &self.event_kind))
    }
}

impl RuntimeVaultRequirement {
    /// The exact caps this requirement requests for `bundle_id`.
    pub fn requested_caps<'a>(
        &'a self,
        bundle_id: &'a str,
    ) -> impl Iterator<Item = String> + 'a {
        self.operations
            .iter()
            .map(move |op| runtime_vault_cap(op, bundle_id, &self.namespace))
    }
}

/// The exact set of callback caps `reqs` requests for `bundle_id`.
pub fn requested_runtime_caps(
    reqs: &RuntimeCapabilityRequirements,
    bundle_id: &str,
) -> BTreeSet<String> {
    let mut caps = BTreeSet::new();
    for req in &reqs.callbacks.bundle_events {
        caps.extend(req.requested_caps(bundle_id));
    }
    for req in &reqs.callbacks.runtime_vault {
        caps.extend(req.requested_caps(bundle_id));
    }
    caps
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
    for req in &caps.callbacks.bundle_events {
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
    for req in &caps.callbacks.runtime_vault {
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
    Ok(())
}

/// True when a user-composed grant could satisfy *any* capability the manifest
/// runtime-authority minter can produce — i.e. it overlaps a `(verb, kind)`
/// surface in [`AUTHORITY_SURFACES`], including wildcard forms (`*`, `ryeos.*`,
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
    AUTHORITY_SURFACES.iter().any(|(verb, kind)| {
        (grant_verb == "*" || grant_verb == *verb) && (grant_kind == "*" || grant_kind == *kind)
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
    /// namespace (bundle events / vault).
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
                "composed permission '{grant}' is reserved: bundle-event and runtime-vault \
                 capabilities are manifest-backed runtime requirements. Declare them under \
                 `requires.capabilities.callbacks`, not `permissions:` — the signed manifest is \
                 the authority upper bound and the item selects the subset it needs"
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
            bundle_event_cap(&BundleEventOperation::Append, "example-bundle", "example_event"),
            "ryeos.append.bundle-events.example-bundle/example_event"
        );
        assert_eq!(
            runtime_vault_cap(&RuntimeVaultOperation::Get, "example-bundle", "oauth"),
            "ryeos.get.vault.example-bundle/oauth"
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
            "ryeos.scan.bundle-events.*",
            "ryeos.put.*",
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
        for grant in ["ryeos.scan.bundle-events.*", "ryeos.*.vault.*", "ryeos.put.*", "ryeos.*"] {
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
                "callbacks": {
                    "bundle_events": [
                        { "event_kind": "arc_pattern_event", "operations": ["append", "scan"] }
                    ],
                    "runtime_vault": [
                        { "namespace": "oauth", "operations": ["get"] }
                    ]
                }
            }
        }))
        .unwrap();
        let caps = requested_runtime_caps(&reqs, "arc");
        assert_eq!(
            caps.into_iter().collect::<Vec<_>>(),
            vec![
                "ryeos.append.bundle-events.arc/arc_pattern_event".to_string(),
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
            json!({ "capabilites": {} }),                       // top-level typo
            json!({ "capabilities": { "callback": {} } }),      // callbacks typo
            json!({ "capabilities": { "callbacks": {
                "bundle_events": [ { "event_kind": "e", "operations": ["append"], "extra": 1 } ]
            } } }),                                              // unknown entry field
        ] {
            assert!(
                parse_runtime_requires(&value).is_err(),
                "expected error for {value}"
            );
        }
    }

    #[test]
    fn unknown_operation_fails_static_validation() {
        let value = json!({ "capabilities": { "callbacks": {
            "bundle_events": [ { "event_kind": "e", "operations": ["frobnicate"] } ]
        } } });
        assert!(parse_runtime_requires(&value).is_err());
    }

    #[test]
    fn empty_operations_fail_static_validation() {
        let value = json!({ "capabilities": { "callbacks": {
            "bundle_events": [ { "event_kind": "e", "operations": [] } ]
        } } });
        let err = parse_runtime_requires(&value).unwrap_err();
        assert!(err.contains("at least one operation"), "got: {err}");

        let value = json!({ "capabilities": { "callbacks": {
            "runtime_vault": [ { "namespace": "oauth", "operations": [] } ]
        } } });
        let err = parse_runtime_requires(&value).unwrap_err();
        assert!(err.contains("at least one operation"), "got: {err}");
    }

    #[test]
    fn raw_cap_strings_fail_static_validation() {
        // Authors must not paste `ryeos.*` strings under requires.
        let value = json!({ "capabilities": { "callbacks": {
            "bundle_events": ["ryeos.append.bundle-events.arc/arc_pattern_event"]
        } } });
        assert!(parse_runtime_requires(&value).is_err());
    }

    #[test]
    fn empty_event_kind_or_namespace_fails() {
        let value = json!({ "capabilities": { "callbacks": {
            "bundle_events": [ { "event_kind": "", "operations": ["append"] } ]
        } } });
        assert!(parse_runtime_requires(&value).is_err());

        let value = json!({ "capabilities": { "callbacks": {
            "runtime_vault": [ { "namespace": "  ", "operations": ["get"] } ]
        } } });
        assert!(parse_runtime_requires(&value).is_err());
    }
}

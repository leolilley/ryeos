//! The publication lane at stage time and at apply time.
//!
//! A bundle pulled from a publication is staged as inert bytes and installed
//! only by a signed offline operation. The lane's rules run in both places and
//! by the same function ([`ryeos_bundle::lane`]), with the policy and the
//! kind registry passed in: at stage time on the verified tree (a cheap
//! refusal that stages nothing), and at apply on the install's own completed
//! copy, with the policy as it stands then. Staging confers nothing: what a
//! bundle is lifted to is read from the policy at apply, never from the stage.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use ryeos_node::bundle_staging::apply::{ApplyRefusal, ApplyRefusalCode};
use ryeos_node::bundle_staging::{self as staging, StagedBundle};
use serde_json::{Value, json};

use super::bundle_install::{InstallAuthority, LaneInstall};
use super::bundle_pull::{
    Pins, PublicKeyDoc, check_publisher_pinned, export_response, lane_receipt,
    publisher_fingerprint, verify_publication_at,
};

/// The evidence files a publication stage carries. Apply reads only the bytes
/// the verified stage returned under these names.
pub(crate) const EVIDENCE_PUBLICATION: &str = "publication.json";
pub(crate) const EVIDENCE_MANIFEST: &str = "manifest.json";
pub(crate) const EVIDENCE_ORIGIN_KEY: &str = "origin-public-key.json";

/// Every code a lane audit refusal carries, as the audit states them.
const LANE_AUDIT_CODES: [&str; 8] = [
    "bundle_lane_kinds",
    "publication_lane_never_admitted",
    "publication_lane_facet_not_admitted",
    "publication_lane_kind_refused",
    "publication_lane_kind_undeclared",
    "publication_lane_stray_file",
    "publication_lane_executable_image",
    "bundle_lane_unreadable",
];

/// What this node's `publication_lane` policy admits for `bundle_name`, from a
/// policy snapshot. A snapshot without the section is an error: the lane is
/// off, never defaulted.
pub(crate) fn grants_from_snapshot(
    snapshot: &ryeos_node_runtime::node_policy::NodePolicySnapshot,
    bundle_name: &str,
) -> Result<ryeos_bundle::lane::LaneGrants> {
    use ryeos_node_runtime::node_policy::sections::publication_lane::PublicationLanePolicy;
    Ok(snapshot
        .require::<PublicationLanePolicy>()?
        .grants_for(bundle_name))
}

/// The lane audit of a verified tree, as a typed 422 refusal. This is the one
/// call stage and apply both make.
pub(crate) fn audit_tree(
    tree: &std::path::Path,
    bundle_name: &str,
    kinds: &ryeos_engine::kind_registry::KindRegistry,
    grants: &ryeos_bundle::lane::LaneGrants,
) -> Result<()> {
    ryeos_bundle::lane::audit_installed_lane_bundle(tree, bundle_name, kinds, grants).map_err(
        |refusal| {
            anyhow::Error::new(ryeos_app::handler_error::typed_refusal(
                refusal.code(),
                422,
                refusal.to_string(),
            ))
        },
    )
}

/// Whether an error is a lane audit's typed refusal.
pub(crate) fn is_lane_audit_refusal(error: &anyhow::Error) -> bool {
    matches!(
        ryeos_app::handler_error::extract_handler_error(error),
        Some(crate::handler_error::HandlerError::Structured { code, .. })
            if LANE_AUDIT_CODES.contains(&code.as_str())
    )
}

/// What a stage reports about what its bundle would be lifted to. A
/// prediction: the policy is read again at apply, and the stage record carries
/// none of it.
pub(crate) fn predicted_lifts(
    snapshot: &ryeos_node_runtime::node_policy::NodePolicySnapshot,
    bundle_name: &str,
) -> Value {
    const NOTE: &str = "prediction; policy is re-read at apply";
    match grants_from_snapshot(snapshot, bundle_name) {
        Ok(grants) => json!({
            "note": NOTE,
            "invoked": grants.invoked,
            "kinds": grants.kinds,
            "facets": grants.facets.iter().map(|facet| facet.name()).collect::<Vec<_>>(),
        }),
        Err(_) => json!({
            "note": NOTE,
            "lane": "off",
        }),
    }
}

fn refusal(code: ApplyRefusalCode, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ApplyRefusal::new(code, detail))
}

fn evidence_invalid(detail: impl Into<String>) -> anyhow::Error {
    refusal(ApplyRefusalCode::LaneEvidenceInvalid, detail)
}

fn evidence<'a>(files: &'a BTreeMap<String, Vec<u8>>, name: &str) -> Result<&'a [u8]> {
    files
        .get(name)
        .map(Vec::as_slice)
        .ok_or_else(|| evidence_invalid(format!("the stage carries no `{name}` evidence")))
}

fn parse_json(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|error| evidence_invalid(format!("{what}: {error}")))
}

/// What a laned apply needs, from the verified stage as it stands now.
///
/// `None` for a stage that is not in the lane. For a publication stage every
/// step re-derives from the evidence bytes the verified stage returned, in
/// this order, and refuses with a closed code at the first that fails: the
/// source is one this build knows; the name may be taken by a publication;
/// the publication's record, its countersignature and its manifest verify
/// again (the offer window is not checked: it governs serving and pulling, and
/// the bytes were verified inside it); the record's closure equals the stage's
/// tree digest; the publisher is a signer this node trusts now; the lane
/// policy loads as it is now. The lane marker is built from those, never from
/// the stage record's say-so alone.
pub(crate) fn prepare_apply_lane(
    staged: &StagedBundle,
    files: &BTreeMap<String, Vec<u8>>,
    authority: &InstallAuthority<'_>,
) -> Result<Option<LaneInstall>> {
    let record = &staged.record;
    let name = record.bundle_name.as_str();
    match record.source.kind.as_str() {
        staging::SOURCE_REMOTE => return Ok(None),
        staging::SOURCE_PUBLICATION => {}
        other => {
            return Err(evidence_invalid(format!(
                "the stage's source `{other}` is not one this node knows; nothing is admitted from it"
            )));
        }
    }

    refuse_name_held(name, authority)?;

    let (Some(record_hash), Some(pinned_node), Some(manifest_hash)) = (
        record.source.publication_record_hash.as_deref(),
        record.source.pinned_node.as_deref(),
        record.source.manifest_hash.as_deref(),
    ) else {
        return Err(evidence_invalid(
            "a publication stage names no record hash, pinned node or manifest hash",
        ));
    };
    let pins = Pins {
        node: pinned_node.to_owned(),
        expect: record_hash.to_owned(),
    };

    let key_doc: PublicKeyDoc = serde_json::from_value(parse_json(
        evidence(files, EVIDENCE_ORIGIN_KEY)?,
        "the origin key document",
    )?)
    .map_err(|error| evidence_invalid(format!("the origin key document: {error}")))?;
    let node_key = crate::remote::config::decode_signing_key(&key_doc.signing_key)
        .map_err(|error| evidence_invalid(format!("the origin signing key: {error:#}")))?;
    if ryeos_engine::trust::compute_fingerprint(&node_key) != pins.node {
        return Err(evidence_invalid(
            "the origin signing key does not hash to the pinned node fingerprint",
        ));
    }

    let mut body = parse_json(
        evidence(files, EVIDENCE_PUBLICATION)?,
        "the publication record",
    )?;
    let manifest_value = parse_json(evidence(files, EVIDENCE_MANIFEST)?, "the manifest")?;
    body.as_object_mut()
        .ok_or_else(|| evidence_invalid("the publication record is not an object"))?
        .insert("manifest".to_owned(), manifest_value);
    let verified = verify_publication_at(&body, &node_key, &pins, None)
        .map_err(|error| evidence_invalid(format!("the publication evidence: {error}")))?;

    if verified.served.record.root.manifest_hash != manifest_hash {
        return Err(evidence_invalid(
            "the record's manifest hash is not the one the stage's record names",
        ));
    }
    if verified.manifest.bundle_name != name {
        return Err(evidence_invalid(format!(
            "the publication is for `{}` and the stage holds `{name}`",
            verified.manifest.bundle_name
        )));
    }
    let files_listed: Vec<staging::TreeFile> = export_response(&verified.manifest)
        .entries
        .iter()
        .map(|entry| staging::TreeFile {
            path: entry.path.clone(),
            hash: entry.hash.clone(),
            size: entry.size,
            mode: entry.mode,
        })
        .collect();
    let closure = staging::tree_digest(name, &files_listed)
        .map_err(|error| evidence_invalid(format!("the publication's closure: {error}")))?;
    if closure != record.tree_digest {
        return Err(evidence_invalid(
            "the publication's file list does not hash to the stage's tree digest",
        ));
    }
    check_publisher_pinned(authority.node_trust_store, &verified.manifest).map_err(|error| {
        evidence_invalid(format!(
            "the publisher is not a signer this node trusts now: {error}"
        ))
    })?;

    // The policy as it is now, through the loader that verifies it. A missing
    // or unreadable section turns the lane off for this apply.
    let table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
    let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
        authority.app_root,
        authority.node_trust_store,
        &table,
    )
    .map_err(|error| {
        refusal(
            ApplyRefusalCode::LanePolicyUnavailable,
            format!("the node's policy generation does not load: {error:#}"),
        )
    })?;
    let grants = grants_from_snapshot(&snapshot, name).map_err(|error| {
        refusal(
            ApplyRefusalCode::LanePolicyUnavailable,
            format!("the node has no usable publication_lane policy: {error:#}"),
        )
    })?;

    let marker = ryeos_engine::trust::BundleLane::publication(
        publisher_fingerprint(&verified.manifest),
        pins.expect.clone(),
        pins.node.clone(),
        verified.manifest.bundle_version.clone(),
    );
    marker
        .validate_shape()
        .map_err(|error| evidence_invalid(format!("the lane marker: {error}")))?;
    Ok(Some(LaneInstall { marker, grants }))
}

/// A publication never takes `core` or a name a native install holds. A name
/// held by a laned bundle is left to the install's own replacement guard.
fn refuse_name_held(name: &str, authority: &InstallAuthority<'_>) -> Result<()> {
    let name_refused = |detail: String| refusal(ApplyRefusalCode::LaneNameRefused, detail);
    if name == "core" {
        return Err(name_refused(
            "the `core` bundle is never installed through a publication".to_owned(),
        ));
    }
    let records = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root: authority.app_root,
        trust_store: authority.node_trust_store,
    }
    .load_bundle_section()
    .context("load current bundle registrations to check the name")?;
    if records
        .iter()
        .any(|record| record.name == name && record.lane.is_none())
    {
        return Err(name_refused(format!(
            "bundle '{name}' is installed on this node by a native install; a publication never takes or replaces a natively installed name"
        )));
    }
    Ok(())
}

/// The lane's part of an apply report: what was admitted and lifted, from the
/// policy as it stood at this apply.
pub(crate) fn applied_lane_report(lane: &LaneInstall) -> Value {
    json!({
        "name": lane.marker.name,
        "publisher_fingerprint": lane.marker.publisher_fingerprint,
        "record_hash": lane.marker.record_hash,
        "node_fingerprint": lane.marker.node_fingerprint,
        "bundle_version": lane.marker.bundle_version,
        "lifted": {
            "invoked": lane.grants.invoked,
            "kinds": lane.grants.kinds,
            "facets": lane.grants.facets.iter().map(|facet| facet.name()).collect::<Vec<_>>(),
        },
        "receipt": lane_receipt(
            &lane.marker.publisher_fingerprint,
            &lane.marker.node_fingerprint,
            &lane.grants,
        ),
    })
}

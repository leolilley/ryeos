//! Operational stopped-node update from one pinned bundle catalog remote.
//!
//! Network authentication, closure transfer, and publication-policy admission
//! are kept behind a purpose-owned authority.  This coordinator owns the
//! security-sensitive ordering: resolve an immutable coordinate, fetch and
//! admit the *complete* prospective set, explicitly load offline operator
//! custody, and only then enter the stopped-node filesystem transaction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use ryeos_app::bundle_publication::consumer::{ExactSetPlan, ResolvedSetCoordinate};
use ryeos_app::bundle_set_transaction::StoppedBundleSetApplyRequest;
use ryeos_bundle_publication_contract::{
    MigrationDecision, NODE_BUNDLE_SELECTION_KIND, NODE_BUNDLE_SELECTION_SCHEMA,
    NodeBundleSelection,
};
use ryeos_state::objects::Attestation;
use serde::{Deserialize, Serialize};

/// A caller selects either an authenticated channel head with an optional
/// anti-race expectation, or one fully immutable catalog coordinate.  There is
/// deliberately no "latest", legacy ref, or individual-bundle variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "mode", rename_all = "snake_case")]
pub enum BundleSetUpdateSelection {
    Channel {
        set_name: String,
        channel: String,
        expected_catalog_publication_attestation_hash: Option<String>,
    },
    Exact {
        catalog_publication_attestation_hash: String,
        catalog_publication_hash: String,
        catalog_snapshot_hash: String,
        set_attestation_hash: String,
        set_hash: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoppedBundleSetUpdateRequest {
    pub schema: String,
    /// Closed operation discriminator; prevents a generic request document
    /// from being reinterpreted as this mutating stopped-node ceremony.
    pub operation: StoppedBundleSetUpdateOperation,
    /// Name from the node's signed operator remote configuration, never a URL.
    pub catalog_remote: String,
    pub catalog_namespace: String,
    pub selection: BundleSetUpdateSelection,
    /// Explicit operator custody. Environment/default-key discovery is not
    /// allowed for this mutating operation.
    pub operator_signing_key: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppedBundleSetUpdateOperation {
    StoppedExactSetUpdate,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoppedBundleSetUpdateReport {
    pub catalog_remote: String,
    pub catalog_namespace: String,
    pub coordinate: ResolvedSetCoordinateReport,
    pub selection_hash: String,
    pub installed_set_digest: String,
    pub actions: ExactSetActionCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedSetCoordinateReport {
    pub catalog_publication_attestation_hash: String,
    pub catalog_publication_hash: String,
    pub catalog_snapshot_hash: String,
    pub set_attestation_hash: String,
    pub set_hash: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExactSetActionCounts {
    pub add: usize,
    pub replace: usize,
    pub remove: usize,
    pub keep: usize,
}

/// Product of remote resolution, exact closure transfer, current policy/trust
/// verification, complete prospective admission, and offline completion
/// signing. Implementations must use `consumer::{resolve_set_channel,
/// verify_curated_set, plan_fetch, plan_exact_set,
/// admit_node_bundle_selection}` and `prepare_bundle_set_init_completion`.
pub struct PreparedStoppedBundleSetUpdate {
    pub coordinate: ResolvedSetCoordinate,
    pub plan: ExactSetPlan,
    pub apply: StoppedBundleSetApplyRequest,
    pub installed_set_digest: String,
}

/// Immutable local deployment authority produced with the operator key. Both
/// values are CAS-ready; callers store them under the returned hashes before
/// invoking consumer admission.
pub struct OfflineDeploymentSelection {
    pub selection: NodeBundleSelection,
    pub selection_hash: String,
    pub authorization: Attestation,
    pub authorization_hash: String,
}

pub struct OfflineDeploymentSelectionInput {
    pub target_identity: String,
    pub substrate_image_digest: String,
    pub substrate_protocol: u32,
    pub bundle_publication_policy_section_digest: String,
    pub node_policy_generation_digest: String,
    pub expected_active_selection: Option<String>,
    pub issued_at: String,
}

/// Load the exact stopped-node consumer authority from the fully verified
/// init-completion fence, current signed policy generation, and pinned trust.
/// No daemon state, private node key, environment trust, or caller key is used.
pub fn load_current_consumer_publication_policy(
    app_root: &Path,
    catalog_namespace: &str,
) -> anyhow::Result<ryeos_app::bundle_publication::consumer::CurrentConsumerPublicationPolicy> {
    let completion = crate::verify_init_completion(app_root)?
        .context("node has no verified initialization completion")?;
    let config_root = app_root.join(ryeos_engine::AI_DIR).join("config");
    let trust_store = ryeos_engine::trust::TrustStore::load(None, &config_root)
        .context("load pinned consumer trust store")?;
    let table = ryeos_app::node_policy::NodePolicyTable::new();
    let generation =
        ryeos_app::node_policy::generation::load_policy_generation(app_root, &trust_store, &table)
            .context("load current signed node-policy generation")?;
    if generation.digest() != completion.policy_generation_digest {
        bail!("consumer policy generation differs from verified init completion");
    }
    let snapshot = ryeos_app::node_policy::compile_generation(
        app_root,
        &table,
        &generation,
        &completion.node_fingerprint,
    )
    .context("compile verified consumer node-policy snapshot")?;
    ryeos_app::bundle_publication::consumer::CurrentConsumerPublicationPolicy::from_verified_snapshot(
        &snapshot,
        &trust_store,
        catalog_namespace,
        &completion.operator_fingerprint,
    )
}

/// Reconstruct the exact installed generation map through the active
/// selection's immutable CAS chain. Once a published selection exists, the
/// mutable registry is never treated as generation authority.
pub fn load_active_published_set(
    app_root: &Path,
    cas: &lillux::CasStore,
) -> anyhow::Result<
    Option<(
        ryeos_app::bundle_set_transaction::ActiveBundleSelection,
        NodeBundleSelection,
        ryeos_bundle_publication_contract::BundleSet,
    )>,
> {
    let Some(active) =
        ryeos_app::bundle_set_transaction::ActiveBundleSelectionStore::new(app_root).load()?
    else {
        return Ok(None);
    };
    let selection_value = ryeos_app::bundle_publication::PublicationObjectReader::get_object(
        cas,
        &active.selection_hash,
    )?
    .context("active bundle selection is absent from local CAS")?;
    let selection = NodeBundleSelection::from_current_value(&selection_value)?;
    let set_value = ryeos_app::bundle_publication::PublicationObjectReader::get_object(
        cas,
        &selection.bundle_set_hash,
    )?
    .context("active bundle set is absent from local CAS")?;
    let set = ryeos_bundle_publication_contract::BundleSet::from_current_value(&set_value)?;
    if set.substrate_protocol != selection.substrate_protocol {
        bail!("active selection and retained bundle set disagree on substrate protocol");
    }
    Ok(Some((active, selection, set)))
}

/// Retain the node's actual runtime CAS directory for this stopped-node
/// operation. Do not create a second, invisible CAS at `.ai/objects`, and do
/// not re-resolve mutable directory names between network import and proof.
pub fn open_stopped_bundle_cas(app_root: &Path) -> anyhow::Result<lillux::CasStore> {
    let app =
        lillux::PinnedDirectory::open(app_root)?.context("bundle update app root is absent")?;
    let ai = app
        .open_child_directory(std::ffi::OsStr::new(ryeos_engine::AI_DIR))?
        .context("bundle update requires an initialized .ai directory")?;
    let state = ai.open_or_create_child(std::ffi::OsStr::new("state"), 0o700)?;
    let objects = state.open_or_create_child(std::ffi::OsStr::new("objects"), 0o700)?;
    Ok(lillux::CasStore::from_pinned_root(objects))
}

pub fn installed_generation_map(
    active: Option<&ryeos_bundle_publication_contract::BundleSet>,
) -> BTreeMap<String, String> {
    active
        .into_iter()
        .flat_map(|set| set.entries.iter())
        .map(|entry| (entry.bundle_name.clone(), entry.generation_hash.clone()))
        .collect()
}

/// Sign the exact local deployment decision. This is intentionally separate
/// from publisher authorization: only the explicitly supplied operator key
/// can authorize a published set for this node and policy generation.
pub fn prepare_offline_deployment_selection(
    operator_key_path: &Path,
    coordinate: &ResolvedSetCoordinate,
    input: OfflineDeploymentSelectionInput,
) -> anyhow::Result<OfflineDeploymentSelection> {
    require_explicit_private_key(operator_key_path)?;
    let signer = ryeos_app::identity::NodeIdentity::load(operator_key_path)
        .context("load explicit offline deployment-selection signer")?;
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&signer);
    let selection = NodeBundleSelection {
        schema: NODE_BUNDLE_SELECTION_SCHEMA.to_owned(),
        kind: NODE_BUNDLE_SELECTION_KIND.to_owned(),
        target_node_or_app_root_identity: input.target_identity,
        substrate_image_digest: input.substrate_image_digest,
        substrate_protocol: input.substrate_protocol,
        bundle_set_hash: coordinate.set_hash.clone(),
        curated_set_attestation_hash: Some(coordinate.set_attestation_hash.clone()),
        bundle_publication_policy_section_digest: input.bundle_publication_policy_section_digest,
        node_policy_generation_digest: input.node_policy_generation_digest,
        expected_active_selection: input.expected_active_selection,
        migration_decision: MigrationDecision::None,
    };
    selection.validate()?;
    let selection_value = selection.to_value()?;
    let selection_hash =
        lillux::cas::sha256_hex(lillux::canonical_json(&selection_value)?.as_bytes());
    let authorization = Attestation::unsigned(
        selection_hash.clone(),
        ryeos_app::bundle_publication::consumer::DEPLOYMENT_SELECTION_CLAIM.to_owned(),
        ryeos_app::bundle_publication::consumer::BUNDLE_DEPLOYMENT_POLICY.to_owned(),
        input.issued_at,
        None,
        serde_json::json!({
            "catalog_publication_attestation_hash": coordinate.catalog_publication_attestation_hash,
            "catalog_publication_hash": coordinate.catalog_publication_hash,
            "catalog_snapshot_hash": coordinate.catalog_snapshot_hash,
            "set_attestation_hash": coordinate.set_attestation_hash,
            "set_hash": coordinate.set_hash,
        }),
    )
    .sign(&signer)?;
    let authorization_value = serde_json::to_value(&authorization)?;
    let authorization_hash =
        lillux::cas::sha256_hex(lillux::canonical_json(&authorization_value)?.as_bytes());
    Ok(OfflineDeploymentSelection {
        selection,
        selection_hash,
        authorization,
        authorization_hash,
    })
}

pub trait StoppedBundleSetUpdateAuthority {
    /// Resolve/fetch/verify/admit while retaining only exact CAS identities.
    /// The key path is explicit so an implementation cannot silently borrow
    /// daemon or publisher signing custody.
    async fn prepare(
        &self,
        app_root: &Path,
        request: &StoppedBundleSetUpdateRequest,
    ) -> anyhow::Result<PreparedStoppedBundleSetUpdate>;

    /// Recheck the admitted immutable roots from the transaction journal at
    /// the last boundary before local mutation.
    fn admit_journal(
        &self,
        prepared: &PreparedStoppedBundleSetUpdate,
        journal: &ryeos_app::bundle_set_transaction::BundleSetJournal,
    ) -> anyhow::Result<()>;
}

pub async fn update_stopped_bundle_set(
    app_root: &Path,
    request: StoppedBundleSetUpdateRequest,
    authority: &impl StoppedBundleSetUpdateAuthority,
) -> anyhow::Result<StoppedBundleSetUpdateReport> {
    validate_request(&request)?;
    require_explicit_private_key(&request.operator_signing_key)?;
    let prepared = authority
        .prepare(app_root, &request)
        .await
        .context("prepare complete prospective bundle-set update")?;
    bind_requested_coordinate(&request.selection, &prepared.coordinate)?;
    if !prepared.plan.requires_stopped_node
        || prepared.plan.set_hash != prepared.coordinate.set_hash
        || prepared.plan.set_attestation_hash != prepared.coordinate.set_attestation_hash
        || prepared.apply.selection_hash.len() != 64
    {
        bail!("prepared update does not bind one exact stopped-node selection");
    }
    let selection_hash = prepared.apply.selection_hash.clone();
    let mut counts = ExactSetActionCounts::default();
    for entry in &prepared.plan.entries {
        use ryeos_app::bundle_publication::consumer::ExactSetAction;
        match entry.action {
            ExactSetAction::Add => counts.add += 1,
            ExactSetAction::Replace => counts.replace += 1,
            ExactSetAction::Remove => counts.remove += 1,
            ExactSetAction::Keep => counts.keep += 1,
        }
    }
    ryeos_node_apply(app_root, prepared.apply.clone(), |journal| {
        authority.admit_journal(&prepared, journal)
    })?;
    Ok(StoppedBundleSetUpdateReport {
        catalog_remote: request.catalog_remote,
        catalog_namespace: request.catalog_namespace,
        coordinate: coordinate_report(prepared.coordinate),
        selection_hash,
        installed_set_digest: prepared.installed_set_digest,
        actions: counts,
    })
}

fn bind_requested_coordinate(
    requested: &BundleSetUpdateSelection,
    resolved: &ResolvedSetCoordinate,
) -> anyhow::Result<()> {
    match requested {
        BundleSetUpdateSelection::Channel {
            expected_catalog_publication_attestation_hash: Some(expected),
            ..
        } if expected != &resolved.catalog_publication_attestation_hash => {
            bail!("resolved catalog head differs from the caller's expected head")
        }
        BundleSetUpdateSelection::Exact {
            catalog_publication_attestation_hash,
            catalog_publication_hash,
            catalog_snapshot_hash,
            set_attestation_hash,
            set_hash,
        } if catalog_publication_attestation_hash
            != &resolved.catalog_publication_attestation_hash
            || catalog_publication_hash != &resolved.catalog_publication_hash
            || catalog_snapshot_hash != &resolved.catalog_snapshot_hash
            || set_attestation_hash != &resolved.set_attestation_hash
            || set_hash != &resolved.set_hash =>
        {
            bail!("remote result differs from the caller's exact catalog coordinate")
        }
        _ => Ok(()),
    }
}

fn ryeos_node_apply(
    app_root: &Path,
    request: StoppedBundleSetApplyRequest,
    admit: impl Fn(&ryeos_app::bundle_set_transaction::BundleSetJournal) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    crate::bundle_set_apply::apply_stopped_bundle_set(app_root, request, admit)
}

fn validate_request(request: &StoppedBundleSetUpdateRequest) -> anyhow::Result<()> {
    if request.schema != "ryeos.bundle-set-update-request.v1" {
        bail!("unsupported bundle-set update request schema");
    }
    for (label, value) in [
        ("catalog remote", request.catalog_remote.as_str()),
        ("catalog namespace", request.catalog_namespace.as_str()),
    ] {
        if value.is_empty() || value.len() > 128 || value.chars().any(char::is_whitespace) {
            bail!("{label} is empty, oversized, or contains whitespace");
        }
    }
    match &request.selection {
        BundleSetUpdateSelection::Channel {
            set_name,
            channel,
            expected_catalog_publication_attestation_hash,
        } => {
            if set_name.is_empty() || channel.is_empty() {
                bail!("channel selection requires an exact set name and channel");
            }
            if let Some(hash) = expected_catalog_publication_attestation_hash {
                require_hash("expected catalog head", hash)?;
            }
        }
        BundleSetUpdateSelection::Exact {
            catalog_publication_attestation_hash,
            catalog_publication_hash,
            catalog_snapshot_hash,
            set_attestation_hash,
            set_hash,
        } => {
            for (label, hash) in [
                (
                    "catalog publication attestation",
                    catalog_publication_attestation_hash,
                ),
                ("catalog publication", catalog_publication_hash),
                ("catalog snapshot", catalog_snapshot_hash),
                ("set attestation", set_attestation_hash),
                ("set", set_hash),
            ] {
                require_hash(label, hash)?;
            }
        }
    }
    Ok(())
}

fn require_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{label} must be a lowercase SHA-256 digest");
    }
    Ok(())
}

fn require_explicit_private_key(path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute() {
        bail!("operator signing key path must be absolute");
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect explicit operator signing key {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("operator signing key must be a regular non-symlink file");
    }
    Ok(())
}

fn coordinate_report(value: ResolvedSetCoordinate) -> ResolvedSetCoordinateReport {
    ResolvedSetCoordinateReport {
        catalog_publication_attestation_hash: value.catalog_publication_attestation_hash,
        catalog_publication_hash: value.catalog_publication_hash,
        catalog_snapshot_hash: value.catalog_snapshot_hash,
        set_attestation_hash: value.set_attestation_hash,
        set_hash: value.set_hash,
    }
}

#[cfg(test)]
mod cas_tests {
    use super::*;

    #[test]
    fn bundle_import_uses_runtime_cas_and_retains_its_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".ai")).unwrap();
        let cas = open_stopped_bundle_cas(temp.path()).unwrap();
        let runtime_cas = temp.path().join(".ai/state/objects");
        assert_eq!(cas.root(), runtime_cas);
        let retained = temp.path().join(".ai/state/retained-objects");
        std::fs::rename(&runtime_cas, &retained).unwrap();
        std::fs::create_dir(&runtime_cas).unwrap();
        let object = serde_json::json!({"test": "retained authority"});
        let stored = cas.put_object(&object).unwrap();
        assert_eq!(cas.get_object(&stored.hash).unwrap(), Some(object.clone()));
        assert_eq!(
            lillux::CasStore::new(retained)
                .get_object(&stored.hash)
                .unwrap(),
            Some(object)
        );
        assert!(
            lillux::CasStore::new(runtime_cas)
                .get_object(&stored.hash)
                .unwrap()
                .is_none()
        );
        assert!(!temp.path().join(".ai/objects").exists());
    }

    #[cfg(unix)]
    #[test]
    fn bundle_import_refuses_symlinked_runtime_state() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".ai")).unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join(".ai/state")).unwrap();
        assert!(open_stopped_bundle_cas(temp.path()).is_err());
        assert!(!outside.path().join("objects").exists());
    }
}

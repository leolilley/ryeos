//! Read-only derivation of the consumer qualification context.
//!
//! A runtime-qualification policy must carry, in its verifier parameters, the
//! six digests that the admitted external candidate use will later reproduce.
//! Authoring them by hand would make the policy a selection; this operation
//! derives them from the same admitted Worker source, environment and runtime
//! product that qualification itself uses, and emits them as data. It writes no
//! node state: the staged content publication is dropped unpublished.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_INPUT_RECORD_NAME, CONSUMER_VERIFIER_REMOTE_NAME, ConsumerArchiveBudget,
    ConsumerRuntimeVerifierSelection,
};
use ryeos_external_execution_contract::staging_package::GuestStagingEntry;
use ryeos_state::external_content::products::composition::{
    ExternalProductSlotDeclaration, ProductRelationship, ProductRelationships,
};
use ryeos_state::external_content::products::qualification::ProductQualificationConsumerExecutionContext;
use ryeos_state::external_content::products::qualification_variants::{
    NodeGuarantees, QualificationVariant, QualificationVariants, require_policy_guarantees,
};
use ryeos_state::external_content::qualification_consumer_record::MAX_CONSUMER_INPUT_RECORD_BYTES;
use ryeos_state::objects::{
    ExternalContentKind, ExternalContentManifestEntryKind, ExternalContentRealization,
    ExternalContentRealizationSet,
};
use serde::{Deserialize, Serialize};

use super::product_qualification::{
    AdmittedUseInputs, ProductArchiveMember, RemoteVerifierFacts, RemoteVerifierLane,
    load_product_archive_members, load_runtime_manifest_identity,
    prepare_current_bundle_consumer_content_inputs, realize_runtime_slot, remote_verifier_facts,
    require_unguarded_resolution, resolve_consumer_definition_in_generation,
    resolve_current_bundle_qualification_policy,
};
use crate::handler_context::HandlerContext;
use crate::state::AppState;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;

/// Inputs name the signed variant set, the signed relationship Config
/// that selects the runtime product, and the exact manifest of the runtime
/// product the consumer will be handed. The variant set lists each accepted
/// policy (worker, environment, worker execution and runtime declaration)
/// with the guarantees it requires; the guarantees this node reports select
/// exactly one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerQualificationContextRequest {
    pub variants_ref: String,
    pub relationship_ref: String,
    pub subject_manifest_hash: String,
}

impl ConsumerQualificationContextRequest {
    fn validate(&self) -> anyhow::Result<()> {
        if !lillux::valid_hash(&self.subject_manifest_hash) {
            bail!("subject_manifest_hash must be a 64-character lowercase hex digest");
        }
        for (field, value) in [
            ("variants_ref", &self.variants_ref),
            ("relationship_ref", &self.relationship_ref),
        ] {
            if value.is_empty() {
                bail!("{field} must not be empty");
            }
        }
        Ok(())
    }
}

/// A probe-lane qualification policy (one whose remote scenarios ship a probe
/// input to a restored guest, with no consumer Worker) has no Worker, variant
/// set or relationship to derive a use context from; its snapshot qualification
/// binding still needs, per remote scenario, the verifier selection the launch
/// will require. This form derives only those selections, from the same
/// prepared verifier sources the launch uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeVerifierSelectionRequest {
    pub policy_ref: String,
    pub subject_manifest_hash: String,
}

impl ProbeVerifierSelectionRequest {
    fn validate(&self) -> anyhow::Result<()> {
        if !lillux::valid_hash(&self.subject_manifest_hash) {
            bail!("subject_manifest_hash must be a 64-character lowercase hex digest");
        }
        if self.policy_ref.is_empty() {
            bail!("policy_ref must not be empty");
        }
        Ok(())
    }
}

/// `consumer_verifier_selections` has the same meaning and file use as in
/// [`ConsumerQualificationContextResponse`].
#[derive(Debug, Clone, Serialize)]
pub struct ProbeVerifierSelectionResponse {
    pub policy_ref: String,
    pub requires_guarantees: Vec<lillux::process_control::CapabilityRequirement>,
    pub subject_manifest_hash: String,
    pub consumer_verifier_selections: BTreeMap<String, ConsumerRuntimeVerifierSelection>,
}

/// The two request forms of the derivation service, told apart by their
/// closed field sets.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum QualificationContextDerivation {
    Worker(ConsumerQualificationContextRequest),
    Probe(ProbeVerifierSelectionRequest),
}

pub async fn derive_any(
    state: Arc<AppState>,
    context: HandlerContext,
    request: QualificationContextDerivation,
) -> anyhow::Result<serde_json::Value> {
    match request {
        QualificationContextDerivation::Worker(request) => Ok(serde_json::to_value(
            derive(state, context, request).await?,
        )?),
        QualificationContextDerivation::Probe(request) => {
            crate::operator_authority::require_admitted_operator(&state, &context)?;
            request.validate()?;
            let response = tokio::task::spawn_blocking(move || {
                let reported = state.isolation.reported_guarantees();
                derive_probe_for_guarantees(
                    &state,
                    &request,
                    NodeGuarantees::new(&reported, state.isolation.capability_floor()),
                )
            })
            .await
            .context("probe verifier selection task stopped")??;
            Ok(serde_json::to_value(response)?)
        }
    }
}

fn derive_probe_for_guarantees(
    state: &AppState,
    request: &ProbeVerifierSelectionRequest,
    guarantees: NodeGuarantees<'_>,
) -> anyhow::Result<ProbeVerifierSelectionResponse> {
    require_unguarded_resolution("probe verifier selection derivation")?;
    let policy_source = resolve_current_bundle_qualification_policy(state, &request.policy_ref)?;
    ensure!(
        policy_source.policy.consumer_execution_context.is_none(),
        "qualification policy {} has a consumer execution context; derive it from its variant set and relationship",
        request.policy_ref
    );
    require_policy_guarantees(
        &policy_source.policy.requires_guarantees,
        guarantees,
        &request.policy_ref,
    )?;
    let mut consumer_verifier_selections = BTreeMap::new();
    for (scenario_id, facts) in
        remote_verifier_facts(state, &policy_source, &request.subject_manifest_hash)?
    {
        ensure!(
            facts.lane == RemoteVerifierLane::Probe,
            "remote scenario {scenario_id} of {} is not a probe scenario",
            request.policy_ref
        );
        // The probe delivery ships the record, the verifier and the signed
        // archive members; no retained product enters it.
        let archive_budget = archive_budget_for(&[], &facts)
            .with_context(|| format!("remote scenario {scenario_id} archive budget"))?;
        consumer_verifier_selections.insert(
            scenario_id,
            ConsumerRuntimeVerifierSelection {
                scenario_source_digest: facts.scenario_source_digest,
                verifier_artifact_hash: facts.verifier_artifact_hash,
                archive_budget,
            },
        );
    }
    ensure!(
        !consumer_verifier_selections.is_empty(),
        "qualification policy {} selects no remote verifier",
        request.policy_ref
    );
    Ok(ProbeVerifierSelectionResponse {
        policy_ref: request.policy_ref.clone(),
        requires_guarantees: policy_source.policy.requires_guarantees.clone(),
        subject_manifest_hash: request.subject_manifest_hash.clone(),
        consumer_verifier_selections,
    })
}

/// Size of the tar block every archive header, data run and trailer is
/// rounded to.
const TAR_BLOCK_BYTES: u64 = 512;

/// A tar header name or link target longer than this is carried in an extra
/// GNU long-name member.
const TAR_INLINE_NAME_BYTES: usize = 100;

/// The emitted document. `verifier_parameters` is the fragment to merge into
/// the policy's `verifier_parameters`; `consumer_execution_context` is the
/// signed context the digests were derived under. `consumer_verifier_selections`
/// maps each scenario id that selects a remote verifier to the protected
/// selection its consumer launch will require; that object, written alone to
/// a file, is the `--consumer-verifiers` input of
/// `snapshot-qualification-compose`. The digests are exact; `archive_budget` is
/// a proposed allowance for the operator to sign into the profile.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumerQualificationContextResponse {
    pub policy_ref: String,
    pub relationship_name: String,
    /// The guarantees the selected variant required of this node.
    pub requires_guarantees: Vec<lillux::process_control::CapabilityRequirement>,
    pub subject_manifest_hash: String,
    pub bundle_generation_identity: String,
    pub verifier_parameters: serde_json::Value,
    pub consumer_execution_context: ProductQualificationConsumerExecutionContext,
    pub consumer_verifier_selections: BTreeMap<String, ConsumerRuntimeVerifierSelection>,
}

pub async fn derive(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ConsumerQualificationContextRequest,
) -> anyhow::Result<ConsumerQualificationContextResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || derive_blocking(&state, &request))
        .await
        .context("consumer qualification context task stopped")?
}

fn derive_blocking(
    state: &AppState,
    request: &ConsumerQualificationContextRequest,
) -> anyhow::Result<ConsumerQualificationContextResponse> {
    let reported = state.isolation.reported_guarantees();
    derive_for_guarantees(
        state,
        request,
        NodeGuarantees::new(&reported, state.isolation.capability_floor()),
    )
}

/// Derive for the variant selected by `guarantees`, the guarantees this node
/// reports; kept separate so selection is exercised against any reported set.
fn derive_for_guarantees(
    state: &AppState,
    request: &ConsumerQualificationContextRequest,
    guarantees: NodeGuarantees<'_>,
) -> anyhow::Result<ConsumerQualificationContextResponse> {
    require_unguarded_resolution("consumer qualification context derivation")?;
    let variant = select_variant(state, &request.variants_ref, guarantees)?;
    let policy_source = resolve_current_bundle_qualification_policy(state, &variant.policy_ref)?;
    // The variant set and the policy it names must agree on what they
    // require, so a policy cannot be selected under weaker guarantees than
    // the policy itself declares (or than the set advertised).
    ensure!(
        policy_source.policy.requires_guarantees == variant.requires_guarantees,
        "qualification policy {} declares different required guarantees than the variant set that selected it",
        variant.policy_ref
    );
    require_policy_guarantees(
        &policy_source.policy.requires_guarantees,
        guarantees,
        &variant.policy_ref,
    )?;
    let signed_context = policy_source
        .policy
        .consumer_execution_context
        .clone()
        .context("qualification policy has no consumer execution context")?;
    let relationship =
        resolve_relationship(state, &request.relationship_ref, &variant.relationship_name)?;
    let prepared = prepare_current_bundle_consumer_content_inputs(
        state,
        &policy_source,
        &relationship,
        &request.relationship_ref,
    )?
    .context("qualification policy has no consumer execution context")?;
    let requirement = prepared
        .worker_source
        .profile
        .external_candidate_requirement()?
        .context("consumer Worker has no external candidate requirement")?;
    if requirement.runtime_product_declaration_id != signed_context.product_declaration_id {
        bail!("external Worker runtime product differs from the signed consumer context");
    }
    let slot = prepared
        .runtime_slot()
        .context("consumer Worker declares no runtime product slot")?;
    require_slot_matches_context(slot, &signed_context)?;
    let authority = state.state_store.pinned_state_authority()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let (_executable_hash, entry_count, total_bytes) = load_runtime_manifest_identity(
        &authority,
        limits,
        &request.subject_manifest_hash,
        &requirement.runtime_recipe.executable_relative_path,
    )?;
    let runtime = realize_runtime_slot(
        slot,
        &request.subject_manifest_hash,
        entry_count,
        total_bytes,
    );
    let qualification_use = AdmittedUseInputs {
        requirement: &requirement,
        profile: &prepared.worker_source.profile,
        source: &prepared.worker_source.source,
        worker_literals: &prepared.worker_literals,
        runtime: runtime.clone(),
        environment: &prepared.environment.realizations,
        executable_search: &prepared.environment.definition.executable_search,
        process_environment: &prepared.environment.definition.process_environment,
    }
    .derive()?;
    let product_entries = consumer_product_entries(
        &authority,
        limits,
        &ExternalContentRealizationSet::new(
            prepared
                .worker_literals
                .iter()
                .chain(prepared.environment.realizations.iter())
                .chain(std::iter::once(&runtime))
                .cloned()
                .collect(),
        )?,
    )?;
    let mut consumer_verifier_selections = BTreeMap::new();
    for (scenario_id, facts) in
        remote_verifier_facts(state, &policy_source, &request.subject_manifest_hash)?
    {
        let archive_budget = archive_budget_for(&product_entries, &facts)
            .with_context(|| format!("remote scenario {scenario_id} archive budget"))?;
        consumer_verifier_selections.insert(
            scenario_id,
            ConsumerRuntimeVerifierSelection {
                scenario_source_digest: facts.scenario_source_digest,
                verifier_artifact_hash: facts.verifier_artifact_hash,
                archive_budget,
            },
        );
    }
    // Nothing was published: dropping the staged content leaves node state
    // exactly as it was.
    Ok(ConsumerQualificationContextResponse {
        policy_ref: variant.policy_ref.clone(),
        relationship_name: variant.relationship_name.clone(),
        requires_guarantees: variant.requires_guarantees.clone(),
        subject_manifest_hash: request.subject_manifest_hash.clone(),
        bundle_generation_identity: prepared.definitions.bundle_generation_identity.clone(),
        verifier_parameters: qualification_use.parameters()?,
        consumer_execution_context: signed_context,
        consumer_verifier_selections,
    })
}

/// The archive entries the Worker delivery adds for the retained products, as
/// the delivery's own inventory lists them: per product, a root entry (a
/// directory for a tree), its content entries, and the canonical manifest
/// record `product-NN-manifest.json`, whose bytes are not in the product's
/// `total_bytes`. Entry paths and sizes come from the pinned manifests.
fn consumer_product_entries(
    authority: &ryeos_state::PinnedStateAuthority,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    realizations: &ExternalContentRealizationSet,
) -> anyhow::Result<Vec<GuestStagingEntry>> {
    let mut entries = Vec::new();
    for (index, realization) in realizations.iter().enumerate() {
        entries.extend(product_entries(
            &format!("product-{index:02}"),
            realization,
            load_product_archive_members(authority, limits, &realization.manifest_hash)?,
        ));
    }
    Ok(entries)
}

fn product_entries(
    name: &str,
    realization: &ExternalContentRealization,
    (members, manifest_bytes): (Vec<ProductArchiveMember>, u64),
) -> Vec<GuestStagingEntry> {
    let mut entries = Vec::new();
    match realization.kind {
        ExternalContentKind::File => entries.push(sized_file(name, realization.total_bytes)),
        ExternalContentKind::Tree => {
            entries.push(GuestStagingEntry::Directory {
                path: name.into(),
                mode: 0o755,
            });
            for member in members {
                let path = format!("{name}/{}", member.path);
                entries.push(match member.kind {
                    ExternalContentManifestEntryKind::File => sized_file(&path, member.size),
                    ExternalContentManifestEntryKind::Dir => {
                        GuestStagingEntry::Directory { path, mode: 0o755 }
                    }
                    ExternalContentManifestEntryKind::Symlink => GuestStagingEntry::Symlink {
                        path,
                        target: member.target.unwrap_or_default(),
                    },
                });
            }
        }
    }
    entries.push(sized_file(&format!("{name}-manifest.json"), manifest_bytes));
    entries
}

/// Only the path and size of a regular entry shape the archive; mode and
/// digest are fixed-width in the tar encoding.
fn sized_file(path: &str, bytes: u64) -> GuestStagingEntry {
    GuestStagingEntry::RegularFile {
        path: path.into(),
        mode: 0o400,
        bytes,
        sha256: "0".repeat(64),
    }
}

/// The allowance proposed for one scenario: the exact archive inventory its
/// delivery builds, with the accepted input record, whose size is only known
/// per occurrence, charged at its contract maximum. The Worker lane ships the
/// record, the verifier and the retained products; the probe lane ships the
/// record, the verifier and the signed archive members. The operator signs
/// this allowance into the profile; the daemon does not select it.
fn archive_budget_for(
    product_entries: &[GuestStagingEntry],
    facts: &RemoteVerifierFacts,
) -> anyhow::Result<ConsumerArchiveBudget> {
    let mut entries = vec![
        sized_file(
            CONSUMER_INPUT_RECORD_NAME,
            MAX_CONSUMER_INPUT_RECORD_BYTES as u64,
        ),
        sized_file(CONSUMER_VERIFIER_REMOTE_NAME, facts.verifier_bytes),
    ];
    match facts.lane {
        RemoteVerifierLane::Worker => entries.extend_from_slice(product_entries),
        RemoteVerifierLane::Probe => entries.extend(
            facts
                .archive_members
                .iter()
                .map(|(name, bytes)| sized_file(name, *bytes)),
        ),
    }
    let mut regular_bytes = 0u64;
    for entry in &entries {
        if let GuestStagingEntry::RegularFile { bytes, .. } = entry {
            regular_bytes = regular_bytes
                .checked_add(*bytes)
                .context("archive regular byte total overflow")?;
        }
    }
    ConsumerArchiveBudget::new(entries.len(), regular_bytes, tar_framed_bytes(&entries)?)
}

/// Exact length of the tar the consumer archive writer produces for these
/// entries: a header block per entry, a GNU long-name member for a path or
/// link target over the inline limit, file data rounded up to whole blocks,
/// and the two-block trailer.
fn tar_framed_bytes(entries: &[GuestStagingEntry]) -> anyhow::Result<u64> {
    let long_member = |name_bytes: usize| -> u64 {
        if name_bytes > TAR_INLINE_NAME_BYTES {
            TAR_BLOCK_BYTES + blocks(name_bytes as u64 + 1)
        } else {
            0
        }
    };
    let mut total = 2 * TAR_BLOCK_BYTES;
    for entry in entries {
        let entry_bytes = TAR_BLOCK_BYTES
            + long_member(entry.path().len())
            + match entry {
                GuestStagingEntry::RegularFile { bytes, .. } => blocks(*bytes),
                GuestStagingEntry::Directory { .. } => 0,
                GuestStagingEntry::Symlink { target, .. } => long_member(target.len()),
            };
        total = total
            .checked_add(entry_bytes)
            .context("archive framed byte total overflow")?;
    }
    Ok(total)
}

fn blocks(bytes: u64) -> u64 {
    bytes.div_ceil(TAR_BLOCK_BYTES) * TAR_BLOCK_BYTES
}

/// The digests must come from the slot qualification itself realizes: the
/// one the signed context names as the runtime product.
fn require_slot_matches_context(
    slot: &ExternalProductSlotDeclaration,
    context: &ProductQualificationConsumerExecutionContext,
) -> anyhow::Result<()> {
    if slot.id != context.product_declaration_id {
        bail!("consumer runtime slot differs from the signed consumer context product");
    }
    Ok(())
}

/// Read the signed variant set and select the one variant whose required
/// guarantees this node holds.
fn select_variant(
    state: &AppState,
    variants_ref: &str,
    guarantees: NodeGuarantees<'_>,
) -> anyhow::Result<QualificationVariant> {
    state.engine.with_checked_bundle_generation(|generation| {
        let authored =
            resolve_consumer_definition_in_generation(generation, variants_ref, "config")?;
        let set = QualificationVariants::from_value(
            authored
                .composed
                .composed
                .get("qualification_variants")
                .context("variant Config has no qualification_variants")?,
        )?;
        Ok(set.select(guarantees)?.clone())
    })
}

fn resolve_relationship(
    state: &AppState,
    relationship_ref: &str,
    name: &str,
) -> anyhow::Result<ProductRelationship> {
    state.engine.with_checked_bundle_generation(|generation| {
        let authored =
            resolve_consumer_definition_in_generation(generation, relationship_ref, "config")?;
        let relationships = ProductRelationships::from_value(
            authored
                .composed
                .composed
                .get("product_relationships")
                .context("product relationship Config has no relationships")?
                .clone(),
        )?;
        Ok(relationships.select(name)?.clone())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use anyhow::{Result, ensure};
    use lillux::crypto::SigningKey;
    use ryeos_engine::trust::{TrustStore, TrustedSigner};
    use ryeos_state::external_execution::admission::ExternalCandidateQualificationUse;
    use ryeos_state::external_execution::admission::test_support;
    use ryeos_state::objects::{
        ExternalContentKind, ExternalContentMode, ExternalContentMountRoot,
        ExternalContentRealization, ExternalContentRealizationSet,
    };

    use ryeos_engine::isolation::GuaranteeHost;

    use super::*;

    fn environment_realization() -> ExternalContentRealization {
        ExternalContentRealization {
            id: "environment".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "f".repeat(64),
            entry_count: 2,
            total_bytes: 12,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "environment".into(),
        }
    }

    fn derive_with(
        environment: &ExternalContentRealizationSet,
    ) -> Result<ExternalCandidateQualificationUse> {
        let requirement = test_support::fixture_requirement();
        let profile = test_support::fixture_profile(&requirement)?;
        let source = test_support::fixture_source_projection();
        let runtime = test_support::fixture_provider_realizations()?
            .iter()
            .next()
            .cloned()
            .context("fixture has no provider realization")?;
        AdmittedUseInputs {
            requirement: &requirement,
            profile: &profile,
            source: &source,
            worker_literals: &ExternalContentRealizationSet::new(vec![])?,
            runtime,
            environment,
            executable_search: &[],
            process_environment: &BTreeMap::new(),
        }
        .derive()
    }

    #[test]
    fn digests_are_deterministic_and_match_the_admitted_use() -> Result<()> {
        let empty = ExternalContentRealizationSet::new(vec![])?;
        let first = derive_with(&empty)?;
        let second = derive_with(&empty)?;
        ensure!(
            first == second,
            "same admitted inputs derived different digests"
        );
        ensure!(
            first == test_support::fixture_qualification_use(&test_support::fixture_requirement())?,
            "derivation differs from the admitted use constructor"
        );
        let parameters = first.parameters()?;
        let context = &parameters["external_candidate_qualification_context"];
        for field in [
            "requirement_digest",
            "profile_hash",
            "source_binding_hash",
            "source_content_manifest_hash",
            "provider_executable_manifest_hash",
            "execution_environment_digest",
        ] {
            ensure!(
                context[field].as_str().is_some_and(lillux::valid_hash),
                "{field} is not a 64-hex digest"
            );
        }
        let with_environment = derive_with(&ExternalContentRealizationSet::new(vec![
            environment_realization(),
        ])?)?;
        ensure!(
            with_environment.parameters()?["external_candidate_qualification_context"]["execution_environment_digest"]
                != context["execution_environment_digest"],
            "environment content did not change the environment digest"
        );
        Ok(())
    }

    #[test]
    fn environment_identity_that_contradicts_the_runtime_is_refused() -> Result<()> {
        let mut conflicting = environment_realization();
        conflicting.id = "codex".into();
        let error = derive_with(&ExternalContentRealizationSet::new(vec![conflicting])?)
            .err()
            .context("conflicting environment identity was accepted")?;
        ensure!(
            error
                .to_string()
                .contains("conflicts with inherited environment"),
            "unexpected refusal: {error}"
        );
        Ok(())
    }

    #[test]
    fn slot_that_is_not_the_signed_runtime_product_is_refused() -> Result<()> {
        let context = ProductQualificationConsumerExecutionContext {
            worker_ref: "worker:codex/external-hosted-authoring".into(),
            product_declaration_id: "guest-runtime".into(),
            environment_ref: "config:codex/environments/external-authoring".into(),
            worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
            environment_binding: "environment".into(),
        };
        let mut slot = ExternalProductSlotDeclaration {
            id: "guest-runtime".into(),
            relationship_ref: "config:codex/guest-runtime-products".into(),
            relationship: "runtime_to_external_authoring_worker".into(),
            kind: ExternalContentKind::Tree,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "codex-guest-runtime".into(),
        };
        require_slot_matches_context(&slot, &context)?;
        slot.id = "other-runtime".into();
        ensure!(
            require_slot_matches_context(&slot, &context).is_err(),
            "a different slot was accepted"
        );
        Ok(())
    }

    fn tree_realization(total_bytes: u64) -> ExternalContentRealization {
        ExternalContentRealization {
            id: "runtime".into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "a".repeat(64),
            entry_count: 2,
            total_bytes,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "runtime".into(),
        }
    }

    fn member(
        path: &str,
        kind: ExternalContentManifestEntryKind,
        size: u64,
    ) -> ProductArchiveMember {
        ProductArchiveMember {
            path: path.into(),
            kind,
            size,
            target: None,
        }
    }

    #[test]
    fn a_tree_product_adds_a_root_entry_and_its_manifest_record() -> Result<()> {
        let entries = product_entries(
            "product-00",
            &tree_realization(700),
            (
                vec![
                    member("bin", ExternalContentManifestEntryKind::Dir, 0),
                    member("bin/runner", ExternalContentManifestEntryKind::File, 700),
                ],
                300,
            ),
        );
        // Two content entries, the tree root, and the manifest record.
        ensure!(entries.len() == 4, "unexpected entries: {entries:?}");
        ensure!(entries[0].path() == "product-00", "tree root is not first");
        ensure!(
            entries[3].path() == "product-00-manifest.json",
            "manifest record is not last"
        );
        let facts = RemoteVerifierFacts {
            lane: RemoteVerifierLane::Worker,
            scenario_source_digest: "b".repeat(64),
            verifier_artifact_hash: "c".repeat(64),
            verifier_bytes: 1000,
            archive_members: BTreeMap::from([("extra".to_owned(), 5000)]),
        };
        let budget = archive_budget_for(&entries, &facts)?;
        ensure!(budget.maximum_entries == 6, "entries: {budget:?}");
        // Product bytes plus the manifest record, the verifier and the record maximum;
        // the Worker lane ships no archive members.
        ensure!(
            budget.maximum_regular_bytes
                == 700 + 300 + 1000 + MAX_CONSUMER_INPUT_RECORD_BYTES as u64,
            "regular bytes: {budget:?}"
        );
        // Six headers and the trailer, with data rounded to whole blocks.
        let expected = 6 * 512
            + 1024
            + blocks(MAX_CONSUMER_INPUT_RECORD_BYTES as u64)
            + blocks(1000)
            + blocks(700)
            + blocks(300);
        ensure!(
            budget.maximum_framed_bytes == expected,
            "framed: {budget:?}"
        );
        Ok(())
    }

    #[test]
    fn a_probe_budget_charges_archive_members_and_no_products() -> Result<()> {
        let facts = RemoteVerifierFacts {
            lane: RemoteVerifierLane::Probe,
            scenario_source_digest: "b".repeat(64),
            verifier_artifact_hash: "c".repeat(64),
            verifier_bytes: 1000,
            archive_members: BTreeMap::from([("extra".to_owned(), 5000)]),
        };
        let products = vec![sized_file("product-00", 1 << 40)];
        let budget = archive_budget_for(&products, &facts)?;
        ensure!(budget.maximum_entries == 3, "entries: {budget:?}");
        ensure!(
            budget.maximum_regular_bytes == 6000 + MAX_CONSUMER_INPUT_RECORD_BYTES as u64,
            "regular bytes: {budget:?}"
        );
        Ok(())
    }

    #[test]
    fn long_names_and_targets_are_charged_their_long_name_members() -> Result<()> {
        let short = tar_framed_bytes(&[sized_file("a", 10)])?;
        let long = tar_framed_bytes(&[sized_file(&"a".repeat(101), 10)])?;
        // Long-name member: its header plus the name and NUL in one block.
        ensure!(long == short + 512 + 512, "long name: {long} vs {short}");
        let link = tar_framed_bytes(&[GuestStagingEntry::Symlink {
            path: "p/l".into(),
            target: "t".repeat(101),
        }])?;
        ensure!(link == 1024 + 512 + 512 + 512, "long target: {link}");
        Ok(())
    }

    #[test]
    fn an_overflowing_or_unbounded_delivery_has_no_budget() {
        let facts = RemoteVerifierFacts {
            lane: RemoteVerifierLane::Worker,
            scenario_source_digest: "b".repeat(64),
            verifier_artifact_hash: "c".repeat(64),
            verifier_bytes: 1,
            archive_members: BTreeMap::new(),
        };
        assert!(archive_budget_for(&[sized_file("product-00", u64::MAX)], &facts).is_err());
        assert!(
            archive_budget_for(&[sized_file("product-00", 5 * 1024 * 1024 * 1024)], &facts)
                .is_err()
        );
    }

    #[test]
    fn request_refuses_a_malformed_manifest_digest() {
        let request = ConsumerQualificationContextRequest {
            variants_ref: "config:test/qualification-variants".into(),
            relationship_ref: "config:codex/guest-runtime-products".into(),
            subject_manifest_hash: "not-a-digest".into(),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn absent_environment_definition_is_refused() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (_state_owner, mut state) = crate::state::test_support::build(root.path())?;
        let core = ryeos_engine::test_support::core_bundle_root();
        let workflow = ryeos_engine::test_support::workflow_bundle_roots();
        let codex = ryeos_engine::test_support::workspace_root().join("bundles/codex");
        let test_bundle = root.path().join("test-bundle");
        let policy_path = test_bundle.join(".ai/config/test/consumer-qualification.yaml");
        fs::create_dir_all(policy_path.parent().unwrap())?;
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let variants_path = test_bundle.join(".ai/config/test/qualification-variants.yaml");
        let variants = r#"category: test
version: "1.0.0"
qualification_variants:
  schema: ryeos.qualification_variants.v1
  variants:
    - policy_ref: config:test/consumer-qualification
      relationship_name: runtime_to_external_authoring_worker
      requires_guarantees: [test:held]
"#;
        fs::write(
            &variants_path,
            lillux::signature::sign_content(variants, &signer, "#", None),
        )?;
        let policy = r#"category: test
version: "1.0.0"
product_qualification_policy:
  schema: ryeos.product_qualification_policy.v5
  verifier_ref: tool:fixtures/qualify_runtime
  requires_guarantees: [test:held]
  subject_declaration_id: guest-runtime
  allowed_claims: [runtime_qualified]
  minimum_verifier_process_settlement: scope_empty
  verifier_parameters: {}
  consumer_execution_context:
    worker_ref: worker:codex/external-hosted-authoring
    product_declaration_id: guest-runtime
    environment_ref: config:codex/environments/absent
    worker_execution_ref: worker_execution:codex/bounded-turn
    environment_binding: environment
"#;
        fs::write(
            &policy_path,
            lillux::signature::sign_content(policy, &signer, "#", None),
        )?;
        let mut roots = vec![core.clone()];
        roots.extend(workflow.iter().cloned());
        roots.push(codex);
        roots.push(test_bundle);
        let mut trust = ryeos_engine::test_support::live_trust_store();
        let verifier = signer.verifying_key();
        trust.extend_from(&TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: lillux::signature::compute_fingerprint(&verifier),
            verifying_key: verifier,
            label: Some("consumer context fixture".into()),
        }]));
        let mut kind_roots = vec![core.join(".ai/node/engine/kinds")];
        kind_roots.extend(
            workflow
                .iter()
                .map(|root| root.join(".ai/node/engine/kinds")),
        );
        let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(&kind_roots, &trust)?;
        let (parsers, _) =
            ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
        let handlers = ryeos_engine::test_support::load_live_handler_registry();
        let dispatcher =
            ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
        let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
        let registered = ["core", "knowledge", "directive", "graph", "codex", "test"]
            .into_iter()
            .zip(roots.iter().cloned())
            .map(
                |(name, canonical_root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                    owned_namespaces: Vec::new(),
                    name: name.to_owned(),
                    canonical_root,
                },
            )
            .collect();
        state.engine = Arc::new(
            ryeos_engine::engine::Engine::new(kinds, dispatcher, roots)
                .with_trust_store(trust.clone())
                .with_node_trust_store(trust)
                .with_composers(composers)
                .with_registered_bundle_roots(registered),
        );
        let request = ConsumerQualificationContextRequest {
            variants_ref: "config:test/qualification-variants".into(),
            relationship_ref: "config:codex/guest-runtime-products".into(),
            subject_manifest_hash: "a".repeat(64),
        };
        // A node reporting no guarantee holds neither fixture variant: the
        // refusal names each candidate and what it lacks.
        let none = derive_blocking(&state, &request)
            .err()
            .context("a node holding no variant's guarantees selected one")?;
        ensure!(
            none.to_string().contains("no qualification variant")
                && none
                    .to_string()
                    .contains("config:test/consumer-qualification")
                && none
                    .to_string()
                    .contains("does not know how to prove test:held"),
            "refusal does not name the candidate and its unmet guarantee: {none:#}"
        );
        let mut held = lillux::process_control::HostCapabilities::default();
        held.provide(
            "test:held",
            lillux::process_control::ProofSource::Measured,
            "test",
            "held by the fixture node",
        );
        let refusal = derive_for_guarantees(&state, &request, NodeGuarantees::new(&held, &[]))
            .err()
            .context("a missing environment definition was admitted")?;
        ensure!(
            refusal
                .to_string()
                .contains("config:codex/environments/absent"),
            "refusal does not name the absent environment: {refusal:#}"
        );
        // The probe form never derives a Worker-lane policy: its use context
        // would be skipped.
        let probe = ProbeVerifierSelectionRequest {
            policy_ref: "config:test/consumer-qualification".into(),
            subject_manifest_hash: "a".repeat(64),
        };
        let refusal = derive_probe_for_guarantees(&state, &probe, NodeGuarantees::new(&held, &[]))
            .err()
            .context("the probe form derived a consumer-context policy")?;
        ensure!(
            refusal
                .to_string()
                .contains("has a consumer execution context"),
            "{refusal:#}"
        );
        Ok(())
    }

    #[test]
    fn the_two_request_forms_are_told_apart_by_their_closed_fields() -> Result<()> {
        let worker: QualificationContextDerivation = serde_json::from_value(serde_json::json!({
            "variants_ref": "config:v", "relationship_ref": "config:r",
            "subject_manifest_hash": "a".repeat(64)}))?;
        ensure!(matches!(worker, QualificationContextDerivation::Worker(_)));
        let probe: QualificationContextDerivation = serde_json::from_value(serde_json::json!({
            "policy_ref": "config:p", "subject_manifest_hash": "a".repeat(64)}))?;
        ensure!(matches!(probe, QualificationContextDerivation::Probe(_)));
        for mixed in [
            serde_json::json!({"policy_ref": "config:p", "variants_ref": "config:v",
                "relationship_ref": "config:r", "subject_manifest_hash": "a".repeat(64)}),
            serde_json::json!({"policy_ref": "config:p"}),
        ] {
            ensure!(
                serde_json::from_value::<QualificationContextDerivation>(mixed.clone()).is_err(),
                "accepted {mixed}"
            );
        }
        let malformed = ProbeVerifierSelectionRequest {
            policy_ref: "config:p".into(),
            subject_manifest_hash: "not-a-digest".into(),
        };
        ensure!(malformed.validate().is_err());
        Ok(())
    }

    fn inspection(
        trusted_process_group: bool,
        cgroup_scope: bool,
        pid_namespace: bool,
    ) -> ryeos_engine::isolation::IsolationInspection {
        use ryeos_engine::isolation::{
            DescendantContainmentReadiness, PidNamespaceContainmentQualification,
        };
        let mut inspection = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring()
            .inspection()
            .clone();
        inspection.filesystem.readable = vec!["{verified_code}".into()];
        inspection.filesystem.writable = vec!["{project}".into()];
        inspection.process_scope_readiness.descendant_containment =
            DescendantContainmentReadiness {
                cgroup_scope,
                pid_namespace: if pid_namespace {
                    PidNamespaceContainmentQualification::qualified_by(
                        lillux::process_control::ProofSource::Measured,
                        "probed",
                    )
                } else {
                    PidNamespaceContainmentQualification::not_probed("not probed")
                },
                trusted_process_group,
            };
        inspection
    }

    #[test]
    fn shipped_codex_variants_are_selected_by_the_guarantees_a_node_reports() -> Result<()> {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/codex/.ai/config/codex/qualification-variants.yaml");
        let document: serde_json::Value = serde_yaml::from_str(&fs::read_to_string(path)?)?;
        let variants = QualificationVariants::from_value(&document["qualification_variants"])?;
        let relationship = |view: &lillux::process_control::HostCapabilities| {
            variants
                .select(NodeGuarantees::new(view, &[]))
                .map(|variant| variant.relationship_name.clone())
                .map_err(|error| error.to_string())
        };
        // Enforced isolation with a qualified containment mechanism.
        for (cgroup_scope, pid_namespace) in [(true, false), (false, true), (true, true)] {
            let view = inspection(false, cgroup_scope, pid_namespace)
                .reported_guarantees(true, &GuaranteeHost::default());
            ensure!(
                relationship(&view) == Ok("runtime_to_external_authoring_worker".to_owned()),
                "contained node: {:?}",
                relationship(&view)
            );
        }
        // Node policy accepting trusted sessions on a node that does not
        // enforce isolation; a qualified mechanism alone is not enforcement.
        let view =
            inspection(true, false, false).reported_guarantees(false, &GuaranteeHost::default());
        ensure!(
            relationship(&view)
                == Ok("runtime_to_external_authoring_worker_trusted_lane".to_owned()),
            "trusted node: {:?}",
            relationship(&view)
        );
        // Neither: enforced without containment, or nothing at all.
        for (enforced, trusted) in [(true, false), (false, false)] {
            let view = inspection(trusted, false, false)
                .reported_guarantees(enforced, &GuaranteeHost::default());
            let refusal = relationship(&view)
                .err()
                .context("a node holding neither selected a variant")?;
            ensure!(
                refusal.contains("no qualification variant")
                    && refusal.contains("config:codex/consumer-qualification-policy-trusted-lane"),
                "{refusal}"
            );
        }
        // Enforced isolation never lets a trusted-session acceptance stand in.
        let view =
            inspection(true, false, false).reported_guarantees(true, &GuaranteeHost::default());
        ensure!(
            relationship(&view).is_err(),
            "enforced uncontained node selected a variant"
        );
        Ok(())
    }
}

//! Recompose installed node bindings against the installed bundles.
//!
//! A payload change (a new lifecycle adapter, restoration verifier, consumer
//! verifier, connector or spec) leaves installed bindings pinning artifacts
//! that are no longer installed; `ryeos node doctor` names each pin through
//! [`crate::node_config::coherence`]. Recompose re-pins exactly those
//! artifact identities, in dependency order (production, then the
//! qualifications that name a production binding, then placements), and
//! carries every fact the operator stated, the credential generation among
//! them: the vault key of a credential is its owner (signer, backend,
//! account) and generation, neither of which a re-pin changes, so the
//! provisioned secret stays valid.
//!
//! What a re-pin cannot decide is refused, typed, before anything is
//! published: an artifact role nothing installed fills, a settings schema the
//! carried settings were not stated under, and consumer selections whose
//! scenario source digests only the running node's derive can supply.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection;
use serde::Serialize;
use serde_json::Value;

use crate::node_config::NodeConfigSnapshot;
use crate::node_config::coherence::{
    self, BindingFinding, FindingSeverity, InstalledArtifactHashes, PlacementCredentialPresence,
};
use crate::node_config::sections::external_execution::InstalledExternalExecutionBinding;
use crate::node_config::sections::external_execution::compose::{
    ProviderArtifactFacts, load_node_identity, publish_signed_document, read_installed_binding,
    repin_placement,
};
use crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding;
use crate::node_config::sections::runtime_snapshot_production::compose::{
    installed_production_binding, repin_production,
};
use crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding;
use crate::node_config::sections::runtime_snapshot_qualification::compose::{
    installed_qualification_binding, repin_qualification,
};
use crate::node_config::sections::{
    external_execution, runtime_snapshot_production, runtime_snapshot_qualification,
};

/// Nothing installed fills an artifact role the binding pins, so there is
/// nothing to re-pin to.
pub const RECOMPOSE_ARTIFACT_NOT_INSTALLED: &str = "recompose_artifact_not_installed";
/// The installed adapter declares a settings schema other than the one the
/// binding's settings were stated under; only the operator can restate them.
pub const RECOMPOSE_SETTINGS_SCHEMA_CHANGED: &str = "recompose_settings_schema_changed";
/// A qualification to recompose selects consumer verifiers and no
/// `--consumer-verifiers` file (the running node's derive output) was given.
pub const RECOMPOSE_CONSUMER_VERIFIERS_REQUIRED: &str = "recompose_consumer_verifiers_required";
/// The `--consumer-verifiers` file has no selection for a scenario the
/// qualification selects.
pub const RECOMPOSE_CONSUMER_SELECTION_MISSING: &str = "recompose_consumer_selection_missing";
/// A named binding is not installed, or a qualification names a production
/// binding that is not.
pub const RECOMPOSE_BINDING_NOT_INSTALLED: &str = "recompose_binding_not_installed";

/// The sections whose bindings pin installed artifacts.
const PINNING_SECTIONS: [&str; 3] = [
    runtime_snapshot_production::SECTION_NAME,
    runtime_snapshot_qualification::SECTION_NAME,
    external_execution::SECTION_NAME,
];

/// One installed binding, named `<section>/<id>` by the operator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BindingRef {
    pub section: String,
    pub id: String,
}

impl BindingRef {
    pub fn parse(value: &str) -> Result<Self> {
        let (section, id) = value
            .split_once('/')
            .with_context(|| format!("binding `{value}` is not `<section>/<id>`"))?;
        anyhow::ensure!(
            PINNING_SECTIONS.contains(&section),
            "binding `{value}` names section `{section}`; bindings that pin artifacts live in {}",
            PINNING_SECTIONS.join(", ")
        );
        anyhow::ensure!(
            !id.is_empty() && !id.contains('/'),
            "binding `{value}` has no plain id"
        );
        Ok(Self {
            section: section.to_owned(),
            id: id.to_owned(),
        })
    }
}

impl std::fmt::Display for BindingRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.section, self.id)
    }
}

impl TryFrom<String> for BindingRef {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<BindingRef> for String {
    fn from(value: BindingRef) -> Self {
        value.to_string()
    }
}

/// Findings a re-pin answers. A credential finding is answered by
/// provisioning, not by recomposing.
const REPINNABLE: [&str; 6] = [
    coherence::BINDING_ADAPTER_NOT_INSTALLED,
    coherence::BINDING_RESTORATION_VERIFIER_NOT_INSTALLED,
    coherence::BINDING_PROVIDER_SPEC_DIFFERS,
    coherence::BINDING_SNAPSHOT_SPEC_DIFFERS,
    coherence::BINDING_PRODUCER_DIGEST_STALE,
    coherence::BINDING_CONSUMER_VERIFIER_NOT_INSTALLED,
];

/// A re-pin refused for one binding; nothing was published.
#[derive(Debug, Clone, Serialize)]
pub struct RecomposeRefusal {
    pub code: &'static str,
    pub binding: BindingRef,
    pub detail: String,
    pub fix: String,
}

impl std::fmt::Display for RecomposeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}: {}; nothing was published; fix: {}",
            self.code, self.binding, self.detail, self.fix
        )
    }
}

impl std::error::Error for RecomposeRefusal {}

/// One section re-pin, prepared but not yet published.
#[derive(Debug, Clone)]
pub struct RepinOutcome<T> {
    pub old: T,
    pub new: T,
    /// Signed bytes to publish; `None` when no pinned identity moved.
    pub signed: Option<String>,
    /// Document fields whose value changed, as dotted paths.
    pub fields_changed: Vec<String>,
}

impl<T: Clone> RepinOutcome<T> {
    pub(crate) fn unchanged(installed: T) -> Self {
        Self {
            old: installed.clone(),
            new: installed,
            signed: None,
            fields_changed: Vec::new(),
        }
    }
}

/// Dotted paths of every leaf that differs between two documents.
pub(crate) fn changed_fields(old: &Value, new: &Value) -> Vec<String> {
    fn walk(prefix: &str, old: &Value, new: &Value, out: &mut Vec<String>) {
        match (old, new) {
            (Value::Object(old), Value::Object(new)) => {
                let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
                for key in keys {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(
                        &path,
                        old.get(key).unwrap_or(&Value::Null),
                        new.get(key).unwrap_or(&Value::Null),
                        out,
                    );
                }
            }
            _ if old != new => out.push(prefix.to_owned()),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk("", old, new, &mut out);
    out
}

/// One operator request to recompose bindings on a stopped node.
pub struct RecomposeRequest<'a> {
    pub app_root: &'a Path,
    /// Proof that the node is stopped.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLock,
    /// Bindings to recompose. Empty selects every binding with a pin the
    /// coherence inspection fails; qualifications whose production binding is
    /// recomposed follow it either way.
    pub bindings: Vec<BindingRef>,
    /// Consumer verifier selections keyed by scenario id: the running node's
    /// derive output. Required when a qualification to recompose selects any.
    pub consumer_verifiers: Option<BTreeMap<String, ConsumerRuntimeVerifierSelection>>,
    /// Prepare and report only; publish nothing.
    pub dry_run: bool,
}

/// One recomposed binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RecomposedBinding {
    pub binding: BindingRef,
    pub old_digest: String,
    /// The digest of the prepared document. A dry run's is indicative only:
    /// the signature time is part of the signed source.
    pub new_digest: String,
    pub fields_changed: Vec<String>,
    /// The carried credential generation; `None` for a qualification, which
    /// uses its producer's credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_generation: Option<String>,
    /// A recompose never mints a credential generation: the secret already
    /// provisioned for it stays valid.
    pub credential_carried: bool,
    pub published: bool,
}

/// Everything one recompose did or would do.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
pub struct RecomposeReport {
    pub dry_run: bool,
    /// In the order they were (or would be) published.
    pub recomposed: Vec<RecomposedBinding>,
    /// Selected bindings whose pins already match what is installed.
    pub unchanged: Vec<BindingRef>,
    /// Credential findings recompose does not answer: provision the secret.
    pub credentials_absent: Vec<BindingRef>,
}

/// Where a placement re-pin reads installed lifecycle generations and
/// provider executables (byte counts included, which the hash-only inventory
/// does not carry).
pub(crate) trait PlacementArtifactSource {
    fn generation(
        &self,
        backend: &str,
    ) -> Result<crate::external_placement::InstalledLifecycleGeneration>;
    fn provider(&self, provider_declaration_id: &str) -> Result<ProviderArtifactFacts>;
}

impl PlacementArtifactSource for crate::external_artifacts::ResolvedExternalExecutionArtifacts {
    fn generation(
        &self,
        backend: &str,
    ) -> Result<crate::external_placement::InstalledLifecycleGeneration> {
        self.placement_backends.installed_generation(backend)
    }
    fn provider(&self, provider_declaration_id: &str) -> Result<ProviderArtifactFacts> {
        let (configuration_adapter_artifact_hash, configuration_adapter_artifact_bytes) = self
            .provider_configurations
            .coordinate_for_provider(provider_declaration_id)?;
        let connector_hash = self
            .provider_connector_hashes
            .get(provider_declaration_id)
            .context("external provider declaration names no installed connector")?;
        let (connector_artifact_hash, connector_artifact_bytes) =
            self.connectors.coordinate_for_hash(connector_hash)?;
        Ok(ProviderArtifactFacts {
            configuration_adapter_artifact_hash,
            configuration_adapter_artifact_bytes,
            connector_artifact_hash,
            connector_artifact_bytes,
        })
    }
}

/// Resolves the installed artifacts once, and only when a placement is
/// recomposed: resolution captures and inspects the installed adapters, as
/// an explicit compose does.
struct LazyResolved<'a> {
    app_root: &'a Path,
    resolved: std::cell::OnceCell<crate::external_artifacts::ResolvedExternalExecutionArtifacts>,
}

impl LazyResolved<'_> {
    fn get(&self) -> Result<&crate::external_artifacts::ResolvedExternalExecutionArtifacts> {
        if let Some(resolved) = self.resolved.get() {
            return Ok(resolved);
        }
        let resolved =
            crate::node_config::sections::external_execution::compose::resolve_installed_artifacts(
                self.app_root,
            )?;
        Ok(self.resolved.get_or_init(|| resolved))
    }
}

impl PlacementArtifactSource for LazyResolved<'_> {
    fn generation(
        &self,
        backend: &str,
    ) -> Result<crate::external_placement::InstalledLifecycleGeneration> {
        self.get()?.generation(backend)
    }
    fn provider(&self, provider_declaration_id: &str) -> Result<ProviderArtifactFacts> {
        self.get()?.provider(provider_declaration_id)
    }
}

/// The ids installed under `.ai/node/<section>/`.
fn installed_ids(app_root: &Path, section: &str) -> Result<Vec<String>> {
    let directory = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(section);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("list {}", directory.display()));
        }
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".yaml")) else {
            continue;
        };
        ids.push(id.to_owned());
    }
    ids.sort();
    Ok(ids)
}

/// The installed bindings that pin artifacts, each admitted the way the
/// loader admits it.
fn installed_bindings(
    app_root: &Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<NodeConfigSnapshot> {
    let production = installed_ids(app_root, runtime_snapshot_production::SECTION_NAME)?
        .iter()
        .map(|id| installed_production_binding(app_root, id, identity))
        .collect::<Result<Vec<_>>>()?;
    let qualification = installed_ids(app_root, runtime_snapshot_qualification::SECTION_NAME)?
        .iter()
        .map(|id| installed_qualification_binding(app_root, id, identity))
        .collect::<Result<Vec<_>>>()?;
    let placement = installed_ids(app_root, external_execution::SECTION_NAME)?
        .iter()
        .map(|id| read_installed_binding(app_root, id, identity))
        .collect::<Result<Vec<_>>>()?;
    Ok(NodeConfigSnapshot {
        external_execution: placement,
        runtime_snapshot_production: production,
        guest_runtime_materialization: Vec::new(),
        runtime_snapshot_qualification: qualification,
        bundles: Vec::new(),
        routes: Vec::new(),
        commands: Vec::new(),
    })
}

/// Recompose the selected bindings of a stopped node against its installed
/// bundles. Reads the bundles hash-only under the bundle-generation read
/// lock and the vault without writing it; a placement re-pin also resolves
/// the installed adapters, as compose does.
pub fn recompose_bindings(request: RecomposeRequest<'_>) -> Result<RecomposeReport> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    let identity = load_node_identity(request.app_root)?;
    let snapshot = installed_bindings(request.app_root, &identity)?;
    let pins = !snapshot.runtime_snapshot_production.is_empty()
        || !snapshot.runtime_snapshot_qualification.is_empty()
        || !snapshot.external_execution.is_empty();
    let hashes = if pins {
        let trust = ryeos_engine::trust::TrustStore::load_node_registry(request.app_root)
            .context("load the node trust store")?;
        coherence::read_installed_artifact_hashes(request.app_root, &trust)?
    } else {
        InstalledArtifactHashes::default()
    };
    let vault = crate::vault::SealedEnvelopeVault::open_read_only(request.app_root);
    let placements = LazyResolved {
        app_root: request.app_root,
        resolved: std::cell::OnceCell::new(),
    };
    let prepared = prepare_recompose(
        request.app_root,
        &identity,
        &snapshot,
        &hashes,
        &vault,
        &placements,
        &request.bindings,
        request.consumer_verifiers.as_ref(),
    )?;
    publish_prepared(request.app_root, prepared, request.dry_run)
}

/// Re-pins prepared for publication, in dependency order.
pub(crate) struct PreparedRecompose {
    entries: Vec<PreparedEntry>,
    unchanged: Vec<BindingRef>,
    credentials_absent: Vec<BindingRef>,
}

struct PreparedEntry {
    binding: BindingRef,
    old_digest: String,
    new_digest: String,
    fields_changed: Vec<String>,
    credential_generation: Option<String>,
    signed: String,
}

fn refusal(code: &'static str, binding: BindingRef, detail: String, fix: String) -> anyhow::Error {
    RecomposeRefusal {
        code,
        binding,
        detail,
        fix,
    }
    .into()
}

fn binding_ref(section: &str, id: &str) -> BindingRef {
    BindingRef {
        section: section.to_owned(),
        id: id.to_owned(),
    }
}

/// Decide and sign every re-pin. Pure over its arguments apart from reading
/// the node identity it is given; publishes nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_recompose(
    app_root: &Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    snapshot: &NodeConfigSnapshot,
    hashes: &InstalledArtifactHashes,
    credentials: &dyn PlacementCredentialPresence,
    placements: &dyn PlacementArtifactSource,
    named: &[BindingRef],
    consumer_verifiers: Option<&BTreeMap<String, ConsumerRuntimeVerifierSelection>>,
) -> Result<PreparedRecompose> {
    let findings = coherence::inspect_binding_coherence(snapshot, hashes, credentials);
    let installed: BTreeSet<BindingRef> = snapshot
        .runtime_snapshot_production
        .iter()
        .map(|b| binding_ref(runtime_snapshot_production::SECTION_NAME, b.id()))
        .chain(
            snapshot
                .runtime_snapshot_qualification
                .iter()
                .map(|b| binding_ref(runtime_snapshot_qualification::SECTION_NAME, b.id())),
        )
        .chain(
            snapshot
                .external_execution
                .iter()
                .map(|b| binding_ref(external_execution::SECTION_NAME, b.id())),
        )
        .collect();
    let selected: BTreeSet<BindingRef> = if named.is_empty() {
        stranded(&findings)
    } else {
        for binding in named {
            if !installed.contains(binding) {
                return Err(refusal(
                    RECOMPOSE_BINDING_NOT_INSTALLED,
                    binding.clone(),
                    "no binding with this id is installed".to_owned(),
                    "name an installed binding; `ryeos node doctor` lists them".to_owned(),
                ));
            }
        }
        named.iter().cloned().collect()
    };
    let credentials_absent: Vec<BindingRef> = findings
        .iter()
        .filter(|finding| finding.code == coherence::BINDING_CREDENTIAL_ABSENT)
        .map(|finding| binding_ref(finding.binding.section, &finding.binding.id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut entries = Vec::new();
    let mut unchanged = Vec::new();

    // 1. Production bindings: adapter generation and snapshot spec.
    let mut producers: BTreeMap<String, InstalledRuntimeSnapshotProductionBinding> =
        BTreeMap::new();
    for producer in &snapshot.runtime_snapshot_production {
        let binding = binding_ref(runtime_snapshot_production::SECTION_NAME, producer.id());
        if !selected.contains(&binding) {
            producers.insert(producer.id().to_owned(), producer.clone());
            continue;
        }
        let lifecycle = hashes
            .lifecycle_adapters
            .get(producer.backend())
            .ok_or_else(|| {
                refusal(
                    RECOMPOSE_ARTIFACT_NOT_INSTALLED,
                    binding.clone(),
                    format!(
                        "no installed bundle declares lifecycle adapter `{}` for this host",
                        producer.backend()
                    ),
                    "install the bundle that ships the adapter, then recompose".to_owned(),
                )
            })?;
        let spec = lifecycle
            .snapshot_production_spec
            .as_deref()
            .ok_or_else(|| {
                refusal(
                    RECOMPOSE_ARTIFACT_NOT_INSTALLED,
                    binding.clone(),
                    format!(
                        "the installed `{}` adapter declares no snapshot production spec",
                        producer.backend()
                    ),
                    "install an adapter generation that produces snapshots".to_owned(),
                )
            })?;
        let outcome = repin_production(app_root, producer, &lifecycle.adapter, spec, identity)?;
        record(
            &mut entries,
            &mut unchanged,
            binding,
            outcome.old.digest(),
            outcome.new.digest(),
            outcome.fields_changed,
            Some(outcome.new.credential_generation().to_owned()),
            outcome.signed,
        );
        producers.insert(producer.id().to_owned(), outcome.new);
    }

    // 2. Qualifications: the producer's digest, the producer adapter's
    //    provider spec and restoration verifier, and consumer selections.
    for qualification in &snapshot.runtime_snapshot_qualification {
        let binding = binding_ref(
            runtime_snapshot_qualification::SECTION_NAME,
            qualification.id(),
        );
        let producer = producers.get(qualification.production_binding_id());
        let follows_producer = producer
            .is_some_and(|producer| producer.digest() != qualification.production_binding_digest())
            && selected.iter().any(|selected| {
                selected.section == runtime_snapshot_production::SECTION_NAME
                    && selected.id == qualification.production_binding_id()
            });
        if !selected.contains(&binding) && !follows_producer {
            continue;
        }
        let producer = producer.ok_or_else(|| {
            refusal(
                RECOMPOSE_BINDING_NOT_INSTALLED,
                binding.clone(),
                format!(
                    "production binding `{}` is not installed",
                    qualification.production_binding_id()
                ),
                "compose the production binding first, or compose this qualification against \
                 another one with snapshot-qualification-compose"
                    .to_owned(),
            )
        })?;
        let lifecycle = hashes
            .lifecycle_adapters
            .get(producer.backend())
            .ok_or_else(|| {
                refusal(
                    RECOMPOSE_ARTIFACT_NOT_INSTALLED,
                    binding.clone(),
                    format!(
                        "no installed bundle declares lifecycle adapter `{}` for this host",
                        producer.backend()
                    ),
                    "install the bundle that ships the adapter, then recompose".to_owned(),
                )
            })?;
        let verifier = lifecycle.restoration_verifier.as_deref().ok_or_else(|| {
            refusal(
                RECOMPOSE_ARTIFACT_NOT_INSTALLED,
                binding.clone(),
                format!(
                    "the installed `{}` adapter ships no restoration verifier",
                    producer.backend()
                ),
                "install an adapter generation that ships a restoration verifier".to_owned(),
            )
        })?;
        let selections = consumer_selections(&binding, qualification, consumer_verifiers, hashes)?;
        let outcome = repin_qualification(
            app_root,
            qualification,
            producer.digest(),
            &lifecycle.provider_spec,
            verifier,
            selections,
            identity,
        )?;
        record(
            &mut entries,
            &mut unchanged,
            binding,
            outcome.old.digest(),
            outcome.new.digest(),
            outcome.fields_changed,
            None,
            outcome.signed,
        );
    }

    // 3. Placements: lifecycle generation and provider executables.
    for placement in &snapshot.external_execution {
        let binding = binding_ref(external_execution::SECTION_NAME, placement.id());
        if !selected.contains(&binding) {
            continue;
        }
        let outcome = repin_one_placement(app_root, &binding, placement, placements, identity)?;
        record(
            &mut entries,
            &mut unchanged,
            binding,
            outcome.old.digest(),
            outcome.new.digest(),
            outcome.fields_changed,
            Some(outcome.new.credential_generation().to_owned()),
            outcome.signed,
        );
    }

    Ok(PreparedRecompose {
        entries,
        unchanged,
        credentials_absent,
    })
}

fn repin_one_placement(
    app_root: &Path,
    binding: &BindingRef,
    placement: &InstalledExternalExecutionBinding,
    placements: &dyn PlacementArtifactSource,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<RepinOutcome<InstalledExternalExecutionBinding>> {
    let not_installed = |error: anyhow::Error| {
        refusal(
            RECOMPOSE_ARTIFACT_NOT_INSTALLED,
            binding.clone(),
            format!("{error:#}"),
            "install the bundle that ships the pinned role, then recompose".to_owned(),
        )
    };
    let generation = placements
        .generation(placement.backend())
        .map_err(not_installed)?;
    let provider = placement
        .provider_declaration_id()
        .map(|declaration| placements.provider(declaration))
        .transpose()
        .map_err(not_installed)?;
    repin_placement(
        app_root,
        placement,
        &generation,
        provider.as_ref(),
        identity,
    )
}

/// The consumer selections a recomposed qualification takes. Its scenario
/// set is the operator's and is carried; each selection's value comes from
/// the derive output, because the scenario source digest is only known to
/// the running node.
fn consumer_selections(
    binding: &BindingRef,
    qualification: &InstalledRuntimeSnapshotQualificationBinding,
    supplied: Option<&BTreeMap<String, ConsumerRuntimeVerifierSelection>>,
    hashes: &InstalledArtifactHashes,
) -> Result<Option<BTreeMap<String, ConsumerRuntimeVerifierSelection>>> {
    let installed = qualification.consumer_verifiers();
    if installed.is_empty() {
        return Ok(None);
    }
    let supplied = supplied.ok_or_else(|| {
        refusal(
            RECOMPOSE_CONSUMER_VERIFIERS_REQUIRED,
            binding.clone(),
            format!(
                "it selects consumer verifiers for {}; their scenario source digests come from \
                 the running node's derive",
                installed.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
            "start the node, run the consumer qualification context derive, stop it, and pass \
             its output with --consumer-verifiers <file>"
                .to_owned(),
        )
    })?;
    let mut selections = BTreeMap::new();
    for scenario in installed.keys() {
        let selection = supplied.get(scenario).ok_or_else(|| {
            refusal(
                RECOMPOSE_CONSUMER_SELECTION_MISSING,
                binding.clone(),
                format!("the consumer verifiers file has no selection for scenario `{scenario}`"),
                "derive the selection for every scenario the qualification selects".to_owned(),
            )
        })?;
        selection.validate()?;
        if !hashes.payloads.contains(&selection.verifier_artifact_hash) {
            return Err(refusal(
                RECOMPOSE_ARTIFACT_NOT_INSTALLED,
                binding.clone(),
                format!(
                    "no installed bundle ships consumer verifier {} selected for `{scenario}`",
                    selection.verifier_artifact_hash
                ),
                "install the bundle that ships the consumer verifier, then derive again".to_owned(),
            ));
        }
        selections.insert(scenario.clone(), selection.clone());
    }
    Ok(Some(selections))
}

#[allow(clippy::too_many_arguments)]
fn record(
    entries: &mut Vec<PreparedEntry>,
    unchanged: &mut Vec<BindingRef>,
    binding: BindingRef,
    old_digest: &str,
    new_digest: &str,
    fields_changed: Vec<String>,
    credential_generation: Option<String>,
    signed: Option<String>,
) {
    match signed {
        Some(signed) => entries.push(PreparedEntry {
            binding,
            old_digest: old_digest.to_owned(),
            new_digest: new_digest.to_owned(),
            fields_changed,
            credential_generation,
            signed,
        }),
        None => unchanged.push(binding),
    }
}

/// Bindings with a failing pin a re-pin answers.
fn stranded(findings: &[BindingFinding]) -> BTreeSet<BindingRef> {
    findings
        .iter()
        .filter(|finding| {
            finding.severity == FindingSeverity::Fail && REPINNABLE.contains(&finding.code)
        })
        .map(|finding| binding_ref(finding.binding.section, &finding.binding.id))
        .collect()
}

/// Publish prepared re-pins in dependency order. A crash between two
/// publications leaves later bindings stale against earlier ones; the
/// coherence inspection names them and a rerun recomposes exactly those.
pub(crate) fn publish_prepared(
    app_root: &Path,
    prepared: PreparedRecompose,
    dry_run: bool,
) -> Result<RecomposeReport> {
    let mut report = RecomposeReport {
        dry_run,
        unchanged: prepared.unchanged,
        credentials_absent: prepared.credentials_absent,
        ..RecomposeReport::default()
    };
    for entry in prepared.entries {
        if !dry_run {
            publish_signed_document(
                app_root,
                &entry.binding.section,
                &entry.binding.id,
                entry.signed.as_bytes(),
            )?;
        }
        report.recomposed.push(RecomposedBinding {
            credential_carried: entry.credential_generation.is_some(),
            binding: entry.binding,
            old_digest: entry.old_digest,
            new_digest: entry.new_digest,
            fields_changed: entry.fields_changed,
            credential_generation: entry.credential_generation,
            published: !dry_run,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests;

//! Pin coherence between installed node bindings and installed artifacts.
//!
//! A node binding pins exact identities: the lifecycle adapter generation,
//! the restoration and consumer verifiers, the provider specs, the producer
//! binding a qualification follows, and the credential generation in the
//! node vault. Nothing re-checks a pin until an operation uses it, so a pin
//! that no longer matches what is installed used to surface only as a failed
//! launch. This module compares, read only, every pin of every installed
//! binding with the installed signed bundles and the node vault and names
//! each mismatch. `ryeos node doctor` reports the findings; a bundle
//! replacement or a recompose can use the same inspection.
//!
//! A finding is a failure when the next fresh operation through that binding
//! would refuse. Generations the node retains only to clean up older
//! placements live in the runtime database, never in node config, so they are
//! not inspected here and never fail a node.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::Value;

use crate::node_config::NodeConfigSnapshot;
use crate::node_config::sections::external_execution::ExternalWorkloadBinding;
use crate::node_config::sections::{
    external_execution, runtime_snapshot_production, runtime_snapshot_qualification,
};
use crate::vault::placement::PlacementCredentialAccess;

/// A binding pins a lifecycle adapter, supervisor, launcher, provider
/// configuration adapter or connector that no installed bundle ships.
pub const BINDING_ADAPTER_NOT_INSTALLED: &str = "binding_adapter_not_installed";
/// A qualification binding pins a restoration verifier the producer's
/// installed lifecycle adapter does not ship.
pub const BINDING_RESTORATION_VERIFIER_NOT_INSTALLED: &str =
    "binding_restoration_verifier_not_installed";
/// A qualification binding pins a provider spec the producer's installed
/// lifecycle adapter does not declare.
pub const BINDING_PROVIDER_SPEC_DIFFERS: &str = "binding_provider_spec_differs";
/// A production binding pins a snapshot production spec the installed
/// lifecycle adapter does not declare.
pub const BINDING_SNAPSHOT_SPEC_DIFFERS: &str = "binding_snapshot_spec_differs";
/// A qualification binding names a production binding that is absent or
/// whose digest moved.
pub const BINDING_PRODUCER_DIGEST_STALE: &str = "binding_producer_digest_stale";
/// A qualification binding selects a consumer verifier that no installed
/// bundle ships.
pub const BINDING_CONSUMER_VERIFIER_NOT_INSTALLED: &str = "binding_consumer_verifier_not_installed";
/// The node vault holds no credential for the generation a binding names.
pub const BINDING_CREDENTIAL_ABSENT: &str = "binding_credential_absent";
/// The node vault could not be read without writing it (for example, a rewrap
/// is pending), so credential presence is unknown.
pub const BINDING_CREDENTIAL_UNVERIFIED: &str = "binding_credential_unverified";
/// A binding file has a schema or protocol this node no longer admits.
pub const BINDING_SCHEMA_UNSUPPORTED: &str = "binding_schema_unsupported";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    /// The next fresh operation through this binding refuses.
    Fail,
    /// The pin could not be decided; nothing is known to refuse.
    Warn,
}

/// Which installed binding a finding is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingIdentity {
    pub section: &'static str,
    pub id: String,
    pub digest: String,
}

/// One pin that does not match what is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingFinding {
    pub code: &'static str,
    pub severity: FindingSeverity,
    pub binding: BindingIdentity,
    /// The binding field that holds the pin.
    pub field: String,
    pub pinned: String,
    /// What is installed in the pinned role; empty when nothing is.
    pub installed: Vec<String>,
    pub detail: String,
    pub fix: String,
}

/// Installed identities of one signed lifecycle adapter declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledLifecycleArtifactHashes {
    pub adapter: String,
    pub supervisor: String,
    pub launcher: String,
    pub restoration_verifier: Option<String>,
    pub provider_spec: String,
    pub snapshot_production_spec: Option<String>,
}

/// Installed identities of one signed external provider declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledProviderArtifactHashes {
    pub connector: String,
    pub configuration_adapter: String,
}

/// What the installed signed bundles ship, by role. Built by
/// [`crate::external_artifacts::installed_artifact_hashes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledArtifactHashes {
    /// Lifecycle adapter declaration id (a binding's `backend`) to its
    /// executables and specs for this host.
    pub lifecycle_adapters: BTreeMap<String, InstalledLifecycleArtifactHashes>,
    /// Provider declaration id to its executables for this host.
    pub providers: BTreeMap<String, InstalledProviderArtifactHashes>,
    /// Content identity of every signed executable any installed bundle
    /// ships, for any target.
    pub payloads: BTreeSet<String>,
}

/// Whether the node vault holds a placement credential generation, decided
/// without writing the vault.
pub trait PlacementCredentialPresence {
    fn placement_credential_present(&self, access: &PlacementCredentialAccess) -> Result<bool>;
}

impl PlacementCredentialPresence for crate::vault::ReadOnlySealedVault {
    fn placement_credential_present(&self, access: &PlacementCredentialAccess) -> Result<bool> {
        crate::vault::ReadOnlySealedVault::placement_credential_present(self, access)
    }
}

/// The installed bindings inspected and every finding about them.
#[derive(Debug, Clone, Serialize)]
pub struct BindingCoherenceReport {
    pub bindings: Vec<BindingIdentity>,
    pub findings: Vec<BindingFinding>,
    /// Pins this offline inspection cannot decide, and what checks them.
    pub not_checked: Vec<String>,
}

impl BindingCoherenceReport {
    pub fn has_failures(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == FindingSeverity::Fail)
    }

    pub fn has_warnings(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == FindingSeverity::Warn)
    }
}

/// Inspect the bindings of an installed node against its installed bundles
/// and vault. Reads only: the bundle generation is held under the shared
/// registry read lock, no executable is captured or run, and the vault is
/// opened without rewrap recovery.
pub fn inspect_installed_binding_coherence(
    app_root: &Path,
    node_trust_store: &ryeos_engine::trust::TrustStore,
    snapshot: &NodeConfigSnapshot,
) -> Result<BindingCoherenceReport> {
    let installed = read_installed_artifact_hashes(app_root, node_trust_store)?;
    let vault = crate::vault::SealedEnvelopeVault::open_read_only(app_root);
    let findings = inspect_binding_coherence(snapshot, &installed, &vault);
    let mut not_checked = Vec::new();
    if snapshot
        .runtime_snapshot_qualification
        .iter()
        .any(|binding| !binding.consumer_verifiers().is_empty())
    {
        not_checked.push(
            "consumer_verifiers.*.scenario_source_digest: freshness needs the running node's \
             consumer source derive; a launch with a stale selection refuses with a typed error"
                .to_owned(),
        );
    }
    if !snapshot.runtime_snapshot_production.is_empty() || !snapshot.external_execution.is_empty() {
        not_checked.push(
            "snapshots recorded in the runtime database (a superseded producer, a bound \
             snapshot's guest runtime manifest): the runtime database has no read-only offline \
             open"
                .to_owned(),
        );
    }
    Ok(BindingCoherenceReport {
        bindings: binding_inventory(snapshot),
        findings,
        not_checked,
    })
}

pub(crate) fn read_installed_artifact_hashes(
    app_root: &Path,
    node_trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<InstalledArtifactHashes> {
    let _read_lock =
        ryeos_node_runtime::registry::BundleRegistryReadLock::acquire_for_composition(app_root)
            .context("acquire the bundle-generation read lock")?;
    let records = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store: node_trust_store,
    }
    .load_bundle_section()
    .context("load installed bundle registrations")?;
    let generation = ryeos_node_runtime::engine_init::VerifiedBundleGeneration::capture(
        records,
        node_trust_store,
    )
    .context("capture verified bundle generation")?;
    generation.checked(node_trust_store, || {
        crate::external_artifacts::installed_artifact_hashes(&generation.roots(), node_trust_store)
    })
}

fn binding_inventory(snapshot: &NodeConfigSnapshot) -> Vec<BindingIdentity> {
    let mut bindings = Vec::new();
    for binding in &snapshot.runtime_snapshot_production {
        bindings.push(production_identity(binding));
    }
    for binding in &snapshot.runtime_snapshot_qualification {
        bindings.push(qualification_identity(binding));
    }
    for binding in &snapshot.external_execution {
        bindings.push(placement_identity(binding));
    }
    bindings
}

fn production_identity(
    binding: &runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
) -> BindingIdentity {
    BindingIdentity {
        section: runtime_snapshot_production::SECTION_NAME,
        id: binding.id().to_owned(),
        digest: binding.digest().to_owned(),
    }
}

fn qualification_identity(
    binding: &runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
) -> BindingIdentity {
    BindingIdentity {
        section: runtime_snapshot_qualification::SECTION_NAME,
        id: binding.id().to_owned(),
        digest: binding.digest().to_owned(),
    }
}

fn placement_identity(
    binding: &external_execution::InstalledExternalExecutionBinding,
) -> BindingIdentity {
    BindingIdentity {
        section: external_execution::SECTION_NAME,
        id: binding.id().to_owned(),
        digest: binding.digest().to_owned(),
    }
}

struct Findings(Vec<BindingFinding>);

impl Findings {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        code: &'static str,
        severity: FindingSeverity,
        binding: &BindingIdentity,
        field: impl Into<String>,
        pinned: &str,
        installed: Vec<String>,
        detail: impl Into<String>,
        fix: &str,
    ) {
        self.0.push(BindingFinding {
            code,
            severity,
            binding: binding.clone(),
            field: field.into(),
            pinned: pinned.to_owned(),
            installed,
            detail: detail.into(),
            fix: fix.to_owned(),
        });
    }

    fn credential(
        &mut self,
        binding: &BindingIdentity,
        access: Result<PlacementCredentialAccess>,
        generation: &str,
        credentials: &dyn PlacementCredentialPresence,
        fix: &str,
    ) {
        match access.and_then(|access| credentials.placement_credential_present(&access)) {
            Ok(true) => {}
            Ok(false) => self.push(
                BINDING_CREDENTIAL_ABSENT,
                FindingSeverity::Fail,
                binding,
                "credential_generation",
                generation,
                Vec::new(),
                "the node vault holds no credential for this generation",
                fix,
            ),
            Err(error) => self.push(
                BINDING_CREDENTIAL_UNVERIFIED,
                FindingSeverity::Warn,
                binding,
                "credential_generation",
                generation,
                Vec::new(),
                format!("{error:#}"),
                "rerun `ryeos node doctor` once the vault is readable",
            ),
        }
    }
}

/// Compare every pin of every installed binding in `snapshot` with
/// `installed` and the vault. Pure: reads nothing beyond its arguments.
pub fn inspect_binding_coherence(
    snapshot: &NodeConfigSnapshot,
    installed: &InstalledArtifactHashes,
    credentials: &dyn PlacementCredentialPresence,
) -> Vec<BindingFinding> {
    let mut findings = Findings(Vec::new());
    for producer in &snapshot.runtime_snapshot_production {
        inspect_production(&mut findings, producer, installed, credentials);
    }
    for qualification in &snapshot.runtime_snapshot_qualification {
        inspect_qualification(&mut findings, qualification, snapshot, installed);
    }
    for placement in &snapshot.external_execution {
        inspect_placement(&mut findings, placement, installed, credentials);
    }
    findings.0
}

fn inspect_production(
    findings: &mut Findings,
    producer: &runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    installed: &InstalledArtifactHashes,
    credentials: &dyn PlacementCredentialPresence,
) {
    use FindingSeverity::Fail;
    let binding = production_identity(producer);
    let recompose = format!(
        "recompose against the installed bundles on the stopped node: \
         ryeos node external-execution snapshot-compose --id {} ...",
        producer.id()
    );
    match installed.lifecycle_adapters.get(producer.backend()) {
        None => findings.push(
            BINDING_ADAPTER_NOT_INSTALLED,
            Fail,
            &binding,
            "adapter_artifact_hash",
            producer.adapter_artifact_hash(),
            Vec::new(),
            format!(
                "no installed bundle declares lifecycle adapter `{}` for this host",
                producer.backend()
            ),
            &recompose,
        ),
        Some(lifecycle) => {
            if lifecycle.adapter != producer.adapter_artifact_hash() {
                findings.push(
                    BINDING_ADAPTER_NOT_INSTALLED,
                    Fail,
                    &binding,
                    "adapter_artifact_hash",
                    producer.adapter_artifact_hash(),
                    vec![lifecycle.adapter.clone()],
                    format!(
                        "the installed `{}` lifecycle adapter is a different generation",
                        producer.backend()
                    ),
                    &recompose,
                );
            }
            if lifecycle.snapshot_production_spec.as_deref()
                != Some(producer.snapshot_spec_sha256())
            {
                findings.push(
                    BINDING_SNAPSHOT_SPEC_DIFFERS,
                    Fail,
                    &binding,
                    "snapshot_spec_sha256",
                    producer.snapshot_spec_sha256(),
                    lifecycle.snapshot_production_spec.iter().cloned().collect(),
                    format!(
                        "the installed `{}` lifecycle adapter declares a different snapshot production spec",
                        producer.backend()
                    ),
                    &recompose,
                );
            }
        }
    }
    findings.credential(
        &binding,
        producer.credential_access(),
        producer.credential_generation(),
        credentials,
        &format!(
            "ryeos node external-execution provision-credential --section {} --binding {}",
            runtime_snapshot_production::SECTION_NAME,
            producer.id()
        ),
    );
}

fn inspect_qualification(
    findings: &mut Findings,
    qualification: &runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    snapshot: &NodeConfigSnapshot,
    installed: &InstalledArtifactHashes,
) {
    use FindingSeverity::Fail;
    let binding = qualification_identity(qualification);
    let recompose = format!(
        "recompose against the current producer and installed bundles on the stopped node: \
         ryeos node external-execution snapshot-qualification-compose --id {} ...",
        qualification.id()
    );
    let producer = snapshot
        .runtime_snapshot_production
        .iter()
        .find(|producer| producer.id() == qualification.production_binding_id());
    match producer {
        None => findings.push(
            BINDING_PRODUCER_DIGEST_STALE,
            Fail,
            &binding,
            "production_binding_id",
            qualification.production_binding_id(),
            snapshot
                .runtime_snapshot_production
                .iter()
                .map(|producer| producer.id().to_owned())
                .collect(),
            "no installed production binding has this id",
            &recompose,
        ),
        Some(producer) => {
            if producer.digest() != qualification.production_binding_digest() {
                findings.push(
                    BINDING_PRODUCER_DIGEST_STALE,
                    Fail,
                    &binding,
                    "production_binding_digest",
                    qualification.production_binding_digest(),
                    vec![producer.digest().to_owned()],
                    format!(
                        "production binding `{}` was recomposed after this qualification",
                        producer.id()
                    ),
                    &recompose,
                );
            }
            // A producer whose adapter is absent is named on the producer.
            if let Some(lifecycle) = installed.lifecycle_adapters.get(producer.backend()) {
                if lifecycle.provider_spec != qualification.provider_spec_digest() {
                    findings.push(
                        BINDING_PROVIDER_SPEC_DIFFERS,
                        Fail,
                        &binding,
                        "provider_spec_digest",
                        qualification.provider_spec_digest(),
                        vec![lifecycle.provider_spec.clone()],
                        format!(
                            "the installed `{}` lifecycle adapter declares a different provider spec",
                            producer.backend()
                        ),
                        &recompose,
                    );
                }
                if lifecycle.restoration_verifier.as_deref()
                    != Some(qualification.verifier_artifact_hash())
                {
                    findings.push(
                        BINDING_RESTORATION_VERIFIER_NOT_INSTALLED,
                        Fail,
                        &binding,
                        "verifier_artifact_hash",
                        qualification.verifier_artifact_hash(),
                        lifecycle.restoration_verifier.iter().cloned().collect(),
                        format!(
                            "the installed `{}` lifecycle adapter ships a different restoration verifier",
                            producer.backend()
                        ),
                        &recompose,
                    );
                }
            }
        }
    }
    for (scenario, selection) in qualification.consumer_verifiers() {
        if !installed
            .payloads
            .contains(&selection.verifier_artifact_hash)
        {
            findings.push(
                BINDING_CONSUMER_VERIFIER_NOT_INSTALLED,
                Fail,
                &binding,
                format!("consumer_verifiers.{scenario}.verifier_artifact_hash"),
                &selection.verifier_artifact_hash,
                Vec::new(),
                format!(
                    "no installed bundle ships the consumer verifier selected for `{scenario}`"
                ),
                &recompose,
            );
        }
    }
}

fn inspect_placement(
    findings: &mut Findings,
    placement: &external_execution::InstalledExternalExecutionBinding,
    installed: &InstalledArtifactHashes,
    credentials: &dyn PlacementCredentialPresence,
) {
    use FindingSeverity::Fail;
    let binding = placement_identity(placement);
    let contract = placement.backend_contract();
    let verb = match &contract.workload {
        ExternalWorkloadBinding::QualificationStructuredSession(_) => {
            "qualification-session-compose"
        }
        _ => "compose",
    };
    let recompose = format!(
        "recompose against the installed bundles on the stopped node: \
         ryeos node external-execution {verb} --id {} ...",
        placement.id()
    );
    let lifecycle = installed.lifecycle_adapters.get(&contract.backend);
    for (field, pinned, actual) in [
        (
            "backend_artifact_hash",
            contract.backend_artifact_hash.as_str(),
            lifecycle.map(|lifecycle| lifecycle.adapter.as_str()),
        ),
        (
            "supervisor_artifact_hash",
            contract.supervisor_artifact_hash.as_str(),
            lifecycle.map(|lifecycle| lifecycle.supervisor.as_str()),
        ),
        (
            "launcher_artifact_hash",
            contract.launcher_artifact_hash.as_str(),
            lifecycle.map(|lifecycle| lifecycle.launcher.as_str()),
        ),
    ] {
        if actual != Some(pinned) {
            findings.push(
                BINDING_ADAPTER_NOT_INSTALLED,
                Fail,
                &binding,
                field,
                pinned,
                actual.map(str::to_owned).into_iter().collect(),
                match actual {
                    None => format!(
                        "no installed bundle declares lifecycle adapter `{}` for this host",
                        contract.backend
                    ),
                    Some(_) => format!(
                        "the installed `{}` lifecycle adapter ships a different generation",
                        contract.backend
                    ),
                },
                &recompose,
            );
        }
    }
    // A direct command names no provider executables.
    if let Ok(session) = contract.workload.session_artifacts() {
        let provider = installed.providers.get(session.provider_declaration_id);
        for (field, pinned, actual) in [
            (
                "configuration_adapter_artifact_hash",
                session.configuration_adapter_artifact_hash,
                provider.map(|provider| provider.configuration_adapter.as_str()),
            ),
            (
                "connector_artifact_hash",
                session.connector_artifact_hash,
                provider.map(|provider| provider.connector.as_str()),
            ),
        ] {
            if actual != Some(pinned) {
                findings.push(
                    BINDING_ADAPTER_NOT_INSTALLED,
                    Fail,
                    &binding,
                    field,
                    pinned,
                    actual.map(str::to_owned).into_iter().collect(),
                    match actual {
                        None => format!(
                            "no installed bundle declares provider `{}` for this host",
                            session.provider_declaration_id
                        ),
                        Some(_) => format!(
                            "the installed provider `{}` ships a different generation",
                            session.provider_declaration_id
                        ),
                    },
                    &recompose,
                );
            }
        }
    }
    findings.credential(
        &binding,
        placement.credential_access(),
        placement.credential_generation(),
        credentials,
        &format!(
            "ryeos node external-execution provision-credential --binding {}",
            placement.id()
        ),
    );
}

/// A binding file whose schema or protocol this node does not admit. The
/// loader names the file; this names what was seen and what is admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingSchemaUnsupported {
    pub section: &'static str,
    pub seen_schema: Option<u64>,
    pub seen_protocol: Option<String>,
    pub admitted_schemas: Vec<u64>,
    pub admitted_protocol: &'static str,
}

impl BindingSchemaUnsupported {
    pub fn code(&self) -> &'static str {
        BINDING_SCHEMA_UNSUPPORTED
    }

    pub fn fix(&self) -> String {
        format!(
            "move the file out of .ai/node/{}/ on the stopped node and recompose the binding under the admitted schema",
            self.section
        )
    }
}

impl std::fmt::Display for BindingSchemaUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let schema = self
            .seen_schema
            .map(|schema| schema.to_string())
            .unwrap_or_else(|| "absent".to_owned());
        let protocol = self.seen_protocol.as_deref().unwrap_or("absent");
        let admitted = self
            .admitted_schemas
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "{BINDING_SCHEMA_UNSUPPORTED}: {} binding has schema {schema} and protocol `{protocol}`; \
             this node admits schema {admitted} with protocol `{}`",
            self.section, self.admitted_protocol
        )
    }
}

impl std::error::Error for BindingSchemaUnsupported {}

/// Refuse a binding body whose schema or protocol this node does not admit,
/// before any field is decoded, so an older binding is named rather than
/// reported as an opaque decode error.
pub(crate) fn require_admitted_binding_schema(
    section: &'static str,
    body: &Value,
    admitted_schemas: &[u64],
    admitted_protocol: &'static str,
) -> Result<()> {
    let seen_schema = body.get("schema").and_then(Value::as_u64);
    let seen_protocol = body.get("protocol").and_then(Value::as_str);
    if seen_schema.is_some_and(|schema| admitted_schemas.contains(&schema))
        && seen_protocol == Some(admitted_protocol)
    {
        return Ok(());
    }
    Err(BindingSchemaUnsupported {
        section,
        seen_schema,
        seen_protocol: seen_protocol.map(str::to_owned),
        admitted_schemas: admitted_schemas.to_vec(),
        admitted_protocol,
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::sections::external_execution::InstalledExternalExecutionBinding;
    use crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding;
    use crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding;
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerArchiveBudget, ConsumerRuntimeVerifierSelection,
    };

    struct Present;
    impl PlacementCredentialPresence for Present {
        fn placement_credential_present(&self, _: &PlacementCredentialAccess) -> Result<bool> {
            Ok(true)
        }
    }

    struct Absent;
    impl PlacementCredentialPresence for Absent {
        fn placement_credential_present(&self, _: &PlacementCredentialAccess) -> Result<bool> {
            Ok(false)
        }
    }

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    /// Installed artifacts that match every fixture binding's pins. The
    /// production fixture pins adapter `b…` and snapshot spec `c…` for
    /// `test-backend`; the placement fixture pins backend `d…`, supervisor
    /// `1…`, launcher `e…`, configuration adapter `2…` and connector `f…`.
    fn installed() -> InstalledArtifactHashes {
        let mut installed = InstalledArtifactHashes::default();
        installed.lifecycle_adapters.insert(
            "test-backend".into(),
            InstalledLifecycleArtifactHashes {
                adapter: hash('b'),
                supervisor: hash('5'),
                launcher: hash('6'),
                restoration_verifier: Some(hash('7')),
                provider_spec: hash('8'),
                snapshot_production_spec: Some(hash('c')),
            },
        );
        installed.lifecycle_adapters.insert(
            "fixture".into(),
            InstalledLifecycleArtifactHashes {
                adapter: hash('d'),
                supervisor: hash('1'),
                launcher: hash('e'),
                restoration_verifier: None,
                provider_spec: hash('8'),
                snapshot_production_spec: None,
            },
        );
        installed.providers.insert(
            "codex-hosted".into(),
            InstalledProviderArtifactHashes {
                connector: hash('f'),
                configuration_adapter: hash('2'),
            },
        );
        installed.payloads = [
            hash('b'),
            hash('5'),
            hash('6'),
            hash('7'),
            hash('d'),
            hash('1'),
            hash('e'),
            hash('f'),
            hash('2'),
            hash('3'),
        ]
        .into_iter()
        .collect();
        installed
    }

    fn consumer(verifier: char) -> BTreeMap<String, ConsumerRuntimeVerifierSelection> {
        BTreeMap::from([(
            "routed-consumer".to_owned(),
            ConsumerRuntimeVerifierSelection {
                scenario_source_digest: hash('4'),
                verifier_artifact_hash: hash(verifier),
                archive_budget: ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
            },
        )])
    }

    fn qualification(
        producer: &InstalledRuntimeSnapshotProductionBinding,
        verifier: char,
        consumer_verifier: char,
    ) -> InstalledRuntimeSnapshotQualificationBinding {
        InstalledRuntimeSnapshotQualificationBinding::test_fixture(
            producer.id(),
            producer.digest(),
            &hash('8'),
            &hash(verifier),
            consumer(consumer_verifier),
        )
    }

    fn snapshot(
        production: Vec<InstalledRuntimeSnapshotProductionBinding>,
        qualification: Vec<InstalledRuntimeSnapshotQualificationBinding>,
        external_execution: Vec<InstalledExternalExecutionBinding>,
    ) -> NodeConfigSnapshot {
        NodeConfigSnapshot {
            external_execution,
            runtime_snapshot_production: production,
            guest_runtime_materialization: Vec::new(),
            runtime_snapshot_qualification: qualification,
            bundles: Vec::new(),
            routes: Vec::new(),
            commands: Vec::new(),
        }
    }

    fn full_snapshot() -> NodeConfigSnapshot {
        let producer = InstalledRuntimeSnapshotProductionBinding::test_fixture();
        let qualification = qualification(&producer, '7', '3');
        snapshot(
            vec![producer],
            vec![qualification],
            vec![InstalledExternalExecutionBinding::test_fixture()],
        )
    }

    fn named(findings: &[BindingFinding]) -> Vec<(&'static str, &'static str, &str)> {
        findings
            .iter()
            .map(|finding| {
                (
                    finding.code,
                    finding.binding.section,
                    finding.field.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn coherent_node_has_no_findings() {
        let findings = inspect_binding_coherence(&full_snapshot(), &installed(), &Present);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn production_adapter_pin_absent_is_named() {
        let snapshot = snapshot(
            vec![InstalledRuntimeSnapshotProductionBinding::test_fixture()],
            Vec::new(),
            Vec::new(),
        );
        let mut replaced = installed();
        replaced
            .lifecycle_adapters
            .get_mut("test-backend")
            .unwrap()
            .adapter = hash('9');
        let findings = inspect_binding_coherence(&snapshot, &replaced, &Present);
        assert_eq!(
            named(&findings),
            vec![(
                BINDING_ADAPTER_NOT_INSTALLED,
                runtime_snapshot_production::SECTION_NAME,
                "adapter_artifact_hash"
            )]
        );
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Fail);
        assert_eq!(finding.binding.id, "snapshot-producer");
        assert_eq!(finding.pinned, hash('b'));
        assert_eq!(finding.installed, vec![hash('9')]);
        assert!(
            finding
                .fix
                .contains("snapshot-compose --id snapshot-producer")
        );

        // With the backend gone entirely, nothing is installed in the role.
        let mut removed = installed();
        removed.lifecycle_adapters.remove("test-backend");
        let findings = inspect_binding_coherence(&snapshot, &removed, &Present);
        assert_eq!(
            named(&findings),
            vec![(
                BINDING_ADAPTER_NOT_INSTALLED,
                runtime_snapshot_production::SECTION_NAME,
                "adapter_artifact_hash"
            )]
        );
        assert!(findings[0].installed.is_empty());
    }

    #[test]
    fn qualification_producer_digest_stale_is_named() {
        let producer = InstalledRuntimeSnapshotProductionBinding::test_fixture();
        // Composed against an earlier generation of the same producer.
        let stale = InstalledRuntimeSnapshotQualificationBinding::test_fixture(
            producer.id(),
            &hash('0'),
            &hash('8'),
            &hash('7'),
            consumer('3'),
        );
        let findings = inspect_binding_coherence(
            &snapshot(vec![producer.clone()], vec![stale], Vec::new()),
            &installed(),
            &Present,
        );
        assert_eq!(
            named(&findings),
            vec![(
                BINDING_PRODUCER_DIGEST_STALE,
                runtime_snapshot_qualification::SECTION_NAME,
                "production_binding_digest"
            )]
        );
        assert_eq!(findings[0].pinned, hash('0'));
        assert_eq!(findings[0].installed, vec![producer.digest().to_owned()]);

        // A qualification whose producer is gone names the missing id.
        let findings = inspect_binding_coherence(
            &snapshot(
                Vec::new(),
                vec![qualification(&producer, '7', '3')],
                Vec::new(),
            ),
            &installed(),
            &Present,
        );
        assert_eq!(
            named(&findings),
            vec![(
                BINDING_PRODUCER_DIGEST_STALE,
                runtime_snapshot_qualification::SECTION_NAME,
                "production_binding_id"
            )]
        );
    }

    #[test]
    fn qualification_verifier_pin_absent_is_named() {
        let producer = InstalledRuntimeSnapshotProductionBinding::test_fixture();
        let findings = inspect_binding_coherence(
            &snapshot(
                vec![producer.clone()],
                vec![qualification(&producer, '0', '3')],
                Vec::new(),
            ),
            &installed(),
            &Present,
        );
        assert_eq!(
            named(&findings),
            vec![(
                BINDING_RESTORATION_VERIFIER_NOT_INSTALLED,
                runtime_snapshot_qualification::SECTION_NAME,
                "verifier_artifact_hash"
            )]
        );
        assert_eq!(findings[0].installed, vec![hash('7')]);
    }

    #[test]
    fn consumer_verifier_pin_absent_is_named() {
        let producer = InstalledRuntimeSnapshotProductionBinding::test_fixture();
        let findings = inspect_binding_coherence(
            &snapshot(
                vec![producer.clone()],
                vec![qualification(&producer, '7', '0')],
                Vec::new(),
            ),
            &installed(),
            &Present,
        );
        assert_eq!(
            named(&findings),
            vec![(
                BINDING_CONSUMER_VERIFIER_NOT_INSTALLED,
                runtime_snapshot_qualification::SECTION_NAME,
                "consumer_verifiers.routed-consumer.verifier_artifact_hash"
            )]
        );
        assert_eq!(findings[0].pinned, hash('0'));
    }

    #[test]
    fn external_execution_artifact_pins_checked_per_role() {
        let snapshot = snapshot(
            Vec::new(),
            Vec::new(),
            vec![InstalledExternalExecutionBinding::test_fixture()],
        );
        let mut changed = installed();
        {
            let lifecycle = changed.lifecycle_adapters.get_mut("fixture").unwrap();
            lifecycle.supervisor = hash('9');
            lifecycle.launcher = hash('8');
        }
        changed.providers.get_mut("codex-hosted").unwrap().connector = hash('0');
        let findings = inspect_binding_coherence(&snapshot, &changed, &Present);
        let section = external_execution::SECTION_NAME;
        assert_eq!(
            named(&findings),
            vec![
                (
                    BINDING_ADAPTER_NOT_INSTALLED,
                    section,
                    "supervisor_artifact_hash"
                ),
                (
                    BINDING_ADAPTER_NOT_INSTALLED,
                    section,
                    "launcher_artifact_hash"
                ),
                (
                    BINDING_ADAPTER_NOT_INSTALLED,
                    section,
                    "connector_artifact_hash"
                ),
            ]
        );
        assert_eq!(findings[0].installed, vec![hash('9')]);
        assert!(
            findings[0]
                .fix
                .contains("external-execution compose --id fixture")
        );

        // A provider no bundle declares names both of its executables.
        let mut no_provider = installed();
        no_provider.providers.clear();
        let findings = inspect_binding_coherence(&snapshot, &no_provider, &Present);
        assert_eq!(
            named(&findings),
            vec![
                (
                    BINDING_ADAPTER_NOT_INSTALLED,
                    section,
                    "configuration_adapter_artifact_hash"
                ),
                (
                    BINDING_ADAPTER_NOT_INSTALLED,
                    section,
                    "connector_artifact_hash"
                ),
            ]
        );
    }

    #[test]
    fn absent_credential_generation_is_named() {
        let findings = inspect_binding_coherence(&full_snapshot(), &installed(), &Absent);
        assert_eq!(
            named(&findings),
            vec![
                (
                    BINDING_CREDENTIAL_ABSENT,
                    runtime_snapshot_production::SECTION_NAME,
                    "credential_generation"
                ),
                (
                    BINDING_CREDENTIAL_ABSENT,
                    external_execution::SECTION_NAME,
                    "credential_generation"
                ),
            ]
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.severity == FindingSeverity::Fail)
        );
        assert!(
            findings[0]
                .fix
                .contains("--section runtime_snapshot_production")
        );
    }

    #[test]
    fn retained_generation_is_not_a_failure() {
        // Older generations the node keeps only to clean up placements sit
        // beside the current ones in the installed set (and in the runtime
        // database, which this inspection never reads). An installed
        // generation that no node binding pins is not a finding.
        let mut with_retained = installed();
        with_retained.lifecycle_adapters.insert(
            "retired-backend".into(),
            InstalledLifecycleArtifactHashes {
                adapter: hash('a'),
                supervisor: hash('a'),
                launcher: hash('a'),
                restoration_verifier: None,
                provider_spec: hash('a'),
                snapshot_production_spec: None,
            },
        );
        with_retained.payloads.insert(hash('a'));
        let findings = inspect_binding_coherence(&full_snapshot(), &with_retained, &Present);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn unreadable_vault_warns_without_failing() {
        struct Pending;
        impl PlacementCredentialPresence for Pending {
            fn placement_credential_present(&self, _: &PlacementCredentialAccess) -> Result<bool> {
                anyhow::bail!("vault rewrap is pending")
            }
        }
        let findings = inspect_binding_coherence(&full_snapshot(), &installed(), &Pending);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|finding| {
            finding.code == BINDING_CREDENTIAL_UNVERIFIED
                && finding.severity == FindingSeverity::Warn
                && finding.detail.contains("rewrap is pending")
        }));
    }

    #[test]
    fn unsupported_binding_schema_names_seen_and_admitted() {
        let body = serde_json::json!({
            "kind": "node",
            "schema": 3,
            "protocol": "ryeos.runtime-snapshot-qualification.v3",
        });
        let error = require_admitted_binding_schema(
            runtime_snapshot_qualification::SECTION_NAME,
            &body,
            &[4],
            "ryeos.runtime-snapshot-qualification.v4",
        )
        .unwrap_err();
        let refusal = error
            .downcast_ref::<BindingSchemaUnsupported>()
            .expect("typed schema refusal");
        assert_eq!(refusal.code(), BINDING_SCHEMA_UNSUPPORTED);
        assert_eq!(refusal.seen_schema, Some(3));
        assert_eq!(
            refusal.seen_protocol.as_deref(),
            Some("ryeos.runtime-snapshot-qualification.v3")
        );
        let message = error.to_string();
        assert!(message.contains("schema 3"), "{message}");
        assert!(message.contains("admits schema 4"), "{message}");

        let current = serde_json::json!({
            "schema": 4,
            "protocol": "ryeos.runtime-snapshot-qualification.v4",
        });
        require_admitted_binding_schema(
            runtime_snapshot_qualification::SECTION_NAME,
            &current,
            &[4],
            "ryeos.runtime-snapshot-qualification.v4",
        )
        .unwrap();
    }
}

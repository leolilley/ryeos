use super::*;
use crate::node_config::coherence::InstalledLifecycleArtifactHashes;
use crate::node_config::sections::runtime_snapshot_production::compose::tests::{
    SPEC, StoppedNode, install_production, stopped_node,
};
use crate::node_config::sections::runtime_snapshot_qualification::compose::{
    QualificationComposeRequest, QualificationLimits,
    compose_runtime_snapshot_qualification_binding,
};
use crate::vault::placement::PlacementCredentialAccess;
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget;

const PROVIDER_SPEC: &[u8] = br#"{"schema":1}"#;
const PRODUCER: &str = "render-snapshot";
const QUALIFIER: &str = "render-snapshot-qualification";

struct Present;
impl PlacementCredentialPresence for Present {
    fn placement_credential_present(&self, _: &PlacementCredentialAccess) -> Result<bool> {
        Ok(true)
    }
}

struct NoPlacements;
impl PlacementArtifactSource for NoPlacements {
    fn generation(
        &self,
        backend: &str,
    ) -> Result<crate::external_placement::InstalledLifecycleGeneration> {
        anyhow::bail!("no placement generation `{backend}` in this fixture")
    }
    fn provider(&self, provider_declaration_id: &str) -> Result<ProviderArtifactFacts> {
        anyhow::bail!("no provider `{provider_declaration_id}` in this fixture")
    }
}

fn hash(byte: char) -> String {
    byte.to_string().repeat(64)
}

/// Installed artifacts matching the fixture production binding (adapter
/// `b…`, spec [`SPEC`]) and qualification (verifier `3…`, provider spec
/// [`PROVIDER_SPEC`]), with consumer verifiers `5…` and `6…` shipped.
fn installed() -> InstalledArtifactHashes {
    let mut installed = InstalledArtifactHashes::default();
    installed.lifecycle_adapters.insert(
        "test-backend".into(),
        InstalledLifecycleArtifactHashes {
            adapter: hash('b'),
            supervisor: hash('1'),
            launcher: hash('2'),
            restoration_verifier: Some(hash('3')),
            provider_spec: lillux::sha256_hex(PROVIDER_SPEC),
            snapshot_production_spec: Some(lillux::sha256_hex(SPEC)),
        },
    );
    installed.payloads = [hash('b'), hash('3'), hash('5'), hash('6')]
        .into_iter()
        .collect();
    installed
}

fn selection(verifier: char) -> ConsumerRuntimeVerifierSelection {
    ConsumerRuntimeVerifierSelection {
        scenario_source_digest: hash('4'),
        verifier_artifact_hash: hash(verifier),
        archive_budget: ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
    }
}

fn install_qualification(node: &StoppedNode, consumer: Option<char>) {
    compose_runtime_snapshot_qualification_binding(QualificationComposeRequest {
        app_root: node.root.path(),
        state_lock: &node.lock,
        id: QUALIFIER.into(),
        snapshot_binding_id: PRODUCER.into(),
        provider_spec: PROVIDER_SPEC.to_vec(),
        verifier_artifact_hash: hash('3'),
        consumer_verifiers: consumer.map(|verifier| {
            serde_json::to_value(BTreeMap::from([(
                "routed-consumer".to_owned(),
                selection(verifier),
            )]))
            .unwrap()
        }),
        limits: QualificationLimits::default(),
    })
    .unwrap();
}

fn prepare(
    node: &StoppedNode,
    hashes: &InstalledArtifactHashes,
    named: &[BindingRef],
    consumer: Option<&BTreeMap<String, ConsumerRuntimeVerifierSelection>>,
) -> Result<PreparedRecompose> {
    let identity = load_node_identity(node.root.path()).unwrap();
    let snapshot = installed_bindings(node.root.path(), &identity).unwrap();
    prepare_recompose(
        node.root.path(),
        &identity,
        &snapshot,
        hashes,
        &Present,
        &NoPlacements,
        named,
        consumer,
    )
}

/// A payload change: the adapter generation and its restoration verifier.
fn replaced() -> InstalledArtifactHashes {
    let mut replaced = installed();
    let lifecycle = replaced.lifecycle_adapters.get_mut("test-backend").unwrap();
    lifecycle.adapter = hash('9');
    lifecycle.restoration_verifier = Some(hash('a'));
    replaced.payloads.insert(hash('9'));
    replaced.payloads.insert(hash('a'));
    replaced
}

fn read_document(node: &StoppedNode, section: &str, id: &str) -> Value {
    let identity = load_node_identity(node.root.path()).unwrap();
    let signed =
        crate::node_config::sections::external_execution::compose::read_installed_document(
            node.root.path(),
            section,
            id,
        )
        .unwrap();
    crate::node_config::sections::external_execution::compose::verify_node_signed(
        &signed, &identity, "binding",
    )
    .unwrap()
}

#[test]
fn recompose_is_a_noop_on_a_coherent_node() {
    let node = stopped_node();
    install_production(&node, PRODUCER);
    install_qualification(&node, None);
    let prepared = prepare(&node, &installed(), &[], None).unwrap();
    assert!(prepared.entries.is_empty());
    assert!(prepared.unchanged.is_empty());
    // Named but coherent: reported unchanged, nothing to publish.
    let named = [BindingRef::parse(&format!("runtime_snapshot_production/{PRODUCER}")).unwrap()];
    let prepared = prepare(&node, &installed(), &named, None).unwrap();
    assert!(prepared.entries.is_empty());
    assert_eq!(prepared.unchanged, named.to_vec());
}

#[test]
fn recompose_carries_every_operator_fact_and_repins_only_artifacts() {
    let node = stopped_node();
    install_production(&node, PRODUCER);
    install_qualification(&node, None);
    let before_production =
        read_document(&node, runtime_snapshot_production::SECTION_NAME, PRODUCER);
    let before_qualification = read_document(
        &node,
        runtime_snapshot_qualification::SECTION_NAME,
        QUALIFIER,
    );

    let prepared = prepare(&node, &replaced(), &[], None).unwrap();
    let report = publish_prepared(node.root.path(), prepared, false).unwrap();
    assert_eq!(
        report
            .recomposed
            .iter()
            .map(|entry| (entry.binding.to_string(), entry.fields_changed.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                format!("runtime_snapshot_production/{PRODUCER}"),
                vec!["adapter_artifact_hash".to_owned()]
            ),
            (
                format!("runtime_snapshot_qualification/{QUALIFIER}"),
                vec![
                    "production_binding_digest".to_owned(),
                    "verifier_artifact_hash".to_owned()
                ]
            ),
        ]
    );
    let after_production =
        read_document(&node, runtime_snapshot_production::SECTION_NAME, PRODUCER);
    let after_qualification = read_document(
        &node,
        runtime_snapshot_qualification::SECTION_NAME,
        QUALIFIER,
    );
    assert_eq!(
        changed_fields(&before_production, &after_production),
        ["adapter_artifact_hash"]
    );
    assert_eq!(after_production["adapter_artifact_hash"], hash('9'));
    assert_eq!(
        changed_fields(&before_qualification, &after_qualification),
        ["production_binding_digest", "verifier_artifact_hash"]
    );
    assert_eq!(
        after_qualification["production_binding_digest"],
        report.recomposed[0].new_digest
    );
    assert_eq!(after_qualification["verifier_artifact_hash"], hash('a'));

    // A rerun of the same recompose finds nothing left to re-pin.
    assert!(
        prepare(&node, &replaced(), &[], None)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn recompose_orders_qualification_after_production() {
    let node = stopped_node();
    install_production(&node, PRODUCER);
    install_qualification(&node, None);
    // Name only the producer: its qualification follows it, after it.
    let named = [BindingRef::parse(&format!("runtime_snapshot_production/{PRODUCER}")).unwrap()];
    let mut moved = installed();
    moved
        .lifecycle_adapters
        .get_mut("test-backend")
        .unwrap()
        .adapter = hash('9');
    let prepared = prepare(&node, &moved, &named, None).unwrap();
    let order: Vec<String> = prepared
        .entries
        .iter()
        .map(|entry| entry.binding.section.clone())
        .collect();
    assert_eq!(
        order,
        [
            runtime_snapshot_production::SECTION_NAME,
            runtime_snapshot_qualification::SECTION_NAME
        ]
    );
    assert_eq!(
        prepared.entries[1].fields_changed,
        ["production_binding_digest"]
    );
}

#[test]
fn recompose_refuses_stranded_consumer_selection_without_input() {
    let node = stopped_node();
    install_production(&node, PRODUCER);
    install_qualification(&node, Some('5'));
    let error = prepare(&node, &replaced(), &[], None)
        .err()
        .expect("refused");
    let refusal = error
        .downcast_ref::<RecomposeRefusal>()
        .unwrap_or_else(|| panic!("typed refusal: {error:#}"));
    assert_eq!(refusal.code, RECOMPOSE_CONSUMER_VERIFIERS_REQUIRED);
    assert_eq!(
        refusal.binding.to_string(),
        format!("runtime_snapshot_qualification/{QUALIFIER}")
    );

    // A file without the scenario is refused too, naming it.
    let other = BTreeMap::from([("other".to_owned(), selection('6'))]);
    let error = prepare(&node, &replaced(), &[], Some(&other))
        .err()
        .expect("refused");
    assert_eq!(
        error.downcast_ref::<RecomposeRefusal>().unwrap().code,
        RECOMPOSE_CONSUMER_SELECTION_MISSING
    );

    // The derive output supplies the selection.
    let derived = BTreeMap::from([("routed-consumer".to_owned(), selection('6'))]);
    let prepared = prepare(&node, &replaced(), &[], Some(&derived)).unwrap();
    let qualification = &prepared.entries[1];
    assert!(
        qualification
            .fields_changed
            .contains(&"consumer_verifiers.routed-consumer.verifier_artifact_hash".to_owned()),
        "{:?}",
        qualification.fields_changed
    );
}

#[test]
fn recompose_carries_credential_generation_for_same_account() {
    let node = stopped_node();
    let composed = install_production(&node, PRODUCER);
    let prepared = prepare(&node, &replaced(), &[], None).unwrap();
    let report = publish_prepared(node.root.path(), prepared, false).unwrap();
    let production = &report.recomposed[0];
    assert!(production.credential_carried);
    assert_eq!(
        production.credential_generation.as_deref(),
        Some(composed.credential_generation.as_str())
    );
    let after = read_document(&node, runtime_snapshot_production::SECTION_NAME, PRODUCER);
    assert_eq!(
        after["credential_generation"],
        composed.credential_generation
    );
    assert_eq!(after["account"], composed.account);
    assert_eq!(after["backend"], composed.backend);
}

#[test]
fn a_dry_run_publishes_nothing() {
    let node = stopped_node();
    install_production(&node, PRODUCER);
    let before = read_document(&node, runtime_snapshot_production::SECTION_NAME, PRODUCER);
    let prepared = prepare(&node, &replaced(), &[], None).unwrap();
    let report = publish_prepared(node.root.path(), prepared, true).unwrap();
    assert!(report.dry_run);
    assert_eq!(report.recomposed.len(), 1);
    assert!(!report.recomposed[0].published);
    assert_eq!(
        read_document(&node, runtime_snapshot_production::SECTION_NAME, PRODUCER),
        before
    );
}

#[test]
fn an_uninstalled_role_or_binding_is_refused_typed() {
    let node = stopped_node();
    install_production(&node, PRODUCER);
    let mut removed = installed();
    removed.lifecycle_adapters.clear();
    let named = [BindingRef::parse(&format!("runtime_snapshot_production/{PRODUCER}")).unwrap()];
    let error = prepare(&node, &removed, &named, None).err().unwrap();
    assert_eq!(
        error.downcast_ref::<RecomposeRefusal>().unwrap().code,
        RECOMPOSE_ARTIFACT_NOT_INSTALLED
    );
    let absent = [BindingRef::parse("external_execution/absent").unwrap()];
    let error = prepare(&node, &installed(), &absent, None).err().unwrap();
    assert_eq!(
        error.downcast_ref::<RecomposeRefusal>().unwrap().code,
        RECOMPOSE_BINDING_NOT_INSTALLED
    );
    assert!(BindingRef::parse("bundles/core").is_err());
}

#[test]
fn changed_fields_name_nested_leaves() {
    let old = serde_json::json!({"a": 1, "b": {"c": 2, "d": 3}});
    let new = serde_json::json!({"a": 1, "b": {"c": 4, "d": 3}, "e": 5});
    assert_eq!(changed_fields(&old, &new), ["b.c", "e"]);
}

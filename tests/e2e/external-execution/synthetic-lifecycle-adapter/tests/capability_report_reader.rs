//! The e2e node's capability report goes through the daemon's own reader:
//! the signed fixture report admits the real codex profile's controller
//! requirements, and the reader refuses a report that lacks one of them, one
//! signed by another key, and one from another policy generation.

#[path = "../../support/node_capability_report.rs"]
mod node_capability_report;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use node_capability_report::{FixtureEnvironment, signed_report};
use ryeos_app::extension_state::ExtensionState;
use ryeos_app::node_requirement_admission::{
    NodeCapabilityReportReader, NodeRequirementRefused, admit_structured_session_controller,
};
use ryeos_node_runtime::identity::NodeIdentity;

const POLICY_GENERATION: &str = "test-policy-generation";

fn codex_contract() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../bundles/codex/.ai/workers/codex/lib/hosted/external-authoring.profile.json");
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

/// One name per `requires.controller` entry of the real codex profile: what a
/// node that meets it states.
fn controller_names(contract: &serde_json::Value) -> Vec<String> {
    ryeos_engine::structured_session_profile::placement_requirements(contract)
        .unwrap()
        .unwrap()
        .controller
        .iter()
        .map(|entry| entry.names()[0].clone())
        .collect()
}

fn node(root: &Path) -> NodeIdentity {
    let identity = NodeIdentity::create(&root.join("identity/node-key.pem")).unwrap();
    identity
        .write_public_identity(&ryeos_engine::trust::node_public_identity_path(root))
        .unwrap();
    identity
}

fn reading(root: &Path, policy_generation: &str) -> ExtensionState {
    let root = root.to_path_buf();
    let policy_generation = policy_generation.to_owned();
    let extensions = ExtensionState::new();
    extensions.insert(Arc::new(NodeCapabilityReportReader::new(move || {
        ryeos_node::capability_report::current_node_capability_view(&root, &policy_generation)
    })));
    extensions
}

fn record(root: &Path, signer: &NodeIdentity, policy_generation: &str, provided: &[String]) {
    let names: Vec<&str> = provided.iter().map(String::as_str).collect();
    signed_report(
        root,
        signer,
        policy_generation,
        &FixtureEnvironment {
            provided: &names,
            missing: &[],
        },
    )
    .write(root)
    .unwrap();
}

fn refusal(error: anyhow::Error) -> String {
    match error.downcast_ref::<NodeRequirementRefused>() {
        Some(refused) => refused.0.clone(),
        None => format!("{error:#}"),
    }
}

#[test]
fn a_signed_report_stating_the_codex_controller_names_admits_the_launch() {
    let root = tempfile::tempdir().unwrap();
    let identity = node(root.path());
    let contract = codex_contract();
    record(
        root.path(),
        &identity,
        POLICY_GENERATION,
        &controller_names(&contract),
    );
    admit_structured_session_controller(&reading(root.path(), POLICY_GENERATION), &contract)
        .unwrap();
}

#[test]
fn a_report_lacking_a_required_name_refuses_the_launch_naming_it() {
    let root = tempfile::tempdir().unwrap();
    let identity = node(root.path());
    let contract = codex_contract();
    let mut names = controller_names(&contract);
    let dropped = names
        .iter()
        .position(|name| name == "persistence:durable_state")
        .expect("the codex profile requires durable state");
    names.remove(dropped);
    record(root.path(), &identity, POLICY_GENERATION, &names);
    let text = refusal(
        admit_structured_session_controller(&reading(root.path(), POLICY_GENERATION), &contract)
            .unwrap_err(),
    );
    assert!(text.contains("persistence:durable_state"), "{text}");
}

#[test]
fn a_report_signed_by_another_key_is_refused_by_the_reader() {
    let root = tempfile::tempdir().unwrap();
    let _identity = node(root.path());
    let other_root = tempfile::tempdir().unwrap();
    let other = NodeIdentity::create(&other_root.path().join("other-key.pem")).unwrap();
    let contract = codex_contract();
    record(
        root.path(),
        &other,
        POLICY_GENERATION,
        &controller_names(&contract),
    );
    let text = refusal(
        admit_structured_session_controller(&reading(root.path(), POLICY_GENERATION), &contract)
            .unwrap_err(),
    );
    assert!(text.contains("not signed by this node"), "{text}");
}

#[test]
fn a_report_from_another_policy_generation_is_refused_as_stale() {
    let root = tempfile::tempdir().unwrap();
    let identity = node(root.path());
    let contract = codex_contract();
    record(
        root.path(),
        &identity,
        "an-earlier-policy-generation",
        &controller_names(&contract),
    );
    let text = refusal(
        admit_structured_session_controller(&reading(root.path(), POLICY_GENERATION), &contract)
            .unwrap_err(),
    );
    assert!(text.contains("stale"), "{text}");
}

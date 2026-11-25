//! A test node's signed capability report, read back through the production
//! reader.
//!
//! The test states the environment it stands in for: the capability names its
//! node proves. The report is the real `NodeCapabilityReport` with the host's
//! real generation, signed by the test node's own key and written where the
//! daemon records it; the reader registered over it is the daemon's own
//! `current_node_capability_view`, which verifies the digest and signature and
//! refuses a report that is stale for this host or this policy generation.
//! Nothing here skips verification.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use lillux::process_control::{CapabilityEvidence, HOST_PROVIDER, HostCapabilities, ProofSource};
use ryeos_app::node_requirement_admission::NodeCapabilityReportReader;
use ryeos_node::capability_report::{
    CAPABILITY_REPORT_SCHEMA, DurableIdentityFacts, NodeCapabilityReport, NodeComposition,
    NodeExposure, ReportGeneration, SignedNodeCapabilityReport, match_floor, resident_region_bytes,
};
use ryeos_node_runtime::identity::NodeIdentity;

/// What a fixture report states: the names the node proves, each as measured
/// by the host, and the names it refuses with their reasons.
pub struct FixtureEnvironment<'a> {
    pub provided: &'a [&'a str],
    pub missing: &'a [(&'a str, &'a str)],
}

/// Build the report for `app_root` under the host's real generation and
/// `policy_generation`, signed by `signer`.
pub fn signed_report(
    app_root: &Path,
    signer: &NodeIdentity,
    policy_generation: &str,
    environment: &FixtureEnvironment<'_>,
) -> SignedNodeCapabilityReport {
    let (boot_id, cgroup_enclosure) = lillux::process_control::current_host_generation()
        .map_err(anyhow::Error::msg)
        .expect("the test host reports its generation");
    let mut view = HostCapabilities::default();
    for name in environment.provided {
        view.provide(
            name,
            ProofSource::Measured,
            HOST_PROVIDER,
            "stated by the test environment",
        );
    }
    for (name, reason) in environment.missing {
        view.refuse(name, *reason);
    }
    let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
    let credential_home = lillux::process_control::PrivateDirectoryFacts::observe(
        app_root,
        &ryeos_app::private_artifact_home::root(&state_dir),
    );
    let host = lillux::process_control::HostFacts::observe(
        resident_region_bytes().expect("the test host reports its resident region"),
        app_root,
    );
    let floor = match_floor(&[], &view);
    let provided: BTreeMap<String, CapabilityEvidence> = view.provided.clone();
    NodeCapabilityReport {
        schema: CAPABILITY_REPORT_SCHEMA.to_owned(),
        node_fingerprint: signer.fingerprint().to_owned(),
        daemon_digest: "test-node".to_owned(),
        generated_at: lillux::time::iso8601_now(),
        generation: ReportGeneration {
            boot_id,
            cgroup_enclosure,
        },
        subject: "node".to_owned(),
        composition: NodeComposition {
            profile: None,
            host_class: "test".to_owned(),
            bundles: Vec::new(),
            compiled_features: Vec::new(),
            policy_generation: policy_generation.to_owned(),
        },
        exposure: NodeExposure::unattested(Vec::new()),
        host,
        credential_home,
        durable_identity: DurableIdentityFacts::observe(app_root)
            .expect("the test root's durable identity"),
        provided,
        missing: view.missing,
        capacity: BTreeMap::new(),
        floor,
        established_residency: None,
        publications: ryeos_node::capability_report::PublicationsReport::empty(1 << 30),
        in_place_roots: BTreeMap::new(),
    }
    .sign(signer.signing_key())
    .expect("sign the fixture report")
}

/// Record a signed report for `state`'s node and register the daemon's own
/// reader over it.
pub fn install(state: &ryeos_app::state::AppState, environment: &FixtureEnvironment<'_>) {
    let app_root = state.config.app_root.clone();
    let policy_generation = state.node_policy.generation_digest().to_owned();
    signed_report(&app_root, &state.identity, &policy_generation, environment)
        .write(&app_root)
        .expect("record the fixture report");
    state
        .extensions
        .insert(Arc::new(NodeCapabilityReportReader::new(move || {
            ryeos_node::capability_report::current_node_capability_view(
                &app_root,
                &policy_generation,
            )
        })));
}

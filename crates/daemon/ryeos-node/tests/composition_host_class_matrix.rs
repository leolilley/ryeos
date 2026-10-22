//! Offline qualification matrix: each shipped node init profile's floor for
//! each host class it declares, judged against synthetic host evidence for
//! each kind of host a composition image is deployed on. The floors are read
//! from the shipped profile files when the test runs; the evidence goes
//! through the same `HostFacts::capabilities` derivation and `match_floor`
//! admission a node uses at startup.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use lillux::process_control::{
    CapabilityEvidence, CapabilityRequirement, CgroupFacts, HOST_FACTS_VERSION, HOST_PROVIDER,
    HostCapabilities, HostFacts, MemlockFacts, ProbeOutcome, ProofSource, ResidencyProof,
    RootStorageFacts, SCOPE_LIMITS, floor_entry_refusal,
};
use ryeos_app::extension_state::ExtensionState;
use ryeos_app::node_requirement_admission::{
    NodeCapabilityReportReader, NodeRequirementRefused, admit_item_requirements,
    admit_structured_session_controller, admit_structured_session_start,
};
use ryeos_node::capability_report::match_floor;

const CREDENTIAL_PRIVATE_HOME: &str = "credential:private_home";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn profiles_dir() -> PathBuf {
    repo_root().join("bundles/.ai/node/init/profiles")
}

fn load_profile(name: &str) -> serde_yaml::Value {
    let path = profiles_dir().join(format!("{name}.yaml"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_yaml::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn profile_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(profiles_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn host_classes(profile: &serde_yaml::Value) -> &serde_yaml::Mapping {
    profile["policies"]["isolation"]["policy"]["host_classes"]
        .as_mapping()
        .expect("every shipped profile declares host_classes")
}

fn floor(profile: &serde_yaml::Value, class: &str) -> Vec<CapabilityRequirement> {
    let entry = host_classes(profile)
        .get(class)
        .unwrap_or_else(|| panic!("profile does not declare host class {class}"));
    serde_yaml::from_value(entry["requires_capabilities"].clone()).expect("a parseable floor")
}

fn declares(profile: &serde_yaml::Value, class: &str) -> bool {
    host_classes(profile).contains_key(class)
}

/// One kind of host a composition image is deployed on.
struct Host {
    name: &'static str,
    facts: HostFacts,
    /// Whether a supervisor (runit, or the OCI hook) delegates qualified
    /// per-job process scopes to the node. The real derivation needs a live
    /// supervisor, so these hosts get the scope capabilities injected under a
    /// fixed host-runtime provenance: a row that admits on them shows the
    /// floor names the scope entries, not that a probe produces them.
    scopes: bool,
}

impl Host {
    fn capabilities(&self) -> HostCapabilities {
        let mut view = self.facts.capabilities();
        let reason = "no host service delegated a writable cgroup subtree to this node";
        if self.scopes {
            for capability in lillux::ProcessScopeCapability::ALL {
                view.provide(
                    capability.capability_name(),
                    ProofSource::Measured,
                    "host-runtime",
                    "probe scope",
                );
            }
            view.provide(
                SCOPE_LIMITS,
                ProofSource::Measured,
                "host-runtime",
                "per-scope limits",
            );
        } else {
            for capability in lillux::ProcessScopeCapability::ALL {
                view.refuse(capability.capability_name(), reason);
            }
            view.refuse(SCOPE_LIMITS, reason);
        }
        view
    }

    /// The view plus the node-private credential home a Codex login binds.
    fn capabilities_with_credential_home(&self) -> HostCapabilities {
        let mut view = self.capabilities();
        view.provide(
            CREDENTIAL_PRIVATE_HOME,
            ProofSource::Trusted,
            HOST_PROVIDER,
            "private home",
        );
        view
    }
}

fn storage(
    filesystem: &str,
    f_type: u64,
    mount_id: u64,
    kernel_kind: &str,
    private_pid: bool,
) -> RootStorageFacts {
    RootStorageFacts {
        path: "/data".to_owned(),
        filesystem: Some(filesystem.to_owned()),
        f_type: Some(f_type),
        mount_id: Some(mount_id),
        mount_point: Some("/data".to_owned()),
        mount_source: Some("/dev/vdb".to_owned()),
        root_mount_id: Some(20),
        private_pid_namespace: Some(private_pid),
        hypervisor: Some(!private_pid),
        hypervisor_basis: "fixture".to_owned(),
        kernel_kind: kernel_kind.to_owned(),
    }
}

fn volume() -> RootStorageFacts {
    storage("ext4", 0xEF53, 40, "native", true)
}

fn container_root() -> RootStorageFacts {
    storage("overlay", 0x794c7630, 20, "native", true)
}

fn facts(storage: RootStorageFacts) -> HostFacts {
    let kernel_kind = storage.kernel_kind.clone();
    HostFacts {
        version: HOST_FACTS_VERSION,
        kernel_release: "6.8.0".to_owned(),
        kernel_kind,
        boot_id: "boot-1".to_owned(),
        pid_namespace: Some(4026532000),
        initial_pid_namespace: false,
        cgroup_namespace: Some(4026532001),
        initial_cgroup_namespace: false,
        container_enclosure: Some(42),
        node_pid: 7,
        parent_pid: 1,
        pid1: Some("tini".to_owned()),
        pid1_executable: Some("tini".to_owned()),
        enclosure_exit: Some(CapabilityEvidence {
            source: ProofSource::Trusted,
            provider: HOST_PROVIDER.to_owned(),
            basis: "the node's parent is pid 1, tini, named by node policy".to_owned(),
        }),
        enclosure_exit_refusal: None,
        uid_map: Some("0 0 4294967295".to_owned()),
        cgroup: CgroupFacts {
            filesystem: "cgroup2".to_owned(),
            membership: Some("/".to_owned()),
            enclosure_inode: Some(42),
            mount_read_only: Some(true),
            nsdelegate: Some(true),
            membership_writable: false,
            memory_max: None,
            memory_swap_max: None,
            pids_max: None,
        },
        swap_devices: Some(Vec::new()),
        memlock: MemlockFacts {
            soft: Some(8 << 20),
            hard: Some(8 << 20),
            locked_bytes: Some(0),
            cap_ipc_lock: false,
        },
        unshare_user: ProbeOutcome::Refused {
            errno: 1,
            reason: "EPERM".to_owned(),
        },
        unshare_user_pid_mount: ProbeOutcome::Refused {
            errno: 1,
            reason: "EPERM".to_owned(),
        },
        pidfd: ProbeOutcome::Allowed,
        subreaper: ProbeOutcome::Allowed,
        resident_bytes: 64 << 20,
        residency: Some(ResidencyProof::SwapAbsent {
            basis: "no swap device is active".to_owned(),
        }),
        residency_refusal: None,
        app_root_storage: storage,
        declared_lifetime: None,
    }
}

fn with_storage(mut base: HostFacts, storage: RootStorageFacts) -> HostFacts {
    base.kernel_kind = storage.kernel_kind.clone();
    base.app_root_storage = storage;
    base
}

fn locked(mut base: HostFacts) -> HostFacts {
    base.residency = Some(ResidencyProof::Locked {
        basis: "mlock of the resident region".to_owned(),
    });
    base.memlock.cap_ipc_lock = true;
    base
}

/// Artix with runit on a machine's own disk: no container, node policy's
/// supervisor delegates qualified process scopes.
fn runit() -> Host {
    let mut base = facts(storage("ext4", 0xEF53, 20, "native", false));
    base.initial_pid_namespace = true;
    base.initial_cgroup_namespace = true;
    base.container_enclosure = None;
    base.parent_pid = 600;
    base.pid1 = Some("runit".to_owned());
    base.pid1_executable = Some("runit".to_owned());
    base.enclosure_exit = None;
    base.enclosure_exit_refusal =
        Some("the node is not pid 1 and its parent is not an init policy trusts".to_owned());
    base.unshare_user_pid_mount = ProbeOutcome::Allowed;
    base.unshare_user = ProbeOutcome::Allowed;
    without_hypervisor(&mut base);
    Host {
        name: "runit",
        facts: locked(base),
        scopes: true,
    }
}

/// The host's own disk is not behind a hypervisor and is the root mount.
fn without_hypervisor(base: &mut HostFacts) {
    base.app_root_storage.hypervisor = Some(false);
    base.app_root_storage.mount_id = base.app_root_storage.root_mount_id;
}

/// Plain Docker under tini with `/data` a volume and the memory lock granted.
fn local_docker() -> Host {
    Host {
        name: "local-docker",
        facts: locked(facts(volume())),
        scopes: false,
    }
}

fn local_docker_no_volume() -> Host {
    Host {
        name: "local-docker-no-volume",
        facts: locked(with_storage(facts(volume()), container_root())),
        scopes: false,
    }
}

/// Docker whose OCI hook delegates qualified per-job process scopes.
fn local_docker_oci_hook() -> Host {
    Host {
        name: "local-docker-oci-hook",
        facts: locked(facts(volume())),
        scopes: true,
    }
}

/// A hosting provider's container with an attached volume and no swap lock.
/// A Render service with a persistent disk proves the same facts (a volume
/// mount, no scope delegation, no memory lock), so it is not a separate host.
fn railway() -> Host {
    Host {
        name: "railway",
        facts: facts(volume()),
        scopes: false,
    }
}

/// A Render web service: the railway shape plus user, pid and mount
/// namespaces, which Render containers allow and the node proves by probe.
fn render_service() -> Host {
    let mut base = facts(volume());
    base.unshare_user_pid_mount = ProbeOutcome::Allowed;
    base.unshare_user = ProbeOutcome::Allowed;
    Host {
        name: "render-service",
        facts: base,
        scopes: false,
    }
}

fn railway_no_volume() -> Host {
    Host {
        name: "railway-no-volume",
        facts: with_storage(facts(volume()), container_root()),
        scopes: false,
    }
}

/// A gVisor sandbox with no persistent disk; the node is its pid 1.
fn render_sandbox() -> Host {
    let mut base = with_storage(
        facts(volume()),
        storage("overlay", 0x794c7630, 20, "gvisor", true),
    );
    base.parent_pid = 0;
    base.node_pid = 1;
    base.enclosure_exit = Some(CapabilityEvidence {
        source: ProofSource::Measured,
        provider: HOST_PROVIDER.to_owned(),
        basis: "the node is pid 1 of its pid namespace".to_owned(),
    });
    Host {
        name: "render-sandbox",
        facts: base,
        scopes: false,
    }
}

enum Expect {
    Admit,
    /// Refused, with at least these floor entries unmet.
    Refuse(&'static [&'static str]),
}

fn check(profile_name: &str, class: &str, host: &Host, expect: &Expect) {
    let profile = load_profile(profile_name);
    let requires = floor(&profile, class);
    let admission = match_floor(&requires, &host.capabilities());
    let context = format!("{profile_name} [{class}] on {}: {admission:#?}", host.name);
    match expect {
        Expect::Admit => assert!(admission.admitted, "expected admission, {context}"),
        Expect::Refuse(entries) => {
            assert!(!admission.admitted, "expected refusal, {context}");
            for entry in *entries {
                assert!(
                    admission
                        .unmet
                        .get(*entry)
                        .is_some_and(|reason| !reason.is_empty()),
                    "expected {entry} unmet with a reason, {context}"
                );
            }
        }
    }
}

const SCOPE_TRIO: &[&str] = &["limits:scope", "recovery:scope", "termination:scope"];
const NO_DISK: &[&str] = &["storage:durable_root"];
const NO_NAMESPACES: &[&str] = &["isolation:namespaces"];
const NO_ENCLOSURE_LIMIT: &[&str] = &["limits:enclosure"];
const NO_ACCOUNTING: &[&str] = &["accounting"];
/// What bare runit lacks of the controller's `local` floor: a container
/// enclosure to recover and terminate jobs through.
const NO_LOCAL_ENCLOSURE: &[&str] = &[
    "any_of(recovery:enclosure, recovery:enclosure_trusted_init)",
    "any_of(termination:enclosure, termination:enclosure_trusted_init)",
];
/// The same, for the controller's `hosted` floor.
const NO_HOSTED_ENCLOSURE: &[&str] = &[
    "recovery:enclosure_trusted_init",
    "termination:enclosure_trusted_init",
];

fn core_cases() -> Vec<(&'static str, Host, Expect)> {
    vec![
        ("local", runit(), Expect::Admit),
        ("local", local_docker(), Expect::Refuse(SCOPE_TRIO)),
        ("local", local_docker_oci_hook(), Expect::Admit),
        ("local", railway(), Expect::Refuse(SCOPE_TRIO)),
        ("hosted", local_docker(), Expect::Admit),
        ("hosted", railway(), Expect::Admit),
        ("hosted", railway_no_volume(), Expect::Refuse(NO_DISK)),
        ("hosted", local_docker_no_volume(), Expect::Refuse(NO_DISK)),
        ("hosted", render_sandbox(), Expect::Refuse(NO_DISK)),
    ]
}

#[test]
fn the_core_compositions_follow_the_host_class_matrix() {
    for profile in ["core", "core-scheduling", "core-workflow", "core-knowledge"] {
        for (class, host, expect) in core_cases() {
            check(profile, class, &host, &expect);
        }
    }
}

#[test]
fn every_shipped_profile_declares_the_host_classes_its_composition_runs_on() {
    let both = ["hosted", "local"];
    let local_only = ["local"];
    let hosted_only = ["hosted"];
    let controller = ["hosted", "hosted_enforced", "local"];
    let development = ["local", "local_direct"];
    let expected: [(&str, &[&str]); 16] = [
        ("core", &both),
        ("core-scheduling", &both),
        ("core-workflow", &both),
        ("core-knowledge", &both),
        ("standard", &both),
        ("f5r-controller", &controller),
        ("standard-central-auth", &local_only),
        ("central-host", &local_only),
        ("bundle-source", &local_only),
        ("local-inference", &local_only),
        ("contained-workflow", &local_only),
        ("release-authority", &local_only),
        ("development", &development),
        ("full", &local_only),
        ("hosted-workflow", &hosted_only),
        ("hosted-node", &hosted_only),
    ];
    let shipped: BTreeSet<String> = profile_names().into_iter().collect();
    let covered: BTreeSet<String> = expected
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    assert_eq!(
        shipped, covered,
        "every shipped profile has a declared-classes row"
    );
    for (profile, classes) in expected {
        let loaded = load_profile(profile);
        let declared: BTreeSet<String> = host_classes(&loaded)
            .keys()
            .map(|key| key.as_str().unwrap().to_owned())
            .collect();
        let want: BTreeSet<String> = classes.iter().map(|class| (*class).to_owned()).collect();
        assert_eq!(declared, want, "{profile}");
    }
}

#[test]
fn standard_follows_the_host_class_matrix() {
    let cases: Vec<(&str, Host, Expect)> = vec![
        ("local", runit(), Expect::Admit),
        ("local", local_docker(), Expect::Admit),
        ("local", local_docker_oci_hook(), Expect::Admit),
        ("local", local_docker_no_volume(), Expect::Refuse(NO_DISK)),
        ("local", railway(), Expect::Refuse(NO_ACCOUNTING)),
        ("hosted", local_docker(), Expect::Admit),
        ("hosted", railway(), Expect::Admit),
        ("hosted", railway_no_volume(), Expect::Refuse(NO_DISK)),
        ("hosted", render_sandbox(), Expect::Refuse(NO_DISK)),
    ];
    for (class, host, expect) in &cases {
        check("standard", class, host, expect);
    }
}

/// The local floor of the central-auth, central-host and bundle-source
/// compositions: accounting, a durable root and time, no process scopes.
fn plain_local_cases() -> Vec<(&'static str, Host, Expect)> {
    vec![
        ("local", runit(), Expect::Admit),
        ("local", local_docker(), Expect::Admit),
        ("local", local_docker_oci_hook(), Expect::Admit),
        ("local", railway(), Expect::Refuse(NO_ACCOUNTING)),
        ("local", local_docker_no_volume(), Expect::Refuse(NO_DISK)),
        ("local", render_sandbox(), Expect::Refuse(NO_DISK)),
    ]
}

#[test]
fn the_central_and_bundle_source_compositions_follow_the_host_class_matrix() {
    for profile in ["standard-central-auth", "central-host", "bundle-source"] {
        for (class, host, expect) in plain_local_cases() {
            check(profile, class, &host, &expect);
        }
    }
}

#[test]
fn the_hosted_compositions_follow_the_host_class_matrix() {
    let hosted_node: Vec<(&str, Host, Expect)> = vec![
        ("hosted", runit(), Expect::Admit),
        ("hosted", local_docker(), Expect::Admit),
        ("hosted", railway(), Expect::Admit),
        ("hosted", railway_no_volume(), Expect::Admit),
        (
            "hosted",
            render_sandbox(),
            Expect::Refuse(&["any_of(accounting, residency:swap_absent)"]),
        ),
    ];
    for (class, host, expect) in &hosted_node {
        check("hosted-node", class, host, expect);
    }
    let hosted_workflow: Vec<(&str, Host, Expect)> = vec![
        ("hosted", local_docker(), Expect::Admit),
        ("hosted", railway(), Expect::Admit),
        ("hosted", railway_no_volume(), Expect::Refuse(NO_DISK)),
        ("hosted", local_docker_no_volume(), Expect::Refuse(NO_DISK)),
        ("hosted", render_sandbox(), Expect::Refuse(NO_DISK)),
    ];
    for (class, host, expect) in &hosted_workflow {
        check("hosted-workflow", class, host, expect);
    }
}

#[test]
fn the_local_inference_and_scoped_compositions_follow_the_host_class_matrix() {
    let scoped = |isolation: Expect| -> Vec<(&'static str, Host, Expect)> {
        vec![
            ("local", runit(), Expect::Admit),
            ("local", local_docker(), Expect::Refuse(SCOPE_TRIO)),
            ("local", local_docker_oci_hook(), isolation),
            ("local", railway(), Expect::Refuse(SCOPE_TRIO)),
        ]
    };
    for (profile, cases) in [
        ("local-inference", scoped(Expect::Admit)),
        ("full", scoped(Expect::Admit)),
        // The OCI hook delegates scopes but not user namespaces.
        ("contained-workflow", scoped(Expect::Refuse(NO_NAMESPACES))),
        ("development", scoped(Expect::Refuse(NO_NAMESPACES))),
    ] {
        for (class, host, expect) in &cases {
            check(profile, class, host, expect);
        }
    }
}

#[test]
fn the_release_authority_composition_needs_an_enclosure_limit_no_host_here_proves() {
    for host in [runit(), local_docker(), local_docker_oci_hook(), railway()] {
        check(
            "release-authority",
            "local",
            &host,
            &Expect::Refuse(NO_ENCLOSURE_LIMIT),
        );
    }
    check(
        "release-authority",
        "local",
        &local_docker(),
        &Expect::Refuse(NO_NAMESPACES),
    );
}

#[test]
fn the_f5r_controller_node_follows_the_host_class_matrix() {
    let cases: Vec<(&str, Host, Expect)> = vec![
        ("hosted", local_docker(), Expect::Admit),
        ("hosted", railway(), Expect::Admit),
        ("hosted", railway_no_volume(), Expect::Refuse(NO_DISK)),
        ("hosted", render_sandbox(), Expect::Refuse(NO_DISK)),
        ("local", local_docker(), Expect::Admit),
        ("local", railway(), Expect::Refuse(NO_ACCOUNTING)),
        ("local", local_docker_no_volume(), Expect::Refuse(NO_DISK)),
        ("local", runit(), Expect::Refuse(NO_LOCAL_ENCLOSURE)),
        ("hosted", runit(), Expect::Refuse(NO_HOSTED_ENCLOSURE)),
        ("hosted_enforced", render_service(), Expect::Admit),
        ("hosted_enforced", render_sandbox(), Expect::Refuse(NO_DISK)),
        (
            "hosted_enforced",
            runit(),
            Expect::Refuse(NO_HOSTED_ENCLOSURE),
        ),
    ];
    for (class, host, expect) in &cases {
        check("f5r-controller", class, host, expect);
    }
}

#[test]
fn the_development_direct_class_admits_an_operator_host_without_a_scope_association() {
    let cases: Vec<(&str, Host, Expect)> = vec![
        ("local_direct", runit(), Expect::Admit),
        ("local_direct", local_docker(), Expect::Admit),
        (
            "local_direct",
            local_docker_no_volume(),
            Expect::Refuse(NO_DISK),
        ),
        ("local_direct", railway(), Expect::Refuse(NO_ACCOUNTING)),
    ];
    for (class, host, expect) in &cases {
        check("development", class, host, expect);
    }
}

#[test]
fn the_admission_names_the_proof_that_met_each_enclosure_entry() {
    let profile = load_profile("f5r-controller");
    let admission = match_floor(&floor(&profile, "hosted"), &railway().capabilities());
    assert!(admission.admitted, "{admission:#?}");
    let termination = &admission.met_by["termination:enclosure_trusted_init"];
    assert_eq!(termination.name, "termination:enclosure_trusted_init");
    assert_eq!(termination.source, ProofSource::Trusted);
    let accounting = &admission.met_by["any_of(accounting, residency:swap_absent)"];
    assert_eq!(accounting.name, "residency:swap_absent");
}

#[test]
fn every_shipped_profile_declares_valid_floors_that_a_runit_host_meets_in_its_local_class() {
    let runit = runit();
    let view = runit.capabilities();
    for name in profile_names() {
        let profile = load_profile(&name);
        assert!(
            !host_classes(&profile).is_empty(),
            "{name} declares no host class"
        );
        for (class, entry) in host_classes(&profile) {
            let class = class.as_str().unwrap();
            let requires = floor(&profile, class);
            for requirement in &requires {
                assert_eq!(
                    floor_entry_refusal(requirement),
                    None,
                    "{name} [{class}]: {requirement:?}"
                );
            }
            assert!(
                entry["requires_capabilities"].is_sequence(),
                "{name} [{class}]"
            );
        }
        // The profiles that need an enclosure limit or a container enclosure
        // ask for more than a bare runit host proves.
        let beyond_runit = ["release-authority", "f5r-controller"];
        if name == "f5r-controller" {
            let admission = match_floor(&floor(&profile, "local"), &view);
            assert!(
                !admission.admitted,
                "{name} [local] on runit: {admission:#?}"
            );
            for entry in NO_LOCAL_ENCLOSURE {
                assert!(
                    admission.unmet.contains_key(*entry),
                    "{name} [local] on runit lacks {entry}: {admission:#?}"
                );
            }
        }
        if declares(&profile, "local") && !beyond_runit.contains(&name.as_str()) {
            let admission = match_floor(&floor(&profile, "local"), &view);
            assert!(
                admission.admitted,
                "{name} [local] on runit: {admission:#?}"
            );
        }
    }
}

fn node_reporting(view: HostCapabilities) -> ExtensionState {
    let extensions = ExtensionState::new();
    extensions.insert(Arc::new(NodeCapabilityReportReader::new(move || {
        Ok(view.clone())
    })));
    extensions
}

fn codex_contract() -> serde_json::Value {
    let path = repo_root()
        .join("bundles/codex/.ai/workers/codex/lib/hosted/external-authoring.profile.json");
    serde_json::from_slice(
        &std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display())),
    )
    .unwrap()
}

fn refusal_text(error: anyhow::Error) -> String {
    error
        .downcast_ref::<NodeRequirementRefused>()
        .unwrap_or_else(|| panic!("not a requirement refusal: {error:#}"))
        .0
        .clone()
}

#[test]
fn the_codex_controller_launch_is_judged_on_the_enclosure_each_host_proves() {
    let contract = codex_contract();
    for host in [local_docker(), railway()] {
        let node = node_reporting(host.capabilities_with_credential_home());
        admit_structured_session_controller(&node, &contract)
            .unwrap_or_else(|error| panic!("{}: {error:#}", host.name));
        admit_structured_session_start(&node, &contract, Some("credential_profile"))
            .unwrap_or_else(|error| panic!("{}: {error:#}", host.name));
    }
    for (host, unmet) in [
        (railway_no_volume(), "persistence:durable_state"),
        (local_docker_no_volume(), "persistence:durable_state"),
        (render_sandbox(), "persistence:durable_state"),
    ] {
        let node = node_reporting(host.capabilities_with_credential_home());
        let text = refusal_text(admit_structured_session_controller(&node, &contract).unwrap_err());
        assert!(text.contains(unmet), "{}: {text}", host.name);
    }
}

#[test]
fn the_codex_controller_launch_is_refused_on_bare_runit() {
    let contract = codex_contract();
    let node = node_reporting(runit().capabilities_with_credential_home());
    let text = refusal_text(admit_structured_session_controller(&node, &contract).unwrap_err());
    assert!(text.contains("requires.controller"), "{text}");
    assert!(text.contains("recovery:enclosure"), "{text}");
    assert!(text.contains("termination:enclosure"), "{text}");
}

#[test]
fn a_controller_whose_enclosure_is_unproven_refuses_the_codex_launch() {
    let contract = codex_contract();
    let mut host = railway();
    host.facts.enclosure_exit = None;
    host.facts.enclosure_exit_refusal =
        Some("the node is not pid 1 and its parent is not a trusted init".to_owned());
    let node = node_reporting(host.capabilities_with_credential_home());
    let text = refusal_text(admit_structured_session_controller(&node, &contract).unwrap_err());
    assert!(text.contains("requires.controller"), "{text}");
    assert!(text.contains("recovery:enclosure"), "{text}");
    assert!(text.contains("termination:enclosure"), "{text}");
}

#[test]
fn the_codex_login_is_refused_without_a_credential_private_home() {
    let contract = codex_contract();
    let node = node_reporting(railway().capabilities());
    let text = refusal_text(
        admit_structured_session_start(&node, &contract, Some("credential_profile")).unwrap_err(),
    );
    assert!(text.contains(CREDENTIAL_PRIVATE_HOME), "{text}");
}

const PROBE_REF: &str = "tool:ryeos/core/qualification/namespaces-probe";

/// The `requires` block of the signed probe item the core bundle ships,
/// read from the item file itself.
fn shipped_probe_requires() -> ryeos_bundle::runtime_authority::RuntimeRequires {
    let path =
        repo_root().join("bundles/core/.ai/tools/ryeos/core/qualification/namespaces-probe.yaml");
    let document: serde_yaml::Value = serde_yaml::from_slice(
        &std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display())),
    )
    .unwrap();
    let requires = serde_json::to_value(&document["requires"]).unwrap();
    ryeos_bundle::runtime_authority::parse_runtime_requires_block(&requires)
        .unwrap_or_else(|reason| panic!("{}: {reason}", path.display()))
}

#[test]
fn the_shipped_probe_declares_namespace_isolation_as_data() {
    let requires = shipped_probe_requires();
    let node: Vec<String> = requires.node.iter().map(|entry| entry.label()).collect();
    assert_eq!(node, vec!["isolation:namespaces".to_owned()]);
    assert!(requires.credential_kinds.is_empty());
}

#[test]
fn workloads_are_admitted_or_refused_on_what_the_host_proves() {
    let none = ryeos_bundle::runtime_authority::RuntimeRequires::default();
    let probe = PROBE_REF;
    let namespaces = shipped_probe_requires();
    // An item that declares no requirement, such as
    // `tool:ryeos/core/identity/public_key`, is admitted on every host.
    for host in [runit(), local_docker(), railway(), render_sandbox()] {
        let node = node_reporting(host.capabilities());
        admit_item_requirements(&node, "tool:ryeos/core/identity/public_key", &none)
            .unwrap_or_else(|error| panic!("{}: {error:#}", host.name));
    }
    // An item that needs namespace isolation is admitted only where the
    // host allows the unshare.
    let node = node_reporting(runit().capabilities());
    admit_item_requirements(&node, probe, &namespaces).unwrap();
    for host in [local_docker(), railway()] {
        let node = node_reporting(host.capabilities());
        let text = refusal_text(admit_item_requirements(&node, probe, &namespaces).unwrap_err());
        assert!(
            text.contains("isolation:namespaces"),
            "{}: {text}",
            host.name
        );
    }
}

#[test]
fn core_carries_no_scheduling_item_and_the_scheduling_bundle_does() {
    // A scheduling item is refused on a core image because the item is not
    // there to resolve: the scheduler services live only in the scheduling
    // bundle that core-scheduling adds.
    let services = |bundle: &str| {
        repo_root()
            .join("bundles")
            .join(bundle)
            .join(".ai/services/scheduler")
    };
    assert!(!services("core").exists(), "core ships scheduler services");
    assert!(services("scheduling").join("register.yaml").is_file());
}

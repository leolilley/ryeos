// A UI seat reads the records services through the service items' own
// `disclosure` tiers, narrowed by the node's `read_disclosure` entries.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use ryeos_node_runtime::disclosure::{DisclosureClass, ServiceDisclosure, shape_result};
use ryeos_node_runtime::node_policy::NodePolicyContext;
use ryeos_node_runtime::node_policy::NodePolicySection as _;
use ryeos_node_runtime::node_policy::sections::read_disclosure::{
    ReadDisclosurePolicy, ReadDisclosurePolicySection, SECTION_NAME,
};
use ryeos_ui::seat_read_policy::{RequestFacts, SeatReadRefusal, SessionFacts, decide};

const GET: &str = "service:records/get";
const KINDS: &str = "service:records/kinds";

fn profile_paths() -> Vec<std::path::PathBuf> {
    let dir = ryeos_engine::test_support::workspace_root().join("bundles/.ai/node/init/profiles");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    paths.sort();
    paths
}

fn profile(path: &std::path::Path) -> Value {
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn policy_of(path: &std::path::Path) -> ReadDisclosurePolicy {
    let record = ReadDisclosurePolicySection
        .parse(
            &NodePolicyContext {
                section: SECTION_NAME.to_owned(),
                source_file: path.to_owned(),
                signer_fingerprint: "test".to_owned(),
            },
            &profile(path)["policies"][SECTION_NAME],
        )
        .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    record
        .as_any()
        .downcast_ref::<ReadDisclosurePolicy>()
        .unwrap()
        .clone()
}

fn standard() -> ReadDisclosurePolicy {
    policy_of(
        &ryeos_engine::test_support::workspace_root()
            .join("bundles/.ai/node/init/profiles/standard.yaml"),
    )
}

fn service_item(name: &str) -> Value {
    let path = ryeos_engine::test_support::workspace_root()
        .join(format!("bundles/core/.ai/services/records/{name}.yaml"));
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The parameters the service's own descriptor declares.
fn declared(name: &str) -> BTreeMap<String, String> {
    service_item(name)["schema"]
        .as_object()
        .expect("the service declares its parameters")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
        .collect()
}

fn call(
    policy: &ReadDisclosurePolicy,
    target: &str,
    params: Value,
) -> Result<ryeos_ui::seat_read_policy::DetachedShaping, SeatReadRefusal> {
    let name = target.rsplit('/').next().unwrap();
    let admitted = decide(
        policy,
        target,
        &declared(name),
        Some(RequestFacts {
            params,
            session: SessionFacts {
                principal_id: "fp:test",
                project_root: None,
            },
        }),
    )?;
    Ok(admitted
        .shaping
        .expect("a policy read has shaping")
        .detach())
}

/// The service item's own tier for a principal, then the policy entry's
/// narrowing: the answer a seat receives. A refusal is its stable code.
/// (The operator class no longer exists in the seat policy, so there is one
/// view.)
fn seat_view(
    name: &str,
    shaping: &ryeos_ui::seat_read_policy::DetachedShaping,
    answer: Value,
) -> Result<Value, String> {
    let item = service_item(name);
    let tier = ServiceDisclosure::parse(&item["disclosure"]).unwrap();
    let by_item = shape_result(
        DisclosureClass::Principal,
        Some(&tier),
        answer,
        tier.bounds(),
    )
    .map_err(|refusal| refusal.code().to_owned())?;
    shaping
        .apply(by_item)
        .map_err(|refusal| refusal.code().to_owned())
}

fn report_request() -> Value {
    json!({"record_kind": "capability_report"})
}

/// What `records/get` answers for the capability report, with fields a seat
/// must not see: free-text reasons, paths, addresses, a failure detail and a
/// field a later release might add.
fn answer() -> Value {
    json!({
        "record_kind": "capability_report",
        "key": "singleton",
        "hash": "d1",
        "stored_as": "file",
        "bytes": 1234,
        "truncated": false,
        "verify": {
            "status": "verified",
            "scheme": "node_signed_json_digest",
            "signer": "fp:node",
            "signer_status": "node",
            "reason": null,
            "detail": "/home/operator/.ai/state/node-capability-report.json",
            "fresh": false,
            "new_verify_field": 1
        },
        "body": {
            "schema": "ryeos.node.capability-report/v9",
            "generated_at": "2026-11-23T00:00:00Z",
            "node_fingerprint": "node",
            "subject": "node",
            "daemon_digest": "dd",
            "generation": {"boot_id": "b"},
            "credential_home": {"path": "/home/operator/.credentials"},
            "in_place_roots": {"r": {"path": "/srv/root"}},
            "composition": {
                "host_class": "container",
                "profile": "standard",
                "policy_generation": "pg",
                "bundles": ["core", "standard"],
                "compiled_features": ["scheduling"]
            },
            "exposure": {
                "listeners": ["0.0.0.0:8080"],
                "ingress": {"source": "attested", "provider": "edge", "basis": "https://edge.internal"}
            },
            "host": {"kernel_kind": "native", "kernel_release": "6.9.1", "node_pid": 42, "uid_map": "0 1000 1"},
            "provided": {
                "isolation:pid_ns": {
                    "source": "measured",
                    "provider": "pid_namespace_init",
                    "basis": "probe failed at /proc/self/ns/pid",
                    "new_field": "leak"
                }
            },
            "missing": {"quiescence:pid_namespace": "never provided: /proc/x"},
            "capacity": {"enclosure_memory_max_bytes": 4096},
            "floor": {
                "requires": ["x"],
                "admitted": true,
                "met_by": {"isolation": {"name": "isolation:pid_ns", "source": "measured", "provider": "pid_namespace_init", "extra": 1}},
                "unmet": {"quiescence": "never provided: /proc/x"}
            }
        },
        "collections": {
            "provided": [{"name": "isolation:pid_ns", "source": "measured", "provider": "pid_namespace_init", "basis": "probe failed at /proc/self/ns/pid", "new_field": "leak"}],
            "missing": [{"name": "quiescence:pid_namespace", "value": "never provided: /proc/x"}],
            "floor_met": [{"label": "isolation", "name": "isolation:pid_ns", "source": "measured", "provider": "pid_namespace_init", "extra": 1}],
            "floor_unmet": [{"label": "quiescence", "value": "never provided: /proc/x"}],
            "capacity": [{"name": "enclosure_memory_max_bytes", "value": 4096}]
        },
        "collections_coverage": {"provided": "present", "missing": "present", "floor_met": "present", "floor_unmet": "present", "capacity": "present"}
    })
}

fn shown() -> Value {
    let shaping = call(&standard(), GET, report_request()).expect("the report read is admitted");
    seat_view("get", &shaping, answer()).expect("the answer fits its bounds")
}

#[test]
fn every_profile_has_one_entry_per_records_read_and_admits_it() {
    for path in profile_paths() {
        let policy = policy_of(&path);
        for target in [GET, KINDS] {
            assert_eq!(
                policy.entries_for(target).len(),
                1,
                "{} {target}",
                path.display()
            );
            // Mint time: no request yet.
            decide(
                &policy,
                target,
                &declared(target.rsplit('/').next().unwrap()),
                None,
            )
            .unwrap_or_else(|error| panic!("{} {target}: {error}", path.display()));
        }
    }
}

#[test]
fn every_profile_lets_a_session_hold_the_record_read_capabilities() {
    for path in profile_paths() {
        let scopes = profile(&path)["policies"]["ui_browser_sessions"]["session_scopes"].clone();
        let scopes: Vec<&str> = scopes
            .as_array()
            .unwrap()
            .iter()
            .map(|scope| scope.as_str().unwrap())
            .collect();
        for needed in [
            "ryeos.execute.service.records/get",
            "ryeos.execute.service.records/kinds",
        ] {
            assert!(scopes.contains(&needed), "{}: {needed}", path.display());
        }
    }
}

#[test]
fn a_principal_sees_names_grades_and_providers_and_no_free_text() {
    let shown = shown();
    let text = serde_json::to_string(&shown).unwrap();
    // What is kept, including a report from an earlier policy generation
    // arriving marked not fresh.
    assert_eq!(shown["record_kind"], "capability_report");
    assert_eq!(shown["key"], "singleton");
    assert_eq!(shown["verify"]["status"], "verified");
    assert_eq!(shown["verify"]["scheme"], "node_signed_json_digest");
    assert_eq!(shown["verify"]["fresh"], false);
    // What is left out: the body and its reasons, probe text, paths,
    // addresses, capacity, kernel release, lists of what is installed, the
    // signer, the verify detail, and any field no tier names.
    for gone in [
        "/proc/",
        "/home/operator",
        "/srv/root",
        "0.0.0.0",
        "edge.internal",
        "6.9.1",
        "leak",
        "new_field",
        "new_verify_field",
        "never provided",
        "standard",
        "scheduling",
        "daemon_digest",
        "credential_home",
        "in_place_roots",
        "enclosure_memory_max_bytes",
        "node_pid",
        "uid_map",
        "fp:node",
        "\"signer\"",
        "probe failed",
    ] {
        assert!(
            !text.contains(gone),
            "principal view leaks `{gone}`: {text}"
        );
    }
    // What the capability view exists to show: names, grades, providers and the
    // closed coverage words, by literal collection name.
    assert_eq!(shown["verify"]["signer_status"], "node");
    assert_eq!(shown["body"]["composition"]["host_class"], "container");
    assert_eq!(shown["body"]["floor"]["admitted"], true);
    assert_eq!(shown["body"]["host"]["kernel_kind"], "native");
    let provided = &shown["collections"]["provided"][0];
    assert_eq!(provided["name"], "isolation:pid_ns");
    assert_eq!(provided["source"], "measured");
    assert_eq!(provided["provider"], "pid_namespace_init");
    assert!(provided.get("basis").is_none(), "{provided}");
    assert_eq!(
        shown["collections"]["missing"],
        json!([{"name": "quiescence:pid_namespace"}]),
        "a missing proof is named without its reason"
    );
    assert_eq!(
        shown["collections"]["floor_unmet"],
        json!([{"label": "quiescence"}])
    );
    assert_eq!(
        shown["collections"]["floor_met"][0],
        json!({"label": "isolation", "name": "isolation:pid_ns", "source": "measured", "provider": "pid_namespace_init"})
    );
    assert_eq!(
        shown["collections"]["capacity"],
        json!([{"name": "enclosure_memory_max_bytes"}]),
        "capacity is named and carries no number"
    );
    for collection in [
        "provided",
        "missing",
        "floor_met",
        "floor_unmet",
        "capacity",
    ] {
        assert_eq!(shown["collections_coverage"][collection], "present");
    }
    // The body itself is only the named leaves.
    assert!(shown["body"].get("credential_home").is_none());
    assert!(shown["body"].get("provided").is_none());
}

/// A node whose report says nothing of enforcement carries no
/// `isolation:enforced` row, so a seat is told nothing of posture: the
/// posture is that row's presence, never computed here.
#[test]
fn a_report_silent_on_enforcement_shows_no_posture_row() {
    let mut silent = answer();
    silent["collections"]["provided"] = json!([]);
    let shaping = call(&standard(), GET, report_request()).expect("admitted");
    let shown = seat_view("get", &shaping, silent).expect("fits");
    assert_eq!(shown["collections"]["provided"], json!([]));
    assert!(
        !serde_json::to_string(&shown)
            .unwrap()
            .contains("isolation:enforced")
    );
}

#[test]
fn a_record_that_failed_verification_reaches_a_seat_without_a_body() {
    let failed = json!({
        "record_kind": "capability_report",
        "key": "singleton",
        "hash": null,
        "stored_as": "file",
        "bytes": null,
        "truncated": false,
        "verify": {"status": "failed", "scheme": "node_signed_json_digest", "signer": null, "signer_status": null, "reason": "stale_format", "detail": "/home/operator/x", "fresh": null}
    });
    let shaping = call(&standard(), GET, report_request()).unwrap();
    let shown = seat_view("get", &shaping, failed).unwrap();
    assert_eq!(shown["verify"]["status"], "failed");
    assert_eq!(shown["verify"]["reason"], "stale_format");
    assert!(shown.get("body").is_none());
    assert!(
        !serde_json::to_string(&shown)
            .unwrap()
            .contains("/home/operator")
    );
}

#[test]
fn the_kinds_read_hides_a_load_error_from_a_seat() {
    let kinds = json!({"kinds": [
        {"record_kind": "capability_report", "usable": true, "reason": null, "storage": "node_file", "verifier": "node_signed_json_digest", "key": "singleton", "holder": null, "freshness": "node_policy_generation", "max_bytes": 1048576},
        {"record_kind": "broken", "usable": false, "reason": "the declaration did not load: /home/operator/.ai/x"}
    ]});
    let shaping = call(&standard(), KINDS, json!({})).unwrap();
    let shown = seat_view("kinds", &shaping, kinds).unwrap();
    assert_eq!(shown["kinds"][0]["record_kind"], "capability_report");
    assert_eq!(shown["kinds"][0]["usable"], true);
    assert_eq!(shown["kinds"][1]["record_kind"], "broken");
    assert_eq!(shown["kinds"][1]["usable"], false);
    for row in shown["kinds"].as_array().unwrap() {
        assert!(row.get("reason").is_none());
    }
    let text = serde_json::to_string(&shown).unwrap();
    assert!(!text.contains("/home/operator"), "{text}");
    assert!(!text.contains("did not load"), "{text}");
}

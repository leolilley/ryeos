//! Every service a hosted browser session can reach must say what a caller
//! that is not the node operator may see of its answer, or say plainly that it
//! may see nothing. A service with neither is refused to that caller at run
//! time, so a new service that forgets its block fails here instead of in the
//! browser.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ryeos_node_runtime::disclosure::ServiceDisclosure;
use serde_json::Value;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn yaml(path: &Path) -> Value {
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

/// Every service item in every bundle: its name and parsed body.
fn services() -> Vec<(String, Value)> {
    let mut found = Vec::new();
    let bundles = repo().join("bundles");
    for bundle in std::fs::read_dir(&bundles).unwrap().flatten() {
        let root = bundle.path().join(".ai/services");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for file in files {
            let name = file
                .strip_prefix(&root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            found.push((name, yaml(&file)));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

/// The services each route serves, with whether the route needs no
/// authentication. A route's caller is whoever the route admits, so a service
/// behind one is reachable by callers other than the operator.
fn route_targets() -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let bundles = repo().join("bundles");
    for bundle in std::fs::read_dir(&bundles).unwrap().flatten() {
        let mut files = Vec::new();
        collect(&bundle.path().join(".ai/node/routes"), &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let open = text.lines().any(|line| line.trim() == "auth: none");
            for line in text.lines() {
                let mut rest = line;
                while let Some(at) = rest.find("service:") {
                    rest = &rest[at + "service:".len()..];
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || "/_-.".contains(*c))
                        .collect();
                    if !name.is_empty() {
                        found.push((name, open));
                    }
                }
            }
        }
    }
    found
}

/// Services deliberately served to no caller but the operator. Adding to this
/// list is a decision to be reviewed, never a way to silence the test.
const OPERATOR_ONLY: &[&str] = &[
    "bundle-release/authority-calibrate",
    "bundle-release/authority-measure",
    "bundle-release/catalog-remote-publish",
    "bundle-release/catalog-request-publication",
    "bundle-release/core-seed-build",
    "bundle-release/core-seed-capture",
    "bundle-release/core-seed-inspect",
    "bundle-release/core-seed-qualify",
    "bundle-release/generation-build",
    "bundle-release/generation-capture",
    "bundle-release/generation-finalize",
    "bundle-release/generation-qualify",
    "bundle-release/genesis-set-compose",
    "bundle-release/input-inspect",
    "bundle-release/request-authorization",
    "bundle-release/request-tree-signing",
    "bundle-release/set-compose",
    "bundle-release/status",
    "bundle-release/submit",
    "bundle-release/substrate-build",
    "bundle-release/substrate-qualify",
    "bundle-release/substrate-release-authorization",
    "bundle-release/substrate-release-finalize",
    "bundle/install",
    "bundle/manifest-sign",
    "bundle/profile-sign",
    "bundle/publish",
    "bundle/remove",
    "bundle/sign",
    "bundle/smoke",
    "bundle/verify",
    "credential-profiles/abandon-login",
    "credential-profiles/delete",
    "credential-profiles/get",
    "external-content/activate",
    "external-content/bind",
    "external-content/bindings",
    "external-content/bootstrap-runtime-source",
    "external-content/bootstrap-runtime-source-operation",
    "external-content/bootstrap-runtime-source-termination",
    "external-content/capture-product",
    "external-content/compose-product",
    "external-content/continue-runtime-snapshot-create",
    "external-content/create-runtime-snapshot-qualification",
    "external-content/derive-consumer-qualification-context",
    "external-content/export-qualification-testimony",
    "external-content/import",
    "external-content/launch-content-qualification",
    "external-content/launch-product-qualification",
    "external-content/observe-bootstrap-runtime-source",
    "external-content/observe-runtime-snapshot-qualification",
    "external-content/observe-runtime-snapshot-readiness",
    "external-content/produce-runtime-snapshot",
    "external-content/product",
    "external-content/push-product",
    "external-content/qualify-content",
    "external-content/qualify-product",
    "external-content/rebind",
    "external-content/receive-product",
    "external-content/release",
    "external-content/run-product-qualification-worker",
    "external-content/runtime-snapshot",
    "external-content/runtime-snapshot-qualification",
    "external-content/runtime-snapshot-qualification-termination",
    "external-content/scrub",
    "external-content/terminate-bootstrap-runtime-source",
    "external-content/terminate-runtime-snapshot-qualification",
    "external-content/unregister",
    "external-content/verify-runtime-snapshot-qualification",
    "external-execution/admit-candidate-program",
    "external-execution/admit-product-program",
    "external-execution/provider-inventory",
    "external-execution/settle-unknown-allocation",
    "external-execution/status",
    "fetch",
    "identity/authorize-client",
    "identity/mint-admission-token",
    "maintenance/gc",
    "node/doctor",
    "node/journal",
    "node/logs",
    "node/machine/declare",
    "node/machine/remove",
    "project/snapshot-status",
    "projection/rebuild",
    "projection/verify",
    "remote-worker-workflows/query",
    "remote-worker-workflows/resume",
    "remote-worker-workflows/start",
    "remote/bind-project",
    "remote/bundle-install",
    "remote/configure",
    "remote/doctor",
    "remote/pin",
    "remote/reconcile-project-head",
    "remote/threads",
    "system/routes",
    "threads/accounting/summary",
    "trust/show",
    "ui/sessions/list",
    "verify",
    "worker-executions/candidate-result",
    "worker-executions/command",
    "worker-executions/discard",
    "worker-executions/publish",
    "worker-executions/qualify-candidate",
    "worker-executions/start-candidate-integration",
    "worker-executions/status",
];

#[test]
fn every_service_a_hosted_session_reaches_declares_its_disclosure() {
    let routes = route_targets();
    let unauthenticated: BTreeSet<String> = routes
        .iter()
        .filter(|(_, open)| *open)
        .map(|(name, _)| name.clone())
        .collect();
    let mut no_public_tier = Vec::new();
    let mut missing = Vec::new();
    let mut declared_operator_only = BTreeSet::new();
    for (name, body) in services() {
        let Some(block) = body.get("disclosure") else {
            missing.push(name);
            continue;
        };
        ServiceDisclosure::parse(block)
            .unwrap_or_else(|error| panic!("{name}: invalid disclosure block: {error:#}"));
        let lists = |tier: &str| {
            block
                .get(tier)
                .and_then(Value::as_array)
                .is_some_and(|entries| !entries.is_empty())
        };
        if unauthenticated.contains(&name) && !lists("public") {
            no_public_tier.push(name.clone());
        }
        if !lists("principal") && !lists("public") {
            declared_operator_only.insert(name);
        }
    }
    assert!(
        no_public_tier.is_empty(),
        "services behind an unauthenticated route need a `public` tier: {no_public_tier:#?}"
    );
    assert!(
        missing.is_empty(),
        "services reachable by a hosted session with no `disclosure` block \
         (add tiers, or `disclosure: {{}}` to serve the operator alone): {missing:#?}"
    );
    let expected: BTreeSet<String> = OPERATOR_ONLY
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(
        declared_operator_only, expected,
        "the operator-only services changed: review each and update OPERATOR_ONLY"
    );
}

#[test]
fn every_declared_block_parses() {
    for (name, body) in services() {
        if let Some(block) = body.get("disclosure") {
            ServiceDisclosure::parse(block)
                .unwrap_or_else(|error| panic!("{name}: invalid disclosure block: {error:#}"));
        }
    }
}

/// Free-text leaves a principal still receives, because the seat that owns the
/// answer reads them today: service, pointer, and the milestone that removes
/// it. Each is an exception to the rule that free text never sits in a tier.
/// The list is pinned: it can only change by a reviewed edit here.
const INTERIM_FREE_TEXT: &[(&str, &str, &str)] = &[
    ("objects/closure/get", "/entries/*/value", "m4"),
    ("objects/closure/get", "/entries/*/data", "m4"),
    ("objects/get-any", "/entries/*/value", "m4"),
    ("objects/get-any", "/entries/*/data", "m4"),
    ("objects/get-any", "/blob_chunk/data", "m4"),
    ("objects/get-any", "/large_object_chunk/data", "m4"),
    ("objects/get", "/entries/*/value", "m4"),
    ("objects/get", "/entries/*/data", "m4"),
    ("objects/get", "/blob_chunk/data", "m4"),
    ("objects/get", "/large_object_chunk/data", "m4"),
    (
        "worker-executions/approvals",
        "/approvals/*/requested_authority/display",
        "per-chain publication",
    ),
    (
        "worker-placements/prepare",
        "/credential_reservation/profile_id",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/credential_reservation/owner_principal",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/credential_reservation/upstream_session_id",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/placement/credential_reservation/profile_id",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/placement/credential_reservation/owner_principal",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/placement/credential_reservation/upstream_session_id",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/placement/owner_principal",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/placement/target_operator_authority",
        "m4",
    ),
    (
        "worker-placements/prepare",
        "/placement/project_rebind",
        "m4",
    ),
    ("worker-placements/prepare", "/placement/accounting", "m4"),
    ("projects/list", "/projects/*/root", "m4"),
    ("projects/open", "/ui_transition/surface/title", "m4"),
    (
        "projects/open",
        "/ui_transition/surface/saved_surface",
        "m4",
    ),
    (
        "projects/open",
        "/ui_transition/attachment/project_path",
        "m4",
    ),
    (
        "projects/open",
        "/ui_transition/attachment/presentation",
        "m4",
    ),
    (
        "ui/invocations/dispatch",
        "/result",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/config/get", "/saved_view_sets", "m4"),
    ("ui/ryeos-ui/config/get", "/particular_view_sets", "m4"),
    ("ui/ryeos-ui/config/get", "/reusable_surfaces", "m4"),
    ("ui/ryeos-ui/config/get", "/particular_surfaces", "m4"),
    ("ui/ryeos-ui/config/get", "/view_set_save_context", "m4"),
    (
        "ui/ryeos-ui/config/get",
        "/particular_view_set_save_context",
        "m4",
    ),
    (
        "ui/ryeos-ui/config/get",
        "/reusable_surface_save_context",
        "m4",
    ),
    (
        "ui/ryeos-ui/config/get",
        "/particular_surface_save_context",
        "m4",
    ),
    (
        "ui/ryeos-ui/execution/occurrence",
        "/evidence/*/attributes",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/execution/occurrence",
        "/coverage/*/warnings",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/comparison",
        "/entities/*/attributes",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/comparison",
        "/relations/*/attributes",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/comparison",
        "/previews",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/comparison",
        "/metrics",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/comparison",
        "/warnings",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/execution",
        "/entities/*/attributes",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/execution",
        "/relations/*/attributes",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/execution",
        "/previews",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/execution",
        "/metrics",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/field/execution",
        "/warnings",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/files/read", "/path", "m4"),
    ("ui/ryeos-ui/files/read", "/content", "m4"),
    ("ui/ryeos-ui/files/tree", "/entries/*/path", "m4"),
    ("ui/ryeos-ui/files/tree", "/entries/*/name", "m4"),
    ("ui/ryeos-ui/item/inspect", "/item/title", "m4"),
    ("ui/ryeos-ui/item/inspect", "/item/trust/signer", "m4"),
    ("ui/ryeos-ui/item/inspect", "/item/trust/reason", "m4"),
    ("ui/ryeos-ui/item/inspect", "/item/shadowed", "m4"),
    ("ui/ryeos-ui/item/inspect", "/effective", "m4"),
    ("ui/ryeos-ui/items/list", "/items/*/title", "m4"),
    (
        "ui/ryeos-ui/node/activity",
        "/events/*/event_hash",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/node/activity",
        "/events/*/event_type",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/node/activity",
        "/events/*/thread_id",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/node/machines", "/rows/*/name", "m4"),
    ("ui/ryeos-ui/node/machines", "/rows/*/label", "m4"),
    ("ui/ryeos-ui/node/machines", "/rows/*/relations_text", "m4"),
    (
        "ui/ryeos-ui/node/machines",
        "/rows/*/disagreeing_text",
        "m4",
    ),
    ("ui/ryeos-ui/node/machines", "/rows/*/missing_text", "m4"),
    ("ui/ryeos-ui/node/machines", "/rows/*/facts_text", "m4"),
    ("ui/ryeos-ui/node/machines", "/rows/*/facts/*/text", "m4"),
    ("ui/ryeos-ui/projects/list", "/projects/*/name", "m4"),
    ("ui/ryeos-ui/projects/list", "/projects/*/root", "m4"),
    ("ui/ryeos-ui/surfaces/resume", "/particular_surface", "m4"),
    ("ui/ryeos-ui/surfaces/resume", "/particular_selection", "m4"),
    (
        "ui/ryeos-ui/surfaces/resume",
        "/ui_transition/surface",
        "m4",
    ),
    (
        "ui/ryeos-ui/surfaces/resume",
        "/ui_transition/attachment",
        "m4",
    ),
    (
        "ui/ryeos-ui/surfaces/resume",
        "/ui_transition/tab_attachments",
        "m4",
    ),
    (
        "ui/ryeos-ui/surfaces/resume",
        "/ui_transition/opening",
        "m4",
    ),
    ("ui/ryeos-ui/thread/inspect", "/thread/project_root", "m4"),
    (
        "ui/ryeos-ui/thread/inspect",
        "/result/outcome_code",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/result/error",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/artifacts/*/artifact_id",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/artifacts/*/artifact_type",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/artifacts/*/metadata",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/launch_facts/*/properties/isolation_enforced",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/launch_facts/*/properties/isolation_policy_digest",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/inspect",
        "/launch_facts/*/properties/isolation_backend_inspection_digest",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/thread/tree", "/threads/*/project/name", "m4"),
    ("ui/ryeos-ui/thread/tree", "/threads/*/project/path", "m4"),
    ("ui/ryeos-ui/thread/tree", "/threads/*/requested_by", "m4"),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/role",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/display_state",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/phase",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/follow_node",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/child_thread_id",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/child_chain_root_id",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/child_terminal_status",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/parent_successor_thread_id",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/cohort/done",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/thread/tree",
        "/threads/*/follow/cohort/expected",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/threads/list", "/threads/*/project/name", "m4"),
    ("ui/ryeos-ui/threads/list", "/threads/*/project/path", "m4"),
    ("ui/ryeos-ui/threads/list", "/threads/*/requested_by", "m4"),
    (
        "ui/ryeos-ui/timeline/raw",
        "/events/*/payload",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/timeline/raw",
        "/events/*/prev_chain_event_hash",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/timeline/raw",
        "/events/*/prev_thread_event_hash",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/timeline/thread",
        "/items",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/timeline/thread",
        "/order",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/timeline/thread",
        "/undeclared",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/view-sets/resume", "/particular_view_set", "m4"),
    (
        "ui/ryeos-ui/view-sets/resume",
        "/particular_selection",
        "m4",
    ),
    (
        "ui/ryeos-ui/view-sets/resume",
        "/ui_transition/surface",
        "m4",
    ),
    (
        "ui/ryeos-ui/view-sets/resume",
        "/ui_transition/attachment",
        "m4",
    ),
    (
        "ui/ryeos-ui/view-sets/resume",
        "/ui_transition/tab_attachments",
        "m4",
    ),
    (
        "ui/ryeos-ui/view-sets/resume",
        "/ui_transition/opening",
        "m4",
    ),
    (
        "ui/ryeos-ui/work/approval-history",
        "/history/*/requested_authority",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/approval-history",
        "/history/*/decision_principal",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/attention",
        "/attention/*/project_root",
        "m4",
    ),
    (
        "ui/ryeos-ui/work/attention",
        "/attention/*/detail/summary",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/attention",
        "/attention/*/detail/requested_authority",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/attention",
        "/attention/*/detail/publication_result",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/attention",
        "/attention/*/detail/project_root",
        "m4",
    ),
    ("ui/ryeos-ui/work/attention", "/counts/by_project", "m4"),
    ("ui/ryeos-ui/work/attention", "/running/by_project", "m4"),
    (
        "ui/ryeos-ui/work/attention",
        "/running/items/*/project_root",
        "m4",
    ),
    (
        "ui/ryeos-ui/work/candidate",
        "/candidates/*/publication_result",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/candidate",
        "/candidates/*/terminal_reason",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/candidate",
        "/changes/*/path",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/candidate",
        "/changes/*/change",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/candidate",
        "/changes/*/base_file_hash",
        "per-chain publication",
    ),
    (
        "ui/ryeos-ui/work/candidate",
        "/changes/*/candidate_file_hash",
        "per-chain publication",
    ),
    ("ui/ryeos-ui/work/list", "/work/*/owner_principal_id", "m4"),
    ("ui/ryeos-ui/work/list", "/work/*/project/name", "m4"),
    ("ui/ryeos-ui/work/list", "/work/*/project/path", "m4"),
    ("ui/seat/append", "/events/*/payload", "m4"),
    ("ui/seat/replay", "/receipt/events/*/payload", "m4"),
    ("ui/session/attachments/revise", "/attachment", "m4"),
    (
        "ui/session/attachments/revise",
        "/ui_transition/surface",
        "m4",
    ),
    (
        "ui/session/attachments/revise",
        "/ui_transition/attachment",
        "m4",
    ),
    (
        "ui/session/attachments/revise",
        "/ui_transition/opening",
        "m4",
    ),
    (
        "ui/session/attachments/revise",
        "/ui_transition/tab_attachments",
        "m4",
    ),
    (
        "ui/session/attachments/revise",
        "/ui_transition/prepared",
        "m4",
    ),
    ("ui/session/current", "/presentations", "m4"),
    ("ui/session/current", "/binding_attachments", "m4"),
    ("ui/session/surfaces/open", "/ui_transition/surface", "m4"),
    (
        "ui/session/surfaces/open",
        "/ui_transition/attachment",
        "m4",
    ),
    (
        "ui/session/surfaces/open",
        "/ui_transition/tab_attachments",
        "m4",
    ),
    ("ui/session/surfaces/open", "/ui_transition/opening", "m4"),
];

/// The `interim_free_text` entries of every service item, read from the item's
/// own text so each entry's `# removed by:` milestone is visible.
fn declared_interim() -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    let bundles = repo().join("bundles");
    for bundle in std::fs::read_dir(&bundles).unwrap().flatten() {
        let root = bundle.path().join(".ai/services");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for file in files {
            let name = file
                .strip_prefix(&root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&file).unwrap();
            let mut inside = false;
            for line in text.lines() {
                if line.starts_with("  interim_free_text:") {
                    inside = true;
                    continue;
                }
                let Some(entry) = line.strip_prefix("    - ").filter(|_| inside) else {
                    inside = false;
                    continue;
                };
                let (pointer, tag) = entry
                    .split_once('#')
                    .unwrap_or_else(|| panic!("{name}: interim entry `{entry}` has no milestone"));
                let milestone = tag
                    .trim()
                    .strip_prefix("removed by:")
                    .unwrap_or_else(|| panic!("{name}: interim entry `{entry}` has no milestone"))
                    .trim();
                found.push((
                    name.clone(),
                    pointer.trim().trim_matches('"').to_owned(),
                    milestone.to_owned(),
                ));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn interim_free_text_is_exactly_the_reviewed_list_and_each_names_its_milestone() {
    let mut pinned: Vec<(String, String, String)> = INTERIM_FREE_TEXT
        .iter()
        .map(|(service, pointer, milestone)| {
            (
                (*service).to_owned(),
                (*pointer).to_owned(),
                (*milestone).to_owned(),
            )
        })
        .collect();
    pinned.sort();
    let declared = declared_interim();
    assert_eq!(
        declared, pinned,
        "the interim free-text list changed: each entry is an exception to review"
    );
    for (service, pointer, milestone) in &declared {
        assert!(
            matches!(milestone.as_str(), "m4" | "per-chain publication"),
            "{service} {pointer}: unknown removal milestone `{milestone}`"
        );
    }
}

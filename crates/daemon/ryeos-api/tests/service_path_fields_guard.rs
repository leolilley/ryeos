//! Every request field of a service that names a host path, a directory, a
//! URL, a principal or a key must be classified in `CLASSIFIED`, and the
//! handler file the entry names must still contain the call that enforces its
//! class. A field that carries one of those meanings without an entry fails
//! here, so a new service or a new field cannot reach a handler unjudged.
//!
//! The classes are:
//! * `Admitted`: the value is admitted as a live project path before use.
//! * `OperatorOnly`: the service is refused to any key that is not the node's
//!   operator before the request is read.
//! * `OwnerFiltered`: the value is only honoured against what the caller's
//!   owner holds, or the caller acts under its own grant.
//! * `LocalOnly`: the service is `availability: local`, so it has no daemon
//!   handler at all (see `local_only_services.rs`).
//! * `StoppedNodeOnly`: the service is `availability: stopped_node`, so it is
//!   absent or refused while the daemon runs; the test
//!   `a_stopped_node_service_is_absent_or_refused_while_the_daemon_runs` in
//!   `local_only_services.rs` proves it, and no handler call is required.
//! * `NoHostResource`: the field names no host path, directory, URL or other
//!   principal's data; each such field carries a reason in `NO_HOST_RESOURCE`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[path = "../../../../tests/support/rust_source_text.rs"]
mod rust_source_text;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Admitted,
    OperatorOnly,
    OwnerFiltered,
    LocalOnly,
    StoppedNodeOnly,
    NoHostResource,
}

impl Class {
    /// The call texts a handler file may show as the enforcement of this
    /// class. `LocalOnly`, `StoppedNodeOnly` and `NoHostResource` need no
    /// handler call, so their only text is the empty one.
    fn enforcing_calls(self) -> &'static [&'static str] {
        match self {
            Self::Admitted => &["admit_handler_live_path(", "admit_live_project_path("],
            Self::OperatorOnly => &[
                "require_admitted_operator(",
                "require_local_configured_operator(",
                "resolve_admitted(",
                "operator_caller(",
            ],
            // Spelled in two parts so the owner-resolver guard, which reads
            // every source for this call, does not count a name in this list
            // as a call.
            Self::OwnerFiltered => &[
                concat!("speaks_", "for("),
                "owner_for(",
                "owner_principal(",
                "is_owner(",
                "require_owner(",
                "key_owns_project(",
                "caller_authorized_by(",
                "was_granted_by(",
                "named_remote_client(",
                "require_authenticated_source_node(",
            ],
            Self::LocalOnly | Self::StoppedNodeOnly | Self::NoHostResource => &[""],
        }
    }
}

/// The words that mark a field. A field is marked when its whole name is one
/// of them or its name ends with `_` and one of them (`project_path`,
/// `output_dir`, `outbound_principal`, `expected_signing_key`), never on a
/// substring (`keyword`, `directory`). The field named exactly `project` is
/// marked too: the remote handlers use it for a path.
const MARKER_WORDS: &[&str] = &[
    "path",
    "dir",
    "root",
    "checkout",
    "url",
    "uri",
    "endpoint",
    "principal",
    "fingerprint",
    "owner",
    "signer",
    "key",
];

fn is_marked(field: &str) -> bool {
    field == "project"
        || MARKER_WORDS
            .iter()
            .any(|word| field == *word || field.ends_with(&format!("_{word}")))
}

/// (service id, field, class, handler file relative to the workspace root,
/// the call text the handler's production source must contain).
const CLASSIFIED: &[(&str, &str, Class, &str, &str)] = &[
    (
        "bundle-release/authority-calibrate",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_local_configured_operator(",
    ),
    (
        "bundle-release/request-authorization",
        "expected_publisher_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/substrate-release-authorization",
        "expected_publisher_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "core_seed_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "native_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "portable_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "publisher_executable_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "publisher_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "substrate_build_signer_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/authority-measure",
        "substrate_qualification_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/core-seed-inspect",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/input-inspect",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/submit",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "admit_handler_live_path(",
    ),
    (
        "bundle-release/substrate-build",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle-release/substrate-qualify",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
        "require_admitted_operator(",
    ),
    (
        "bundle/install",
        "source_path",
        Class::StoppedNodeOnly,
        "",
        "",
    ),
    ("bundle/profile-sign", "key", Class::LocalOnly, "", ""),
    ("bundle/publish", "owner", Class::LocalOnly, "", ""),
    ("bundle/publish", "registry_root", Class::LocalOnly, "", ""),
    (
        "bundle/pull",
        "url",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_pull.rs",
        "require_admitted_operator(",
    ),
    ("bundle/sign", "registry_root", Class::LocalOnly, "", ""),
    (
        "bundle/verify",
        "registry_root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/bundle_verify.rs",
        "require_local_configured_operator(",
    ),
    (
        "commands/dispatch",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
        "admit_live_project_path(",
    ),
    (
        "endorsements/list",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/endorsements.rs",
        "key_owns_project(",
    ),
    (
        "external-content/activate",
        "offline_archive_root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_activate.rs",
        "require_admitted_operator(",
    ),
    (
        "external-content/bind",
        "product_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/bind",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/bindings",
        "product_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/bindings",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/import",
        "binding_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/operator_external_content.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/import",
        "path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/operator_external_content.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/import",
        "root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/operator_external_content.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/rebind",
        "product_owner_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    (
        "external-content/rebind",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/external_content_bind.rs",
        "require_local_configured_operator(",
    ),
    ("fetch", "project_path", Class::LocalOnly, "", ""),
    (
        "identity/authorize-client",
        "public_key",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/authorize_client.rs",
        "require_local_configured_operator(",
    ),
    (
        "identity/authorize-key",
        "public_key",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/authorize_key.rs",
        "caller_authorized_by(",
    ),
    (
        "identity/renew-key",
        "fingerprint",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/renew_key.rs",
        "was_granted_by(",
    ),
    (
        "identity/revoke-key",
        "fingerprint",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/revoke_key.rs",
        "was_granted_by(",
    ),
    (
        "items/effective",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/items_effective.rs",
        "admit_handler_live_path(",
    ),
    (
        "items/revised",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/items_revised.rs",
        "admit_handler_live_path(",
    ),
    ("node/doctor", "registry_root", Class::LocalOnly, "", ""),
    ("node/logs", "app_root", Class::LocalOnly, "", ""),
    (
        "objects/closure/describe",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/closure/get",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/get",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/has",
        "root",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "owner_principal(",
    ),
    (
        "objects/put",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/objects_put.rs",
        "owner_principal(",
    ),
    (
        "project/apply-published",
        "checkout",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/project_apply_published.rs",
        "owner_principal(",
    ),
    (
        "project/apply-snapshot",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/project_apply_snapshot.rs",
        "admit_handler_live_path(",
    ),
    (
        "project/snapshot-status",
        "project_path",
        Class::OperatorOnly,
        "crates/daemon/ryeos-app/src/runtime_project_snapshot_service.rs",
        "require_local_configured_operator(",
    ),
    (
        "project/status",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/project_status.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/authorize",
        "public_key",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/remote_authorize.rs",
        "named_remote_client(",
    ),
    (
        "remote/bind-project",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_bind_project.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/configure",
        "expect_fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_configure.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/configure",
        "url",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_configure.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/descriptor",
        "url",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_descriptor.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/doctor",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_doctor.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/execute",
        "project",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/remote_execute.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/project-status",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_project_status.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/import-admitted-root-advanced",
        "expected_signing_key",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_import_admitted_root.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/pull",
        "output_dir",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_pull.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/pull",
        "root",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_pull.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/pull-worker-result",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/remote_pull_worker_result.rs",
        concat!("speaks_", "for("),
    ),
    (
        "remote/push",
        "outbound_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_push.rs",
        "require_admitted_operator(",
    ),
    (
        "remote/push",
        "project",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/remote_push.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/reconcile-project-head",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_reconcile_project_head.rs",
        "require_local_configured_operator(",
    ),
    (
        "remote/revoke",
        "fingerprint",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/remote_revoke.rs",
        "named_remote_client(",
    ),
    (
        "remote/run",
        "outbound_principal",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_run.rs",
        "resolve_admitted(",
    ),
    (
        "remote/run",
        "project",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/remote_run.rs",
        "admit_handler_live_path(",
    ),
    (
        "remote/sync-project-ai",
        "project",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/remote_sync_project_ai.rs",
        "require_admitted_operator(",
    ),
    (
        "revisions/get",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/revisions.rs",
        "admit_handler_live_path(",
    ),
    (
        "revisions/list",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/revisions.rs",
        "admit_handler_live_path(",
    ),
    (
        "scheduler/register",
        "project_root",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/scheduler_register.rs",
        "admit_handler_live_path(",
    ),
    (
        "system/push-head",
        "project",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/push_head.rs",
        "owner_principal(",
    ),
    (
        "tool/env-check",
        "project_path",
        Class::Admitted,
        "crates/daemon/ryeos-api/src/handlers/tool_env_check.rs",
        "admit_handler_live_path(",
    ),
    (
        "trust/show",
        "fingerprint",
        Class::OperatorOnly,
        "crates/daemon/ryeos-api/src/handlers/trust_list.rs",
        "require_admitted_operator(",
    ),
    ("verify", "project_path", Class::LocalOnly, "", ""),
    (
        "worker-placements/preflight",
        "owner_principal",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/worker_placements.rs",
        "require_authenticated_source_node(",
    ),
    (
        "worker-placements/preflight",
        "target_project_path",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/worker_placements.rs",
        "require_authenticated_source_node(",
    ),
    (
        "worker-placements/prepare",
        "target_project_path",
        Class::OwnerFiltered,
        "crates/daemon/ryeos-api/src/handlers/worker_placements.rs",
        "require_authenticated_source_node(",
    ),
];

/// Fields that carry a marked name but name no host resource. Each carries the
/// reason, which must say what the value is and why it reaches nothing of the
/// host or of another principal.
const NO_HOST_RESOURCE: &[(&str, &str, &str)] = &[
    (
        "admission/claim",
        "public_key",
        "the claiming key's own public key, admitted only under a one-time token and the claim signature",
    ),
    (
        "records/get",
        "key",
        "a record key inside the caller's readable records; cross-principal reads by key are covered by the two-person isolation test",
    ),
    (
        "system/routes",
        "path",
        "a substring filter over the node's route table, never a host path",
    ),
    (
        "threads/set_facet",
        "key",
        "a facet name on a thread the caller owns",
    ),
    (
        "worker-executions/command",
        "idempotency_key",
        "an idempotency key is stored and looked up under the placement thread of a session the caller owns (unique per placement and key), so it names nothing outside the caller's own session",
    ),
];

/// Order pins: inside the named function of the file's production text, the
/// `call` must appear before the first `before` text, so the enforcement
/// precedes the first use of the field's data. (service, field, file,
/// function, call, before).
const ORDER_PINS: &[(&str, &str, &str, &str, &str, &str)] = &[
    (
        "objects/get",
        "root",
        "crates/daemon/ryeos-api/src/handlers/objects_get.rs",
        "handle",
        "resolve_scope(",
        "serve(",
    ),
    (
        "objects/has",
        "root",
        "crates/daemon/ryeos-api/src/handlers/objects_has.rs",
        "handle",
        "resolve_scope(",
        "acquire_cas_read(",
    ),
    (
        "objects/closure/describe",
        "root",
        "crates/daemon/ryeos-api/src/handlers/objects_closure_describe.rs",
        "handle",
        "require_roots_in_scope(",
        "handle_authorized(",
    ),
    (
        "objects/closure/describe",
        "root",
        "crates/daemon/ryeos-api/src/handlers/objects_closure_describe.rs",
        "require_roots_in_scope",
        "resolve_scope(",
        "Ok(())",
    ),
    (
        "objects/closure/get",
        "root",
        "crates/daemon/ryeos-api/src/handlers/objects_closure_get.rs",
        "handle",
        "require_roots_in_scope(",
        "handle_authorized(",
    ),
    (
        "objects/get",
        "root",
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "resolve_scope",
        "require_verified(",
        "closure_scope(",
    ),
    (
        "objects/get",
        "root",
        "crates/daemon/ryeos-api/src/objects_read_scope.rs",
        "resolve_project_head",
        "owner_principal(",
        "head_key(",
    ),
    (
        "worker-executions/command",
        "idempotency_key",
        "crates/daemon/ryeos-api/src/handlers/dedicated_sessions.rs",
        "command",
        "owned_session(",
        "idempotency_key",
    ),
    (
        "worker-executions/command",
        "idempotency_key",
        "crates/daemon/ryeos-api/src/handlers/dedicated_sessions.rs",
        "owned_session",
        "require_owner(",
        "Ok(session)",
    ),
];

/// The text of function `name` in `text`: from `fn <name>(` to its matching
/// closing brace, counting braces from the first `{` after the signature.
fn function_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("fn {name}("))?;
    let open = start + text[start..].find('{')?;
    let mut depth = 0_i64;
    for (offset, byte) in text.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// A source file's text outside its tests.
fn production_source(file: &str) -> String {
    let source = std::fs::read_to_string(repo().join(file))
        .unwrap_or_else(|error| panic!("{file}: {error}"));
    rust_source_text::production_text(file, &source)
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
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

struct Service {
    availability: String,
    fields: Vec<String>,
}

/// Every service item in every bundle, by its name under `.ai/services`.
fn services() -> BTreeMap<String, Service> {
    let mut found = BTreeMap::new();
    for bundle in std::fs::read_dir(repo().join("bundles")).unwrap().flatten() {
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
            let value: Value = serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            let availability = value
                .get("availability")
                .and_then(Value::as_str)
                .unwrap_or("daemon_only")
                .to_owned();
            let fields = value
                .get("schema")
                .and_then(Value::as_object)
                .map(|schema| schema.keys().cloned().collect())
                .unwrap_or_default();
            found.insert(
                name,
                Service {
                    availability,
                    fields,
                },
            );
        }
    }
    found
}

#[test]
fn every_path_principal_or_key_field_is_classified_and_enforced() {
    let services = services();
    assert!(
        services.len() > 250,
        "the sweep found {} services",
        services.len()
    );
    let mut problems = Vec::new();

    let mut seen = BTreeSet::new();
    for (service, field, ..) in CLASSIFIED {
        if !seen.insert((*service, *field)) {
            problems.push(format!("{service}.{field}: classified twice"));
        }
    }

    for (service, field, _) in NO_HOST_RESOURCE {
        if !seen.insert((*service, *field)) {
            problems.push(format!(
                "{service}.{field}: classified twice (CLASSIFIED and NO_HOST_RESOURCE)"
            ));
        }
    }
    // (a) every marked field has an entry.
    for (name, service) in &services {
        for field in service.fields.iter().filter(|field| is_marked(field)) {
            let classified = CLASSIFIED
                .iter()
                .any(|(id, f, ..)| *id == name.as_str() && *f == field.as_str())
                || NO_HOST_RESOURCE
                    .iter()
                    .any(|(id, f, _)| *id == name.as_str() && *f == field.as_str());
            if !classified {
                problems.push(format!(
                    "{name}.{field}: a marked field with no CLASSIFIED or NO_HOST_RESOURCE entry"
                ));
            }
        }
    }

    // NoHostResource rows: a real marked field and a stated reason.
    for (id, field, reason) in NO_HOST_RESOURCE {
        // The class needs no handler call, only the stated reason.
        debug_assert_eq!(Class::NoHostResource.enforcing_calls(), &[""]);
        match services.get(*id) {
            None => problems.push(format!("{id}.{field}: no such service")),
            Some(service) if !service.fields.iter().any(|f| f.as_str() == *field) => {
                problems.push(format!("{id}.{field}: the service has no such field"));
            }
            Some(_) => {}
        }
        if !is_marked(field) {
            problems.push(format!("{id}.{field}: the field is not a marked name"));
        }
        if reason.trim().is_empty() {
            problems.push(format!("{id}.{field}: a NoHostResource row needs a reason"));
        }
    }

    // Order pins: the enforcing call precedes the first use of the field's
    // data inside the named handler function.
    for (id, field, file, function, call, before) in ORDER_PINS {
        let known = CLASSIFIED.iter().any(|(i, f, ..)| i == id && f == field)
            || NO_HOST_RESOURCE
                .iter()
                .any(|(i, f, _)| i == id && f == field);
        if !known {
            problems.push(format!(
                "{id}.{field}: an order pin for an unclassified field"
            ));
        }
        let text = production_source(file);
        let Some(body) = function_body(&text, function) else {
            problems.push(format!("{id}.{field}: {file} has no function `{function}`"));
            continue;
        };
        match (body.find(call), body.find(before)) {
            (Some(enforced), Some(used)) if enforced < used => {}
            (Some(_), Some(_)) => problems.push(format!(
                "{id}.{field}: in `{function}` of {file}, `{before}` comes before `{call}`"
            )),
            (None, _) => problems.push(format!(
                "{id}.{field}: `{function}` of {file} has no `{call}`"
            )),
            (_, None) => problems.push(format!(
                "{id}.{field}: `{function}` of {file} has no `{before}` to order against"
            )),
        }
    }

    let mut sources: BTreeMap<&str, String> = BTreeMap::new();
    for (id, field, class, file, call) in CLASSIFIED {
        // (b) the entry still names a real, marked field.
        let Some(service) = services.get(*id) else {
            problems.push(format!("{id}.{field}: no such service"));
            continue;
        };
        if !service.fields.iter().any(|f| f.as_str() == *field) {
            problems.push(format!("{id}.{field}: the service has no such field"));
            continue;
        }
        if !is_marked(field) {
            problems.push(format!("{id}.{field}: the field is not a marked name"));
        }
        // The class must agree with the YAML's availability and with an
        // enforcing call of its own kind.
        let local = service.availability == "local";
        if local != (*class == Class::LocalOnly) {
            problems.push(format!(
                "{id}.{field}: class {class:?} against availability {}",
                service.availability
            ));
        }
        let stopped = service.availability == "stopped_node";
        if stopped != (*class == Class::StoppedNodeOnly) {
            problems.push(format!(
                "{id}.{field}: class {class:?} against availability {}",
                service.availability
            ));
        }
        if !class.enforcing_calls().contains(call) {
            problems.push(format!(
                "{id}.{field}: `{call}` is not an enforcing call of class {class:?}"
            ));
        }
        if *class == Class::LocalOnly {
            if !file.is_empty() || !call.is_empty() {
                problems.push(format!("{id}.{field}: a local-only entry names no handler"));
            }
            continue;
        }
        if *class == Class::StoppedNodeOnly {
            if !file.is_empty() || !call.is_empty() {
                problems.push(format!(
                    "{id}.{field}: a stopped-node entry names no handler"
                ));
            }
            continue;
        }
        // (c) the handler's production text carries the enforcing call.
        let text = sources.entry(*file).or_insert_with(|| {
            let source = std::fs::read_to_string(repo().join(file))
                .unwrap_or_else(|error| panic!("{file}: {error}"));
            rust_source_text::production_text(file, &source)
        });
        if !text.contains(call) {
            problems.push(format!(
                "{id}.{field}: {file} has no `{call}` outside its tests"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "request-field classification is out of step with the services:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_marker_list_catches_a_new_field_of_each_kind() {
    for marked in [
        "project_path",
        "path",
        "output_dir",
        "dir",
        "app_root",
        "root",
        "checkout",
        "url",
        "remote_url",
        "uri",
        "callback_uri",
        "endpoint",
        "peer_endpoint",
        "outbound_principal",
        "principal",
        "publisher_fingerprint",
        "fingerprint",
        "binding_owner",
        "owner",
        "build_signer",
        "signer",
        "expected_signing_key",
        "key",
        "project",
    ] {
        assert!(is_marked(marked), "{marked} must be marked");
    }
    for unmarked in [
        "keyword",
        "directory",
        "pathology",
        "rootless",
        "chain_root_id",
        "principal_class",
        "forwarding_key_id",
        "item_ref",
        "projects",
        "project_name",
        "reason",
        "ownership",
    ] {
        assert!(!is_marked(unmarked), "{unmarked} must not be marked");
    }
}

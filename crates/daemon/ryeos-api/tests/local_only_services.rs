//! A service declared `availability: local` runs as a tool in the caller's
//! own process and has no daemon handler. Nothing in the daemon may be able to
//! reach it: its endpoint is absent from the service registry and from the
//! descriptor table, the executor's availability lookup does not know it, and a
//! route cannot be compiled against it. A `stopped_node` service is either
//! absent in the same way or marked as refused while the daemon is live.
//!
//! Reaching a handler needs a running daemon, so this test goes as far as the
//! registry, the descriptor table, the executor's availability lookup and the
//! route compiler: the places that decide whether a service invocation can
//! resolve an endpoint at all.

use std::path::{Path, PathBuf};

use ryeos_api::handlers::ALL;
use ryeos_api::registry::{ServiceAvailability, build_service_registry};
use ryeos_api::routes::invokers::compile_service_invoker_from;
use ryeos_executor::executor::availability_for_endpoint;
use serde_json::Value;

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

/// (service id, endpoint) of every service item whose `availability` is
/// `wanted`.
fn services_with(wanted: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for bundle in std::fs::read_dir(repo().join("bundles")).unwrap().flatten() {
        let root = bundle.path().join(".ai/services");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for file in files {
            let value: Value = serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            if value.get("availability").and_then(Value::as_str) != Some(wanted) {
                continue;
            }
            let id = file
                .strip_prefix(&root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            let endpoint = value
                .get("endpoint")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{}: no endpoint", file.display()))
                .to_owned();
            found.push((id, endpoint));
        }
    }
    found.sort();
    found
}

fn compile_error(service_ref: &str) -> Option<String> {
    compile_service_invoker_from(service_ref, "service", "test-route", ALL)
        .err()
        .map(|error| error.to_string())
}

#[test]
fn a_local_service_has_no_daemon_handler_and_no_route() {
    let local = services_with("local");
    assert!(local.len() >= 8, "found {} local services", local.len());
    let registry = build_service_registry();
    for (id, endpoint) in &local {
        let service_ref = format!("service:{id}");
        assert!(
            !registry.has(endpoint),
            "{id}: the daemon registry serves a local service at {endpoint}"
        );
        assert!(
            !ALL.iter()
                .any(|d| d.endpoint == endpoint || d.service_ref == service_ref),
            "{id}: the descriptor table carries a local service"
        );
        let lookup = availability_for_endpoint(ALL, endpoint)
            .expect_err(&format!("{id}: the executor knows endpoint {endpoint}"));
        assert!(
            lookup.to_string().contains("unknown service endpoint"),
            "{id}: {lookup}"
        );
        let refused = compile_error(&service_ref)
            .unwrap_or_else(|| panic!("{id}: a route compiled against a local service"));
        assert!(
            refused.contains("no service descriptor found"),
            "{id}: {refused}"
        );
    }
}

#[test]
fn a_stopped_node_service_is_absent_or_refused_while_the_daemon_runs() {
    let stopped = services_with("stopped_node");
    assert!(!stopped.is_empty(), "no stopped_node services found");
    for (id, endpoint) in &stopped {
        let service_ref = format!("service:{id}");
        match ALL.iter().find(|d| d.endpoint == endpoint) {
            Some(descriptor) => {
                assert_eq!(
                    descriptor.availability,
                    ServiceAvailability::StoppedNodeOnly,
                    "{id}: a stopped_node service is registered as live"
                );
                assert_eq!(
                    availability_for_endpoint(ALL, endpoint).unwrap(),
                    ServiceAvailability::StoppedNodeOnly,
                    "{id}"
                );
                let refused = compile_error(&service_ref)
                    .unwrap_or_else(|| panic!("{id}: a route compiled against it"));
                assert!(
                    refused.contains("requires a stopped node"),
                    "{id}: {refused}"
                );
            }
            None => {
                assert!(
                    !build_service_registry().has(endpoint),
                    "{id}: registry serves it without a descriptor"
                );
                assert!(compile_error(&service_ref).is_some(), "{id}");
            }
        }
    }
}

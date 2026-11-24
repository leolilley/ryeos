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

fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|inner| find(inner, key))),
        Value::Array(items) => items.iter().find_map(|inner| find(inner, key)),
        _ => None,
    }
}

/// The service scopes a hosted browser session is minted with.
fn session_service_patterns() -> Vec<String> {
    let profile = yaml(&repo().join("bundles/.ai/node/init/profiles/core.yaml"));
    find(&profile, "ui_browser_sessions")
        .and_then(|section| section.get("session_scopes"))
        .and_then(Value::as_array)
        .expect("core profile names the browser session scopes")
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|scope| scope.strip_prefix("ryeos.execute.service."))
        .map(str::to_owned)
        .collect()
}

fn covers(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn is_reachable(name: &str, body: &Value, patterns: &[String]) -> bool {
    let open = body
        .get("required_caps")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    open || patterns.iter().any(|pattern| covers(pattern, name))
}

/// Services deliberately served to no caller but the operator. Adding to this
/// list is a decision to be reviewed, never a way to silence the test.
const OPERATOR_ONLY: &[&str] = &[
    "bundle/manifest-sign",
    "bundle/profile-sign",
    "bundle/publish",
    "bundle/sign",
    "node/doctor",
    "node/logs",
    "system/ingest-ignore",
    "ui/ryeos-ui/files/read",
    "ui/ryeos-ui/files/tree",
];

#[test]
fn every_service_a_hosted_session_reaches_declares_its_disclosure() {
    let patterns = session_service_patterns();
    let mut missing = Vec::new();
    let mut declared_operator_only = BTreeSet::new();
    for (name, body) in services() {
        if !is_reachable(&name, &body, &patterns) {
            continue;
        }
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
        if !lists("principal") && !lists("public") {
            declared_operator_only.insert(name);
        }
    }
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

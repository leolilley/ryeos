// A shipped Surface's capability ceiling names the services its views bind,
// exactly, and never a wildcard (rulings 1.6 and 1.71).
//
// The narrowing lands one Surface per merge. `STILL_WILDCARD` is the exact set
// of Surfaces that still declare a wildcard lane; the guard fails when a
// Surface outside it declares one and when a Surface inside it no longer does,
// so each merge removes its Surface from the list and the list ends empty.

use std::collections::BTreeSet;
use std::path::PathBuf;

const STILL_WILDCARD: &[&str] = &[
    "assistant-observe",
    "lens",
    "lens-observe",
    "project",
    "thread",
    "thread-crystal",
    "workbench",
];

const SERVICE_PREFIX: &str = "ryeos.execute.service.";

fn root() -> PathBuf {
    ryeos_engine::test_support::workspace_root()
}

/// (surface name, lane name, pattern) for every pattern every Surface declares.
fn lane_patterns() -> Vec<(String, &'static str, String)> {
    let dir = root().join("bundles/ryeos-ui/.ai/surfaces/ryeos/ui");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "yaml") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for lane in ["sources", "affordances"] {
            let Some(list) = doc
                .get("capabilities")
                .and_then(|c| c.get(lane))
                .and_then(serde_yaml::Value::as_sequence)
            else {
                continue;
            };
            for pattern in list {
                out.push((
                    name.clone(),
                    lane,
                    pattern.as_str().expect("pattern is a string").to_string(),
                ));
            }
        }
    }
    out
}

fn is_wildcard(pattern: &str) -> bool {
    pattern.contains('*')
}

fn service_items() -> std::collections::BTreeMap<String, serde_yaml::Value> {
    let mut out = std::collections::BTreeMap::new();
    for bundle in std::fs::read_dir(root().join("bundles")).unwrap() {
        let services = bundle.unwrap().path().join(".ai/services");
        collect(&services, &services, &mut out);
    }
    out
}

fn collect(
    base: &std::path::Path,
    dir: &std::path::Path,
    out: &mut std::collections::BTreeMap<String, serde_yaml::Value>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(base, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            let rel = path.strip_prefix(base).unwrap().with_extension("");
            let doc: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            out.insert(rel.to_string_lossy().to_string(), doc);
        }
    }
}

#[test]
fn only_the_listed_surfaces_still_declare_a_wildcard_lane() {
    let found: BTreeSet<String> = lane_patterns()
        .into_iter()
        .filter(|(_, _, pattern)| is_wildcard(pattern))
        .map(|(surface, _, _)| surface)
        .collect();
    let expected: BTreeSet<String> = STILL_WILDCARD.iter().map(|s| s.to_string()).collect();
    assert_eq!(found, expected);
}

#[test]
fn no_surface_lane_names_the_capability_facts_service() {
    for (surface, lane, pattern) in lane_patterns() {
        assert!(
            !pattern.contains("node/capabilities"),
            "{surface} {lane} names {pattern}"
        );
    }
}

#[test]
fn every_exact_ceiling_names_an_installed_service_item() {
    let services = service_items();
    for (surface, lane, pattern) in lane_patterns() {
        if is_wildcard(&pattern) {
            continue;
        }
        let name = pattern
            .strip_prefix(SERVICE_PREFIX)
            .unwrap_or_else(|| panic!("{surface} {lane}: `{pattern}` is not a service capability"));
        assert!(
            services.contains_key(name),
            "{surface} {lane} names `{name}`, which no bundle ships as a service item"
        );
    }
}

#[test]
fn every_service_a_surface_lists_as_an_act_has_required_caps() {
    let services = service_items();
    for (surface, lane, pattern) in lane_patterns() {
        if lane != "affordances" || is_wildcard(&pattern) {
            continue;
        }
        let name = pattern.strip_prefix(SERVICE_PREFIX).unwrap();
        let caps = services[name]
            .get("required_caps")
            .and_then(serde_yaml::Value::as_sequence)
            .map(Vec::len)
            .unwrap_or(0);
        assert!(
            caps > 0,
            "{surface} lists `{name}` as an act, but it has no required_caps"
        );
    }
}

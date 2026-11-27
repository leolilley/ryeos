// Two data guards for the day a node admits a non-operator key.
//
// 1. No profile that is not a known single-operator profile may grant a
//    browser session a capability that can point a tool or a live project at
//    a host path, by name or by wildcard. The shipped profiles all name the
//    single-operator set below; a new profile (a member class's) is judged
//    by the rule from its first day.
// 2. No shipped Surface or view binding passes a path-like parameter to a
//    service or tool except as the session's own project marker, and none
//    lets the renderer vary one. A session can reach an item only through a
//    signed binding, so this keeps the host-path arguments of `fetch`,
//    `verify` and `node/doctor` out of every session's reach.

use std::path::{Path, PathBuf};

/// Capabilities a non-operator class must never cover.
const FORBIDDEN_FOR_NON_OPERATOR_CLASSES: &[&str] = &[
    "ryeos.read.project.live",
    "ryeos.write.project.live",
    "ryeos.execute.service.projects/open",
    "ryeos.execute.tool.ryeos/core/fetch",
    "ryeos.execute.tool.ryeos/core/verify",
    "ryeos.execute.tool.ryeos/core/node/doctor",
    "ryeos.execute.tool.ryeos/knowledge/compose",
];

/// The profiles of nodes with a single operator: they never admit another
/// key, so their sessions are the operator's own. A profile not named here
/// is held to the rule above.
const SINGLE_OPERATOR_PROFILES: &[&str] = &[
    "bundle-source",
    "central-host",
    "contained-workflow",
    "core-knowledge",
    "core-scheduling",
    "core-workflow",
    "core",
    "development",
    "hosted-controller",
    "hosted-node",
    "hosted-workflow",
    "local-inference",
    "release-authority",
    "standard-central-auth",
    "standard",
];

/// Parameter names that carry a host path or a registry location.
const PATH_LIKE_KEYS: &[&str] = &["project_path", "source", "registry_roots", "registry_root"];

fn profile_dir() -> PathBuf {
    ryeos_engine::test_support::workspace_root().join("bundles/.ai/node/init/profiles")
}

fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            yaml_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

/// Every forbidden capability some scope in `scopes` covers.
fn covered_forbidden(scopes: &[String]) -> Vec<&'static str> {
    FORBIDDEN_FOR_NON_OPERATOR_CLASSES
        .iter()
        .copied()
        .filter(|required| {
            scopes
                .iter()
                .any(|scope| ryeos_state::capability::grant_matches(scope, required))
        })
        .collect()
}

fn session_scopes(profile: &serde_yaml::Value) -> Vec<String> {
    profile["policies"]["ui_browser_sessions"]["session_scopes"]
        .as_sequence()
        .map(|scopes| {
            scopes
                .iter()
                .filter_map(|scope| scope.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_matcher_sees_a_forbidden_capability_by_name_and_by_wildcard() {
    let scopes = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(covered_forbidden(&scopes(&["ryeos.execute.tool.core/snapshot-status"])).is_empty());
    assert!(covered_forbidden(&scopes(&["ryeos.execute.service.ui/ryeos-ui/*"])).is_empty());
    for covering in [
        "*",
        "ryeos.*",
        "ryeos.read.project.*",
        "ryeos.execute.tool.*",
        "ryeos.execute.tool.ryeos/core/*",
        "ryeos.execute.tool.ryeos/core/fetch",
        "ryeos.execute.service.projects/*",
        "ryeos.write.project.live",
    ] {
        assert!(
            !covered_forbidden(&scopes(&[covering])).is_empty(),
            "{covering} must be seen as covering a forbidden capability"
        );
    }
    assert_eq!(
        covered_forbidden(&scopes(&["ryeos.execute.tool.ryeos/core/fetch"])),
        vec!["ryeos.execute.tool.ryeos/core/fetch"]
    );
}

#[test]
fn no_profile_outside_the_single_operator_set_covers_a_host_path_capability() {
    let mut files = Vec::new();
    yaml_files(&profile_dir(), &mut files);
    assert!(
        files.len() >= SINGLE_OPERATOR_PROFILES.len(),
        "the sweep found too few profiles: {files:?}"
    );
    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        if SINGLE_OPERATOR_PROFILES.contains(&name.as_str()) {
            continue;
        }
        let profile: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let covered = covered_forbidden(&session_scopes(&profile));
        assert!(
            covered.is_empty(),
            "profile {name} is not a single-operator profile and its session scopes cover \
             {covered:?}; a non-operator class must not hold them (member plan V1 and V3)"
        );
    }
}

#[test]
fn every_named_single_operator_profile_exists() {
    for name in SINGLE_OPERATOR_PROFILES {
        assert!(
            profile_dir().join(format!("{name}.yaml")).is_file(),
            "{name} is named single-operator but no such profile ships; drop the name"
        );
    }
}

// ── binding guard ────────────────────────────────────────────────────────

/// Walk `value`; inside any `params` or `args` mapping, a path-like key must
/// carry the session's own project marker. `problems` collects what is not.
fn scan_path_params(
    value: &serde_yaml::Value,
    in_params: bool,
    file: &Path,
    problems: &mut Vec<String>,
) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, child) in map {
                let key = key.as_str().unwrap_or_default();
                if in_params && PATH_LIKE_KEYS.contains(&key) {
                    let marker = child
                        .as_str()
                        .is_some_and(|text| text.starts_with("@session:project_root"));
                    if !marker {
                        problems.push(format!(
                            "{}: `{key}` is passed as {child:?}, not the session's project marker",
                            file.display()
                        ));
                    }
                }
                if key == "dynamic_parameters"
                    && let Some(list) = child.as_sequence()
                {
                    for name in list.iter().filter_map(|name| name.as_str()) {
                        if PATH_LIKE_KEYS.contains(&name) {
                            problems.push(format!(
                                "{}: `{name}` is a dynamic parameter the renderer may vary",
                                file.display()
                            ));
                        }
                    }
                }
                scan_path_params(
                    child,
                    in_params || key == "params" || key == "args",
                    file,
                    problems,
                );
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                scan_path_params(item, in_params, file, problems);
            }
        }
        _ => {}
    }
}

#[test]
fn a_scanned_binding_with_a_free_path_parameter_is_caught() {
    let bad: serde_yaml::Value = serde_yaml::from_str(
        "sources:\n  default:\n    ref: tool:ryeos/core/fetch\n    params:\n      project_path: /etc\n      item_ref: x\n    dynamic_parameters: [project_path]\n",
    )
    .unwrap();
    let mut problems = Vec::new();
    scan_path_params(&bad, false, Path::new("bad.yaml"), &mut problems);
    assert_eq!(problems.len(), 2, "{problems:?}");

    let good: serde_yaml::Value = serde_yaml::from_str(
        "sources:\n  default:\n    ref: service:ui/ryeos-ui/files/read\n    params:\n      root: project\n      project_path: \"@session:project_root\"\n    dynamic_parameters: [root, path]\n",
    )
    .unwrap();
    let mut problems = Vec::new();
    scan_path_params(&good, false, Path::new("good.yaml"), &mut problems);
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn no_shipped_binding_passes_a_free_path_parameter() {
    let bundles = ryeos_engine::test_support::workspace_root().join("bundles");
    let mut files = Vec::new();
    for bundle in std::fs::read_dir(&bundles).unwrap() {
        let ai = bundle.unwrap().path().join(".ai");
        yaml_files(&ai.join("surfaces"), &mut files);
        yaml_files(&ai.join("views"), &mut files);
    }
    assert!(
        files.len() > 10,
        "the sweep found too few bindings: {} files",
        files.len()
    );
    let mut seen_marker = false;
    let mut problems = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        seen_marker |= text.contains("@session:project_root");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        scan_path_params(&parsed, false, file, &mut problems);
    }
    assert!(
        seen_marker,
        "the sweep did not see the shipped project marker"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

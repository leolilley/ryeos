// Every place that turns a request-named path into a live project calls the
// one admission function. A new non-test source file that chooses a live
// filesystem project source must either call it or be named here with the
// reason it needs no admission, so the next site cannot be added open.

use std::path::{Path, PathBuf};

/// What marks a file as choosing a live filesystem project.
const LIVE_MARKERS: &[&str] = &[
    "ProjectSource::LiveFs",
    "ProjectSource::CaptureLiveFullProject",
    "SubjectResolutionAuthority::LiveFs",
];

/// What counts as calling the admission function.
const ADMISSION_CALLS: &[&str] = &[
    "admit_live_project_path(",
    "admit_handler_live_path(",
    "admit_route_live_path(",
];

/// Files that take a path from a request or a stored record and must admit it.
const MUST_ADMIT: &[&str] = &[
    "crates/bin/daemon/src/scheduler_impl.rs",
    "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
    "crates/daemon/ryeos-api/src/handlers/bundle_smoke.rs",
    "crates/daemon/ryeos-api/src/handlers/scheduler_register.rs",
    "crates/daemon/ryeos-api/src/handlers/threads_input.rs",
    "crates/daemon/ryeos-api/src/handlers/tool_env_check.rs",
    "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
];

/// Files that name a live source without a key-supplied path, with why.
const NEEDS_NO_ADMISSION: &[(&str, &str)] = &[
    (
        "crates/daemon/ryeos-api/src/routes/invokers/dispatch_invocation.rs",
        "route-authored: the project path is in the operator-signed route's own source_config, and only its `${path.*}` captures can vary it; no key names it",
    ),
    (
        "crates/daemon/ryeos-api/src/routes/invokers/launch_invocation.rs",
        "route-authored, as dispatch_invocation.rs: the path is the signed route's source_config",
    ),
    (
        "crates/daemon/ryeos-api/src/handlers/bundle_release_execution.rs",
        "runs in a workspace the daemon materialised from a release source generation",
    ),
    (
        "crates/daemon/ryeos-api/src/handlers/product_qualification_launch.rs",
        "runs over a no-project workspace the daemon created",
    ),
    (
        "crates/daemon/ryeos-app/src/operator_external_content/product_composition.rs",
        "validation of a sealed request; it chooses no path",
    ),
    (
        "crates/daemon/ryeos-app/src/thread_lifecycle/sealed_request.rs",
        "continuation and recovery inherit the sealed capsule's authority; no new path",
    ),
    (
        "crates/daemon/ryeos-ui/src/handlers/ui_invocations_dispatch.rs",
        "a session's project is the directory pinned at launch; the key names no path",
    ),
    (
        "crates/engine/ryeos-engine/src/contracts/execution_plan.rs",
        "engine type definitions and matching",
    ),
    (
        "crates/engine/ryeos-engine/src/engine.rs",
        "engine resolution given an already-admitted authority",
    ),
    (
        "crates/engine/ryeos-executor/src/execution/launch_preparation.rs",
        "executor preparation given an already-admitted authority",
    ),
    (
        "crates/engine/ryeos-executor/src/execution/project_source.rs",
        "executor project-source resolution given an already-admitted path",
    ),
    (
        "crates/tools/core-tools/src/actions/doctor.rs",
        "tool subprocess; its path arguments are the V3 item, and the tool is kept out of every non-operator class until a structural fix lands",
    ),
    (
        "crates/tools/core-tools/src/actions/inspect/fetch.rs",
        "tool subprocess; see doctor.rs",
    ),
    (
        "crates/tools/core-tools/src/actions/inspect/verify.rs",
        "tool subprocess; see doctor.rs",
    ),
];

fn workspace_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if matches!(name.as_str(), "tests" | "target" | "benches" | "examples") {
                continue;
            }
            workspace_files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !name.ends_with("tests.rs")
            && !name.contains("test_support")
        {
            out.push(path.strip_prefix(root).unwrap().to_owned());
        }
    }
}

/// The part of a source file before its first `#[cfg(test)]`.
fn non_test_text(text: &str) -> &str {
    text.find("#[cfg(test)]").map_or(text, |end| &text[..end])
}

#[test]
fn every_live_project_site_admits_its_path_or_is_named_with_a_reason() {
    // A new site or a moved exemption changes these counts and so fails here:
    // add the file to the right list, with a reason if it is exempt.
    assert_eq!(MUST_ADMIT.len(), 7, "the must-admit list changed");
    assert_eq!(NEEDS_NO_ADMISSION.len(), 14, "the exemption list changed");
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    workspace_files(&root, &root.join("crates"), &mut files);
    assert!(files.len() > 500, "the sweep found {} files", files.len());

    let mut unexpected = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for file in &files {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let body = non_test_text(&text);
        if !LIVE_MARKERS.iter().any(|marker| body.contains(marker)) {
            continue;
        }
        let name = file.to_string_lossy().replace('\\', "/");
        seen.insert(name.clone());
        let admits = ADMISSION_CALLS.iter().any(|call| body.contains(call));
        let must_admit = MUST_ADMIT.contains(&name.as_str());
        let exempt = NEEDS_NO_ADMISSION.iter().any(|(path, _)| *path == name);
        match (must_admit, exempt, admits) {
            (true, false, true) | (false, true, _) => {}
            (true, _, false) => unexpected.push(format!("{name}: must admit its path but does not call the admission function")),
            (true, true, _) => unexpected.push(format!("{name}: listed both as must-admit and as exempt")),
            (false, false, _) => unexpected.push(format!(
                "{name}: chooses a live filesystem project but is neither calling the admission function nor named with a reason in NEEDS_NO_ADMISSION"
            )),
        }
    }
    for name in MUST_ADMIT
        .iter()
        .copied()
        .chain(NEEDS_NO_ADMISSION.iter().map(|(path, _)| *path))
    {
        if !seen.contains(name) {
            unexpected.push(format!(
                "{name}: listed here but no longer chooses a live project; drop it"
            ));
        }
    }
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
}

fn route_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            route_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

/// The route invokers need no key admission because the live project path is
/// data the operator signed in the route. That holds only while the path is a
/// plain literal in every shipped route; the route compiler refuses an
/// interpolated one (`require_literal_project_path`), and this checks the
/// shipped files too.
#[test]
fn every_shipped_route_names_its_project_path_as_a_plain_literal() {
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    for bundle in std::fs::read_dir(root.join("bundles")).unwrap() {
        route_files(&bundle.unwrap().path().join(".ai/node/routes"), &mut files);
    }
    assert!(files.len() > 20, "the sweep found {} routes", files.len());
    let mut interpolated = Vec::new();
    for file in &files {
        let route: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let Some(path) = route["response"]["source_config"]["project_path"].as_str() else {
            continue;
        };
        if path.contains("${") {
            interpolated.push(format!("{}: {path}", file.display()));
        }
    }
    assert!(
        interpolated.is_empty(),
        "a route interpolates its project path:\n{}",
        interpolated.join("\n")
    );
}

/// Route items load only from the node's own space and from installed
/// bundles, never from a project: a member's project cannot ship a route that
/// names a path.
#[test]
fn the_route_loader_scans_node_space_and_installed_bundles_only() {
    let root = ryeos_engine::test_support::workspace_root();
    let text =
        std::fs::read_to_string(root.join("crates/daemon/ryeos-app/src/node_config/loader.rs"))
            .unwrap();
    let production = text
        .find("#[cfg(test)]\nmod tests")
        .map_or(text.as_str(), |end| &text[..end]);
    assert!(production.contains("fn system_scan_root"));
    assert!(production.contains("fn bundle_scan_root"));
    for forbidden in [
        "project_root",
        "project_path",
        "ProjectSource",
        "live_project",
    ] {
        assert!(
            !production.contains(forbidden),
            "the node-config loader names `{forbidden}`: route files must not load from a project"
        );
    }
}

/// Every response mode that carries a project path calls the literal-path
/// check, unless it is named here with the reason it needs none. A new mode
/// cannot reopen the hole by forgetting the call.
const MODES_NEEDING_NO_LITERAL_CHECK: &[(&str, &str)] = &[
    (
        "execute_mode.rs",
        "the project path is a request-body field of a signed /execute call and goes through admit_live_project_path",
    ),
    (
        "handler_mode.rs",
        "a fixed-principal handler route runs in a request-owned workspace the daemon creates; its project_path is not a configured path",
    ),
];

#[test]
fn every_response_mode_that_carries_a_project_path_checks_it_is_literal() {
    let root = ryeos_engine::test_support::workspace_root();
    let dir = root.join("crates/daemon/ryeos-api/src/routes/response_modes");
    let mut seen_exempt = Vec::new();
    let mut checked = 0;
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") || name.ends_with("tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let body = non_test_text(&text);
        if !body.contains("project_path") {
            continue;
        }
        let exempt = MODES_NEEDING_NO_LITERAL_CHECK
            .iter()
            .any(|(file, _)| *file == name);
        if exempt {
            seen_exempt.push(name);
        } else if body.contains("require_literal_project_path(") {
            checked += 1;
        } else {
            problems.push(format!(
                "{name}: carries a project_path but does not call require_literal_project_path and is not named in MODES_NEEDING_NO_LITERAL_CHECK"
            ));
        }
    }
    assert!(checked >= 3, "only {checked} modes call the literal check");
    for (file, _) in MODES_NEEDING_NO_LITERAL_CHECK {
        if !seen_exempt.iter().any(|seen| seen == file) {
            problems.push(format!(
                "{file}: named as exempt but it no longer mentions project_path; drop it"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

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
    "crates/daemon/ryeos-api/src/routes/invokers/dispatch_invocation.rs",
    "crates/daemon/ryeos-api/src/routes/invokers/launch_invocation.rs",
    "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
];

/// Files that name a live source without a key-supplied path, with why.
const NEEDS_NO_ADMISSION: &[(&str, &str)] = &[
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

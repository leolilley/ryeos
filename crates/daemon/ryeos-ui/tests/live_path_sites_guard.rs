// Every place that turns a request-named path into a live project calls the
// one admission function. A new non-test source file that chooses a live
// filesystem project source must either call it or be named here with the
// reason it needs no admission, so the next site cannot be added open.

use std::path::{Path, PathBuf};

#[path = "../../../../tests/support/rust_source_text.rs"]
mod rust_source_text;

/// What marks a file as choosing a live filesystem project.
const LIVE_MARKERS: &[&str] = &[
    "ProjectSource::LiveFs",
    "ProjectSource::CaptureLiveFullProject",
    "SubjectResolutionAuthority::LiveFs",
    "for_live_project_root(",
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
    "crates/daemon/ryeos-api/src/handlers/items_effective.rs",
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
        "crates/engine/ryeos-executor/src/execution/launch.rs",
        "builds a resolution-cache key's root identity: records the root's live path only when the already-chosen subject authority is live",
    ),
    (
        "crates/engine/ryeos-executor/src/execution/effective_program_projection.rs",
        "checks that a plan context's already-chosen authority is live or projectless before projecting the effective program",
    ),
    (
        "crates/engine/ryeos-executor/src/dispatch.rs",
        "one arm of a match on the admission's subject authority: a live authority is not a pinned or projectless resolution",
    ),
    (
        "crates/daemon/ryeos-app/src/execution_provenance.rs",
        "maps an already-built root or child live provenance to the live authority and defines root_live_fs, whose callers are pinned below",
    ),
    (
        "crates/daemon/ryeos-app/src/resolution_cache.rs",
        "validates that a resolution binding and its cache key agree with the admitted subject authority and live project root; it chooses no path",
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
        "crates/daemon/ryeos-app/src/thread_lifecycle.rs",
        "its live markers are a validation match arm and the live non-execution root context, whose production callers are pinned below: admit_non_execution_root takes the node's app root, and the pinned-directory variant takes an admitted binding attachment's retained directory",
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
    (
        "crates/daemon/ryeos-api/src/surface_views.rs",
        "embeds a surface's views under a root its caller already holds; its only non-test callers are items_effective.rs (admitted) and ui_launch_mint.rs (operator-only), pinned below",
    ),
    (
        "crates/daemon/ryeos-ui/src/handlers/ui_field_project.rs",
        "the request has no project parameter; the root is the retained project of the admitted UI session, or none",
    ),
    (
        "crates/daemon/ryeos-ui/src/handlers/ui_items.rs",
        "a seat bound to a project reads that project; only the admitted operator's own seat may name one, and any other seat naming a path is refused (pinned below)",
    ),
    (
        "crates/daemon/ryeos-ui/src/handlers/ui_launch_mint.rs",
        "its project root comes from authorize_launch_project, which admits only the node's operator (project_seat_operator_only for any other key); revalidation reuses the stored binding's root (pinned below)",
    ),
    (
        "crates/daemon/ryeos-ui/src/themes.rs",
        "loads installed themes with no project: the root is None",
    ),
    (
        "crates/bin/cli/src/effective_metadata.rs",
        "local process on the operator's own machine; the path is the operator's command line, not a key's request",
    ),
    (
        "crates/bin/cli/src/lifecycle_host_access.rs",
        "local process on the operator's own machine; the path is the operator's command line, not a key's request",
    ),
    (
        "crates/bin/cli/src/offline_dispatch.rs",
        "local process on the operator's own machine; the path is the operator's command line, not a key's request",
    ),
];

/// The exact marker lines of the five matcher files. A second, different
/// live-path line in one of them is a new site and trips the guard, which a
/// file-level exemption alone would hide.
const PINNED_MARKER_LINES: &[(&str, &[&str])] = &[
    (
        "crates/engine/ryeos-executor/src/execution/launch.rs",
        &["ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs"],
    ),
    (
        "crates/engine/ryeos-executor/src/execution/effective_program_projection.rs",
        &["SubjectResolutionAuthority::LiveFs | SubjectResolutionAuthority::Projectless"],
    ),
    (
        "crates/engine/ryeos-executor/src/dispatch.rs",
        &["SubjectResolutionAuthority::LiveFs => false,"],
    ),
    (
        "crates/daemon/ryeos-app/src/execution_provenance.rs",
        &["ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs"],
    ),
    (
        "crates/daemon/ryeos-app/src/thread_lifecycle.rs",
        &[
            "ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,",
            "&ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,",
            "subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,",
        ],
    ),
    (
        "crates/daemon/ryeos-app/src/resolution_cache.rs",
        &[
            "SubjectResolutionAuthority::LiveFs => {",
            "matches!(self.subject_authority, SubjectResolutionAuthority::LiveFs)",
            "SubjectResolutionAuthority::LiveFs",
            "live_project_root: matches!(subject_authority, SubjectResolutionAuthority::LiveFs)",
            "SubjectResolutionAuthority::LiveFs => {",
            "SubjectResolutionAuthority::Projectless | SubjectResolutionAuthority::LiveFs",
            "live_project_root: matches!(self.subject_authority, SubjectResolutionAuthority::LiveFs)",
            "if matches!(self.subject_authority, SubjectResolutionAuthority::LiveFs) {",
            "SubjectResolutionAuthority::LiveFs => {",
        ],
    ),
];

/// Production files that call `root_live_fs(`, which is where a live path
/// becomes a provenance. Each is a must-admit site, a route-authored site
/// (literal path checked when the route loads), the session-pinned UI
/// dispatch, or the definition itself. A new caller trips the guard.
const ROOT_LIVE_FS_CALLERS: &[&str] = &[
    "crates/bin/daemon/src/scheduler_impl.rs",
    "crates/daemon/ryeos-api/src/handlers/bundle_release.rs",
    "crates/daemon/ryeos-api/src/handlers/bundle_smoke.rs",
    "crates/daemon/ryeos-api/src/handlers/threads_input.rs",
    "crates/daemon/ryeos-api/src/routes/response_modes/execute_mode.rs",
    "crates/daemon/ryeos-api/src/routes/invokers/dispatch_invocation.rs",
    "crates/daemon/ryeos-api/src/routes/invokers/launch_invocation.rs",
    "crates/daemon/ryeos-ui/src/handlers/ui_invocations_dispatch.rs",
    "crates/daemon/ryeos-app/src/execution_provenance.rs",
    // Resume: the path is the stored record's recorded project root, refused
    // unless it still canonicalizes to itself and is a directory.
    "crates/engine/ryeos-executor/src/execution/runner.rs",
];

/// Production files that name a root or child live provenance variant: the
/// definition and derivation from a parent (execution_provenance.rs) and the
/// readers that match on it. Only the definition file builds one; a child
/// inherits its parent's admitted root. A new file naming either variant is a
/// new way to make a live provenance without `root_live_fs`, so it trips the
/// guard.
const LIVE_VARIANT_FILES: &[&str] = &[
    "crates/daemon/ryeos-app/src/execution_provenance.rs",
    "crates/daemon/ryeos-app/src/launch_metadata.rs",
    // A reader: one match arm maps a built live provenance to the admitted
    // project materialization; it chooses no path and builds no provenance.
    "crates/daemon/ryeos-app/src/thread_lifecycle.rs",
    "crates/daemon/ryeos-app/src/thread_lifecycle/sealed_request.rs",
    "crates/engine/ryeos-executor/src/execution/runner.rs",
];

#[test]
fn live_provenance_has_only_the_listed_constructors_and_matchers() {
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    workspace_files(&root, &root.join("crates"), &mut files);
    let mut roots = std::collections::BTreeSet::new();
    let mut children = std::collections::BTreeSet::new();
    let mut root_variant = std::collections::BTreeSet::new();
    for file in &files {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let body = non_test_text(&file.to_string_lossy(), &text);
        let name = file.to_string_lossy().replace('\\', "/");
        if body.contains("root_live_fs(") {
            roots.insert(name.clone());
        }
        if body.contains("RootLiveProject {") {
            root_variant.insert(name.clone());
        }
        if body.contains("ChildLiveProject {") {
            children.insert(name);
        }
    }
    let expect = |list: &[&str]| -> std::collections::BTreeSet<String> {
        list.iter().map(|path| (*path).to_owned()).collect()
    };
    assert_eq!(
        roots,
        expect(ROOT_LIVE_FS_CALLERS),
        "the production callers of root_live_fs changed"
    );
    assert_eq!(
        children,
        expect(LIVE_VARIANT_FILES),
        "the production files naming ChildLiveProject changed"
    );
    assert_eq!(
        root_variant,
        expect(LIVE_VARIANT_FILES),
        "the production files naming RootLiveProject changed"
    );
}

#[test]
fn the_matcher_files_have_exactly_their_pinned_live_path_lines() {
    let root = ryeos_engine::test_support::workspace_root();
    for (path, pinned) in PINNED_MARKER_LINES {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        let production = non_test_text(path, &text);
        let found: Vec<&str> = production
            .lines()
            .map(str::trim)
            .filter(|line| LIVE_MARKERS.iter().any(|marker| line.contains(marker)))
            .collect();
        assert_eq!(&found, pinned, "{path}: its live-path lines changed");
    }
}

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

/// The production text of a source file: the file without its `#[cfg(test)]`
/// items (the shared helper), so code after a test module is read too. The
/// sweep asserts that many files lose text, so a cut that stops matching fails
/// the guard instead of passing silently.
fn non_test_text(label: &str, text: &str) -> String {
    rust_source_text::production_text(label, text)
}

#[test]
fn every_live_project_site_admits_its_path_or_is_named_with_a_reason() {
    // A new site or a moved exemption changes these counts and so fails here:
    // add the file to the right list, with a reason if it is exempt.
    assert_eq!(MUST_ADMIT.len(), 8, "the must-admit list changed");
    assert_eq!(NEEDS_NO_ADMISSION.len(), 28, "the exemption list changed");
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    workspace_files(&root, &root.join("crates"), &mut files);
    assert!(files.len() > 500, "the sweep found {} files", files.len());

    let mut unexpected = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut with_test_module = 0_usize;
    for file in &files {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        with_test_module += usize::from(
            non_test_text(&file.to_string_lossy(), &text)
                .lines()
                .ne(text.lines()),
        );
        let body = non_test_text(&file.to_string_lossy(), &text);
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
    assert!(
        with_test_module > 800,
        "only {with_test_module} files have a top-level test module: the cut marker no longer matches"
    );
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
    let production = non_test_text("node_config/loader.rs", &text);
    assert!(
        production.lines().ne(text.lines()),
        "loader.rs has a test module and the cut still removes it"
    );
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
        let body = non_test_text(&path.display().to_string(), &text);
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

/// The exact lines that name a provenance constructor or variant in the two
/// files the shared production-text cut newly reads. A second, different line
/// there is a new site and trips the guard.
const PINNED_PROVENANCE_LINES: &[(&str, &[&str], &[&str])] = &[
    (
        "crates/engine/ryeos-executor/src/execution/runner.rs",
        &["root_live_fs("],
        &["let provenance = ExecutionProvenance::root_live_fs("],
    ),
    (
        "crates/daemon/ryeos-app/src/thread_lifecycle.rs",
        &["RootLiveProject {", "ChildLiveProject {"],
        &[
            "crate::execution_provenance::ExecutionProvenance::RootLiveProject { .. }",
            "| crate::execution_provenance::ExecutionProvenance::ChildLiveProject { .. } => {",
        ],
    ),
];

#[test]
fn the_resume_and_materialization_provenance_sites_have_exactly_their_pinned_lines() {
    let root = ryeos_engine::test_support::workspace_root();
    for (path, needles, pinned) in PINNED_PROVENANCE_LINES {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        let production = non_test_text(path, &text);
        let found: Vec<&str> = production
            .lines()
            .map(str::trim)
            .filter(|line| needles.iter().any(|needle| line.contains(needle)))
            .collect();
        assert_eq!(&found, pinned, "{path}: its live provenance lines changed");
    }
}

/// `admit_non_execution_root` takes any path and resolves a live project from
/// it, so its callers are pinned: the only production caller is the UI seat
/// admission, which hands it the node's own app root, never a path a caller
/// named; the pinned-directory variant is handed the retained directory of an
/// admitted binding attachment. A new caller, or a different argument, fails.
/// (The structural fix, an admission that takes no path, is carried on IH-77.)
#[test]
fn the_non_execution_root_admissions_are_called_only_with_the_node_root_or_an_admitted_directory() {
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    workspace_files(&root, &root.join("crates"), &mut files);
    let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut by_path = Vec::new();
    let mut by_directory = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let production = squash(&non_test_text(&file.to_string_lossy(), &text));
        let name = file.to_string_lossy().replace('\\', "/");
        if production.contains("::admit_non_execution_root(") {
            by_path.push((name.clone(), production.clone()));
        }
        if production.contains("::admit_non_execution_root_under_project_authority(") {
            by_directory.push((name, production));
        }
    }
    let seat = "crates/daemon/ryeos-ui/src/handlers/ui_seat.rs";
    assert_eq!(
        by_path
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [seat],
        "the production callers of admit_non_execution_root changed"
    );
    assert!(
        by_path[0].1.contains(
            "::admit_non_execution_root( &state.engine, node_history_policy, &state.config.app_root, request, )"
        ),
        "admit_non_execution_root is no longer handed exactly the node's app root"
    );
    assert_eq!(
        by_directory
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [seat],
        "the production callers of admit_non_execution_root_under_project_authority changed"
    );
    assert!(
        by_directory[0].1.contains(
            "::admit_non_execution_root_under_project_authority( &state.engine, node_history_policy, access.directory(), request, )"
        ),
        "the pinned-directory admission is no longer handed exactly an admitted attachment's directory"
    );
}

/// `items.effective` resolves items from the request's `project_path` through
/// `SubjectResolutionAuthority::for_live_project_root`, a spelling the marker
/// scan above does not see, so its admission is pinned here by the call chain:
/// the path reaches the engine only as the admitted canonical root, and the
/// handler never builds a path from the request text itself.
#[test]
fn items_effective_resolves_only_from_an_admitted_project_path() {
    let root = ryeos_engine::test_support::workspace_root();
    let text = std::fs::read_to_string(
        root.join("crates/daemon/ryeos-api/src/handlers/items_effective.rs"),
    )
    .unwrap();
    let body = non_test_text("items_effective.rs", &text);
    let admit = body
        .find("ryeos_app::live_project_path::admit_handler_live_path(")
        .expect("items.effective admits its project path with admit_handler_live_path");
    let first_use = body
        .find("for_live_project_root(")
        .expect("items.effective builds the live authority from the project root");
    assert!(admit < first_use, "the path is admitted before it is used");
    assert!(
        body[admit..first_use].contains(".canonical"),
        "the root the engine receives is the admitted canonical path"
    );
    assert_eq!(
        body.matches("req.project_path").count(),
        1,
        "the request's project_path is read once, to be admitted"
    );
    for forbidden in ["PathBuf::from(", "std::fs::canonicalize("] {
        assert!(
            !body.contains(forbidden),
            "items.effective must not build or canonicalise a path from the request: `{forbidden}`"
        );
    }
}

/// The exempt daemon sites that hand a root on or take it from a seat, pinned
/// by call chain: who may reach `surface_views`, how `ui.launch.mint` gets its
/// root, and how `ui.items` decides which project a seat reads.
#[test]
fn the_exempt_daemon_sites_get_their_roots_only_from_admission() {
    let root = ryeos_engine::test_support::workspace_root();
    let mut files = Vec::new();
    workspace_files(&root, &root.join("crates"), &mut files);
    let mut callers = std::collections::BTreeSet::new();
    for file in &files {
        let name = file.to_string_lossy().replace('\\', "/");
        if name == "crates/daemon/ryeos-api/src/surface_views.rs" {
            continue;
        }
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        if non_test_text(&name, &text).contains("surface_views::embed_") {
            callers.insert(name);
        }
    }
    assert_eq!(
        callers.into_iter().collect::<Vec<_>>(),
        [
            "crates/daemon/ryeos-api/src/handlers/items_effective.rs",
            "crates/daemon/ryeos-ui/src/handlers/ui_launch_mint.rs",
        ],
        "a new caller of surface_views must be admitted or operator-only, and named here"
    );

    let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
    let mint = read("crates/daemon/ryeos-ui/src/handlers/ui_launch_mint.rs");
    let mint = non_test_text("ui_launch_mint.rs", &mint);
    assert!(
        mint.contains("ui_projects::authorize_launch_project("),
        "ui.launch.mint takes its project from authorize_launch_project"
    );
    // rustfmt may split `req.project_path` across lines, so count it with the
    // whitespace removed.
    let compact: String = mint.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(
        compact.matches("req.project_path").count(),
        1,
        "the request's project_path is read once, to be authorized"
    );
    let projects = read("crates/daemon/ryeos-ui/src/handlers/ui_projects.rs");
    let projects = non_test_text("ui_projects.rs", &projects);
    let start = projects
        .find("fn authorize_launch_project(")
        .expect("authorize_launch_project exists");
    let end = projects[start..].find("\n}\n").expect("the function ends") + start;
    let body = &projects[start..end];
    assert!(
        body.contains("require_admitted_operator(") && body.contains("project_seat_operator_only"),
        "authorize_launch_project admits only the operator and refuses every other key"
    );

    let items = read("crates/daemon/ryeos-ui/src/handlers/ui_items.rs");
    let items = non_test_text("ui_items.rs", &items);
    assert_eq!(
        items.matches("seat_project_path(&caller").count(),
        2,
        "items.list and item.inspect both choose their project with seat_project_path"
    );
    assert_eq!(
        items.matches("get(\"project_path\")").count(),
        1,
        "the request's project_path is read only in seat_project_path"
    );
    assert!(
        items.contains("SeatCaller::Operator { .. }, named)"),
        "only the operator's seat may name a project"
    );
}

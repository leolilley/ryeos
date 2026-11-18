//! Which space supplies a UI item when a project is open.
//!
//! These tests drive the engine's ref resolution with the kind schemas and
//! bundle manifests shipped in this repository and a throwaway project. They
//! assert the behaviour as it is today: a project file under the ref of a
//! shipped `view:` or `surface:` item wins resolution over the bundle's item.
//! Tests that document an exposure say so in their comment and are expected
//! to change when the exposure is closed.
//!
//! Resolution here stops at "which file wins". It does not verify signatures,
//! so it says nothing about the trust class the winner then receives.

use std::path::{Path, PathBuf};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::error::EngineError;
use ryeos_engine::item_resolution::{RegisteredBundleRoot, ResolutionRoots, resolve_item_full};

const UI_BUNDLE: &str = "ryeos-ui";

fn bundles_dir() -> PathBuf {
    ryeos_engine::test_support::workspace_root().join("bundles")
}

/// Every bundle shipped in this repository, registered with the name and the
/// owned namespaces its manifest declares. A real node installs a subset, so
/// this is the largest set of reserved namespaces a node built from this tree
/// can have: a ref that is unreserved here is unreserved on every such node.
fn every_shipped_bundle() -> Vec<RegisteredBundleRoot> {
    let mut bundles = Vec::new();
    for entry in std::fs::read_dir(bundles_dir()).expect("read bundles directory") {
        let root = entry.expect("bundle directory entry").path();
        let manifest = [".ai/manifest.yaml", ".ai/manifest.source.yaml"]
            .iter()
            .map(|relative| root.join(relative))
            .find(|path| path.is_file());
        let Some(manifest) = manifest else {
            continue;
        };
        let text = std::fs::read_to_string(&manifest).expect("read bundle manifest");
        let value: serde_yaml::Value = serde_yaml::from_str(&text).expect("parse bundle manifest");
        let name = value
            .get("name")
            .and_then(serde_yaml::Value::as_str)
            .unwrap_or_else(|| panic!("{} declares no name", manifest.display()))
            .to_owned();
        let owned_namespaces = value
            .get("owns_namespaces")
            .and_then(serde_yaml::Value::as_sequence)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.as_str().expect("owned namespace string").to_owned())
                    .collect()
            })
            .unwrap_or_default();
        bundles.push(RegisteredBundleRoot {
            name,
            canonical_root: root,
            owned_namespaces,
        });
    }
    bundles.sort_by(|left, right| left.name.cmp(&right.name));
    assert!(
        bundles.iter().any(|bundle| bundle.name == UI_BUNDLE),
        "the UI bundle must be among the shipped bundles"
    );
    bundles
}

fn roots_with_project(project: Option<&Path>) -> ResolutionRoots {
    ResolutionRoots::from_registered(project.map(Path::to_path_buf), &every_shipped_bundle())
}

fn write_project_item(project: &Path, relative: &str) {
    let path = project.join(".ai").join(relative);
    std::fs::create_dir_all(path.parent().expect("item parent")).expect("create item directory");
    std::fs::write(path, "widget: text\n").expect("write project item");
}

/// Bare ids of every item the UI bundle ships under one kind directory.
fn shipped_ui_bare_ids(directory: &str) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read kind directory") {
            let path = entry.expect("kind directory entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
            {
                let relative = path.with_extension("");
                let relative = relative
                    .strip_prefix(base)
                    .expect("item under kind directory");
                out.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    let base = bundles_dir().join(UI_BUNDLE).join(".ai").join(directory);
    let mut out = Vec::new();
    walk(&base, &base, &mut out);
    out.sort();
    assert!(!out.is_empty(), "the UI bundle ships {directory}");
    out
}

/// Documents an exposure. A project file reserved for an installed bundle is
/// refused at resolution, but the reservation covers only a bundle's own
/// name and the namespaces its manifest declares. The UI bundle declares
/// none and its items live under `ryeos/…`, not under its name, so not one
/// shipped view or surface ref is reserved against a project.
#[test]
fn no_shipped_ui_view_or_surface_ref_is_reserved_against_project_space() {
    let roots = roots_with_project(None);
    let ui = every_shipped_bundle()
        .into_iter()
        .find(|bundle| bundle.name == UI_BUNDLE)
        .expect("UI bundle");
    assert!(
        ui.owned_namespaces.is_empty(),
        "the UI bundle manifest declares no owned namespace"
    );

    for directory in ["views", "surfaces"] {
        for bare_id in shipped_ui_bare_ids(directory) {
            assert!(
                roots.owner_of(&bare_id).is_none(),
                "{directory}/{bare_id} is reserved by {:?}",
                roots.owner_of(&bare_id)
            );
        }
    }
}

/// Neither UI kind names the spaces allowed to supply it, so project space is
/// a candidate for both. The service kind does, and admits bundle space only.
#[test]
fn view_and_surface_kinds_admit_project_space_and_the_service_kind_does_not() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    for kind in ["view", "surface"] {
        let schema = kinds
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} kind schema"));
        assert!(
            schema.launch_trust.is_none(),
            "the {kind} kind declares no space restriction"
        );
    }
    let service = kinds.get("service").expect("service kind schema");
    let launch_trust = service
        .launch_trust
        .as_ref()
        .expect("the service kind restricts its spaces");
    assert!(launch_trust.admits_space(ItemSpace::Bundle));
    assert!(!launch_trust.admits_space(ItemSpace::Project));
}

/// Documents an exposure. With a project open, a project file at the ref of a
/// view the node's surfaces bind is the resolution winner; the UI bundle's
/// view of that name is only recorded as shadowed. Without the project the
/// bundle's view answers.
#[test]
fn project_view_with_a_shipped_view_ref_wins_resolution_over_the_bundle_view() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let schema = kinds.get("view").expect("view kind schema");
    let item = CanonicalRef::parse("view:ryeos/threads/history").expect("view ref");

    let node_only = resolve_item_full(&roots_with_project(None), schema, &item)
        .expect("the bundle view resolves without a project");
    assert_eq!(node_only.winner_space, ItemSpace::Bundle);
    assert_eq!(node_only.winner_label, format!("bundle:{UI_BUNDLE}"));

    let project = tempfile::TempDir::new().expect("project directory");
    write_project_item(project.path(), "views/ryeos/threads/history.yaml");
    let with_project = resolve_item_full(&roots_with_project(Some(project.path())), schema, &item)
        .expect("the project view resolves");
    assert_eq!(with_project.winner_space, ItemSpace::Project);
    assert!(with_project.winner_path.starts_with(project.path()));
    assert_eq!(with_project.shadowed.len(), 1);
    assert_eq!(with_project.shadowed[0].space, ItemSpace::Bundle);
    assert_eq!(
        with_project.shadowed[0].label,
        format!("bundle:{UI_BUNDLE}")
    );
}

/// Documents an exposure. The surface a project is opened under resolves the
/// same way, so a project file at `surface:ryeos/ui/project` replaces the
/// node's project surface, and with it the capability ceiling that surface
/// declares.
#[test]
fn project_surface_with_the_shipped_project_surface_ref_wins_resolution_over_the_bundle_surface() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let schema = kinds.get("surface").expect("surface kind schema");
    let item = CanonicalRef::parse("surface:ryeos/ui/project").expect("surface ref");

    let node_only = resolve_item_full(&roots_with_project(None), schema, &item)
        .expect("the bundle surface resolves without a project");
    assert_eq!(node_only.winner_space, ItemSpace::Bundle);

    let project = tempfile::TempDir::new().expect("project directory");
    write_project_item(project.path(), "surfaces/ryeos/ui/project.yaml");
    let with_project = resolve_item_full(&roots_with_project(Some(project.path())), schema, &item)
        .expect("the project surface resolves");
    assert_eq!(with_project.winner_space, ItemSpace::Project);
    assert_eq!(with_project.shadowed.len(), 1);
    assert_eq!(
        with_project.shadowed[0].label,
        format!("bundle:{UI_BUNDLE}")
    );
}

/// The refusal that exists today: a project item under an installed bundle's
/// own name is refused rather than resolved.
#[test]
fn project_view_under_the_ui_bundles_own_name_is_refused() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let schema = kinds.get("view").expect("view kind schema");
    let project = tempfile::TempDir::new().expect("project directory");
    write_project_item(project.path(), "views/ryeos-ui/threads/history.yaml");
    let item = CanonicalRef::parse("view:ryeos-ui/threads/history").expect("view ref");

    match resolve_item_full(&roots_with_project(Some(project.path())), schema, &item) {
        Err(EngineError::ReservedNamespaceShadow {
            namespace, bundle, ..
        }) => {
            assert_eq!(namespace, UI_BUNDLE);
            assert_eq!(bundle, UI_BUNDLE);
        }
        other => panic!("expected a reserved namespace refusal, got {other:?}"),
    }
}

/// A project cannot redirect what an affordance or source calls: a project
/// file at the ref of a bundle service is never a candidate, so the bundle
/// service still answers and the project file is only reported.
#[test]
fn project_file_at_a_bundle_service_ref_never_wins_resolution() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let schema = kinds.get("service").expect("service kind schema");
    let project = tempfile::TempDir::new().expect("project directory");
    write_project_item(project.path(), "services/threads/input.yaml");
    let item = CanonicalRef::parse("service:threads/input").expect("service ref");

    let resolved = resolve_item_full(&roots_with_project(Some(project.path())), schema, &item)
        .expect("the bundle service resolves");
    assert_eq!(resolved.winner_space, ItemSpace::Bundle);
    assert!(
        resolved
            .shadowed
            .iter()
            .any(|candidate| candidate.space == ItemSpace::Project),
        "the ignored project file is reported"
    );
}

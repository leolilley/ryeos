//! Which space supplies a UI item when a project is open.
//!
//! These tests drive the engine's ref resolution with the kind schemas and
//! bundle manifests shipped in this repository and a throwaway project. The
//! `view` and `surface` kinds declare `project_shadowing: refuse`, so a
//! project file under the ref of a shipped view or surface is refused, naming
//! both files, instead of replacing the bundle's item. Every other kind keeps
//! project-first resolution.
//!
//! Resolution here stops at "which file answers". The refusal is decided on
//! file presence, before any signature is read, so it is the same for a
//! project whose signer is trusted and for one whose signer is not.

use std::path::{Path, PathBuf};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::error::EngineError;
use ryeos_engine::item_resolution::{RegisteredBundleRoot, ResolutionRoots, resolve_item_full};
use ryeos_engine::kind_registry::ProjectShadowing;

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

fn expect_shadow_refusal(
    result: Result<ryeos_engine::item_resolution::ResolutionResult, EngineError>,
    item_ref: &str,
    project: &Path,
) {
    match result {
        Err(EngineError::ProjectShadowsBundleItem {
            canonical_ref,
            bundle,
            project_path,
            bundle_path,
            ..
        }) => {
            assert_eq!(canonical_ref, item_ref);
            assert_eq!(bundle, UI_BUNDLE);
            assert!(project_path.starts_with(project), "{project_path:?}");
            assert!(
                bundle_path.starts_with(bundles_dir().join(UI_BUNDLE)),
                "{bundle_path:?}"
            );
        }
        other => panic!("expected {item_ref} to be refused as a shadow, got {other:?}"),
    }
}

/// No shipped view or surface ref sits in a namespace reserved for a bundle,
/// so the namespace reservation is not what protects them. The kind's own
/// declaration is: a project file at any shipped view or surface ref is
/// refused, without a list of namespaces or bundle names.
#[test]
fn every_shipped_ui_view_and_surface_ref_refuses_a_project_file_of_the_same_ref() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let reserved = roots_with_project(None);
    let project = tempfile::TempDir::new().expect("project directory");

    for (kind, directory) in [("view", "views"), ("surface", "surfaces")] {
        let schema = kinds
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} kind schema"));
        for bare_id in shipped_ui_bare_ids(directory) {
            assert!(
                reserved.owner_of(&bare_id).is_none(),
                "{directory}/{bare_id} is reserved by {:?}",
                reserved.owner_of(&bare_id)
            );
            write_project_item(project.path(), &format!("{directory}/{bare_id}.yaml"));
            let item_ref = format!("{kind}:{bare_id}");
            let item = CanonicalRef::parse(&item_ref).expect("item ref");
            expect_shadow_refusal(
                resolve_item_full(&roots_with_project(Some(project.path())), schema, &item),
                &item_ref,
                project.path(),
            );
        }
    }
}

/// Both UI kinds admit project space, so a project can ship its own views
/// and surfaces, and both declare that a project item never replaces a bundle
/// item. The service kind admits bundle space only.
#[test]
fn view_and_surface_kinds_refuse_project_shadowing_and_the_service_kind_is_bundle_only() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    for kind in ["view", "surface"] {
        let schema = kinds
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} kind schema"));
        assert!(
            schema.launch_trust.is_none(),
            "the {kind} kind declares no space restriction"
        );
        assert_eq!(
            schema.project_shadowing,
            ProjectShadowing::Refuse,
            "the {kind} kind refuses a project item that shadows a bundle item"
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

/// With a project open, a project file at the ref of a view the node's
/// surfaces bind is refused: neither file answers, and the refusal names the
/// project file and the bundle. Without the project the bundle's view
/// answers, and a project view under a ref no bundle ships resolves from the
/// project.
#[test]
fn project_view_with_a_shipped_view_ref_is_refused_and_never_replaces_the_bundle_view() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let schema = kinds.get("view").expect("view kind schema");
    let item = CanonicalRef::parse("view:ryeos/threads/history").expect("view ref");

    let node_only = resolve_item_full(&roots_with_project(None), schema, &item)
        .expect("the bundle view resolves without a project");
    assert_eq!(node_only.winner_space, ItemSpace::Bundle);
    assert_eq!(node_only.winner_label, format!("bundle:{UI_BUNDLE}"));

    let project = tempfile::TempDir::new().expect("project directory");
    write_project_item(project.path(), "views/ryeos/threads/history.yaml");
    write_project_item(project.path(), "views/farm/project/field.yaml");
    let roots = roots_with_project(Some(project.path()));
    expect_shadow_refusal(
        resolve_item_full(&roots, schema, &item),
        "view:ryeos/threads/history",
        project.path(),
    );

    let own = CanonicalRef::parse("view:farm/project/field").expect("view ref");
    let own = resolve_item_full(&roots, schema, &own).expect("the project's own view resolves");
    assert_eq!(own.winner_space, ItemSpace::Project);
    assert!(own.shadowed.is_empty());
}

/// The surface a project is opened under is protected the same way: a project
/// file at `surface:ryeos/ui/project` is refused, so it cannot replace the
/// node's project surface or the capability ceiling that surface declares.
#[test]
fn project_surface_with_the_shipped_project_surface_ref_is_refused() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let schema = kinds.get("surface").expect("surface kind schema");
    let item = CanonicalRef::parse("surface:ryeos/ui/project").expect("surface ref");

    let node_only = resolve_item_full(&roots_with_project(None), schema, &item)
        .expect("the bundle surface resolves without a project");
    assert_eq!(node_only.winner_space, ItemSpace::Bundle);

    let project = tempfile::TempDir::new().expect("project directory");
    write_project_item(project.path(), "surfaces/ryeos/ui/project.yaml");
    expect_shadow_refusal(
        resolve_item_full(&roots_with_project(Some(project.path())), schema, &item),
        "surface:ryeos/ui/project",
        project.path(),
    );
}

/// The namespace reservation still applies first: a project item under an
/// installed bundle's own name is refused as a reserved namespace.
#[test]
fn project_view_under_the_ui_bundles_own_name_is_refused_as_a_reserved_namespace() {
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

/// The refusal is confined to the kinds that declare it. Every other shipped
/// kind declares nothing, and each one that takes items from both project and
/// bundle space still resolves project-first with the bundle item recorded
/// as shadowed, exactly as before.
#[test]
fn every_other_kind_still_resolves_project_first() {
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let mut refusing: Vec<&str> = kinds
        .kinds()
        .filter(|kind| {
            kinds
                .get(kind)
                .is_some_and(|schema| schema.project_shadowing == ProjectShadowing::Refuse)
        })
        .collect();
    refusing.sort_unstable();
    assert_eq!(refusing, ["picture", "surface", "view"]);

    let mut checked = Vec::new();
    for kind in ["directive", "tool", "graph", "knowledge", "config"] {
        let schema = kinds
            .get(kind)
            .unwrap_or_else(|| panic!("{kind} kind schema"));
        assert_eq!(schema.project_shadowing, ProjectShadowing::Allow, "{kind}");
        let both_spaces = schema.launch_trust.as_ref().is_none_or(|launch_trust| {
            launch_trust.admits_space(ItemSpace::Project)
                && launch_trust.admits_space(ItemSpace::Bundle)
        });
        if !both_spaces {
            continue;
        }
        let extension = &schema.extensions.first().expect("kind extension").ext;
        let project = tempfile::TempDir::new().expect("project directory");
        let bundle = tempfile::TempDir::new().expect("bundle directory");
        for root in [project.path(), bundle.path()] {
            let path = root
                .join(".ai")
                .join(&schema.directory)
                .join(format!("shadowcheck/item{extension}"));
            std::fs::create_dir_all(path.parent().expect("item parent")).expect("item directory");
            std::fs::write(path, "name: item\n").expect("write item");
        }
        let roots = ResolutionRoots::from_registered(
            Some(project.path().to_path_buf()),
            &[RegisteredBundleRoot {
                name: "shadowcheck-bundle".to_owned(),
                canonical_root: bundle.path().to_path_buf(),
                owned_namespaces: Vec::new(),
            }],
        );
        let item = CanonicalRef::parse(&format!("{kind}:shadowcheck/item")).expect("item ref");
        let resolved = resolve_item_full(&roots, schema, &item)
            .unwrap_or_else(|error| panic!("{kind} resolves project-first: {error}"));
        assert_eq!(resolved.winner_space, ItemSpace::Project, "{kind}");
        assert_eq!(resolved.shadowed.len(), 1, "{kind}");
        assert_eq!(resolved.shadowed[0].space, ItemSpace::Bundle, "{kind}");
        checked.push(kind);
    }
    assert!(
        checked.contains(&"directive") && checked.contains(&"tool"),
        "project-first resolution was exercised for {checked:?}"
    );
}

//! View sets as items: the `view_set` kind, the Surfaces that list them, and
//! the daemon path that resolves, admits and clamps them.
//!
//! Everything here runs the real engine over this repository's signed
//! bundles. The shipped Surfaces must open with exactly the compositions
//! their former inline lists declared (`shipped-surface-view-sets.json` is
//! that list, captured before the inline form was removed); project-authored
//! sets are held to the project trust gate and the composition validator, and
//! a set that cannot be shown leaves its Surface open with a typed reason.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use ryeos_api::surface_views::EmbeddedViewIdentity;
use ryeos_client_base::surface::view_sets::{ViewSetSeedSpec, parse_view_set_item};
use ryeos_ui::browser_session::AdmittedBindingAttachment;
use ryeos_ui::state::get_ui_state;
use ryeos_ui::test_support as test_state;
use serde_json::Value;

fn golden() -> BTreeMap<String, Vec<ViewSetSeedSpec>> {
    let text = include_str!("fixtures/view-sets/shipped-surface-view-sets.json");
    let raw: BTreeMap<String, Value> = serde_json::from_str(text).expect("golden parses");
    raw.into_iter()
        .map(|(surface, seeds)| {
            let seeds = serde_json::from_value(seeds).expect("golden seeds");
            (surface, seeds)
        })
        .collect()
}

fn bundle_dir() -> std::path::PathBuf {
    ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai")
}

fn signed(body: &str) -> String {
    let key = lillux::crypto::load_signing_key(
        &ryeos_engine::test_support::workspace_root().join(".dev-keys/PUBLISHER_DEV.pem"),
    )
    .expect("established development publisher key");
    lillux::signature::sign_content(body, &key, "#", None)
}

fn write_project_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture file parent"))
        .expect("create fixture parent");
    std::fs::write(path, content).expect("write fixture file");
}

fn attachment(
    state: &ryeos_app::state::AppState,
    session_id: &str,
    attachment_id: &str,
) -> Arc<AdmittedBindingAttachment> {
    get_ui_state(state)
        .expect("UI state")
        .browser_sessions
        .get_session(session_id)
        .expect("active session")
        .attachments
        .get(attachment_id)
        .expect("surface attachment")
        .clone()
}

fn seeds_of(attachment: &AdmittedBindingAttachment) -> Vec<ViewSetSeedSpec> {
    attachment
        .presentation
        .definition
        .surface()
        .expect("a Surface presentation")
        .to_mount_seeds()
        .expect("the embedded view sets admit")
}

fn shipped_view_set_files() -> Vec<std::path::PathBuf> {
    fn visit(path: &Path, out: &mut Vec<std::path::PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .expect("read view_sets directory")
            .map(|entry| entry.expect("directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                visit(&entry, out);
            } else if entry.extension().is_some_and(|ext| ext == "yaml") {
                out.push(entry);
            }
        }
    }
    let mut files = Vec::new();
    visit(&bundle_dir().join("view_sets"), &mut files);
    files
}

fn yaml_value(path: &Path) -> Value {
    serde_yaml::from_str(&std::fs::read_to_string(path).expect("read item"))
        .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()))
}

/// Each shipped Surface opens with the very compositions its inline list
/// declared, in the same order, and every view they arrange is in the
/// binding's admitted closure. The binding names exactly the view sets the
/// Surface lists, all resolved, none attenuated.
#[tokio::test]
async fn shipped_surfaces_open_with_the_compositions_their_inline_lists_declared() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let expected_refs = BTreeMap::from([
        (
            "surface:ryeos/ui/assistant",
            vec![
                "view_set:ryeos/ui/development",
                "view_set:ryeos/ui/overview",
                "view_set:ryeos/ui/review",
                "view_set:ryeos/ui/work",
            ],
        ),
        (
            "surface:ryeos/ui/assistant-observe",
            vec![
                "view_set:ryeos/ui/development",
                "view_set:ryeos/ui/overview",
                "view_set:ryeos/ui/review",
                "view_set:ryeos/ui/work",
            ],
        ),
        (
            "surface:ryeos/ui/base-observe",
            vec![
                "view_set:ryeos/ui/development",
                "view_set:ryeos/ui/home-overview",
                "view_set:ryeos/ui/review",
                "view_set:ryeos/ui/work",
            ],
        ),
    ]);
    let golden = golden();
    assert_eq!(
        golden.keys().map(String::as_str).collect::<Vec<_>>(),
        expected_refs.keys().copied().collect::<Vec<_>>()
    );
    for (surface_ref, seeds) in &golden {
        let (session_id, _ctx, _producer, coordinate) =
            test_state::live_presentation_with_surface(&state, surface_ref).await;
        let attachment = attachment(&state, &session_id, &coordinate.binding_attachment_id);
        assert_eq!(&seeds_of(&attachment), seeds, "{surface_ref}");

        let binding = &attachment.compiled_binding.binding;
        assert_eq!(
            binding
                .view_sets
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected_refs[surface_ref.as_str()],
            "{surface_ref}"
        );
        assert!(
            binding
                .view_sets
                .values()
                .all(|state| matches!(state, EmbeddedViewIdentity::Resolved { .. })),
            "{surface_ref}: every shipped set resolves"
        );
        assert!(
            binding
                .attenuated
                .iter()
                .all(|entry| !entry.coordinate.starts_with("view_set:")),
            "{surface_ref}: no shipped set is refused: {:?}",
            binding.attenuated
        );
        for seed in seeds {
            let mut stack: Vec<_> = seed.root.iter().collect();
            while let Some(node) = stack.pop() {
                match node {
                    ryeos_client_base::surface::view_sets::LayoutSeedSpec::Group {
                        views, ..
                    } => {
                        for view in views {
                            assert!(
                                matches!(
                                    binding.views.get(&view.0),
                                    Some(EmbeddedViewIdentity::Resolved { .. })
                                ),
                                "{surface_ref}: {} is in the admitted closure",
                                view.0
                            );
                        }
                    }
                    ryeos_client_base::surface::view_sets::LayoutSeedSpec::Split {
                        first,
                        second,
                        ..
                    } => {
                        stack.push(first);
                        stack.push(second);
                    }
                }
            }
        }
    }
}

/// One snapshot per shipped item: every file parses as a composition, its id
/// is unique within the Surfaces that list it, it matches the captured inline
/// declaration, and each view it arranges is a shipped view.
#[test]
fn every_shipped_view_set_item_parses_and_matches_its_captured_declaration() {
    let golden = golden();
    let mut by_file_id: BTreeMap<String, ViewSetSeedSpec> = BTreeMap::new();
    for path in shipped_view_set_files() {
        let seed = parse_view_set_item(&yaml_value(&path))
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(seed.schema, 2, "{name}");
        let mut views = Vec::new();
        let mut stack: Vec<_> = seed.root.iter().collect();
        while let Some(node) = stack.pop() {
            match node {
                ryeos_client_base::surface::view_sets::LayoutSeedSpec::Group {
                    views: group,
                    ..
                } => views.extend(group.iter().map(|view| view.0.clone())),
                ryeos_client_base::surface::view_sets::LayoutSeedSpec::Split {
                    first,
                    second,
                    ..
                } => {
                    stack.push(first);
                    stack.push(second);
                }
            }
        }
        for view in views {
            let relative = view.strip_prefix("view:").expect("view ref");
            assert!(
                bundle_dir()
                    .join(format!("views/{relative}.yaml"))
                    .is_file(),
                "{name} arranges {view}, which is not a shipped view"
            );
        }
        by_file_id.insert(name, seed);
    }
    assert_eq!(
        by_file_id.keys().map(String::as_str).collect::<Vec<_>>(),
        ["development", "home-overview", "overview", "review", "work"]
    );
    // Every captured inline declaration is exactly one shipped item.
    for (surface, seeds) in &golden {
        for seed in seeds {
            assert!(
                by_file_id.values().any(|item| item == seed),
                "{surface}: inline set `{}` has no matching item",
                seed.id
            );
        }
    }
}

/// Project-authored sets under one project Surface: a signed set is shown; an
/// unsigned set, a set that does not resolve, a malformed set, a set whose id
/// repeats an earlier one and a project file at a shipped set's ref are each
/// left out with a typed reason, and the Surface still opens with the rest.
#[tokio::test]
async fn project_authored_view_sets_are_held_to_the_trust_gate_and_the_composition_validator() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    std::fs::create_dir(&project).expect("create project");
    let set = |id: &str, root: &str| {
        format!(
            "schema: 2\nid: {id}\ntitle: {id}\nroot:\n  type: group\n  views: [{root}]\n  active: 0\n"
        )
    };
    write_project_file(
        &project,
        ".ai/surfaces/acme/ui/mine.yaml",
        &signed(
            "name: acme-mine\ntitle: Mine\nview_sets:\n  - view_set:ryeos/ui/work\n  - view_set:acme/mine\n  - view_set:acme/unsigned\n  - view_set:acme/missing\n  - view_set:acme/malformed\n  - view_set:acme/twin\n  - view_set:ryeos/ui/review\n",
        ),
    );
    write_project_file(
        &project,
        ".ai/view_sets/acme/mine.yaml",
        &signed(&set("mine", "\"view:ryeos/work/list\"")),
    );
    write_project_file(
        &project,
        ".ai/view_sets/acme/unsigned.yaml",
        &set("unsigned", "\"view:ryeos/work/list\""),
    );
    write_project_file(
        &project,
        ".ai/view_sets/acme/malformed.yaml",
        &signed(&set("malformed", "")),
    );
    write_project_file(
        &project,
        ".ai/view_sets/acme/twin.yaml",
        &signed(&set("mine", "\"view:ryeos/work/list\"")),
    );
    write_project_file(
        &project,
        ".ai/view_sets/ryeos/ui/review.yaml",
        &signed(&set("review", "\"view:ryeos/work/list\"")),
    );

    let (session_id, _ctx, _producer, coordinate) =
        test_state::live_presentation_with_surface_in_project(
            &state,
            &project,
            &project,
            "surface:acme/ui/mine",
        )
        .await;
    let attachment = attachment(&state, &session_id, &coordinate.binding_attachment_id);
    let seeds = seeds_of(&attachment);
    assert_eq!(
        seeds
            .iter()
            .map(|seed| seed.id.as_str())
            .collect::<Vec<_>>(),
        ["work", "mine"],
        "the bundle set and the signed project set are shown, in listed order: {:?} {:?}",
        attachment.compiled_binding.binding.attenuated,
        attachment.compiled_binding.binding.view_sets
    );

    let binding = &attachment.compiled_binding.binding;
    let reasons: BTreeMap<&str, &str> = binding
        .attenuated
        .iter()
        .filter(|entry| entry.coordinate.starts_with("view_set:"))
        .map(|entry| (entry.coordinate.as_str(), entry.reason.as_str()))
        .collect();
    assert_eq!(reasons.len(), 5, "{reasons:?}");
    assert!(reasons["view_set:acme/unsigned"].starts_with("view_set_untrusted: "));
    assert!(reasons["view_set:acme/missing"].starts_with("view_set_unresolved: "));
    assert!(reasons["view_set:acme/malformed"].starts_with("view_set_malformed: "));
    assert!(reasons["view_set:acme/twin"].starts_with("view_set_duplicate_id: "));
    assert!(reasons["view_set:ryeos/ui/review"].starts_with("view_set_unresolved: "));
    assert!(
        reasons["view_set:ryeos/ui/review"].contains("shadows the item of the same ref"),
        "a project file never replaces a shipped set: {}",
        reasons["view_set:ryeos/ui/review"]
    );
    for shown in ["view_set:ryeos/ui/work", "view_set:acme/mine"] {
        assert!(
            matches!(
                binding.view_sets.get(shown),
                Some(EmbeddedViewIdentity::Resolved { .. })
            ),
            "{shown}"
        );
    }
    for refused in reasons.keys() {
        assert!(
            matches!(
                binding.view_sets.get(*refused),
                Some(EmbeddedViewIdentity::Degraded { .. })
            ),
            "{refused}"
        );
    }
}

/// The inline form is gone: a project Surface that still carries a
/// composition under `view_sets` is refused when it is composed, not read.
#[tokio::test]
async fn a_project_surface_with_an_inline_view_set_is_refused() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    std::fs::create_dir(&project).expect("create project");
    write_project_file(
        &project,
        ".ai/surfaces/acme/ui/inline.yaml",
        &signed(
            "name: acme-inline\ntitle: Inline\nview_sets:\n  - schema: 2\n    id: inline\n    title: Inline\n",
        ),
    );
    let operator = test_state::operator_launch_context(&state);
    let principal = operator.fingerprint.clone();
    let refused = ryeos_ui::handlers::ui_launch_mint::handle(
        ryeos_ui::handlers::ui_launch_mint::Request {
            ui_binding_contract_revision: ryeos_ui::UI_BINDING_CONTRACT_REVISION.into(),
            root_ref: "surface:acme/ui/inline".into(),
            project_path: Some(project.display().to_string()),
            user_principal_id: Some(principal.clone()),
            scopes: None,
        },
        operator,
        Arc::new(state.clone()),
    )
    .await;
    let error = match refused {
        Ok(_) => panic!("an inline view set must not open"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("view_sets"), "{error}");
}

fn compose_project_view_set(
    state: &ryeos_app::state::AppState,
    project: &Path,
    item: &str,
) -> Result<Value, String> {
    state
        .engine
        .with_checked_bundle_generation(|generation| {
            generation.effective_item(ryeos_engine::engine::EffectiveItemRequest {
                logical_project: ryeos_engine::contracts::LogicalProject::Absent(
                    ryeos_engine::contracts::LogicalProjectAbsence::NotRegistered,
                ),
                item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(item)?,
                expected_kind: Some("view_set".into()),
                project_root: Some(project.to_path_buf()),
                subject_resolution_authority:
                    ryeos_engine::contracts::SubjectResolutionAuthority::for_live_project_root(
                        Some(project),
                    ),
            })
        })
        .map(|effective| effective.composed_value)
        .map_err(|error| format!("{error:#}"))
}

/// The kind's contract: required fields and field types are checked when the
/// item is composed, so a malformed item never reaches the daemon's
/// composition validator.
#[test]
fn view_set_kind_contract_accepts_a_valid_item_and_refuses_each_malformed_one() {
    let (_state_dir, _state_owner, state) = test_state::build_test_state_with_live_bundles();
    let projects = tempfile::tempdir().expect("projects tempdir");
    let project = projects.path().join("project");
    std::fs::create_dir(&project).expect("create project");
    let valid = "schema: 2\nid: ok\ntitle: Ok\npresentation: page\nscroll: true\nsequence: welcome\noffered: false\nroot:\n  type: group\n  views: [\"view:ryeos/work/list\"]\n  active: 0\n";
    let cases = [
        ("acme/ok", valid, None),
        (
            "acme/empty",
            "schema: 2\nid: empty\ntitle: Empty\nroot: null\n",
            None,
        ),
        ("acme/no-title", "schema: 2\nid: x\n", Some("title")),
        ("acme/no-schema", "id: x\ntitle: X\n", Some("schema")),
        (
            "acme/text-schema",
            "schema: \"2\"\nid: x\ntitle: X\n",
            Some("schema"),
        ),
        (
            "acme/bad-presentation",
            "schema: 2\nid: x\ntitle: X\npresentation: poster\n",
            Some("presentation"),
        ),
        (
            "acme/bad-scroll",
            "schema: 2\nid: x\ntitle: X\nscroll: yes please\n",
            Some("scroll"),
        ),
        (
            "acme/bad-offered",
            "schema: 2\nid: x\ntitle: X\noffered: 1\n",
            Some("offered"),
        ),
        (
            "acme/bad-sequence",
            "schema: 2\nid: x\ntitle: X\nsequence: 3\n",
            Some("sequence"),
        ),
        (
            "acme/text-root",
            "schema: 2\nid: x\ntitle: X\nroot: group\n",
            Some("root"),
        ),
    ];
    for (item, body, refusal) in cases {
        let relative = item.strip_prefix("acme/").unwrap();
        write_project_file(
            &project,
            &format!(".ai/view_sets/acme/{relative}.yaml"),
            &signed(body),
        );
        let composed = compose_project_view_set(&state, &project, &format!("view_set:{item}"));
        match (refusal, composed) {
            (None, Ok(value)) => assert!(value["title"].is_string(), "{item}: {value}"),
            (Some(field), Err(error)) => {
                assert!(error.contains(field), "{item}: {error}");
            }
            (None, Err(error)) => panic!("{item} must compose: {error}"),
            (Some(field), Ok(_)) => panic!("{item} must be refused for {field}"),
        }
    }
}

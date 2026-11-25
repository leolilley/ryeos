use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_client_base::ui::content::views_from_surface;
use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
use ryeos_client_base::ui::view_model::build_view_model;
use ryeos_client_base::ui::{UiBindingAttachment, UiBindingRequestBounds};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::contracts::{LogicalProject, LogicalProjectAbsence};
use ryeos_engine::item_resolution::{RegisteredBundleRoot, ResolutionRoots};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::resolution::run_effective_item_pipeline;
use serde_json::{Map, Value, json};

/// Environment switch: rewrite the checked-in per-view snapshots from the
/// current tree instead of comparing against them.
const UPDATE_SNAPSHOTS_ENV: &str = "RYEOS_UPDATE_SNAPSHOTS";

fn snapshot_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/views")
}

/// `view:ryeos/attention/panel` is stored at `<root>/ryeos/attention/panel.json`.
fn snapshot_path(view_ref: &str) -> PathBuf {
    let id = view_ref
        .strip_prefix("view:")
        .unwrap_or_else(|| panic!("view ref {view_ref} has no view: prefix"));
    snapshot_root().join(format!("{id}.json"))
}

/// Stable text form: object keys sorted, two-space indent, trailing newline.
fn write_stable_json(value: &Value, indent: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match value {
        Value::Object(map) if !map.is_empty() => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push_str("{\n");
            let last = sorted.len() - 1;
            for (index, (key, child)) in sorted.into_iter().enumerate() {
                out.push_str(&pad(indent + 1));
                out.push_str(&serde_json::to_string(key).expect("serialize key"));
                out.push_str(": ");
                write_stable_json(child, indent + 1, out);
                out.push_str(if index == last { "\n" } else { ",\n" });
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
        Value::Array(items) if !items.is_empty() => {
            out.push_str("[\n");
            let last = items.len() - 1;
            for (index, child) in items.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                write_stable_json(child, indent + 1, out);
                out.push_str(if index == last { "\n" } else { ",\n" });
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).expect("serialize leaf")),
    }
}

fn stable_json(value: &Value) -> String {
    let mut out = String::new();
    write_stable_json(value, 0, &mut out);
    out.push('\n');
    out
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, child, out);
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (index, child) in items.iter().enumerate() {
                flatten(&format!("{prefix}[{index}]"), child, out);
            }
        }
        leaf => {
            let mut text = serde_json::to_string(leaf).expect("serialize leaf");
            if text.chars().count() > 160 {
                text = text.chars().take(160).collect::<String>() + "...";
            }
            out.insert(prefix.to_string(), text);
        }
    }
}

/// One line per changed field: `path: old -> new`.
fn field_diff(old: &Value, new: &Value) -> Vec<String> {
    let (mut before, mut after) = (BTreeMap::new(), BTreeMap::new());
    flatten("", old, &mut before);
    flatten("", new, &mut after);
    let mut lines = Vec::new();
    for (path, old_text) in &before {
        match after.get(path) {
            Some(new_text) if new_text == old_text => {}
            Some(new_text) => lines.push(format!("  {path}: {old_text} -> {new_text}")),
            None => lines.push(format!("  {path}: {old_text} -> (removed)")),
        }
    }
    for (path, new_text) in &after {
        if !before.contains_key(path) {
            lines.push(format!("  {path}: (absent) -> {new_text}"));
        }
    }
    lines
}

fn check_view_snapshots(actual: &BTreeMap<String, Value>) {
    let root = snapshot_root();
    if std::env::var_os(UPDATE_SNAPSHOTS_ENV).is_some() {
        for (view_ref, snapshot) in actual {
            let path = snapshot_path(view_ref);
            fs::create_dir_all(path.parent().expect("snapshot parent")).expect("create dir");
            fs::write(&path, stable_json(snapshot))
                .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        }
        for path in json_files_below(&root) {
            let known = actual
                .keys()
                .any(|view_ref| snapshot_path(view_ref) == path);
            if !known {
                fs::remove_file(&path).expect("remove stale snapshot");
            }
        }
        return;
    }

    let mut report = Vec::new();
    for (view_ref, snapshot) in actual {
        let path = snapshot_path(view_ref);
        match fs::read_to_string(&path) {
            Err(_) => report.push(format!("{view_ref}: no snapshot at {}", path.display())),
            Ok(text) => {
                let current = stable_json(snapshot);
                if text == current {
                    continue;
                }
                // Both sides go through the same parser so float re-reading
                // cannot show up as a difference; only real field changes do.
                let parse = |source: &str| -> Value {
                    serde_json::from_str(source)
                        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
                };
                let lines = field_diff(&parse(&text), &parse(&current));
                if lines.is_empty() {
                    report.push(format!("{view_ref}: snapshot file is not in stable form"));
                } else {
                    let mut entry = vec![format!("{view_ref}: behavior changed")];
                    entry.extend(lines);
                    report.push(entry.join("\n"));
                }
            }
        }
    }
    for path in json_files_below(&root) {
        if !actual
            .keys()
            .any(|view_ref| snapshot_path(view_ref) == path)
        {
            report.push(format!("{}: snapshot has no bundled view", path.display()));
        }
    }
    assert!(
        report.is_empty(),
        "per-view behavior snapshots differ (rerun with {UPDATE_SNAPSHOTS_ENV}=1 to rewrite):\n{}",
        report.join("\n")
    );
}

fn json_files_below(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries {
        let path = entry.expect("snapshot entry").path();
        if path.is_dir() {
            found.extend(json_files_below(&path));
        } else if path.extension().is_some_and(|ext| ext == "json") {
            found.push(path);
        }
    }
    found
}

// Exercises the client-side retained initialization contract. The authored
// View is resolved through signed assets below; these deterministic admission
// descriptors are a fixture, not proof of daemon signature/admission checks.
fn initialized_view_core(attachment: UiBindingAttachment) -> RyeOsCore {
    use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
    use ryeos_client_base::ui::binding::{
        UiBindingAttachmentCoordinate, UiPresentationMembership, UiSurfaceMembership,
    };
    use ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest;
    use ryeos_client_base::ui::presentation_initialization::{
        UiPresentationInitialSelection, UiPresentationInitialization,
        UiPresentationInitializationRecord,
    };
    use ryeos_client_base::ui::seat::{SeatEvent, SeatEventKind};

    const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
    const SURFACE: &str = "22222222-2222-4222-8222-222222222222";
    const PREPARE: &str = "33333333-3333-4333-8333-333333333333";
    let root: UiBindingAttachment = serde_json::from_value(json!({
        "binding_attachment_id":"cutover-shared-root", "binding_generation":1,
        "binding_digest":"shared-fixture-binding", "root_ref":"config:fixture/cutover",
        "root_generation":"shared-fixture-generation", "project_path":null,
        "subject":{"kind":"projectless"}, "posture":"observation_only","seat_project":{"state":"projectless"},
        "named_items": [], "binding_request_bounds":attachment.binding_request_bounds,
        "presentation":{"definition":{"kind":"shared_presentation","config":{
            "category":"ui", "shared_presentation":{"schema":2,"title":"Cutover",
                "launcher":"launch", "dialogs":[{"id":"launch","title":"Launch",
                    "view":"view:fixture/launcher"}],
                "capabilities":{"sources":[],"affordances":[]}}}},
            "views":{"view:fixture/launcher":{"widget":"rows"}}}
    }))
    .unwrap();
    let coordinate = |descriptor: &UiBindingAttachment| UiBindingAttachmentCoordinate {
        binding_attachment_id: descriptor.binding_attachment_id.clone(),
        binding_generation: descriptor.binding_generation,
        binding_digest: descriptor.binding_digest.clone(),
    };
    let seeds = attachment
        .presentation
        .definition
        .surface()
        .unwrap()
        .to_mount_seeds()
        .unwrap();
    let active = seeds.first().map(|seed| seed.id.clone());
    let binding = coordinate(&attachment);
    let manifest = UiSurfaceMountManifest::allocate(
        PresentationId::try_new(PRESENTATION).unwrap(),
        SurfaceInstanceId::try_new(SURFACE).unwrap(),
        1,
        seeds
            .into_iter()
            .map(|seed| (seed, binding.clone()))
            .collect(),
        active.as_deref(),
    )
    .unwrap();
    let mut initial = UiPresentationInitialization {
        schema: 2,
        selection: UiPresentationInitialSelection::ConfiguredDefaults,
        presentation_id: PRESENTATION.into(),
        creation_request_id: PREPARE.into(),
        root_binding: coordinate(&root),
        next_mount_id: manifest.next_mount_id,
        surfaces: vec![UiSurfaceMembership {
            surface_instance_id: SURFACE.into(),
            ordinal: 1,
            title: "Cutover".into(),
            binding,
            mount_manifest: manifest,
            saved_surface: None,
        }],
        binding_attachments: vec![root, attachment],
    };
    initial
        .binding_attachments
        .sort_by(|a, b| a.binding_attachment_id.cmp(&b.binding_attachment_id));
    initial.validate().expect("exact cutover initialization");
    let session = BrowserSession {
        ui_binding_contract_revision: ryeos_client_base::UI_BINDING_CONTRACT_REVISION.into(),
        session_id: "cutover-session".into(),
        root_attachment_id: initial.root_binding.binding_attachment_id.clone(),
        binding_attachments: initial.binding_attachments.clone(),
        presentations: vec![UiPresentationMembership {
            presentation_id: PRESENTATION.into(),
            root_binding: initial.root_binding.clone(),
            thread_id: Some("T-cutover".into()),
            chain_root_id: Some("T-cutover".into()),
            next_mount_id: initial.next_mount_id,
            initialization_digest: initial.digest().unwrap(),
            surfaces: initial.surfaces.clone(),
        }],
        ..Default::default()
    };
    let mut core = RyeOsCore::new(session, PRESENTATION, BrowserViewport::default(), 0);
    let event = SeatEvent {
        producer_engine_seq: None,
        seq: 1,
        kind: SeatEventKind::PresentationInitialized(UiPresentationInitializationRecord {
            operation_id: PREPARE.into(),
            initialization: initial,
        }),
    };
    core.validate_replayed_initialization(std::slice::from_ref(&event))
        .unwrap();
    core.replay_seat_events([event]);
    core.require_replayed_initialization().unwrap();
    core
}

fn yaml_files_below(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .map(|entry| entry.expect("read directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                visit(&entry, files);
            } else if entry.extension().and_then(|value| value.to_str()) == Some("yaml") {
                files.push(entry);
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files
}

#[test]
fn node_inventory_views_have_one_origin_and_remotes_do_not_retarget_input() {
    let repository = ryeos_engine::test_support::workspace_root();
    let views = repository.join("bundles/ryeos-ui/.ai/views/ryeos");
    for removed in ["bundles/list", "remotes/list", "gc/status"] {
        assert!(
            !views.join(format!("{removed}.yaml")).exists(),
            "removed View origin must not be restored as an alias: {removed}"
        );
    }
    for (origin, source) in [
        ("node/bundles", "service:bundle/list"),
        ("node/remotes", "service:ui/ryeos-ui/remotes/list"),
        ("node/machines", "service:ui/ryeos-ui/node/machines"),
        ("node/gc", "service:ui/ryeos-ui/gc/status"),
    ] {
        let text = fs::read_to_string(views.join(format!("{origin}.yaml"))).unwrap();
        let definition: Value = serde_yaml::from_str(&text).unwrap();
        assert_eq!(definition["sources"]["default"]["ref"], source);
        assert!(definition.get("extends").is_none());
        if origin == "node/remotes" {
            assert!(definition.get("selection").is_none());
            assert!(definition.get("affordances").is_none());
        }
    }
}

#[test]
fn activity_sections_expose_their_source_coverage_without_executable_activation() {
    let views =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/views/ryeos");
    for (origin, source_channel, coverage) in [
        ("node/events", "pulse", "pulse_coverage"),
        ("node/events", "pulse", "usage_coverage"),
        ("node/events", "recent", "events_coverage"),
        ("home/overview", "movement", "events_coverage"),
    ] {
        let raw: Value = serde_yaml::from_str(
            &fs::read_to_string(views.join(format!("{origin}.yaml"))).unwrap(),
        )
        .unwrap();
        let section = raw["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|section| {
                section["source_channel"] == source_channel && section["collection"] == coverage
            })
            .expect("activity coverage remains visible beside observed rows");
        assert_eq!(section["projection"]["primary"], "state");
        assert_eq!(section["projection"]["meta"], "reason");
        assert!(section.get("activate").is_none());
        let binding: ryeos_client_base::ui::content::SectionBinding =
            serde_json::from_value(section.clone()).unwrap();
        let response =
            serde_json::json!({coverage: {"state":"failed", "reason":"signed_event_read_failed"}});
        let rows = ryeos_client_base::ui::content::project_section(&binding, &response);
        assert_eq!(
            rows.len(),
            1,
            "coverage object projects as an ordinary section record"
        );
        assert_eq!(rows[0].primary, "failed");
    }
}

fn attention_items() -> Value {
    json!({"schema_version": "ryeos.ui.attention.v3", "attention": [
        {
            "id": "approval:A-1", "kind": "approval", "title": "Approval requested",
            "tone": "warn", "state": "pending", "project_root": "/work",
            "subject": {"chain_root_id": "T-chain", "thread_id": "T-place"},
            "detail": {
                "summary": "command_execution", "approval_id": "A-1",
                "request_digest": "d",
            },
            "actions": {"open": true, "accept": false, "decline": true, "acknowledge": false},
        },
        {
            "id": "failed_run:T-head", "kind": "failed_run", "title": "Run failed",
            "tone": "danger", "state": "failed", "project_root": null,
            "subject": {"chain_root_id": "T-chain2", "thread_id": "T-head"},
            "detail": {"summary": "directive:x"},
            "actions": {"open": true, "acknowledge": true},
        },
    ]})
}

#[test]
fn home_attention_section_projects_every_kind_through_the_one_item_shape() {
    let views =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/views/ryeos");
    let raw: Value =
        serde_yaml::from_str(&fs::read_to_string(views.join("home/overview.yaml")).unwrap())
            .unwrap();
    let section = raw["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["collection"] == "attention")
        .expect("home lists the attention queue");
    assert!(
        raw["sections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|section| section["collection"] != "candidates"),
        "candidates arrive as items of the one attention list"
    );
    let binding: ryeos_client_base::ui::content::SectionBinding =
        serde_json::from_value(section.clone()).unwrap();
    let rows = ryeos_client_base::ui::content::project_section(&binding, &attention_items());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].primary, "Approval requested");
    assert_eq!(rows[0].meta.as_deref(), Some("command_execution"));
    assert_eq!(rows[0].tone.as_deref(), Some("warn"));
    assert_eq!(rows[1].primary, "Run failed");
    assert_eq!(rows[1].tone.as_deref(), Some("danger"));

    let activate = binding.activate.as_deref().unwrap();
    let open = raw["affordances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|affordance| affordance["id"] == activate)
        .unwrap();
    assert_eq!(
        open["invoke"]["merge"]["work"]["thread"],
        "{record.subject.thread_id}"
    );
    assert_eq!(open["invoke"]["open_view"], "view:ryeos/work/overview");
}

#[test]
fn review_pending_gates_decisions_on_the_item_actions_map() {
    let views =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/views/ryeos");
    let raw: Value =
        serde_yaml::from_str(&fs::read_to_string(views.join("review/pending.yaml")).unwrap())
            .unwrap();
    assert_eq!(raw["sources"]["default"]["collection"], "attention");
    let affordance = |id: &str| {
        raw["affordances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|affordance| affordance["id"] == id)
            .unwrap()
            .clone()
    };
    let items = attention_items();
    let items = items["attention"].as_array().unwrap();
    let eligibility = |id: &str, record: &Value| {
        ryeos_client_base::ui::content::affordance_eligibility(&affordance(id), record)
    };

    // A pending approval the daemon marked accept-ineligible stays visible but
    // disabled, with the reason; decline stays enabled.
    let accept = eligibility("accept", &items[0]);
    assert!(accept.visible && !accept.enabled);
    assert!(accept.disabled_reason.is_some());
    let decline = eligibility("decline", &items[0]);
    assert!(decline.visible && decline.enabled);
    let mut allowed = items[0].clone();
    allowed["actions"]["accept"] = json!(true);
    assert!(eligibility("accept", &allowed).enabled);

    // A failed run offers no decision, only opening its work.
    assert!(!eligibility("accept", &items[1]).visible);
    assert!(!eligibility("decline", &items[1]).visible);
    assert!(eligibility("inspect_work", &items[1]).visible);

    // The decision binds the exact approval through the item's subject and detail.
    let invoke = &affordance("accept")["invoke"];
    assert_eq!(
        invoke["args"]["chain_root_id"],
        "{record.subject.chain_root_id}"
    );
    assert_eq!(invoke["args"]["approval_id"], "{record.detail.approval_id}");
    assert_eq!(
        invoke["args"]["request_digest"],
        "{record.detail.request_digest}"
    );
}

fn view_yaml(origin: &str) -> Value {
    let views =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/views/ryeos");
    serde_yaml::from_str(&fs::read_to_string(views.join(format!("{origin}.yaml"))).unwrap())
        .unwrap()
}

fn affordance_of(raw: &Value, id: &str) -> Value {
    raw["affordances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|affordance| affordance["id"] == id)
        .unwrap_or_else(|| panic!("affordance {id}"))
        .clone()
}

#[test]
fn acknowledge_is_offered_where_the_item_allows_it_and_names_only_the_item() {
    let items = attention_items();
    let items = items["attention"].as_array().unwrap();
    for origin in ["review/pending", "attention/panel"] {
        let raw = view_yaml(origin);
        let acknowledge = affordance_of(&raw, "acknowledge");
        let eligibility = |record: &Value| {
            ryeos_client_base::ui::content::affordance_eligibility(&acknowledge, record)
        };
        // The failed run may be hidden; the approval may only be decided.
        assert!(
            eligibility(&items[1]).visible && eligibility(&items[1]).enabled,
            "{origin}"
        );
        let approval = eligibility(&items[0]);
        assert!(
            !(approval.visible && approval.enabled),
            "{origin}: an approval cannot be acknowledged"
        );
        // The act carries the item's id and nothing the client could retarget.
        assert_eq!(acknowledge["invoke"]["plane"], "rye");
        assert_eq!(
            acknowledge["invoke"]["ref"],
            "service:ui/ryeos-ui/work/acknowledge"
        );
        assert_eq!(
            acknowledge["invoke"]["args"],
            json!({"item_id": "{record.id}"})
        );
        assert_eq!(acknowledge["category"], "Triage");
    }
}

#[test]
fn the_attention_panel_lists_the_whole_queue_and_acknowledges_on_enter() {
    let raw = view_yaml("attention/panel");
    assert_eq!(
        raw["sources"]["default"]["ref"],
        "service:ui/ryeos-ui/work/attention"
    );
    assert_eq!(raw["sources"]["default"]["collection"], "attention");
    assert_eq!(raw["selection"]["activate"], "acknowledge");
    // A disabled acknowledge explains itself; the panel never decides work.
    let acknowledge = affordance_of(&raw, "acknowledge");
    assert!(acknowledge["disabled_reason"].is_string());
    for decision in ["accept", "decline"] {
        assert!(
            !raw["affordances"]
                .as_array()
                .unwrap()
                .iter()
                .any(|affordance| affordance["id"] == decision),
            "decisions stay in the Work and Review views"
        );
    }
    // Both shared presentations offer the panel as a dialog and admit exactly
    // its source and its act.
    let root =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/config/ryeos/ui");
    for config in ["presentation.yaml", "terminal-presentation.yaml"] {
        let shared: Value =
            serde_yaml::from_str(&fs::read_to_string(root.join(config)).unwrap()).unwrap();
        let shared = &shared["shared_presentation"];
        assert!(
            shared["dialogs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|dialog| dialog["view"] == "view:ryeos/attention/panel"),
            "{config}"
        );
        let admitted = |list: &str, cap: &str| {
            shared["capabilities"][list]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry == cap)
        };
        assert!(
            admitted(
                "sources",
                "ryeos.execute.service.ui/ryeos-ui/work/attention"
            ),
            "{config}"
        );
        assert!(
            admitted(
                "affordances",
                "ryeos.execute.service.ui/ryeos-ui/work/acknowledge"
            ),
            "{config}"
        );
    }
}

#[test]
fn the_bars_source_is_a_surface_source_read_from_the_one_queue() {
    let lens: Value = serde_yaml::from_str(
        &fs::read_to_string(
            ryeos_engine::test_support::workspace_root()
                .join("bundles/ryeos-ui/.ai/surfaces/ryeos/ui/lens.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    let source = &lens["sources"]["attention"];
    assert_eq!(source["ref"], "service:ui/ryeos-ui/work/attention");
    assert_eq!(source["role"], "attention");
    assert_eq!(source["refresh"]["on_hint"], json!(["thread", "activity"]));
}

#[test]
fn every_bundled_view_resolves_and_validates_under_the_named_source_contract() {
    let repository = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .expect("canonical repository root");
    let views_root = repository.join("bundles/ryeos-ui/.ai/views");
    let files = yaml_files_below(&views_root);
    assert_eq!(
        files.len(),
        53,
        "authored inventory includes the Surfaces catalogue and session-details View"
    );

    let mut raw_by_ref = BTreeMap::new();
    for path in files {
        let relative = path
            .strip_prefix(&views_root)
            .expect("view below inventory root")
            .with_extension("");
        let view_ref = format!(
            "view:{}",
            relative
                .to_str()
                .expect("UTF-8 view path")
                .replace(std::path::MAIN_SEPARATOR, "/")
        );
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            !text.lines().any(|line| {
                let trimmed = line.trim_start();
                trimmed == "source:" || trimmed.starts_with("source: ")
            }),
            "{} still declares removed ViewBinding.source",
            path.display()
        );
        for (offset, _) in text.match_indices("selection.thread") {
            let suffix = &text[offset + "selection.thread".len()..];
            let is_thread_id = suffix.starts_with("_id");
            let continues_identifier = suffix
                .chars()
                .next()
                .is_some_and(|character| character.is_alphanumeric() || character == '_');
            assert!(
                is_thread_id || continues_identifier,
                "{} still reads the removed standard thread facet",
                path.display()
            );
        }

        let value: Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        raw_by_ref.insert(view_ref.clone(), value.clone());
    }

    // Exercise the same signed parser + extends composer path that produces
    // the effective view values embedded by the daemon. Raw authored YAML is
    // not a ViewBinding: it may still contain `extends` and omit inherited
    // fields, so validating it directly would test the wrong boundary.
    let trust_store = ryeos_engine::test_support::live_trust_store();
    let core_bundle = repository.join("bundles/core");
    let knowledge_bundle = repository.join("bundles/knowledge");
    let directive_bundle = repository.join("bundles/directive");
    let graph_bundle = repository.join("bundles/graph");
    let ui_bundle = repository.join("bundles/ryeos-ui");
    let bundle_roots = vec![
        core_bundle.clone(),
        knowledge_bundle.clone(),
        directive_bundle.clone(),
        graph_bundle.clone(),
        ui_bundle.clone(),
    ];
    let kinds = ryeos_engine::test_support::load_live_kind_registry();
    let (parser_registry, parser_diagnostics) =
        ParserRegistry::load_base(&bundle_roots, &trust_store, &kinds)
            .expect("load live signed parser registry");
    assert!(
        parser_diagnostics.is_empty(),
        "live parser registry diagnostics: {parser_diagnostics:?}"
    );
    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let parsers = ParserDispatcher::new(parser_registry, Arc::clone(&handlers));
    let composers =
        ComposerRegistry::from_kinds(&kinds, &handlers).expect("bind live signed composers");
    let roots = ResolutionRoots::from_registered(
        None,
        &[
            RegisteredBundleRoot {
                owned_namespaces: Vec::new(),
                name: "core".to_string(),
                canonical_root: core_bundle,
            },
            RegisteredBundleRoot {
                owned_namespaces: Vec::new(),
                name: "knowledge".to_string(),
                canonical_root: knowledge_bundle,
            },
            RegisteredBundleRoot {
                owned_namespaces: Vec::new(),
                name: "directive".to_string(),
                canonical_root: directive_bundle,
            },
            RegisteredBundleRoot {
                owned_namespaces: Vec::new(),
                name: "graph".to_string(),
                canonical_root: graph_bundle,
            },
            RegisteredBundleRoot {
                owned_namespaces: Vec::new(),
                name: "ryeos-ui".to_string(),
                canonical_root: ui_bundle,
            },
        ],
        LogicalProject::Absent(LogicalProjectAbsence::Projectless),
    );

    let mut embedded = Map::new();
    for view_ref in raw_by_ref.keys() {
        let item_ref = CanonicalRef::parse(view_ref)
            .unwrap_or_else(|error| panic!("parse canonical ref {view_ref}: {error}"));
        let effective = run_effective_item_pipeline(
            &item_ref,
            &kinds,
            &parsers,
            &roots,
            &trust_store,
            &composers,
        )
        .unwrap_or_else(|error| panic!("resolve and compose {view_ref}: {error}"));
        embedded.insert(view_ref.clone(), effective.composed.composed);
    }

    let surface = json!({ "views": embedded });
    let parsed = views_from_surface(Some(&surface));
    assert_eq!(parsed.len(), raw_by_ref.len());
    let mut behavior_snapshots = BTreeMap::new();
    for (view_ref, binding) in parsed {
        assert_eq!(
            binding.degraded, None,
            "{view_ref} does not validate under the current binding contract"
        );
        let raw = &surface["views"][&view_ref];
        assert_eq!(
            raw.get("sources").is_some(),
            !binding.sources.is_empty(),
            "{view_ref} named-source presence changed during decoding"
        );

        let mut views = Map::new();
        views.insert(view_ref.clone(), raw.clone());
        let effective_surface = json!({
            "name": "cutover-golden",
            "tiles": [view_ref],
            "views": views,
        });
        let attachment = UiBindingAttachment {
            named_items: Vec::new(),
            binding_attachment_id: "cutover-golden-attachment".into(),
            binding_generation: 1,
            binding_digest: "cutover-golden-binding".into(),
            root_ref: "surface:fixture/cutover-golden".into(),
            root_generation: "cutover-golden-surface".into(),
            presentation:
                ryeos_client_base::presentation::UiCompositionPresentation::from_effective_surface(
                    effective_surface,
                )
                .unwrap(),
            project_path: Some("/fixture/project".into()),
            subject: ryeos_client_base::ui::UiBindingSubject::UnregisteredProject,
            posture: Default::default(),
            seat_project: ryeos_client_base::ui::UiSeatProject::Projectless,
            binding_request_bounds: UiBindingRequestBounds {
                max_request_bytes: 64 * 1024,
                max_input_bytes: 16 * 1024,
                max_seat_append_bytes: 64 * 1024,
            },
        };
        let mut core = initialized_view_core(attachment);
        let mounted = core
            .active_view_set()
            .expect("initialized Surface has its tab");
        assert_eq!(mounted.tiles.len(), 1, "one tested View is really mounted");
        assert_eq!(
            mounted.tiles.values().next().unwrap().view.view_ref,
            view_ref
        );
        let effects = core.initial_effects();
        let snapshot = json!({
            "view_model": build_view_model(&core),
            "accepted_initial_effects": effects,
        });
        behavior_snapshots.insert(view_ref, snapshot);
    }

    check_view_snapshots(&behavior_snapshots);
}

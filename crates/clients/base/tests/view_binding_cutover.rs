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
use ryeos_engine::item_resolution::{RegisteredBundleRoot, ResolutionRoots};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::resolution::run_effective_item_pipeline;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const VIEW_BEHAVIOR_GOLDENS: &[(&str, &str)] = &[
    (
        "view:ryeos/attention/panel",
        "6f5bf7f0a853d18bfa440df7427fa22409ed32ac25fc8b01fd778a93373ffde9",
    ),
    (
        "view:ryeos/chain/timeline",
        "e59cace5ea9bad2161617d5f814d69acea1d35af5aa75ea382f0789c1daec37c",
    ),
    (
        "view:ryeos/commands/grammar",
        "6b0a7edfa5f93872ceaed0d0787432528d9599feaedfa7fdd049c34113dc9267",
    ),
    (
        "view:ryeos/development/changes",
        "ff352c3fc907a1677f924811d14a382788ed564aefce1d6b5041e0a5253c8008",
    ),
    (
        "view:ryeos/development/execution",
        "f104477b8e244e1ed462184f28deb7a085d795d0c9dfad318fdff2409143b495",
    ),
    (
        "view:ryeos/development/explorer",
        "d82814eaa1a5ab2657cf22bf5afe9319a6254e9e18b87d8cc9dc2998837f704c",
    ),
    (
        "view:ryeos/development/occurrence",
        "70b16524090c5dab11bdfb2ff76a857638bb4d77060b1d5b2dd1d0dcc93350d2",
    ),
    (
        "view:ryeos/files/list",
        "65e05d1468af70d3fc487d86c7da7ba01cd8c2eae011ebbf2ddf7128f2a014ab",
    ),
    (
        "view:ryeos/home/overview",
        "7f9e904ba4690989053dc1a128ce1a9172437646f3130a6c74c56a113e0262a6",
    ),
    (
        "view:ryeos/input",
        "ac6b07ccd38c56456122f9df214e3b7b83e979bfd78e3885aa602d606922988a",
    ),
    (
        "view:ryeos/item/explain",
        "8f0baace5445cf3520b61af4693745ccb0f4f820f87228517c9aa5f23a67e10c",
    ),
    (
        "view:ryeos/item/inspector",
        "61265ee7927d7534c96076981fa9dd6cb961e4ad55a56f86348b9cd559209430",
    ),
    (
        "view:ryeos/items/space",
        "cbed664b8e4e23705d3b41bd881b9c1e8351740ed99cb212d64975d371de1fb3",
    ),
    (
        "view:ryeos/node/bundles",
        "3adfd99891dc60dddd13767784da38e31813e9cf22493afe12db9b309e14f673",
    ),
    (
        "view:ryeos/node/events",
        "5609473f16a0a57f429656c64050061d4a59652cf73f555899f823e8fc69cb99",
    ),
    (
        "view:ryeos/node/gc",
        "1920644d8c5a16c5566d4eb041d4c01e92eaaf24535ab03c03a4aff307f245a0",
    ),
    (
        "view:ryeos/node/machines",
        "53c5b9844505443986c5ea7657fec834368dcac414513457a9ca8be8b4bcfb7f",
    ),
    (
        "view:ryeos/node/remotes",
        "b41fed50260fc785b613d5693093a80336ad06e864c9b2148c276d8a75426c04",
    ),
    (
        "view:ryeos/node/status",
        "d13ae92ce50c7e236b38ab0346066377267460982f431cdfedf6019c0c50ec64",
    ),
    (
        "view:ryeos/node/threads/history",
        "b2d600dc0edb853d26366cdbf0c2eb2a223b5f577b86acdd127de4bf8e12b11c",
    ),
    (
        "view:ryeos/node/usage",
        "2df5e36bef882e141c0b61fd9d1e2b4dce61e9b481f74097ea31616c3d75aca6",
    ),
    (
        "view:ryeos/programs/list",
        "5c484740917af6d2ae59372fe67e8463f5ea4ad80a3854f26317f7852c83bf57",
    ),
    (
        "view:ryeos/project/document",
        "f8ae10fb6238c2023bda383bcccfc6c05a02528d8658e028ac52ee61a6440cd1",
    ),
    (
        "view:ryeos/project/files",
        "c6606ea0d3d2d0e35079372763f343e7e1ae2a281aa9dca9c6f72fec17046bfa",
    ),
    (
        "view:ryeos/project/items",
        "5367c34a30467f1d3c6e9c223cbff5a64bfd713a145657b3caba7778adee9b9e",
    ),
    (
        "view:ryeos/project/directives",
        "618d2d9142acbdd0735f1ead9cf52a7aec1bf5fdf42f263270eab92ed13942ba",
    ),
    (
        "view:ryeos/project/graphs",
        "85e8e71d2537497eb3d73e6035ede7e5dc3177c0ea0b606c08446736aa201c13",
    ),
    (
        "view:ryeos/project/knowledge",
        "e977f7ec56e77f5be206269b6c80da4931f21ad91abc1af859e86e4a3f0ad6e7",
    ),
    (
        "view:ryeos/project/schedules",
        "56199dd5a3c02aa1e1887b5930744b24952ff142736995dc72452ac108e80d61",
    ),
    (
        "view:ryeos/projects/list",
        "aff53ed821d3dbebfc24ab059d26a4b138dea26cbc56cfe9fae5b95636305c5c",
    ),
    (
        "view:ryeos/review/history",
        "f7ba9bc61efd765d224af4e373a925f3b28573d0beffc64d3fb87c722aa12572",
    ),
    (
        "view:ryeos/review/pending",
        "2c195149b60c5cc07fd21347bda4c15748ac7c04498a810487eff175d56b1216",
    ),
    (
        "view:ryeos/runs/comparison",
        "094aff6e970bf9b1435beb0f48f9b372b576701f74de41db4fb0fcb4e2497c89",
    ),
    (
        "view:ryeos/schedules/list",
        "f0a630508f665534ea1008ab11900b9291d8e15cda5db9aa1b2d7e885d222082",
    ),
    (
        "view:ryeos/sites/list",
        "180f449aa25dae11d96188a5d0a6a06b925d3d3ea3a11c5f87cafa9b5c323604",
    ),
    (
        "view:ryeos/surfaces/list",
        "5093819b1e1289a50fc6df7d4ad063948b06c4affa1b6220b7b7f5a9b5342b5b",
    ),
    (
        "view:ryeos/thread/conversation",
        "81ec45d6a78960d7a6672e2f37679398d5277d1d1bfd0c39cb810cae3633f9d8",
    ),
    (
        "view:ryeos/thread/transcript",
        "95f894f61e93675c519031d865b2295832a84ff690ae62de7672d06bb071cc0f",
    ),
    (
        "view:ryeos/thread/tree",
        "2d05638f6700dba607bde28d83e7645b9f8b8592921a3857ac2a290748ea9208",
    ),
    (
        "view:ryeos/threads/detail",
        "01f46b7a70b7b2973270165e29e7906f861dcda17004881fa703bc0d5894246f",
    ),
    (
        "view:ryeos/threads/history",
        "efebb42a562725ecd500b1e41de69b4025b64b1b937589be0ffe25bb1980b128",
    ),
    (
        "view:ryeos/threads/list",
        "5e302817697692884d250d28384f7c0c8cf65b3aab3c81e571ed17eae70b2248",
    ),
    (
        "view:ryeos/ui/launcher",
        "3e2e3382b366437ff9aae674a6b42e4520f6929953ed6091de6a6425e5820a27",
    ),
    (
        "view:ryeos/ui/session-details",
        "18c66da373e22e7aecb84be4acdf6e3c47014ee96b2f435024e88e03ef7752a2",
    ),
    (
        "view:ryeos/ui/shortcuts",
        "60dd115e32021b497ac6b9dac67cb784d95d6e5cebd8522b9b251646e67b2c8e",
    ),
    (
        "view:ryeos/ui/status",
        "ee9958aa7905c9cd6bd82bc865da07949546ebfcb6036175655c4fa688b32539",
    ),
    (
        "view:ryeos/view-sets/library",
        "d875b425bd728c66a8ae95558fa12b15a052e19a6f438b8361f672f438851818",
    ),
    (
        "view:ryeos/work/approvals",
        "cc8c99c3ec8a93a5a72232ef48ce95ae1a841f6e32cf3bfb5a30e59c88397514",
    ),
    (
        "view:ryeos/work/candidate",
        "3951453c1339d9fe81582d38181942f578c4eddecc68465c5a7bdf2f9b768155",
    ),
    (
        "view:ryeos/work/children",
        "f89c8fbb023515e64e8cdcf40a0ce762dfb649fb8c79992ad522a52614c3ed0f",
    ),
    (
        "view:ryeos/work/evidence",
        "e1d669dd69af03ec65ed70ae49629b6b1e2049efef05f9037a25e80efda9a35e",
    ),
    (
        "view:ryeos/work/list",
        "6c15d8600e9da37ed8954ada31a26ad7dc8556aa579f6da788fd58ab712949ab",
    ),
    (
        "view:ryeos/work/overview",
        "68b9ebbaf1ebc63a186e74271e5d787e5864800bec8822dd1683a6591cf88d2b",
    ),
];

fn snapshot_digest(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("serialize view behavior snapshot");
    format!("{:x}", Sha256::digest(bytes))
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
        "subject":{"kind":"projectless"}, "posture":"observation_only",
        "binding_request_bounds":attachment.binding_request_bounds,
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
    let mut behavior_digests = BTreeMap::new();
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
        behavior_digests.insert(view_ref, snapshot_digest(&snapshot));
    }

    let expected: BTreeMap<String, String> = VIEW_BEHAVIOR_GOLDENS
        .iter()
        .map(|(view_ref, digest)| ((*view_ref).to_string(), (*digest).to_string()))
        .collect();
    assert_eq!(
        behavior_digests, expected,
        "per-view behavior golden changed"
    );
}

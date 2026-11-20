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
        "b09b0f3238d4e21b7f173a88d0aa0165c9b38059228c142d3675892d081b72a2",
    ),
    (
        "view:ryeos/chain/timeline",
        "1dc7e557524e397f27d23c6ffd961fe2ec1b634dd3608c8058c7a4b1f11a55d5",
    ),
    (
        "view:ryeos/commands/grammar",
        "7ec67da06778753c34891b05ca07e43d9fa97183eb2c38fc75daa9134c72c5de",
    ),
    (
        "view:ryeos/development/changes",
        "da20cc34be836290726db713dffee3069f7d3e47dfad54887a33702e2d66d04a",
    ),
    (
        "view:ryeos/development/execution",
        "fdf23e6ddc0b0eff62b8695eaf1d16d02591eb30adfc5c7ac42b16a2ff91d9f8",
    ),
    (
        "view:ryeos/development/explorer",
        "65d0013f42020df10c273f7dc6f9bf9c6c058823d0ba1853fb656bfdb29c9532",
    ),
    (
        "view:ryeos/development/occurrence",
        "e9205758ecd4c770d7305443007efdf162d92acbf732e6b073639e03d03346f8",
    ),
    (
        "view:ryeos/files/list",
        "3d945dd4bdf7911463cdd9a880a34b2e856465a4c4b6d3521288676125a2311d",
    ),
    (
        "view:ryeos/home/overview",
        "cc736369e333185a2021aaa82e5b35550d7dfc9b962d5f0db4a542e0cfea1036",
    ),
    (
        "view:ryeos/input",
        "2a350e491f2c749271df60c4b4f806c799ce3910b10fd9cc9fa4d535b6d8ab32",
    ),
    (
        "view:ryeos/item/explain",
        "3c711232727cae84a593bcde9cdedae93990cfb5f277bf51a426ff8a3e682501",
    ),
    (
        "view:ryeos/item/inspector",
        "4ef90d00368dcb5ccc75e84b86dfae55293a1eae5766ae75c8f993bac4c6cf81",
    ),
    (
        "view:ryeos/items/space",
        "3e8758e9b9335562959102f062c2c6af0b54506a01dbd4c71aaf86501e4b1511",
    ),
    (
        "view:ryeos/node/bundles",
        "59e6c9128ea423611d05c3f998f05eba766bdcd5b0b469281069ead172c28b7e",
    ),
    (
        "view:ryeos/node/events",
        "764fb72e58c586bc5510a2bb468226fa67aab7611855391d55e2129a9edb3e3f",
    ),
    (
        "view:ryeos/node/gc",
        "6e227f2ed93537fe45dc1b56b5821265994d76b49e6538b2b9ca354aea6cb10f",
    ),
    (
        "view:ryeos/node/machines",
        "d19d79897cccc04fe7034f90c8572586df5a70089a33866b359bfa38c587c36a",
    ),
    (
        "view:ryeos/node/remotes",
        "f0e737543fa91110e7c658d6e00d8da5fe42458c981b8fc65161360ccbe1579a",
    ),
    (
        "view:ryeos/node/status",
        "a51b01936a972367f0fccd3f55f9075be7ccd1c1ed80fec3b5b41cda50b8559c",
    ),
    (
        "view:ryeos/node/threads/history",
        "6ad06e64605da7d2b714a6eca8b8767b7222678a1d84e721452dccecc4a6b170",
    ),
    (
        "view:ryeos/node/usage",
        "c662388baad3471883361e623774bc355098ca0984445d683a4e9c932ad2db8a",
    ),
    (
        "view:ryeos/programs/list",
        "37ccc93c450ba64b87f823205f1b4f53ecbe33dea8549f0d5ffb9f546e8a40dd",
    ),
    (
        "view:ryeos/project/document",
        "dbeee410b83f9782b41393a3b15a6994f68d141f52037df0059a50da0daefd56",
    ),
    (
        "view:ryeos/project/files",
        "e69f039ec3c5bb27363336d66b50d1eff84064ee46bed545d78499cb5bffc5b8",
    ),
    (
        "view:ryeos/project/items",
        "f8bb6b4d67a305f407267855652ae3803e2519213da36b99eb20451f5babff38",
    ),
    (
        "view:ryeos/project/directives",
        "18155b3897d98e8092a1e0ae4b8593403377095b932fb83aecd9ffe62c990b70",
    ),
    (
        "view:ryeos/project/graphs",
        "a7a6b14c63a0ce007858c1016969f8822eacbb07136d9f51a3619523e06cbac0",
    ),
    (
        "view:ryeos/project/knowledge",
        "7c1a12311b4f08a46ef6c3752cda67e3cfa18e88243f4aa401f63c591a841f42",
    ),
    (
        "view:ryeos/project/schedules",
        "a2e3102dd3bed51e72bb4650c9bafa5564cd3fbe6384a230a328953bdc2b9bd4",
    ),
    (
        "view:ryeos/projects/list",
        "1545a04ceea78affe3f6fe5629cc6030f21b8e8e817a807e5c82582cdb206898",
    ),
    (
        "view:ryeos/review/history",
        "ee7cd25cbb3f27315480f79422c269921eb0cd65811ad4e2af069eeecb8969c7",
    ),
    (
        "view:ryeos/review/pending",
        "42785ec0227598415eaac841b6fc865f42078c1be5a00fe9dba21c4bf1c72bfb",
    ),
    (
        "view:ryeos/runs/comparison",
        "0224a63132be6e8861f9894c1a014fb2c8d4e09730493c1b0f3cd407439ffe85",
    ),
    (
        "view:ryeos/schedules/list",
        "ac0aa47259403c1bd64627663c07e0807ba0a2664e62c28479e19063b86b1ebc",
    ),
    (
        "view:ryeos/sites/list",
        "57dbf1eb67c7163864434419ba6df9bb3c55ae86ac1fe6b0d3dc922beb26d8f8",
    ),
    (
        "view:ryeos/surfaces/list",
        "add4a6f21d09658d26bfa52321e15294ebbdf473d9987d61f64778064ceb758d",
    ),
    (
        "view:ryeos/thread/conversation",
        "99b81e5ae071ae9e20849fa4e02323880a3c8d070a43bbadffe4c644625b5a96",
    ),
    (
        "view:ryeos/thread/transcript",
        "96055403dbda276de1508528aeb5f4fc2122eb72e52fe540aba8b31282d2b890",
    ),
    (
        "view:ryeos/thread/tree",
        "4cc371ae27636b531178b9a6bebde858dc6fe0deeed5634a6e632f552fbf9326",
    ),
    (
        "view:ryeos/threads/detail",
        "08c7f4cec18431ce533a70a4801f78ee89ad44d32f168e3c430ed6709c1160e1",
    ),
    (
        "view:ryeos/threads/history",
        "7e5c6dd58f456007614431e6778fb1ccc4fa4d5dfe9196b8e833399567c7d1e4",
    ),
    (
        "view:ryeos/threads/list",
        "b69b89b12aba1c406fb70ba77abc73d147b78a0fe6899c121d94baf7f56bc502",
    ),
    (
        "view:ryeos/ui/launcher",
        "951c06f05c970db5ea10a003fe3677efe1fbae948c87b30e01547519608f2bf8",
    ),
    (
        "view:ryeos/ui/session-details",
        "1d21ef4113788bf9de54d18de02e35ff6317b68886b6a0ed7cf0f6ed913a9b8b",
    ),
    (
        "view:ryeos/ui/shortcuts",
        "60aaede235a92bf0c808ddc9566107f9a0c028066dd5c575878fa8ce11b0ff0a",
    ),
    (
        "view:ryeos/ui/status",
        "979324b637161e34bf3f6c77bca59a506bb94f7dec3d743dbcb99748209847b2",
    ),
    (
        "view:ryeos/view-sets/library",
        "ccd0b4adfd27966f6d6465ccba2c0fa7ce230f21c7ae449a69a1bc7ddde2a692",
    ),
    (
        "view:ryeos/work/approvals",
        "add820160863f94d62540e967ed902bd5fb45e9335d874dd09f4c135eca72d2e",
    ),
    (
        "view:ryeos/work/candidate",
        "629dbdfc3a6dba7939963ee233161200db746a78e230425d9769d0684e65214d",
    ),
    (
        "view:ryeos/work/children",
        "227c9e750c8097ce61170d72efdc91c942cb8cca474904d92279c78f4c416205",
    ),
    (
        "view:ryeos/work/evidence",
        "875ab1491de3915825a7c440dd49b868db9bd4e18d6a3b6568fec787ef142321",
    ),
    (
        "view:ryeos/work/list",
        "a6f33249bd8c5ad5413188cb763641e2190acbe18fa38522410bb66094d92523",
    ),
    (
        "view:ryeos/work/overview",
        "ac3da13ebcfd2a626b8baa1ddaca50acf561b169312986cf8455383630d64fa1",
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

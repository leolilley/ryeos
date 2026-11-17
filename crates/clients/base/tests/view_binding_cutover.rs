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
        "view:ryeos/chain/timeline",
        "2a43ab417a74a535cca93ad78afabaa486037b836b264626b2e85a28886c585e",
    ),
    (
        "view:ryeos/commands/grammar",
        "94eb25644ddd5cde641cca0f2d99f22ff6504df29b6518ada9fb8ae174d6b1e6",
    ),
    (
        "view:ryeos/development/changes",
        "a43a841cab0623eabc740ad507a29011b85e6a40d29bd728c844d73c49e26748",
    ),
    (
        "view:ryeos/development/execution",
        "9ce382f4afed2f4b3b778826d7e86ee112df975800e97cd0c208b27108764f73",
    ),
    (
        "view:ryeos/development/explorer",
        "7d22293dd6cb767817f22fce90857b75b6694a07a0f6dd1e0cdb64f0d4c40c25",
    ),
    (
        "view:ryeos/development/occurrence",
        "43c41de498970891fec296d4318dbd29dc7fca992d31f486f97d85ee407b8ea8",
    ),
    (
        "view:ryeos/files/list",
        "44892d737aef9f103f786b447e6991291a2f2e781683526f8f76a26d534607d5",
    ),
    (
        "view:ryeos/home/overview",
        "7bee4bf0bd63749a10249d4c29af93c7b4bc4b4e151c1391e2d77176c4e3488e",
    ),
    (
        "view:ryeos/input",
        "bbf195abb5764dd11392774a8d46af5eebdd506c8a047ed7003ea4f59e60a17c",
    ),
    (
        "view:ryeos/item/explain",
        "e3175864b67b09bf43b74095e0906a2462abd099f524eec3ce4f413794c81e2f",
    ),
    (
        "view:ryeos/item/inspector",
        "5859747c4d8c82434d28a8e98114ff697d632f812c1f0e321049936303171936",
    ),
    (
        "view:ryeos/items/space",
        "6eca6e100a296439c453869af7bec602249f7a945d0700e8fe12538f41bda485",
    ),
    (
        "view:ryeos/node/bundles",
        "ca17bd7c98944a5e0822b545e44afdc6d24f588551457c76e75b734b7e6efb90",
    ),
    (
        "view:ryeos/node/events",
        "2d0dc2e761a99f230cfb338efa5126682984ea9261939a8628852cd69539b86a",
    ),
    (
        "view:ryeos/node/gc",
        "2176a0c1e7d2f744bd62aeac898e43eb32cc7bda08c0b2b45d42ab74dbd3bead",
    ),
    (
        "view:ryeos/node/remotes",
        "18c80f4430a192d3fc85c018fdff5b13590d84bfba248f328f8b6e955a7c5758",
    ),
    (
        "view:ryeos/node/status",
        "b110962008fd54d7ee01f4c834ea3cd9f38e747bf5cbbd8279c546c39c37fee6",
    ),
    (
        "view:ryeos/node/threads/history",
        "828d0007c41a100f87ee56b6e878cd9aa1b57e192b242070d2bc61f0abf25fa6",
    ),
    (
        "view:ryeos/programs/list",
        "2f31a9720d68e200c187d62b6df414cc914fdfd9ccced3aa2fdbbbd0c58ad0e4",
    ),
    (
        "view:ryeos/project/document",
        "18e69c81a05bc70108671e5c6d1cd09a5532229f346a835b850c08ba36df561b",
    ),
    (
        "view:ryeos/project/files",
        "cbd65618f9477ab5dcea5e2a356d269ba972e690580b67bb9da14d02671d28cd",
    ),
    (
        "view:ryeos/project/items",
        "e6625c75debba23f153ebff394057b5d7b83a87883231d3de4b1593864309882",
    ),
    (
        "view:ryeos/project/schedules",
        "9eb91ca4a11c779339db5088b04fdfdc524d038ed1250499a799762919d9d206",
    ),
    (
        "view:ryeos/projects/list",
        "629918ea41b04170455fdfdc62b94401fea831bc391240915dc61435ff4f8f85",
    ),
    (
        "view:ryeos/review/history",
        "85789c752763f3161710db26570228386c4388982d88d5786e2602d00b806326",
    ),
    (
        "view:ryeos/review/pending",
        "e86b754d17bdffd5897f5bb834dddd804abd532d41303178b4b6480c6c56c0ff",
    ),
    (
        "view:ryeos/runs/comparison",
        "62bfd13b0f419906664e9bb9b69f0ceafbbee0be8962bd7b59b68bd2273fe060",
    ),
    (
        "view:ryeos/schedules/list",
        "232a4f85c651cfaf2a4816dfb08e32bb328579e61e6d6d27e73c32b9ea37ab19",
    ),
    (
        "view:ryeos/sites/list",
        "cf83ae58f2669c6155a2785313b135b56c3db15f9a538007d48d25a0fa24a65a",
    ),
    (
        "view:ryeos/surfaces/list",
        "c1577eb5a573e8a7cd95e1750924bda6e26e6960131621c7032ba2b25bd4fdc7",
    ),
    (
        "view:ryeos/thread/conversation",
        "1f14ab97c751055c4de3ae8a4c1eed5bf38bf4eea32a7d37c500c7e1143e7fb4",
    ),
    (
        "view:ryeos/thread/transcript",
        "4422fb1fb9ecc87d744b9f1407ab0f74bad29b359a87560e53fd5c0cf892f3e7",
    ),
    (
        "view:ryeos/thread/tree",
        "199154dd914e77d51282cd682a004138b584b3b2cbaffce7b6e49b421df4f23c",
    ),
    (
        "view:ryeos/threads/detail",
        "5104802d2a4cdd5f88f044b7efd300169542611d38af9468b10a2178be828dad",
    ),
    (
        "view:ryeos/threads/history",
        "9b08d6d9d1f8f24cffcfda02f4275425dc7c7c1301275a77f0a217a0adea0ca1",
    ),
    (
        "view:ryeos/threads/list",
        "0f9481b34612e1fbad5f428cbcc7792bad93821ad04083d58493d79ff4e86d82",
    ),
    (
        "view:ryeos/ui/launcher",
        "f7e4fcb4e4f78f0227bb79b86cac42367e65d13cd0de703cc3dcca9aa04a41e9",
    ),
    (
        "view:ryeos/ui/session-details",
        "b4608a36ad9aaf4a72a6032c73fcb4e70ff87dabfbda2422665577785cec49b1",
    ),
    (
        "view:ryeos/ui/shortcuts",
        "a83a1ac1fddb9c30c6dffcd6c8badf12d06cde215288099556c076637b7468d6",
    ),
    (
        "view:ryeos/ui/status",
        "b00165804aac4677f15980a2cee350bd5b66d08a1e35ba7aa2786895ad8f9bfe",
    ),
    (
        "view:ryeos/view-sets/library",
        "3adc9c13177b4b60984e07ee51da4be3b7a3edced6f8d875e5ddfe5f3d0a8091",
    ),
    (
        "view:ryeos/work/approvals",
        "97841c736084640aaed19259fda4c7b83bbd073fb0733a44850e2f895c8d7148",
    ),
    (
        "view:ryeos/work/candidate",
        "1f0f6e1762bc4a405cb4f3ebad4bddf091dccb1d15bb266bfffbbb2d2d8b886c",
    ),
    (
        "view:ryeos/work/children",
        "cf2df76bc56eac5cf1f70399e2b9553297e509e8b1522255fff286bd81916876",
    ),
    (
        "view:ryeos/work/evidence",
        "b8c58c7eca81fddd33a3f69c16b32f0de5fb0d7f9da9d90fddb80ba3262189d1",
    ),
    (
        "view:ryeos/work/list",
        "3a3d12144d9335c9cc8cf2b9a428da2342a0bb7f7f9e30deac59b619e3f97a66",
    ),
    (
        "view:ryeos/work/overview",
        "51c5297a8eea21ff55ad64eae7be08220cdc92595f01616850be8b3f8f551035",
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
    json!({"attention": [
        {
            "id": "approval:A-1", "kind": "approval", "title": "Approval requested",
            "tone": "warn", "state": "pending",
            "subject": {"chain_root_id": "T-chain", "thread_id": "T-place"},
            "detail": {
                "summary": "command_execution", "approval_id": "A-1",
                "request_digest": "d",
            },
            "actions": {"open": true, "accept": false, "decline": true},
        },
        {
            "id": "failed_run:T-head", "kind": "failed_run", "title": "Run failed",
            "tone": "danger", "state": "failed",
            "subject": {"chain_root_id": "T-chain2", "thread_id": "T-head"},
            "detail": {"summary": "directive:x"},
            "actions": {"open": true},
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

#[test]
fn every_bundled_view_resolves_and_validates_under_the_named_source_contract() {
    let repository = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .expect("canonical repository root");
    let views_root = repository.join("bundles/ryeos-ui/.ai/views");
    let files = yaml_files_below(&views_root);
    assert_eq!(
        files.len(),
        47,
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

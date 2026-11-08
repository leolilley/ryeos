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
        "610cf228de34cf2f05623cc13981bd23c220ff3290baecf38d08827042e3014b",
    ),
    (
        "view:ryeos/commands/grammar",
        "e60559b82661a01433ca2641a1169e1a41d743668166c27a5cbb815a92315468",
    ),
    (
        "view:ryeos/development/changes",
        "edd2c6e16a79c67ca517acee005dd4adb1c172d401b49d1c6341d7545fa78661",
    ),
    (
        "view:ryeos/development/execution",
        "dfbeaeb22706b8f40b6d09b7207ec03ae8391ddbab77a1ceb16b5f5f80da3e09",
    ),
    (
        "view:ryeos/development/explorer",
        "ad41addc72ca14b6345b128fc65e01e98e4c4b6a6cee6d24a46392c24b74d08f",
    ),
    (
        "view:ryeos/development/occurrence",
        "18e0821fb71ca1b4a195761422f2c58de0114c740849b12e9cd1f3ab36d815f3",
    ),
    (
        "view:ryeos/files/list",
        "ffc9ca80d3fa05620b19f08142eacb40fef770c2261965eccb8e7a1d65f158c4",
    ),
    (
        "view:ryeos/home/overview",
        "e4d39494556688fe853d5198676a25385f2c3fbbe9652b258857d0881f668e8f",
    ),
    (
        "view:ryeos/input",
        "f1e18aead35e2eecb646fe7b7dad1c9489434763ba8162b5c449b0f3fa623ce4",
    ),
    (
        "view:ryeos/item/explain",
        "9b486eb5b113a91ece92473cb44705a56f1f1ca9ec78ec48b1a56341f84ef019",
    ),
    (
        "view:ryeos/item/inspector",
        "180dc39ad6c7f75825e017ac8492745fdd8fbb64bf41a54c8dda6c1405dcbcc6",
    ),
    (
        "view:ryeos/items/space",
        "e0220b733614fb3a25d6a1d8e48695d012bc528f6b9b64090b3a9cc4ecce9197",
    ),
    (
        "view:ryeos/node/bundles",
        "5bb023437191a6f9603c86105c91c4ad4e2bc859a96837c089aa0b1972411829",
    ),
    (
        "view:ryeos/node/events",
        "f74336fc9d665c3d38be1a10813c765d359c9157e7ef43c7ed2c3e1419df2450",
    ),
    (
        "view:ryeos/node/gc",
        "9e0f0f3494a25ddf0ddfee8894357fb7a9d15742619f378c32f7ce669dce5394",
    ),
    (
        "view:ryeos/node/remotes",
        "e2e8397e4885f09b338a57a4c1de6584f2dc7da1c46cde23b36d91fab1a6f5b0",
    ),
    (
        "view:ryeos/node/status",
        "c0259606f85f5b0b62ab37787c5d8700592d37219c014c7a37c34db7ee6f5483",
    ),
    (
        "view:ryeos/node/threads/history",
        "ef650329aa6b8e43f3d3b6644152085daac0bec7396315abd20f2cf84aa30bb5",
    ),
    (
        "view:ryeos/programs/list",
        "d81c4c116405ad850ee6a165fd172edf18fa097c3c10d46add1e45b0efcce816",
    ),
    (
        "view:ryeos/project/document",
        "14a144ed67cebfd3f4263041b53809715efa9f4e29845f522d15576974a8f1cc",
    ),
    (
        "view:ryeos/project/files",
        "64984e488f7259fac2f1aff598b83824a1437873e76f9e8187f24b8f91aedf91",
    ),
    (
        "view:ryeos/project/items",
        "2d7fd3544cf258c44f6ec74a3daf2c0e166a65205b206b566200ac0f573984cc",
    ),
    (
        "view:ryeos/project/schedules",
        "b9ebcd53387915f025c8192f89dcdddf941467f63a20ec063f31f8485a99217b",
    ),
    (
        "view:ryeos/projects/list",
        "cec63d2d8854c48198d59b049d95a6af615ea8a334b445b48029246cab14a2a9",
    ),
    (
        "view:ryeos/review/history",
        "dc5bda39edd8856138f3f72521c373cb67fc8e698b0a0486d4f576756867fdd5",
    ),
    (
        "view:ryeos/review/pending",
        "30167a99ec169b96a74b67d773eab9f95620946311744c04bab96a92afe77e49",
    ),
    (
        "view:ryeos/runs/comparison",
        "b6a45077f686d1bec3586a7a6ae1cc63be17a577e1ba1583a67120bef3ed9426",
    ),
    (
        "view:ryeos/schedules/list",
        "056d3e932db57ed0b87ab5d148002b9c49b86c6a49e011465488d6f7c8f4d6a1",
    ),
    (
        "view:ryeos/sites/list",
        "ebb2e4d4abb8f408ea78ec9af8aa56d1b5ace713ad71478b48cc0f5d31620d70",
    ),
    (
        "view:ryeos/surfaces/list",
        "18a959f2638bd2fa04c6de16716ddd102edd240ebc639694c64e7a7cdc89d51c",
    ),
    (
        "view:ryeos/thread/conversation",
        "59d8e8fef5adbda3e142a8430789961ad1e226c97b3f13b193bec6f0c2f8de81",
    ),
    (
        "view:ryeos/thread/transcript",
        "72ca6ee35b3befba9eea0bc8ec4fd8fcde02c700b20d9274986bc65b5b12f126",
    ),
    (
        "view:ryeos/thread/tree",
        "6521a672ce1fb5d5deb9609651066fa4edd4b763ddbde56cb171d9e6490c3ae6",
    ),
    (
        "view:ryeos/threads/detail",
        "9c33df1467aafd9c8669ac0ff3a13ab129221d358c3cca273340da226b7ac369",
    ),
    (
        "view:ryeos/threads/history",
        "9defb9eb4b3a5504e46a8a1c4ee6954116ca1fa9b5c10636e3c7b9aa1651c608",
    ),
    (
        "view:ryeos/threads/list",
        "b72f5092b1be79d11e03b8b153d00895e35a0162b24559b739cd195aa4059f75",
    ),
    (
        "view:ryeos/ui/launcher",
        "14d29829c7ea61d193420021024e3904309ac53dcd7e63013d1d169b37bfdf8c",
    ),
    (
        "view:ryeos/ui/session-details",
        "f4c1862bbf76bdbb900815c1200380ee10d295376a98a4acaeaa0392aa45c1ea",
    ),
    (
        "view:ryeos/ui/shortcuts",
        "240c3a4a073235fc9af093ab8cda957b61cb7c520a904c456c94c49d3d89a766",
    ),
    (
        "view:ryeos/ui/status",
        "c7d494547d8365f3e424ca5ff8cf598c75ceab22c8a1b4b286245da694dbc2e6",
    ),
    (
        "view:ryeos/view-sets/library",
        "910296c1620d044f1b13feb4e8f4ed5d57327b064505ab92f1a92b28e7ee8f89",
    ),
    (
        "view:ryeos/work/approvals",
        "e13c87f2b9ece1d29acc268dc17bf09d4ae2fb62bf28105d647d0684fc536459",
    ),
    (
        "view:ryeos/work/candidate",
        "aa08a8a5fc1449c9e32b8a4433cbf7d4d284b36b89b26e07f5f79d5f39773e29",
    ),
    (
        "view:ryeos/work/children",
        "f60d2eda767c671dbe45567842e1eec75a43b29ad8722e570c4be64ccfccb85e",
    ),
    (
        "view:ryeos/work/evidence",
        "940e4410a206bfe74c3f41db689616b45795795cfd72ffe3543708e7764b6ee2",
    ),
    (
        "view:ryeos/work/list",
        "5a90d90407323b22d4b2f0d81cd09ba8d288d0f3e7aca3be866a6eb9d10a6882",
    ),
    (
        "view:ryeos/work/overview",
        "32b513805494c4b9f215667b86aec382fe1256eb476358a0972f7c9dea0bc5b4",
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
            "category":"ui", "shared_presentation":{"schema":1,"title":"Cutover",
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
                name: "core".to_string(),
                canonical_root: core_bundle,
            },
            RegisteredBundleRoot {
                name: "knowledge".to_string(),
                canonical_root: knowledge_bundle,
            },
            RegisteredBundleRoot {
                name: "directive".to_string(),
                canonical_root: directive_bundle,
            },
            RegisteredBundleRoot {
                name: "graph".to_string(),
                canonical_root: graph_bundle,
            },
            RegisteredBundleRoot {
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

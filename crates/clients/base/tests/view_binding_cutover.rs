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
        "view:ryeos/atlas",
        "80eba56b117c83af55cc78fdb047773e5804276ab04972eedb15a0574c124618",
    ),
    (
        "view:ryeos/backdrop/prism",
        "8d31482abe34ffb705b0a7ec0cb1a1d1c9baa3560b7952a01b795c8049789143",
    ),
    (
        "view:ryeos/backdrop/prism-shards",
        "09fc98ddf9174ad944520778fb09d547f9bf7b1ac06b942a72c032ad61c9ce69",
    ),
    (
        "view:ryeos/backdrop/splash",
        "b709361e129453a8adc88ff56fb7ffb818fc14e540407fcd238764fd36920adc",
    ),
    (
        "view:ryeos/chain/timeline",
        "b2e0ce2c296c3e28dad6778b9344a8e6ce02f596555777c94b824926184e6ef1",
    ),
    (
        "view:ryeos/commands/grammar",
        "eec96a7a4652707ad2f775e6a4d46b63ffdf28b7d421db55e5ac7e167ecce4d1",
    ),
    (
        "view:ryeos/development/changes",
        "8d36dcd508e89809769ab148b091666388e6311c8fc13197f2f8bc64ebebccce",
    ),
    (
        "view:ryeos/development/execution",
        "9af7000270b81cfe52f711bf255bd31f83ac94637749995f75b5a6000c7d7758",
    ),
    (
        "view:ryeos/development/explorer",
        "7a58af22a59917eac018b7be7f4d4962a1441787610fed354452d001db2e2b80",
    ),
    (
        "view:ryeos/development/occurrence",
        "50b6c786493d17c56b9d37e64a4630be77b8c6b47179f96ad6d78bb783f73ef3",
    ),
    (
        "view:ryeos/files/list",
        "348daf047a0f4a012c2c52103a5dd942d821d15d61abc7916e67f9bdd7fc2b7c",
    ),
    (
        "view:ryeos/graph/topology",
        "6f13f5075bd2dbb94f113d1c9020600f0e75b4ef4cc951b8b63ac632338a0788",
    ),
    (
        "view:ryeos/home/overview",
        "abaf1cfb49ba7e9ec439a5f923cb253c2399fef7a54a2a2345200684fd597195",
    ),
    (
        "view:ryeos/input",
        "be0e9312fa0510c49a1285d7f0f2a7f5ab1431c24e3759ed458cd44933617115",
    ),
    (
        "view:ryeos/item/explain",
        "73598c8eabc70bb9579428bf290a9e2ed1e186df4d43aed96560fc0895bad5f7",
    ),
    (
        "view:ryeos/item/inspector",
        "d3855bdf05e58d5e06f39ad38df259f7cb7870c3714a87d8e711e302e7745e2a",
    ),
    (
        "view:ryeos/items/space",
        "fdae0d8a644a9052c6a42207ec3d7b3c964a92e574a0d7e4d5074a6796b4e86d",
    ),
    (
        "view:ryeos/node/bundles",
        "8d99bf9f6572b1f071c75b1a9dd1dabc61bd5f66f50bb5fd305684eb9aad2e0d",
    ),
    (
        "view:ryeos/node/events",
        "ae507364701c8f75c6ef69adcdf4b72c768ebdf3d8de6b522945dc85425ed938",
    ),
    (
        "view:ryeos/node/gc",
        "f9f806b783070cd32be81b961ba1ede9fcbf16b2c6381b8ffd94cc20500794d9",
    ),
    (
        "view:ryeos/node/remotes",
        "aab45a29779265758bf94bc9a39a1669fa3c9aadab53af31061f3e3fd6cd6d7d",
    ),
    (
        "view:ryeos/node/status",
        "ed7d1c6bcda03df3fd787b1ba69805d601012f2c0498e0df1678f56a6feec888",
    ),
    (
        "view:ryeos/node/threads/history",
        "a17c4332fc90abf54a757fdb78287ecd1f8ceb5aba50441d2a2d6bfd94a6c530",
    ),
    (
        "view:ryeos/programs/list",
        "7d11545ba4104b7e7364c2697fa83114ad5c1047d6a6943b2e87813bae7f158b",
    ),
    (
        "view:ryeos/project/document",
        "08d56fc061ba74f901d47c4a177dac73b30498e3c807e30245fac3c9ee2907c8",
    ),
    (
        "view:ryeos/project/files",
        "5dffe03ef0306564d67bd0ddd55d6d7e99043bc69d8a81de90209bf258714c69",
    ),
    (
        "view:ryeos/project/items",
        "5056c201699de70e0ade2996dc07d3c40c8bb3f34e226b66c967b6ded45f9f04",
    ),
    (
        "view:ryeos/project/schedules",
        "b0f673f411497cf23383f247190cd9b8ff6ede743b71e1a2cf5cdcfdd00494e1",
    ),
    (
        "view:ryeos/projects/list",
        "8a93c6c973658c729f2b0900d503c82c7b690210146b76edd6111a2c95821d0e",
    ),
    (
        "view:ryeos/review/history",
        "5b842575aad15f2bfe1c47c920130112a972ab993fcf706980f0affd72ffece2",
    ),
    (
        "view:ryeos/review/pending",
        "3fe1725cc7d284d9cf33adefc745e8496aabb0a6d6318aeb9df20e4b94dd1d73",
    ),
    (
        "view:ryeos/runs/comparison",
        "34a31906596c7d32697767f1ee4d1a43cb99dbc69c2458678add2916af02b165",
    ),
    (
        "view:ryeos/schedules/list",
        "c9e85900a8d9f4d4ebe9febb512bfa849260c6aec151adfb60d0786a65367fb1",
    ),
    (
        "view:ryeos/sites/list",
        "5a95e66465431f0062a413388dab26219e2830bc427bea45a246cd9feba6ee75",
    ),
    (
        "view:ryeos/thread/conversation",
        "ad34a392c9684cc0c02fb21babb95c3396f360df3981e76b0605346555706f1b",
    ),
    (
        "view:ryeos/thread/transcript",
        "200d6e933f2cf728ef255eab373809923a9b85a1e96fc09ca6382061246dda3a",
    ),
    (
        "view:ryeos/thread/tree",
        "bbceaa30a9ee5840153a576967e418281e5261fab6d3953f51bb7cbcde99f58d",
    ),
    (
        "view:ryeos/threads/detail",
        "db5e1bc595034d1896f09d0dba54b4d69ff25199331a889af26281c472c70db7",
    ),
    (
        "view:ryeos/threads/history",
        "355d32c363fb61c84414074c7cfc8db26fcd2ecd02d194d2c1d17a64b759cbaa",
    ),
    (
        "view:ryeos/threads/list",
        "0109b871e88a8f3950579513abda90a7384716ae39d68efa8df0c0228b3456d7",
    ),
    (
        "view:ryeos/ui/status",
        "ef47c98eb233b261e2a6aeaa7c22db18de90bcbf2c72b793012a2be327670f58",
    ),
    (
        "view:ryeos/view-sets/library",
        "513f7ca52ba0782dc9e918c86663284e0827977d504c9809fb4aacc516544567",
    ),
    (
        "view:ryeos/work/approvals",
        "99568eb15100b91ebd58588fb70e748a8e8e71b5ff19f4942f42ea5d7ff9689d",
    ),
    (
        "view:ryeos/work/candidate",
        "edfaf60520d68828395677ab392cafbac9cf28af2e840e1c7d6f7cfc7cd09e58",
    ),
    (
        "view:ryeos/work/children",
        "4c00f27ffc1432849ae87b94f7b3ef9e39c34c0efef04123e33809666ddef1bb",
    ),
    (
        "view:ryeos/work/evidence",
        "f2625f9e79cce7e58810ab7edb46381c37bf755d14481e80a89605f8c51b56b8",
    ),
    (
        "view:ryeos/work/list",
        "7e2cecd13e8897cc1819b615cd310eca34c85a0bd502fc6d19367ffb5b958989",
    ),
    (
        "view:ryeos/work/overview",
        "a0905ef96760628d15b98319f726a26b3da83f75cd3ce74bf42a12a48b4d29f1",
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
        52,
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

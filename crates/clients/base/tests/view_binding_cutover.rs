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
        "3710ecd476be87ccc6e0aedd0e0469b4c56f2dd54a91703a0ea4c9a5fbaec578",
    ),
    (
        "view:ryeos/commands/grammar",
        "5b27011bdb5569f503a8e45bf6a682ac629e2b52198eb19516e851cba420121f",
    ),
    (
        "view:ryeos/development/changes",
        "a45b0597dc79934adef2e096b05c81901d7c1537b110bbd77d387b1b3fdb0a02",
    ),
    (
        "view:ryeos/development/execution",
        "d5c79120849dfa4367d4f740f99e96239b33d7322547314b006e2a67999e8716",
    ),
    (
        "view:ryeos/development/explorer",
        "ad6fe09de5362e8de5aba6188d1618ed0892c8d3eafc59916df19551cac2cb9a",
    ),
    (
        "view:ryeos/development/occurrence",
        "5a22ee8129b9a103350afe653a03bc3896ed1c366f0499b0daf2a803749e41da",
    ),
    (
        "view:ryeos/files/list",
        "b28248ab756bd4b1344f2e479931f136a950d65ab1e9e8bff81b3dcf3ea60b22",
    ),
    (
        "view:ryeos/home/overview",
        "d6ea909186b24348e1a72170a10314e27209aa75558c9c8537ac7e9fc01d4247",
    ),
    (
        "view:ryeos/input",
        "e5015789547fd6cdbe84f4c4dbd1e0cf9823dea2ae6ef0737179826a6773ab6e",
    ),
    (
        "view:ryeos/item/explain",
        "b5866c3aad55d78c121b4bd06cf4bc36fc1e73ad31b5ff09877fcc2b7007af3c",
    ),
    (
        "view:ryeos/item/inspector",
        "700aadd73147116a0e8085c991501ca2469f306375e70e670214c5342430a39f",
    ),
    (
        "view:ryeos/items/space",
        "486ba92ff003fd8c6b38e8bf633156999621e91b32f6ef54f27cabcb14fd7daf",
    ),
    (
        "view:ryeos/node/bundles",
        "7c60ba64a2b50191870f9bc93d6c57420885913c5a8788cbe32d9d64e6d12be1",
    ),
    (
        "view:ryeos/node/events",
        "d469f22d69eb3cdb0c5e7bc51015dda0afd45708cf0a2cba908a89a59b8edbf1",
    ),
    (
        "view:ryeos/node/gc",
        "8a895b21f9d44c38eaa563d8dbc2a3c8c3ccb8f0e0cacb6db1aa5430bb0065de",
    ),
    (
        "view:ryeos/node/remotes",
        "adc424ef15137bd7e125429c338e9f8a861ea90f0233a434ddce7c395c3d4a96",
    ),
    (
        "view:ryeos/node/status",
        "9a48edeb6927a9841751fb7039228479a3c7e8c0e1d9778bbd721a751abc2050",
    ),
    (
        "view:ryeos/node/threads/history",
        "e9c7f8e518314fdbaa84d8d448f67b08d9e4d8a47052eb2b37a1640f10260d43",
    ),
    (
        "view:ryeos/programs/list",
        "49c8c9d122c61ebc2172e88ed5e6c1a237fa9369e4ccaec35c71aac2267159cf",
    ),
    (
        "view:ryeos/project/document",
        "ba26692b7fb5edee183eef066e2aa30a341c85b52cc04eb9ed0c99bececb33bb",
    ),
    (
        "view:ryeos/project/files",
        "339529bf8328f3197c10bc261990c0ec3e533fcdd14d2742e82271ccbfe4cbb9",
    ),
    (
        "view:ryeos/project/items",
        "9ab7025f95dfce92dca489ab0e504fc61848660a1d2ccd3dd68433c98c6a9c39",
    ),
    (
        "view:ryeos/project/schedules",
        "6d95b5d28b2aab44c20ca013fa54323d3ef4ef596914141ec7f8f5cf197febb1",
    ),
    (
        "view:ryeos/projects/list",
        "902f44c24d655ace29c1d5d181dd08fcc0fb7240c65985914363bd111fc66e89",
    ),
    (
        "view:ryeos/review/history",
        "fc504699bd003db693c07aec2836e5cc4a4dd0f1fc1dbe07bf49b63fe8a1e235",
    ),
    (
        "view:ryeos/review/pending",
        "2ce603c83cbb5e52790a6aad83bda5b09889d39b7cd04dfc49c4cbc00fca82f7",
    ),
    (
        "view:ryeos/runs/comparison",
        "843b947077a2730be5d39a9a0fd43665fd4e7d74bf18c96a1ce18c1092c9dc06",
    ),
    (
        "view:ryeos/schedules/list",
        "07d7e134c31711250c492b81b5f3e99f46389dfa3fd62c7d781563336c1ed59b",
    ),
    (
        "view:ryeos/sites/list",
        "b29e6fa4058b22f6de0c78c75bb611e336d787e1dc67f4b2964cb3699f7095c3",
    ),
    (
        "view:ryeos/surfaces/list",
        "fd1b745d9aa5cace159d053ef3246805e1854ef404e9ce326b5188e34f157372",
    ),
    (
        "view:ryeos/thread/conversation",
        "e065eb4204633b08f41af17cb93f99d4a8bfa937434b9982dc2a24c6ff1955f1",
    ),
    (
        "view:ryeos/thread/transcript",
        "92a90d201f7b2e0f9c3557e2b8a9a5e38b052ef7a58badbb1211c378f62a0b0a",
    ),
    (
        "view:ryeos/thread/tree",
        "10922668a8c3bd9ef38b08c52415aa7f0f996ce038282e14afddfd755fe30c5b",
    ),
    (
        "view:ryeos/threads/detail",
        "c07ee26a7bbc7ef8738eea0be0ed1c6d859f06c9af324819f80bd980ada1d9ee",
    ),
    (
        "view:ryeos/threads/history",
        "2e59945f65580f3b1e100b630c5a6127370560a8945abd7386d3d21f5cadc795",
    ),
    (
        "view:ryeos/threads/list",
        "b4dfccae5fe7abdae8ed1d541f0524d1e8ffbb3e343bbeac53c1503282f29f53",
    ),
    (
        "view:ryeos/ui/launcher",
        "7c8d3c79e5e7d952f6ee3655e1a2119683ad5919f8a2d6bed2c9aeb5e3c16bb0",
    ),
    (
        "view:ryeos/ui/session-details",
        "41ce861c02aa06309a0a3042697f6103c56b6d965f455e5df92421e066445228",
    ),
    (
        "view:ryeos/ui/shortcuts",
        "750563f2755ea6f97bc5301df0bdad6b655614503f84d532f67edd496c262b87",
    ),
    (
        "view:ryeos/ui/status",
        "a4855f4e1d150c61d29d6c3dcb6ec4d85adc4e16f732e011763d759ce2852036",
    ),
    (
        "view:ryeos/view-sets/library",
        "6341eb8bbd2602d321577bf6c77e135e80d4705071706c4e49a6e6d957bb4f06",
    ),
    (
        "view:ryeos/work/approvals",
        "5d54e54066c79101f036d99c95f2e78fdcedf3a6d967e98f8074ed7010b004c9",
    ),
    (
        "view:ryeos/work/candidate",
        "72977fc1152d0f866cb21434a02248bb7ee2f07ed8ccfea5dc11b06cb42b37ee",
    ),
    (
        "view:ryeos/work/children",
        "746eb4a199dd4eb94458cab371a6b4136df056067b737a9c6a6ad83910eb0338",
    ),
    (
        "view:ryeos/work/evidence",
        "8472770115de240fc8d38e28cbfd06b7740711b4dbc4520317fcdcd46dcf456c",
    ),
    (
        "view:ryeos/work/list",
        "ae9be57b871a2c3d4ac6b896caaeeb34320d7f0407db45bb5d8d1eb2ebb59e38",
    ),
    (
        "view:ryeos/work/overview",
        "d03a993b0148ec0e2e8d128a1c9eb6992f4ded64791e88f826ddb27c1e5368e1",
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

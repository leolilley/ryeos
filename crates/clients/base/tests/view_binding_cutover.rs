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
        "b33426006813072a63292c0acd721d424bcd4444f468ea83a7de83fc6ff7bc78",
    ),
    (
        "view:ryeos/backdrop/prism",
        "9938fc2fa9aad536875cadadec0e338f51ebc790921c4203a3edabd6f26c4e0d",
    ),
    (
        "view:ryeos/backdrop/prism-shards",
        "94531cca69111546569b1ca89a75306cd3f7fa4710abcce522773569a4bdbffb",
    ),
    (
        "view:ryeos/backdrop/splash",
        "a360c148c3113573619ea3f9ab36e3ec946db37c042aa26c4cb0b4913ee9982e",
    ),
    (
        "view:ryeos/chain/timeline",
        "34ffad6a5653c12e482b04b42ac3935a5f05b1d9457141c3642f62353144a11e",
    ),
    (
        "view:ryeos/commands/grammar",
        "07043a7ea1187e3d2273d2541b980d61c7a4439b9602c6d4a018a76e4fbc084e",
    ),
    (
        "view:ryeos/development/changes",
        "5b85210f0cd6bdaaa637789bb3fc6ffea0cd484d359539bf2dd754f8880ef47a",
    ),
    (
        "view:ryeos/development/execution",
        "1e055730c6bd0d0f0db6ff588f6b8a2229e04976e970141118b33587be540f40",
    ),
    (
        "view:ryeos/development/explorer",
        "f7c5665984f59c28a88ce61fd7f715c9d3ee418307d4c6c19be1ab88eb2aa121",
    ),
    (
        "view:ryeos/development/occurrence",
        "d3fa4fcb14502457337d50bc24fe7dd71eb150d0832beb2e028ccff6747eb567",
    ),
    (
        "view:ryeos/files/list",
        "1ea1638031a7d34ef9f6fb461f2225f4c4778a21514e65241729283951f179eb",
    ),
    (
        "view:ryeos/graph/topology",
        "95af9d33aceff94919f108a0b50bec214212cda043d7b98550c55ba0b0e9cc11",
    ),
    (
        "view:ryeos/home/overview",
        "471d605721e9d1f55f445bb02ded1128a90c99548a098e80df535f4d796507e9",
    ),
    (
        "view:ryeos/input",
        "eeba9abb330e1c29ae848ab303ee0e2a9f34609d825af65569730d7ebfcc69c3",
    ),
    (
        "view:ryeos/item/explain",
        "eed6de8d3fcdbfd2bfb91a48c794f8ae8c3c5c6972dc7383d5f9cd65a585994f",
    ),
    (
        "view:ryeos/item/inspector",
        "9278ee1adb4c2d27ca11deb599f491c5e61b500d463067611f160cb82b8778d0",
    ),
    (
        "view:ryeos/items/space",
        "b1a199ff2845f0729d7cde33a490fab18509c227e1dbdaafcb674ada8fba9513",
    ),
    (
        "view:ryeos/node/bundles",
        "4bd45f0e28e3219fa38ce73b4de7e4b310b7fd7a4e0be226781f81be3df7b090",
    ),
    (
        "view:ryeos/node/events",
        "127f2334e913b91823299c1452d8ef241a4eb009702131ea0349533fac9fc6c9",
    ),
    (
        "view:ryeos/node/gc",
        "3c3b11a195bd0127e69372e63e3741b5a35c3b16caa94826115ee330916539c7",
    ),
    (
        "view:ryeos/node/remotes",
        "6244c791b9205f7229586161a945f1bdc0248c21c2e5b74c3f5d004dc8d917b0",
    ),
    (
        "view:ryeos/node/status",
        "fd99bed944db95732717e734fec1f601d96a44d15861f1391a3cb44c74c226c3",
    ),
    (
        "view:ryeos/node/threads/history",
        "b70ba8f4d9406bc414664ea43a4b3836e3469bb0ae8d2f5276d038603a9f278a",
    ),
    (
        "view:ryeos/programs/list",
        "b524300409d0bd99fd9911528f17fe84b00cfa2e3906b6058fdd8ac0a166f655",
    ),
    (
        "view:ryeos/project/document",
        "2f1fcfe27ed3c1724b806a90f12642cbad0914413ca799a44790d4009a28df89",
    ),
    (
        "view:ryeos/project/files",
        "5c2327032d8cac1dfb58480efb2cc80de8f2250cc42762b275dd4c92b50bd81e",
    ),
    (
        "view:ryeos/project/items",
        "d740623a0a9095c5145af1d53cce029a2af896a5359ec1e7520ce3eaebe3a31f",
    ),
    (
        "view:ryeos/project/schedules",
        "8996e69a0869d893d7e1049c6fdac4540de7bcf357fb3eb883d42365d8b08100",
    ),
    (
        "view:ryeos/projects/list",
        "384360f473482b8d4e6665b38d8808131b186220e9784e48a067fdd9d967765b",
    ),
    (
        "view:ryeos/review/history",
        "7ff18d14a4b14ac41ad2800997669048e833e0422a703ab82e43d0168d829141",
    ),
    (
        "view:ryeos/review/pending",
        "495557d1cba738901d0d453488cc5e5dee9d5426b70af541ae3eefd79dc65e62",
    ),
    (
        "view:ryeos/runs/comparison",
        "2d47ecb1f716864cc28cb9ada444978ea8d3038d19a82c9ce1cb2ee961529619",
    ),
    (
        "view:ryeos/schedules/list",
        "81a49ed5f1dcdef23795c28b5571cc8b195cae672264a42f9fb4690fd2d4edcc",
    ),
    (
        "view:ryeos/sites/list",
        "2cfbb6847a6c07f085d32fc1af94c9913d8ca8e788090458075a2c6e6c2cbbaa",
    ),
    (
        "view:ryeos/surfaces/list",
        "39a0f167f7c43145510507d3eb5f1eab1ae15f65a432bdc626f19de9ff5e0567",
    ),
    (
        "view:ryeos/thread/conversation",
        "16154b7b12dc3022f46d537a422fa0344096dd8fc32415ce6a7fc296d68f4bd2",
    ),
    (
        "view:ryeos/thread/transcript",
        "128d9e196c6220be6040329f576ca4c04c58440587ddaefd81cf095dfe3cdac8",
    ),
    (
        "view:ryeos/thread/tree",
        "d4e0ed69f082c8b7cc8176b8b6d5f6e0648002fd2521bb9812d5992e50a508bd",
    ),
    (
        "view:ryeos/threads/detail",
        "b5cfd618193473832acab24269ba2f9e63d2dc2cf75522ee73a3f81ae2460a8a",
    ),
    (
        "view:ryeos/threads/history",
        "68800863ec4c720db8e93cdab5ac4a50fa1b83a9bbfef11dcb22a2f48213a24e",
    ),
    (
        "view:ryeos/threads/list",
        "9a8c3b06f46a95bf6b1cc74900524ce22e7deac5d4c1e60b8bccb3c10aeaad4c",
    ),
    (
        "view:ryeos/ui/launcher",
        "8bbd1e5991c02e298818de65e5416c9e22964d9b4808665b7fbbe40cd7e75b21",
    ),
    (
        "view:ryeos/ui/session-details",
        "be31eb87abc8c2b69837f8b763f5289f11e60e8d6443b0db7b37d17ded623ef0",
    ),
    (
        "view:ryeos/ui/shortcuts",
        "99576d9f75d0d7a13994555dc5423bfeedd9518cedc9fff511899a44194fed9f",
    ),
    (
        "view:ryeos/ui/status",
        "13ea96c74974baf26099dd8a2bf15d8e696c530d4c779372fa624fdc513dc101",
    ),
    (
        "view:ryeos/view-sets/library",
        "828d326d0e15a0e9fbb1ae3f664a2cabb36716e497b2dd310b44591c6ba0c54d",
    ),
    (
        "view:ryeos/work/approvals",
        "8a9dd397ad4edef0683c5b993de1338c77fa74089864d61af408ad6094797184",
    ),
    (
        "view:ryeos/work/candidate",
        "8fb6f90ec481804e2e455e0f3e503ba98913553bac14cb9524792d4d01562475",
    ),
    (
        "view:ryeos/work/children",
        "6bf4da2c163a43ea7e77a19c0fede423296de5661a3c612e7626e7c3f0cac77d",
    ),
    (
        "view:ryeos/work/evidence",
        "7331b4d69db73ec316667e2adb7ee2c615c29612d93a9fa8013184d4c53809bc",
    ),
    (
        "view:ryeos/work/list",
        "79799f43af8dbc7466126a17b51db83993c6764f7fde32acecd76af059dd99b2",
    ),
    (
        "view:ryeos/work/overview",
        "6e7cccc3f77482c8bbab9b0ee3c2221bdadd6cd6c72efdeb8612511d179c46bd",
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

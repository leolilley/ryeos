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
        "9bb99f9557c7b87b41c48be70f791d1c4e3a135afe7a9f21c0d9f74557a31b1b",
    ),
    (
        "view:ryeos/backdrop/prism",
        "4af3f94d9356470044bdfa8f34b43cb91b8a838b28eb1244ce62f45118176989",
    ),
    (
        "view:ryeos/backdrop/prism-shards",
        "8983a3fbe78178579f49da296b6120d12605c6165d6589a07b6cb6d08b5d509a",
    ),
    (
        "view:ryeos/backdrop/splash",
        "f5acc95a4a5174af836aa76ebb15ca457c29c4a272d915405519a7b9ab7b3bc8",
    ),
    (
        "view:ryeos/bundles/list",
        "6b78b2a88e8d6ec761478270feafcb1a2d276d025b6eaa5330fb10e2d305ab92",
    ),
    (
        "view:ryeos/chain/timeline",
        "61b51a207adc795343df8cda137597613ce613d90e5a455e9a5c3dc0afbbb745",
    ),
    (
        "view:ryeos/commands/grammar",
        "0b8b2b29a159a00e0dd79b2ad5fb6210df930645e76273eed95718aae6f74493",
    ),
    (
        "view:ryeos/development/changes",
        "feec30507d5606768dd42ca5a5350975e3206abaae9a66de1d6d009320f83bcd",
    ),
    (
        "view:ryeos/development/execution",
        "fe94fae1d231107e869451529ac608d704efff0d365d0859d3be70fa166c6834",
    ),
    (
        "view:ryeos/development/explorer",
        "10cea3afd856d60999156e6d46cb35bfcfeffe6b8a8663983d9b1f630af3e733",
    ),
    (
        "view:ryeos/files/list",
        "a55b3d98b70fef93ca96e8997aabc35ddc7d833d9f5d5eecf71a240d2cfa66bf",
    ),
    (
        "view:ryeos/gc/status",
        "545349873118219b2c561e24bc55e12eae68fa8dc562cfa87ba6ebaca6a64b60",
    ),
    (
        "view:ryeos/graph/topology",
        "c56ffc40c316fe503ae2f830f1828573caa878a3789b5396ea7739953d1412e5",
    ),
    (
        "view:ryeos/home/overview",
        "dc80dbf45741b636254d8e6330615721f89ac9d58b72c286e64a6a7bb6d70b95",
    ),
    (
        "view:ryeos/input",
        "e3e84e49fd52198bd77df088b98ff7b9f8fff6cd02782a981196deb3c23ccb51",
    ),
    (
        "view:ryeos/item/explain",
        "e457c4f522908540629d662703e580fd882de4d24b03e2a4ac04566688611dac",
    ),
    (
        "view:ryeos/item/inspector",
        "0dafe3ad156f9e809b7ad2aec286cf247dd5f1fc83ebdfb3074ee12b708d47ec",
    ),
    (
        "view:ryeos/items/space",
        "f57c4c97d06a960af1563c399c82bfa4e68f1ee18d3353799e245631f2b603dd",
    ),
    (
        "view:ryeos/node/bundles",
        "db2d258f3b2728b6e152056eae04b09b441cc93bda4351c5ba3ebc6efe8352a7",
    ),
    (
        "view:ryeos/node/events",
        "bb9b73964504dbb0a955a301610015173b6b598e999f6b980d2bd55b9b9ea3bd",
    ),
    (
        "view:ryeos/node/gc",
        "249d2b7a16da776ac453058b12ccc20577e499b9ad174a3761c2c49de35cc0f9",
    ),
    (
        "view:ryeos/node/remotes",
        "ebda27466e2c42fd161496f52a1049b449b4458eb1795923d3397efb9dbffbd2",
    ),
    (
        "view:ryeos/node/status",
        "3c992e6e98a3f9470c8f134774e7a7a01f2021b71157db1ba958effd4f4c4941",
    ),
    (
        "view:ryeos/node/threads/history",
        "ec68dc4cc0979767115b91a2a49ab155df00b7ba34b67e49003593744bde6de2",
    ),
    (
        "view:ryeos/programs/list",
        "67dcf810ea9add944f30ae19ed641cdd944a9c4c4a94c8b03a2c99244926b0b5",
    ),
    (
        "view:ryeos/project/document",
        "bf27a9524766b9d1d2ff6666477c143dafd8246b7441ef70065c95abece6ae63",
    ),
    (
        "view:ryeos/project/files",
        "c952ba73832eca71511399751a6651f5d6a7acd6e221bb1beffd9768f7ea8fc4",
    ),
    (
        "view:ryeos/project/items",
        "eaeb15d6dde373f620e55b4eb5596575eacdbbcc787501e806e72068fea431e4",
    ),
    (
        "view:ryeos/project/schedules",
        "2a4718905943d0cbb57746d1e9730bb86bfdb9d073fac88d0b68f4f1424051fb",
    ),
    (
        "view:ryeos/projects/list",
        "c0d1f03159d590c7633124d08da020232efc29a7c5ae834c781f6b75756edfae",
    ),
    (
        "view:ryeos/remotes/list",
        "81729b02bde53d9c8f554cf108b7e3a972b63f200efe57c642f2f5fb23919940",
    ),
    (
        "view:ryeos/review/history",
        "0dc65030965693d638d29db68f24afbd306da10b9ececcef550f42a39dce843e",
    ),
    (
        "view:ryeos/review/pending",
        "626f740939c3c717e26f46ba3831a9efb18e9a9a95355d09e072691e51899e6d",
    ),
    (
        "view:ryeos/runs/comparison",
        "c27d80db50868d5a67eb8ff15097086b7d8e73418a128fb0c03b25e316528675",
    ),
    (
        "view:ryeos/schedules/list",
        "6a77ed34d748720cb757ce25953aa6608a23a87ce75afb6719e588e9202084a7",
    ),
    (
        "view:ryeos/sites/list",
        "557393ee6805fbc78a7556abf809bd0e7f194e84ef40939ba74f42d4b5bce0c5",
    ),
    (
        "view:ryeos/thread/conversation",
        "ca98ae4a0b5374ceb01d1030344e1d8f02b3de229a9d3712bf3bfaf2e6c6afd1",
    ),
    (
        "view:ryeos/thread/transcript",
        "f04bc8294400751692481b60ba931604186a72cd64c4b7e9bc35e9132ee7d40d",
    ),
    (
        "view:ryeos/thread/tree",
        "4d4f0b6d75b2992e51569d74f7c566587de54447f315cb505bd2ebc2a8dafe99",
    ),
    (
        "view:ryeos/threads/detail",
        "5bf8c965b9c1d748f6a35f83207df13d80081953b86df3bebce090f77d4bb70e",
    ),
    (
        "view:ryeos/threads/history",
        "e1475df5e9adea58fdf29c9ebaec74638375b8aebb943fdc688672f30c86d856",
    ),
    (
        "view:ryeos/threads/list",
        "f50e3d4e8bdfbe6d5fd8a52dce38f39507e633f812bcfa127e02d7bf042d41b2",
    ),
    (
        "view:ryeos/ui/status",
        "4ecb8167065413a1ab5929b7401789477a3dc9e07b78b2faeeeca6ee99b46130",
    ),
    (
        "view:ryeos/work/approvals",
        "4bb01de22c200a36373d938732f6784ac14a25a09a6f4515a032cc403cac1c0e",
    ),
    (
        "view:ryeos/work/candidate",
        "196d59f790c43690fb607f6766d0d86d24145aeae2c226e4930d177fb7eaf1bd",
    ),
    (
        "view:ryeos/work/children",
        "3abf9afddddcf1cc504f50186c9703134ac56e8fa4facde0413dc9f59cfa8d99",
    ),
    (
        "view:ryeos/work/evidence",
        "905c034cf7fa732ef0495cd5dfad9536d1d0eacec010465a6d4b466a8304846e",
    ),
    (
        "view:ryeos/work/list",
        "6242f79853bb2b5c35c7106dce4bc91a77a8301369b72fc3171ab2ad562f5a0f",
    ),
    (
        "view:ryeos/work/overview",
        "f4607ff6af1f2b896e10fe9d43c2804478cbd54098fdd338776dcdcf0149f0fd",
    ),
];

fn snapshot_digest(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("serialize view behavior snapshot");
    format!("{:x}", Sha256::digest(bytes))
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
fn every_bundled_view_resolves_and_validates_under_the_named_source_contract() {
    let repository = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .expect("canonical repository root");
    let views_root = repository.join("bundles/ryeos-ui/.ai/views");
    let files = yaml_files_below(&views_root);
    assert_eq!(
        files.len(),
        49,
        "the complete signed view inventory changed"
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
    let standard_bundle = repository.join("bundles/standard");
    let ui_bundle = repository.join("bundles/ryeos-ui");
    let bundle_roots = vec![
        core_bundle.clone(),
        standard_bundle.clone(),
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
                name: "standard".to_string(),
                canonical_root: standard_bundle,
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
        let session = BrowserSession {
            surface_attachment_id: "cutover-golden-attachment".into(),
            binding_attachments: vec![UiBindingAttachment {
                binding_attachment_id: "cutover-golden-attachment".into(),
                binding_generation: 1,
                binding_digest: "cutover-golden-binding".into(),
                surface_ref: "surface:fixture/cutover-golden".into(),
                surface_generation: "cutover-golden-surface".into(),
                effective_surface,
                project_path: Some("/fixture/project".into()),
                posture: Default::default(),
                binding_request_bounds: UiBindingRequestBounds {
                    max_request_bytes: 64 * 1024,
                    max_input_bytes: 16 * 1024,
                },
            }],
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
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

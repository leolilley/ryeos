use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_client_base::ui::content::views_from_surface;
use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
use ryeos_client_base::ui::view_model::build_view_model;
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
        "31bbcf35045292c0823ba05c9ccc4ac463ef978d7f8f3797cf14416246db0e42",
    ),
    (
        "view:ryeos/backdrop/prism",
        "6bf6cdd137e3798549ca2845ebee31a7a55488f9097c3add50dc43f91ee6cb37",
    ),
    (
        "view:ryeos/backdrop/prism-shards",
        "185338ff7aed11b4c4ef4d2ec46625c31d7cddc940f047b71d45659bc3ed1289",
    ),
    (
        "view:ryeos/backdrop/splash",
        "a0f7760c221ebf075840de3d2e7c56a99e66ef88f92d8f7e578ca5eece00a32a",
    ),
    (
        "view:ryeos/bundles/list",
        "1618d2b9159a78eebdc0cf99d6094e2aba86fee7f2d2d9cc11482df854c7485e",
    ),
    (
        "view:ryeos/chain/timeline",
        "c34ba90d9f56a40e694505e14081cbe8e4a7670391561ed7957035afc88f73b4",
    ),
    (
        "view:ryeos/commands/grammar",
        "7df858b185e9c4624e38926803f9856fbc2162f3df934bfdd534d4f65557a8d3",
    ),
    (
        "view:ryeos/files/list",
        "a1cfeb5071fe718b74c321461646e7a9b058c1217f5e4c0496f5e95bffb62922",
    ),
    (
        "view:ryeos/gc/status",
        "5d0ac6fb8432d5107306096aaa95f15d8f4c66a3d5bdbf78d3a643c712b611a2",
    ),
    (
        "view:ryeos/graph/topology",
        "f44a505d2883ef3f9fdf671ad67d2bac1cf3174b94dd95dbd6f56712ce30c957",
    ),
    (
        "view:ryeos/home/overview",
        "f44c0df9a28371735252ed48a6d7bb52e69a03cf57ff6ba27f6fa6cf8265b5f3",
    ),
    (
        "view:ryeos/input",
        "01a6f116ecd744015dfbd890e535e5cd3432689a2db14fee57d8b9da5715c0d8",
    ),
    (
        "view:ryeos/item/explain",
        "6dfaca7af100f4a96bd3a82bb8292e21c56c4db9b8524727480267d58c586bda",
    ),
    (
        "view:ryeos/item/inspector",
        "f9b7023efed6713202383eb23c5016f34e2ece0ff246509425c1c927322ab8c2",
    ),
    (
        "view:ryeos/items/space",
        "71a24207033ab85872cc1273427ec8b570fda53455d7933e578f7a1ce71313b7",
    ),
    (
        "view:ryeos/node/bundles",
        "8a4f233e0ec46db3395863bb17789460566c7f173b662a163a08e94d1aead5ac",
    ),
    (
        "view:ryeos/node/events",
        "5655ec6dbdcbbb80d60cf5d9c70d0f2292db1fddacd80117d460c8f916031546",
    ),
    (
        "view:ryeos/node/gc",
        "34d7140d95f9f72bbf859797d95beb1c4770a7c2c64f5f9d2505de409d4c477c",
    ),
    (
        "view:ryeos/node/remotes",
        "6bc1700111efbf1b1645dc93a39239dbd6cd7a28ef745247b704d7de65e5760c",
    ),
    (
        "view:ryeos/node/status",
        "20c6e823a763c66aad6b3c65921c6e089193aef14813b67ecfc2b6037dc5e4f2",
    ),
    (
        "view:ryeos/node/threads/history",
        "25e3cfe016465b13d9add55a6e8865a98d90e8e2ff36ddec06df1c2a904631ec",
    ),
    (
        "view:ryeos/programs/list",
        "b036d149ab102440cb13c0eabba5f3ad42f5fb1661e1d81ff4d562dd875bd393",
    ),
    (
        "view:ryeos/project/files",
        "526b28a0daa5cd033771d4d94f324bb80fdd012abf2fb960ce39738d3aa68d67",
    ),
    (
        "view:ryeos/project/items",
        "487268298d4577a6b841c3c197e087904eb7aefe2fa566d15f72a45fb395456e",
    ),
    (
        "view:ryeos/project/schedules",
        "dae2b20ef979e642499f1aac82a92684fc67b8b219523f73c4243adeae8e1d37",
    ),
    (
        "view:ryeos/projects/list",
        "1e411af2f9be00603682126885c473fda0850c9e2b966052152193096144156f",
    ),
    (
        "view:ryeos/remotes/list",
        "8ab247206b9e0fd39522140e0997aad0037d3008ab19cac4eaae36f38ab5daf2",
    ),
    (
        "view:ryeos/review/history",
        "bb6ed665d5b4ccb3e97957a3800de356569a0fe5b490a27a2debf584d8535fd6",
    ),
    (
        "view:ryeos/review/pending",
        "df87ecd782ebdc271d805c6c114250d88f9d110411e66d98c0820f11c7bd79cd",
    ),
    (
        "view:ryeos/runs/comparison",
        "3bf35cdbb392b4a7deddc6c40c03f904f1a7f6c19492d2e02749cc8fdee1e4d8",
    ),
    (
        "view:ryeos/schedules/list",
        "65c1f49bd315da32ca5aefad3803df118698a95e0368c43c36226021fe879cda",
    ),
    (
        "view:ryeos/sites/list",
        "8bf2c2e33222af4cc505cfa0f139daea560be3dbed1751381c00c482e58192bf",
    ),
    (
        "view:ryeos/thread/conversation",
        "0a6c6627089b99ad57a0f3c192bb17d502d5c5b9810ae38733f575f7f73ef8a8",
    ),
    (
        "view:ryeos/thread/transcript",
        "b0a1f4967833f9242574fd0a74b78bcfbf4bccc6cea3da5b1d30d3b2041edda1",
    ),
    (
        "view:ryeos/thread/tree",
        "a5f0beadbe0c7b1451f2838aecbf8246bbc38d53689780af85a309908672a7e2",
    ),
    (
        "view:ryeos/threads/detail",
        "1030d5260053ff000c0bab2e96f48edec2a3b94e69ed6eea19b7d0b2b0135ffd",
    ),
    (
        "view:ryeos/threads/history",
        "b36eed0f8d9d52ad58730f257f53395b7c4caa5fdc707f3bef8a147ce4dcfbdf",
    ),
    (
        "view:ryeos/threads/list",
        "fd116bc2343417a0a741e215f7cff14e65dba0378084fe2c3340afceca0b80eb",
    ),
    (
        "view:ryeos/ui/status",
        "b48e2fd29ef1b7138d2980253b7aac6a471a7751719b23db53acf6d5200cd51b",
    ),
    (
        "view:ryeos/work/approvals",
        "06fd44368920826d0c1242f823f05b529999ceb497de71cf5c2d59155e00f582",
    ),
    (
        "view:ryeos/work/candidate",
        "baca1aba9b73f820f8a31ab447bf4bc1d04cbfcbec31f35e494a6c8b5fb5000d",
    ),
    (
        "view:ryeos/work/children",
        "ad60c9eecb8294627614ddc3635fff6b186944e62acb4b759198ca904240f488",
    ),
    (
        "view:ryeos/work/evidence",
        "4c3d237f74026c58dd4b64231401c78365846cf73265dcb555c3a125542a3597",
    ),
    (
        "view:ryeos/work/list",
        "f316afa1eb5bde978811682962980312a2175ec0ac9181c71ebbda24edca2bf5",
    ),
    (
        "view:ryeos/work/overview",
        "54f04225dd33c48cd9ca33aad21515a5869183874be21e4b4e2216fbb257eafb",
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
        45,
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
        let session = BrowserSession {
            effective_surface: Some(json!({
                "name": "cutover-golden",
                "tiles": [view_ref],
                "views": views,
            })),
            project_path: Some("/fixture/project".to_string()),
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

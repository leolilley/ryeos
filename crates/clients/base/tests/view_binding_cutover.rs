use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ryeos_client_base::ui::content::views_from_surface;
use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
use ryeos_client_base::ui::view_model::build_view_model;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const VIEW_BEHAVIOR_GOLDENS: &[(&str, &str)] = &[
    (
        "view:ryeos/atlas",
        "06b0fd9479ee06f3e9fca8e8fea473691b392281e9dd8136eaa8d611fabb5e39",
    ),
    (
        "view:ryeos/backdrop/prism",
        "f00e10fe11425ec4f708e124bc5f931e6158162593dbbc435c620642d5bbb4cd",
    ),
    (
        "view:ryeos/backdrop/prism-shards",
        "a3a10b68b35af1672145289189ab9e0c54caa167ff14f95e815fef39f43128c7",
    ),
    (
        "view:ryeos/backdrop/splash",
        "2cead8769d3a01371fdabee15f8733ca921699bc7363feb6abb2f2015f24d728",
    ),
    (
        "view:ryeos/bundles/list",
        "f14e74408627efd3d284c21ac925830639b67b66d90f61f4f7c0ec566f49f7f6",
    ),
    (
        "view:ryeos/chain/timeline",
        "415a093fa80bf196acc7e1ee9990190693dc83ed6268389d8ed723a8c6bad3d5",
    ),
    (
        "view:ryeos/commands/grammar",
        "605ee964151b681c1a63381a8189d55784a13490876adb1ec5bbeeb3652b8810",
    ),
    (
        "view:ryeos/files/list",
        "b81180135ffee5c7a3609d16db310cc340a8a3ebae056dc6d174336d143519c3",
    ),
    (
        "view:ryeos/gc/status",
        "8c37e3fbea502ae64bb90690577b9012249c4173d05d5ff643e728e2c906c56e",
    ),
    (
        "view:ryeos/graph/topology",
        "c470e5e0de935a46577250f64797d212969637ed4c35aba0e8338730d1de114f",
    ),
    (
        "view:ryeos/input",
        "6e78036639f3d35b0e36f31ac16a1421d9b65839658e5635b0532a77866bf31b",
    ),
    (
        "view:ryeos/item/explain",
        "2cab28db91a3a7eb2919ce1e0dc1ae4bad92aea24efabf2749bdb89dff55809e",
    ),
    (
        "view:ryeos/item/inspector",
        "c7570b11507e9c7ac0218d613404a85b8a75e476281f7ee551a37338e08ceb66",
    ),
    (
        "view:ryeos/items/space",
        "2c3c8c6e470421dd7aec4e63ac97cd44d245f704c935901c86921497adf65881",
    ),
    (
        "view:ryeos/node/bundles",
        "f0287ecb969497dd931f37ea4b5d53c3673835c119d079eab99042eefef639d7",
    ),
    (
        "view:ryeos/node/events",
        "fbcb8b39076262bba1afb1dc80aded5adc3306b36f7866b1343111244fed47d8",
    ),
    (
        "view:ryeos/node/gc",
        "8cd3694592b206fe5ba7d98b056d664e1118046509295b604d660077d4022017",
    ),
    (
        "view:ryeos/node/remotes",
        "43b6f36e4ade3b92ddfd124779cae33bd0a20805485b3c03ebd1628c89c0e355",
    ),
    (
        "view:ryeos/node/status",
        "b23635495a49ab27a63fa4cf49fa1caec229965f47aa1b14cf2e80328585149c",
    ),
    (
        "view:ryeos/node/threads/history",
        "00bcd524373903ef2839d31d4e8d79091a02f856c2fb8c370f7532d4004a0357",
    ),
    (
        "view:ryeos/project/files",
        "d969b7ca212a49a631214e3e2cd68deffa0303ac466f7db9b99474a42da3bb4b",
    ),
    (
        "view:ryeos/project/items",
        "f66cd36a31b21232766b31bd8c42e1f543b7d48a8d6ee4979aa29d9261e506e3",
    ),
    (
        "view:ryeos/project/schedules",
        "a34a3795e8e83cea680b07dfdbf4d004f3b8d743fe9a0d0d7fb99ee218250704",
    ),
    (
        "view:ryeos/projects/list",
        "673dffeb47f4968775a3750e59120d222bc5e1f0a92be493b48d501c2ab0d510",
    ),
    (
        "view:ryeos/remotes/list",
        "f5c876713b06bf61d23a77120c52ac0b8b97a1e617a71a8607d785dc7ef7c573",
    ),
    (
        "view:ryeos/runs/comparison",
        "92ead02db906229f1b0b904911a3f4f3d4cf7ff6d4d3bc639650fcd94feb19ab",
    ),
    (
        "view:ryeos/schedules/list",
        "cb0a1007c69df01d78813ef410fe8372f8d5c785ffaa2995dc65cdeb596a1635",
    ),
    (
        "view:ryeos/thread/transcript",
        "0539ed2b695679bdaa7b53fc078e6d1970d994061e22678d0ecf57999124a43e",
    ),
    (
        "view:ryeos/thread/tree",
        "d0dd02c41ca2cc007beddbae1f5bbc04c1eac2ef31135fbc81456068bb13e0bc",
    ),
    (
        "view:ryeos/threads/detail",
        "5afb1a9013e41b2ed00b888d03ea1512928be2b51fb13c939aa8f540471796cd",
    ),
    (
        "view:ryeos/threads/history",
        "8e75bf1a628e2aeaa50214d727ef612900b573ddc864c8fcca2bb15023d06511",
    ),
    (
        "view:ryeos/threads/list",
        "ec030055cc8728c402e45333af7d08ee9e4c022095e3da144ea1a464a1227c2f",
    ),
    (
        "view:ryeos/ui/status",
        "aba29d55e8d750a1b95d62bcc94a6d84b5993faabfc10c5877b98e80162371b9",
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
fn every_bundled_view_uses_and_validates_under_the_named_source_contract() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let views_root = repository.join("bundles/ryeos-ui/.ai/views");
    let files = yaml_files_below(&views_root);
    assert_eq!(
        files.len(),
        33,
        "the complete signed view inventory changed"
    );

    let mut embedded = Map::new();
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
        embedded.insert(view_ref, value);
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
        let raw = &raw_by_ref[&view_ref];
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

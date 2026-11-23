//! The signed view and surface kind-schemas load and judge a source's
//! `collection`: a plain path string and the map form are accepted, any other
//! value type is refused by the schema, and the closed map shape is refused by
//! the typed binding the client reads.

use ryeos_client_base::ui::SourceBinding;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::test_support::{
    core_bundle_root, live_trust_store, workflow_bundle_roots, workspace_root,
};
use serde_json::{Value, json};

fn registry() -> KindRegistry {
    let mut roots = vec![core_bundle_root().join(".ai/node/engine/kinds")];
    roots.extend(
        workflow_bundle_roots()
            .iter()
            .map(|root| root.join(".ai/node/engine/kinds")),
    );
    roots.push(workspace_root().join("bundles/ryeos-ui/.ai/node/engine/kinds"));
    KindRegistry::load_base(&roots, &live_trust_store()).expect("signed kind schemas load")
}

fn authored(relative: &str) -> Value {
    let path = workspace_root().join("bundles/ryeos-ui/.ai").join(relative);
    serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn errors(registry: &KindRegistry, kind: &str, item: &Value) -> Vec<String> {
    registry
        .get(kind)
        .unwrap_or_else(|| panic!("{kind} kind is registered"))
        .composed_value_contract
        .validate_instance(item)
        .errors
        .into_iter()
        .map(|violation| format!("{:?} {}", violation.code, violation.path))
        .collect()
}

fn map_form() -> Value {
    json!({"path": "by_name", "entries": "map", "key_as": "name"})
}

#[test]
fn view_and_surface_schemas_judge_the_collection_value_type() {
    let registry = registry();
    let cases = [
        ("view", "views/ryeos/node/bundles.yaml"),
        ("surface", "surfaces/ryeos/ui/base.yaml"),
    ];
    for (kind, relative) in cases {
        let base = authored(relative);
        let channel = base["sources"]
            .as_object()
            .and_then(|sources| sources.keys().next().cloned())
            .unwrap_or_else(|| panic!("{relative} declares a source"));
        let with = |collection: Value| {
            let mut item = base.clone();
            item["sources"][&channel]["collection"] = collection;
            item
        };
        assert!(
            errors(&registry, kind, &base).is_empty(),
            "{kind}: the shipped item is valid"
        );
        for accepted in [json!("bundles"), map_form()] {
            assert!(
                errors(&registry, kind, &with(accepted.clone())).is_empty(),
                "{kind}: {accepted} is accepted"
            );
        }
        for refused in [json!(7), json!(true), json!(["a"])] {
            assert!(
                !errors(&registry, kind, &with(refused.clone())).is_empty(),
                "{kind}: {refused} is refused"
            );
        }
    }
}

#[test]
fn the_typed_binding_refuses_malformed_map_shapes() {
    let parse = |collection: Value| {
        serde_json::from_value::<SourceBinding>(
            json!({"ref": "service:x", "collection": collection}),
        )
    };
    assert!(parse(json!("bundles")).is_ok());
    assert!(parse(map_form()).is_ok());
    assert!(parse(json!(7)).is_err());
    assert!(parse(json!({"path": "p", "entries": "list", "key_as": "k"})).is_err());
    assert!(parse(json!({"path": "p", "entries": "map", "key_as": "k", "extra": 1})).is_err());
    assert!(parse(json!({"path": "p", "entries": "map"})).is_err());
    assert!(parse(json!({"path": "p", "entries": "map", "key_as": ""})).is_err());
}

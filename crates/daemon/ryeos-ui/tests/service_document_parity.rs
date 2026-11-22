//! Every compiled UI service descriptor has a signed service document with
//! the same endpoint and required capabilities, and every signed UI service
//! document has a compiled descriptor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Document {
    endpoint: String,
    required_caps: Vec<String>,
    metadata: std::collections::HashMap<String, serde_json::Value>,
}

fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, Document>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# ryeos:signed:"),
            "{} is not signed",
            path.display()
        );
        let value: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let relative = path.strip_prefix(root).unwrap().with_extension("");
        let service_ref = format!("service:{}", relative.to_str().unwrap());
        let endpoint = value
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{service_ref} declares no endpoint"))
            .to_string();
        let required_caps = value
            .get("required_caps")
            .map(|caps| {
                caps.as_sequence()
                    .unwrap_or_else(|| panic!("{service_ref} required_caps is not a list"))
                    .iter()
                    .map(|cap| cap.as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let metadata = serde_yaml::from_value(value.clone()).unwrap();
        let previous = out.insert(
            service_ref.clone(),
            Document {
                endpoint,
                required_caps,
                metadata,
            },
        );
        assert!(previous.is_none(), "{service_ref} declared twice");
    }
}

#[test]
fn descriptors_and_signed_service_documents_match_both_ways() {
    let root: PathBuf =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/services");
    let mut documents = BTreeMap::new();
    collect(&root, &root, &mut documents);

    let mut seen = Vec::new();
    for descriptor in ryeos_ui::handlers::ALL {
        let document = documents
            .get(descriptor.service_ref)
            .unwrap_or_else(|| panic!("{} has no signed service document", descriptor.service_ref));
        assert_eq!(
            document.endpoint, descriptor.endpoint,
            "{} endpoint",
            descriptor.service_ref
        );
        let mut document_caps = document.required_caps.clone();
        let mut descriptor_caps: Vec<String> = descriptor
            .required_caps
            .iter()
            .map(|cap| cap.to_string())
            .collect();
        document_caps.sort();
        descriptor_caps.sort();
        assert_eq!(
            document_caps, descriptor_caps,
            "{} required_caps",
            descriptor.service_ref
        );
        seen.push(descriptor.service_ref.to_string());
    }

    for service_ref in documents.keys() {
        if !cfg!(feature = "scheduling") && service_ref == "service:ui/ryeos-ui/schedules/list" {
            continue;
        }
        assert!(
            seen.contains(service_ref),
            "{service_ref} has a signed document but no compiled descriptor"
        );
    }
}

#[test]
fn ui_result_effects_and_dispatch_read_through_the_runtime_extractors() {
    use ryeos_app::service_registry::{
        UiResultEffect, extract_ui_dispatch, extract_ui_result_effect,
    };

    let root: PathBuf =
        ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui/.ai/services");
    let mut documents = BTreeMap::new();
    collect(&root, &root, &mut documents);

    let expected = [
        ("service:projects/open", UiResultEffect::SurfaceOpening),
        (
            "service:ui/session/surfaces/open",
            UiResultEffect::SurfaceOpening,
        ),
        (
            "service:ui/ryeos-ui/surfaces/resume",
            UiResultEffect::ResumeParticularSurface,
        ),
        (
            "service:ui/ryeos-ui/view-sets/resume",
            UiResultEffect::ResumeParticularViewSet,
        ),
    ];
    for (service_ref, effect) in expected {
        let document = &documents[service_ref];
        let read = extract_ui_result_effect(&document.metadata)
            .unwrap_or_else(|error| panic!("{service_ref}: {error}"));
        assert_eq!(read, Some(effect), "{service_ref}");
    }
    for (service_ref, document) in &documents {
        extract_ui_dispatch(&document.metadata)
            .unwrap_or_else(|error| panic!("{service_ref}: {error}"));
        if !expected
            .iter()
            .any(|(candidate, _)| candidate == service_ref)
        {
            assert_eq!(
                extract_ui_result_effect(&document.metadata).unwrap(),
                None,
                "{service_ref} declares an unexpected result effect"
            );
        }
    }
}

fn find_key<'a>(value: &'a serde_yaml::Value, key: &str) -> Option<&'a serde_yaml::Value> {
    match value {
        serde_yaml::Value::Mapping(map) => map.iter().find_map(|(name, child)| {
            if name.as_str() == Some(key) {
                Some(child)
            } else {
                find_key(child, key)
            }
        }),
        _ => None,
    }
}

#[test]
fn service_kind_schema_enumerates_exactly_the_runtime_result_effects() {
    let path = ryeos_engine::test_support::workspace_root()
        .join("bundles/core/.ai/node/engine/kinds/service/service.kind-schema.yaml");
    let value: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let effects: Vec<&str> = find_key(&value, "ui_result_effect").expect("ui_result_effect field")
        ["enum"]
        .as_sequence()
        .expect("ui_result_effect enum")
        .iter()
        .map(|entry| entry.as_str().unwrap())
        .collect();
    assert_eq!(
        effects,
        [
            "surface_opening",
            "resume_particular_view_set",
            "resume_particular_surface"
        ]
    );
}

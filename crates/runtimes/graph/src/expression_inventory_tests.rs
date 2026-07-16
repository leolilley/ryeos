use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::item_resolution::ResolutionRoots;
use ryeos_engine::kind_registry::{KindRegistry, KindSchema};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::resolution::run_resolution_pipeline;

fn visit_kind_files(base: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| {
            panic!(
                "read installable directory {}: {error}",
                directory.display()
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| {
            panic!(
                "enumerate installable directory {}: {error}",
                directory.display()
            )
        });
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("inspect installable path {}: {error}", path.display()));
        if file_type.is_dir() {
            visit_kind_files(base, &path, files);
        } else if file_type.is_file() {
            files.push(
                path.strip_prefix(base)
                    .expect("walked path must remain below its kind directory")
                    .to_path_buf(),
            );
        }
    }
}

fn installable_refs(
    kind: &str,
    schema: &KindSchema,
    bundle_roots: &[PathBuf],
) -> Vec<(String, PathBuf)> {
    let extensions = schema.extension_strs();
    let mut items = BTreeMap::<String, PathBuf>::new();

    for bundle_root in bundle_roots {
        let base = bundle_root.join(".ai").join(&schema.directory);
        if !base.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        visit_kind_files(&base, &base, &mut files);
        for relative in files {
            let relative = relative
                .to_str()
                .unwrap_or_else(|| panic!("installable path is not UTF-8: {}", relative.display()))
                .replace(std::path::MAIN_SEPARATOR, "/");
            let Some(extension) = extensions
                .iter()
                .copied()
                .find(|extension| relative.ends_with(*extension))
            else {
                continue;
            };
            let item_path = relative
                .strip_suffix(extension)
                .expect("matched extension must be removable");
            let canonical_ref = format!("{kind}:{item_path}");
            if let Some(previous) = items.insert(canonical_ref.clone(), bundle_root.clone()) {
                panic!(
                    "installable ref {canonical_ref} is duplicated across {} and {}",
                    previous.display(),
                    bundle_root.display()
                );
            }
        }
    }

    items.into_iter().collect()
}

/// Future-proof schema-driven inventory for bundle graphs. The current RyeOS
/// bundles may contain no graph definitions, but any graph added under the
/// live graph kind's directory/extensions is automatically resolved, verified,
/// composed, strictly loaded, and expression-compiled by this gate.
#[test]
fn installable_graph_expression_fields_compile_through_live_loaders() {
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let bundle_roots = vec![core.clone(), standard.clone()];
    let trust = ryeos_engine::test_support::live_trust_store();
    let kinds = KindRegistry::load_base(
        &[
            core.join(".ai/node/engine/kinds"),
            standard.join(".ai/node/engine/kinds"),
        ],
        &trust,
    )
    .expect("load signed live kind schemas");
    let graph_schema = kinds.get("graph").expect("graph kind schema");
    let items = installable_refs("graph", graph_schema, &bundle_roots);
    if items.is_empty() {
        return;
    }

    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let (parser_registry, duplicates) = ParserRegistry::load_base(&bundle_roots, &trust, &kinds)
        .expect("load signed live parser descriptors");
    assert!(
        duplicates.is_empty(),
        "live parser registry contains duplicate refs: {duplicates:?}"
    );
    let parsers = ParserDispatcher::new(parser_registry, handlers.clone());
    let composers =
        ComposerRegistry::from_kinds(&kinds, &handlers).expect("derive live composer registry");
    let roots = ResolutionRoots::from_flat(
        None,
        bundle_roots.iter().map(|root| root.join(".ai")).collect(),
    );

    for (item_ref, source_root) in items {
        let canonical = CanonicalRef::parse(&item_ref)
            .unwrap_or_else(|error| panic!("parse inventory ref {item_ref}: {error}"));
        let resolved =
            run_resolution_pipeline(&canonical, &kinds, &parsers, &roots, &trust, &composers)
                .unwrap_or_else(|error| {
                    panic!(
                        "resolve signed installable graph {item_ref} from {}: {error}",
                        source_root.display()
                    )
                });
        let yaml = serde_yaml::to_string(&resolved.composed.composed)
            .unwrap_or_else(|error| panic!("serialize composed graph {item_ref}: {error}"));
        crate::model::GraphDefinition::from_yaml(&yaml, Some(&item_ref))
            .unwrap_or_else(|error| panic!("load and compile graph {item_ref}: {error:#}"));
    }
}

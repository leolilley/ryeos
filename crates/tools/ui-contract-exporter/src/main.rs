//! Generate the TypeScript declarations for the Rust-owned browser boundary.
//!
//! `ryeos-client-base` owns the renderer-neutral model and must not acquire a
//! browser/code-generation dependency merely so this client can consume it.
//! This exporter therefore selects the explicit browser roots, computes their
//! local Rust type closure, and adds `Ts` markers only to a private staged
//! source tree. `ts-typegen-build` remains the serde-aware lowering and
//! rendering implementation; this file only supplies ownership and reachability.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::Visit;

const ROOT_TYPES: &[&str] = &[
    "BrowserSession",
    "BrowserViewport",
    "RyeOsEffectResult",
    "RyeOsEnvelope",
    "RyeOsEvent",
    "RyeOsKeyEvent",
    "SeatEvent",
    "UiInvocationRequest",
    "UiPresentationProducerObservation",
    "UiSeatControlPredecessor",
    "UiSeatControlRequest",
    "UiSeatProducerClaim",
];

const SHARED_CONTROL_TYPES: &[&str] = &[
    "UiSeatControlPredecessor",
    "UiSeatControlRequest",
    "UiSeatProducerClaim",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [argument] if argument == "--check" => true,
        arguments => {
            return Err(format!("usage: ui-contract-exporter [--check]; got {arguments:?}").into());
        }
    };
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let source_root = workspace_root.join("crates/clients/base/src");
    let stage_root = workspace_root.join("target/ui-contract-source");
    let generated_root = workspace_root.join("target/ui-contract-generated");
    let output_root = workspace_root.join("crates/clients/web/browser/generated");

    let mut files = read_rust_tree(&source_root)?;
    include_shared_control_types(
        &mut files,
        syn::parse_file(&fs::read_to_string(
            workspace_root.join("crates/clients/presentation-contract/src/seat_control.rs"),
        )?)?,
    )?;
    include_shared_transfer_types(
        &mut files,
        syn::parse_file(&fs::read_to_string(
            workspace_root.join("crates/clients/presentation-contract/src/transfer.rs"),
        )?)?,
    )?;
    let declarations = collect_declarations(&files)?;
    let selected = reachable_types(&declarations)?;
    lower_admission_payload(&mut files)?;
    write_contracts(
        files,
        selected,
        &stage_root,
        &generated_root,
        &output_root,
        check,
    )
}

fn include_shared_transfer_types(
    files: &mut BTreeMap<PathBuf, syn::File>,
    mut transfer: syn::File,
) -> Result<(), Box<dyn std::error::Error>> {
    transfer
        .items
        .retain(|item| matches!(item, syn::Item::Struct(_) | syn::Item::Enum(_)));
    files.insert(PathBuf::from("shared_transfer.rs"), transfer);
    files
        .get_mut(Path::new("lib.rs"))
        .ok_or("client-base root is absent")?
        .items
        .push(syn::parse_quote!(
            pub mod shared_transfer;
        ));
    for parsed in files.values_mut() {
        normalize_transfer_imports(parsed);
        for item in &mut parsed.items {
            if let syn::Item::Use(item) = item
                && let syn::UseTree::Path(root) = &mut item.tree
                && root.ident == "ryeos_ui_presentation_contract"
                && let syn::UseTree::Path(module) = root.tree.as_mut()
                && module.ident == "transfer"
            {
                root.ident = syn::Ident::new("crate", root.ident.span());
                module.ident = syn::Ident::new("shared_transfer", module.ident.span());
            }
        }
    }
    Ok(())
}

fn lower_admission_payload(
    files: &mut BTreeMap<PathBuf, syn::File>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Serde flattens a struct newtype under an internally tagged variant.
    // Lower that shape in the private stage because typegen otherwise drops
    // its payload. Fields still come exclusively from the Rust declaration.
    let admission_fields = files
        .values()
        .flat_map(|file| &file.items)
        .find_map(|item| match item {
            syn::Item::Struct(item) if item.ident == "UiPreparedAdmissionOperation" => {
                Some(item.fields.clone())
            }
            _ => None,
        })
        .ok_or("admission wire declaration is absent")?;
    if !matches!(admission_fields, syn::Fields::Named(_)) {
        return Err("admission wire payload must have named fields".into());
    }
    let mut lowered = 0;
    for file in files.values_mut() {
        for item in &mut file.items {
            if let syn::Item::Enum(item) = item
                && item.ident == "UiPreparedPresentationOperationBody"
            {
                for variant in &mut item.variants {
                    if variant.ident == "Admission" {
                        let expected_payload = matches!(&variant.fields,
                            syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1
                                && matches!(&fields.unnamed[0].ty, syn::Type::Path(path)
                                    if path.qself.is_none() && path.path.is_ident("UiPreparedAdmissionOperation")));
                        if !expected_payload {
                            return Err("admission variant no longer has the expected struct-newtype wire shape".into());
                        }
                        variant.fields = admission_fields.clone();
                        lowered += 1;
                    }
                }
            }
        }
    }

    if lowered != 1 {
        return Err("admission wire variant must have exactly one declaration".into());
    }
    Ok(())
}

fn write_contracts(
    files: BTreeMap<PathBuf, syn::File>,
    selected: BTreeSet<String>,
    stage_root: &Path,
    generated_root: &Path,
    output_root: &Path,
    check: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let declarations = collect_declarations(&files)?;
    if stage_root.exists() {
        fs::remove_dir_all(stage_root)?;
    }
    for (relative, parsed) in files {
        let destination = stage_root.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let staged = mark_selected(parsed, &selected);
        fs::write(destination, prettyplease::unparse(&staged))?;
    }

    if generated_root.exists() {
        fs::remove_dir_all(generated_root)?;
    }
    let mut generator = ts_typegen_build::Config::new()
        .scan(stage_root)
        .output(generated_root)
        // `serde-wasm-bindgen` presents Rust 64-bit integers as JS BigInt.
        // External JSON remains `unknown` until runtime narrowing constructs a
        // Rust event/effect result; it does not reuse that ABI assumption.
        .bigint(ts_typegen_build::BigInt::Bigint)
        .unknown_type(ts_typegen_build::config::UnknownStyle::Unknown)
        .optional_style(ts_typegen_build::OptionalStyle::Nullable)
        .header("/* Rust-owned browser wire contract. */");
    for name in &selected {
        if let Some(mapping) = declarations
            .get(name)
            .and_then(|declaration| declaration.wire_mapping.as_deref())
        {
            generator = generator.map_type(name, mapping);
        }
    }
    generator.run()?;
    normalize_generated_types(generated_root)?;

    if check {
        if read_text_tree(generated_root)? != read_text_tree(output_root)? {
            return Err("generated browser contracts are stale; run ui-contract-exporter".into());
        }
    } else {
        if output_root.exists() {
            fs::remove_dir_all(output_root)?;
        }
        fs::rename(generated_root, output_root)?;
    }

    Ok(())
}

fn normalize_generated_types(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for relative in read_text_tree(root)?.keys() {
        let path = root.join(relative);
        let source = fs::read_to_string(&path)?;
        let normalized = source
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, normalized)?;
    }
    Ok(())
}

fn read_text_tree(root: &Path) -> Result<BTreeMap<PathBuf, String>, Box<dyn std::error::Error>> {
    fn walk(
        root: &Path,
        directory: &Path,
        out: &mut BTreeMap<PathBuf, String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !directory.exists() {
            return Ok(());
        }
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else {
                out.insert(
                    path.strip_prefix(root)?.to_path_buf(),
                    fs::read_to_string(path)?,
                );
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

#[derive(Clone)]
struct Declaration {
    serializable: bool,
    references: BTreeSet<String>,
    wire_mapping: Option<String>,
}

fn read_rust_tree(root: &Path) -> Result<BTreeMap<PathBuf, syn::File>, Box<dyn std::error::Error>> {
    fn walk(
        root: &Path,
        directory: &Path,
        out: &mut BTreeMap<PathBuf, syn::File>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let relative = path.strip_prefix(root)?.to_path_buf();
                let source = fs::read_to_string(&path)?;
                out.insert(relative, syn::parse_file(&source)?);
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn collect_declarations(
    files: &BTreeMap<PathBuf, syn::File>,
) -> Result<BTreeMap<String, Declaration>, Box<dyn std::error::Error>> {
    fn collect_items(
        items: &[syn::Item],
        declarations: &mut BTreeMap<String, Declaration>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for item in items {
            if let syn::Item::Macro(item) = item
                && (item.mac.path.is_ident("id_type") || item.mac.path.is_ident("presentation_id"))
            {
                let name = syn::parse2::<syn::Ident>(item.mac.tokens.clone())?.to_string();
                declarations.insert(
                    name,
                    Declaration {
                        serializable: true,
                        references: BTreeSet::new(),
                        // `id_type!` is a serde-transparent u64 newtype. The
                        // WASM ABI exposes it as BigInt, exactly like its
                        // primitive field.
                        wire_mapping: Some(if item.mac.path.is_ident("presentation_id") {
                            "string".to_string()
                        } else {
                            "bigint".to_string()
                        }),
                    },
                );
                continue;
            }
            let (name, attrs, references) = match item {
                syn::Item::Struct(item) => {
                    let mut references = TypeReferences::default();
                    references.visit_item_struct(item);
                    (item.ident.to_string(), &item.attrs, references.names)
                }
                syn::Item::Enum(item) => {
                    let mut references = TypeReferences::default();
                    references.visit_item_enum(item);
                    (item.ident.to_string(), &item.attrs, references.names)
                }
                syn::Item::Mod(module) => {
                    if let Some((_, nested)) = &module.content {
                        collect_items(nested, declarations)?;
                    }
                    continue;
                }
                _ => continue,
            };
            if declarations.contains_key(&name) {
                return Err(format!("browser contract type name `{name}` is ambiguous").into());
            }
            declarations.insert(
                name,
                Declaration {
                    serializable: derives(attrs, "Serialize"),
                    references,
                    wire_mapping: None,
                },
            );
        }
        Ok(())
    }

    let mut declarations = BTreeMap::new();
    for parsed in files.values() {
        collect_items(&parsed.items, &mut declarations)?;
    }
    Ok(declarations)
}

/// The data-only contract is canonical; client-base only re-exports it. Stage
/// its exact declarations at that existing public browser module path so this
/// ownership move neither duplicates an authored schema nor changes TS imports.
fn include_shared_control_types(
    files: &mut BTreeMap<PathBuf, syn::File>,
    shared: syn::File,
) -> Result<(), Box<dyn std::error::Error>> {
    let declarations = shared
        .items
        .into_iter()
        .filter(|item| {
            let name = match item {
                syn::Item::Struct(item) => item.ident.to_string(),
                syn::Item::Enum(item) => item.ident.to_string(),
                _ => return false,
            };
            SHARED_CONTROL_TYPES.contains(&name.as_str())
        })
        .collect::<Vec<_>>();
    if declarations.len() != SHARED_CONTROL_TYPES.len() {
        return Err("shared producer-control browser declaration set is incomplete".into());
    }
    let binding = files
        .get_mut(Path::new("ui/binding.rs"))
        .ok_or("client-base binding module is absent")?;
    binding.items.retain(|item| {
        !matches!(item, syn::Item::Use(item)
            if matches!(&item.tree, syn::UseTree::Path(root)
                if root.ident == "ryeos_ui_presentation_contract"
                    && matches!(root.tree.as_ref(), syn::UseTree::Path(module)
                        if module.ident == "seat_control")))
    });
    binding.items.extend(declarations);
    Ok(())
}

fn reachable_types(
    declarations: &BTreeMap<String, Declaration>,
) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let mut selected = BTreeSet::new();
    let mut pending = ROOT_TYPES
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    while let Some(name) = pending.pop() {
        if !selected.insert(name.clone()) {
            continue;
        }
        let declaration = declarations
            .get(&name)
            .ok_or_else(|| format!("browser contract root/reference `{name}` was not found"))?;
        if !declaration.serializable {
            return Err(format!("browser contract type `{name}` does not derive Serialize").into());
        }
        pending.extend(
            declaration
                .references
                .iter()
                .filter(|reference| declarations.contains_key(*reference))
                .cloned(),
        );
    }
    Ok(selected)
}

fn mark_selected(mut parsed: syn::File, selected: &BTreeSet<String>) -> syn::File {
    fn mark(items: &mut [syn::Item], selected: &BTreeSet<String>) {
        for item in items {
            match item {
                syn::Item::Struct(item) if selected.contains(&item.ident.to_string()) => {
                    item.attrs.push(syn::parse_quote!(#[derive(Ts)]));
                }
                syn::Item::Enum(item) if selected.contains(&item.ident.to_string()) => {
                    item.attrs.push(syn::parse_quote!(#[derive(Ts)]));
                }
                syn::Item::Mod(module) => {
                    if let Some((_, nested)) = &mut module.content {
                        mark(nested, selected);
                    }
                }
                _ => {}
            }
        }
    }
    mark(&mut parsed.items, selected);
    parsed
}

fn derives(attrs: &[syn::Attribute], sought: &str) -> bool {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("derive"))
        .any(|attr| {
            let mut found = false;
            let _ = attr.parse_nested_meta(|meta| {
                found |= meta
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == sought);
                Ok(())
            });
            found
        })
}

#[derive(Default)]
struct TypeReferences {
    names: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for TypeReferences {
    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        if let Some(segment) = path.path.segments.last() {
            self.names.insert(segment.ident.to_string());
        }
        syn::visit::visit_type_path(self, path);
    }
}

// Split only transfer members out of grouped external imports. Other shared
// owners keep their original path; staging must not redirect their authority.
fn normalize_transfer_imports(file: &mut syn::File) {
    let mut additions = Vec::new();
    for item in &mut file.items {
        let syn::Item::Use(import) = item else {
            continue;
        };
        let template = import.clone();
        let syn::UseTree::Path(root) = &mut import.tree else {
            continue;
        };
        if root.ident != "ryeos_ui_presentation_contract" {
            continue;
        }
        let syn::UseTree::Group(group) = root.tree.as_mut() else {
            continue;
        };
        let mut remaining = syn::punctuated::Punctuated::new();
        for tree in std::mem::take(&mut group.items) {
            if let syn::UseTree::Path(path) = &tree
                && path.ident == "transfer"
            {
                let tail = &path.tree;
                let mut staged = template.clone();
                staged.tree = syn::parse_quote!(crate::shared_transfer::#tail);
                additions.push(syn::Item::Use(staged));
            } else {
                remaining.push(tree);
            }
        }
        group.items = remaining;
    }
    file.items.retain(|item| {
        !matches!(item,
        syn::Item::Use(import) if matches!(&import.tree,
            syn::UseTree::Path(root) if matches!(root.tree.as_ref(),
                syn::UseTree::Group(group) if group.items.is_empty())))
    });
    file.items.extend(additions);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_lowering_refuses_missing_or_changed_wire_owners() {
        for source in [
            "struct UiPreparedAdmissionOperation { request: Request }",
            "struct UiPreparedAdmissionOperation { request: Request } enum UiPreparedPresentationOperationBody { Composition { request: Request } }",
            "struct UiPreparedAdmissionOperation { request: Request } enum UiPreparedPresentationOperationBody { Admission(OtherPayload) }",
            "struct UiPreparedAdmissionOperation { request: Request } enum UiPreparedPresentationOperationBody { Admission { request: Request } }",
            "struct UiPreparedAdmissionOperation(Request); enum UiPreparedPresentationOperationBody { Admission(UiPreparedAdmissionOperation) }",
        ] {
            let mut files = BTreeMap::from([(
                PathBuf::from("binding.rs"),
                syn::parse_file(source).unwrap(),
            )]);
            assert!(
                lower_admission_payload(&mut files).is_err(),
                "must refuse changed wire declaration: {source}"
            );
        }
    }

    #[test]
    fn transfer_staging_retains_canonical_data_but_not_runtime_implementation() {
        let mut files = BTreeMap::from([(
            PathBuf::from("lib.rs"),
            syn::parse_file("use ryeos_ui_presentation_contract::transfer::TransferIdentity;")
                .unwrap(),
        )]);
        let shared = syn::parse_file(include_str!(
            "../../../clients/presentation-contract/src/transfer.rs"
        ))
        .unwrap();
        include_shared_transfer_types(&mut files, shared).unwrap();
        let staged = &files[Path::new("shared_transfer.rs")];
        assert!(
            staged
                .items
                .iter()
                .all(|item| matches!(item, syn::Item::Struct(_) | syn::Item::Enum(_)))
        );
        let declarations = collect_declarations(&files).unwrap();
        for name in [
            "TransferIdentity",
            "TransferSeat",
            "TransferEvidence",
            "TransferPhase",
            "TransferEventPayload",
        ] {
            assert!(
                declarations[name].serializable,
                "{name} must come from the shared wire declaration"
            );
        }
        let root = prettyplease::unparse(&files[Path::new("lib.rs")]);
        assert!(root.contains("crate::shared_transfer::TransferIdentity"));
        assert!(root.contains("pub mod shared_transfer"));
        assert!(
            include_shared_transfer_types(&mut BTreeMap::new(), syn::parse_file("").unwrap())
                .is_err()
        );
    }

    #[test]
    fn admission_lowering_copies_the_rust_payload_without_changing_other_variants() {
        let file = syn::parse_file("struct UiPreparedAdmissionOperation { request: Request, sequence: u64 } enum UiPreparedPresentationOperationBody { Admission(UiPreparedAdmissionOperation), Composition { request: CompositionRequest } }").unwrap();
        let mut files = BTreeMap::from([(PathBuf::from("binding.rs"), file)]);
        lower_admission_payload(&mut files).unwrap();
        let file = &files[Path::new("binding.rs")];
        let syn::Item::Struct(payload) = &file.items[0] else {
            panic!("payload absent")
        };
        let syn::Item::Enum(body) = &file.items[1] else {
            panic!("body absent")
        };
        assert_eq!(body.variants[0].fields, payload.fields);
        assert!(
            matches!(&body.variants[1].fields, syn::Fields::Named(fields) if fields.named.len() == 1)
        );
        assert!(lower_admission_payload(&mut BTreeMap::new()).is_err());
    }

    #[test]
    fn presentation_ids_remain_strings_while_numeric_ids_remain_bigints() {
        let files = BTreeMap::from([(
            PathBuf::from("ids.rs"),
            syn::parse_file("presentation_id!(UiPresentationId); id_type!(TileId);").unwrap(),
        )]);
        let declarations = collect_declarations(&files).unwrap();
        assert_eq!(
            declarations["UiPresentationId"].wire_mapping.as_deref(),
            Some("string")
        );
        assert_eq!(
            declarations["TileId"].wire_mapping.as_deref(),
            Some("bigint")
        );
    }

    #[test]
    fn grouped_transfer_imports_preserve_other_contract_owners() {
        let mut file = syn::parse_file("use ryeos_ui_presentation_contract::{transfer::TransferIdentity, seat_control::UiSeatProducer};").unwrap();
        normalize_transfer_imports(&mut file);
        let output = prettyplease::unparse(&file);
        assert!(output.contains("crate::shared_transfer::TransferIdentity"));
        assert!(output.contains("seat_control::UiSeatProducer"));
        assert!(!output.contains("crate::seat_control"));
    }

    #[test]
    fn shared_control_declarations_keep_the_existing_binding_module_and_root_closure() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut files = read_rust_tree(&root.join("crates/clients/base/src")).unwrap();
        let shared = syn::parse_file(include_str!(
            "../../../clients/presentation-contract/src/seat_control.rs"
        ))
        .unwrap();
        include_shared_control_types(&mut files, shared).unwrap();
        let declarations = collect_declarations(&files).unwrap();
        let selected = reachable_types(&declarations).unwrap();
        let binding = &files[Path::new("ui/binding.rs")];
        for name in SHARED_CONTROL_TYPES {
            assert!(selected.contains(*name));
            assert_eq!(
                binding
                    .items
                    .iter()
                    .filter(|item| { matches!(item, syn::Item::Enum(item) if item.ident == *name) })
                    .count(),
                1,
                "{name} must retain its public browser module with one canonical declaration"
            );
        }
        assert!(!declarations.contains_key("UiSeatProducer"));
        assert!(!declarations.contains_key("UiSeatProducerState"));
    }

    #[test]
    fn absent_shared_control_declaration_is_an_error_not_an_unknown_wire_type() {
        let mut files =
            BTreeMap::from([(PathBuf::from("ui/binding.rs"), syn::parse_file("").unwrap())]);
        assert!(include_shared_control_types(&mut files, syn::parse_file("").unwrap()).is_err());
    }
}

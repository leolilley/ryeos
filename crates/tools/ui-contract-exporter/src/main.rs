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

    let files = read_rust_tree(&source_root)?;
    let declarations = collect_declarations(&files)?;
    let selected = reachable_types(&declarations)?;

    if stage_root.exists() {
        fs::remove_dir_all(&stage_root)?;
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
        fs::remove_dir_all(&generated_root)?;
    }
    let mut generator = ts_typegen_build::Config::new()
        .scan(&stage_root)
        .output(&generated_root)
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
    normalize_generated_types(&generated_root)?;

    if check {
        if read_text_tree(&generated_root)? != read_text_tree(&output_root)? {
            return Err("generated browser contracts are stale; run ui-contract-exporter".into());
        }
    } else {
        if output_root.exists() {
            fs::remove_dir_all(&output_root)?;
        }
        fs::rename(&generated_root, &output_root)?;
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
                && item.mac.path.is_ident("id_type")
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
                        wire_mapping: Some("bigint".to_string()),
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

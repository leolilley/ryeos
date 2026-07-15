//! Strict, provenance-bearing configuration snapshots for runtime launch preparation.
//!
//! This loader is deliberately separate from the permissive runtime config loader:
//! every contributor must be signed, trust/space policy is enforced per signed
//! launch contract, paths never cross the handler boundary, and all output is
//! bounded before a preparer can be spawned.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use ryeos_handler_protocol::{
    ItemSpaceWire, LaunchConfigContributorWire, LaunchConfigEntryWire,
    LaunchConfigSnapshotWire, TrustClassWire,
};
use serde_json::{Map, Value};

use crate::contracts::{ItemSpace, TrustClass as ContractTrustClass};
use crate::error::EngineError;
use crate::item_resolution::{parse_signature_header, ResolutionRoot, ResolutionRoots};
use crate::kind_registry::{ExtensionSpec, KindRegistry};
use crate::parsers::dispatcher::ParserDispatcher;
use crate::runtime_registry::{
    ConfigMergeMode, LaunchConfigInputDecl, LaunchItemSpace,
};
use crate::trust::{content_hash_after_signature, verify_item_signature_with_hash, TrustStore};
use crate::resolution::TrustClass;

const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_CONTRIBUTORS: usize = 16;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_AGGREGATE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct Layer {
    value: Value,
    contributor: LaunchConfigContributorWire,
}

pub fn load_launch_config_snapshots(
    declarations: &BTreeMap<String, LaunchConfigInputDecl>,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
) -> Result<BTreeMap<String, LaunchConfigSnapshotWire>, EngineError> {
    let config_schema = kinds.get("config").ok_or_else(|| invalid(
        "launch_contract.config_inputs",
        "config kind is not registered",
    ))?;
    let mut result = BTreeMap::new();
    let mut aggregate_bytes = 0usize;

    for (name, declaration) in declarations {
        let snapshot = match declaration {
            LaunchConfigInputDecl::Item {
                id,
                required,
                merge,
                allowed_spaces,
                allowed_trust,
            } => {
                let mut layers = Vec::new();
                let mut first_match_selected = false;
                for root in &roots.ordered {
                    if let Some((path, extension)) = item_path(root, id, &config_schema.extensions)? {
                        if *merge == ConfigMergeMode::FirstMatch && first_match_selected {
                            continue;
                        }
                        layers.push(load_layer(
                            &path,
                            id,
                            root,
                            extension,
                            allowed_spaces,
                            allowed_trust,
                            parsers,
                            trust_store,
                        )?);
                        if *merge == ConfigMergeMode::FirstMatch {
                            first_match_selected = true;
                        }
                    }
                }
                if layers.is_empty() {
                    if *required {
                        return Err(EngineError::LaunchConfigMissing {
                            input: name.clone(),
                            detail: format!("required config item `{id}` is absent"),
                        });
                    }
                    LaunchConfigSnapshotWire::Item {
                        present: false,
                        value: None,
                        value_digest: None,
                        contributors: Vec::new(),
                    }
                } else {
                    let (value, contributors) = merge_layers(layers, *merge, name)?;
                    let value_digest = value_digest(&value, name, &mut aggregate_bytes)?;
                    LaunchConfigSnapshotWire::Item {
                        present: true,
                        value: Some(value),
                        value_digest: Some(value_digest),
                        contributors,
                    }
                }
            }
            LaunchConfigInputDecl::Catalog {
                prefix,
                required,
                entry_merge,
                allowed_spaces,
                allowed_trust,
            } => {
                let mut grouped: HashMap<String, Vec<Layer>> = HashMap::new();
                for root in &roots.ordered {
                    let catalog_root = root.ai_root.join("config").join(prefix);
                    match std::fs::symlink_metadata(&catalog_root) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            return Err(invalid(name, format!("catalog root cannot be a symlink: {}", catalog_root.display())));
                        }
                        Ok(metadata) if !metadata.is_dir() => {
                            return Err(invalid(name, format!("catalog root is not a directory: {}", catalog_root.display())));
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(invalid(name, format!("inspect {}: {error}", catalog_root.display()))),
                    }
                    validate_config_directory(&catalog_root, &root.ai_root, name)?;
                    let mut root_ids = HashMap::<String, (PathBuf, &ExtensionSpec)>::new();
                    for path in collect_catalog_files(&catalog_root, &config_schema.extensions)? {
                        let extension = extension_for(&path, &config_schema.extensions).ok_or_else(|| {
                            invalid(name, format!("unsupported config extension: {}", path.display()))
                        })?;
                        let relative = path.strip_prefix(root.ai_root.join("config")).map_err(|_| {
                            invalid(name, format!("catalog entry escaped config root: {}", path.display()))
                        })?;
                        let mut canonical_id = relative
                            .to_str()
                            .ok_or_else(|| {
                                invalid(
                                    name,
                                    format!(
                                        "catalog entry ID is not valid UTF-8: {}",
                                        path.display()
                                    ),
                                )
                            })?
                            .replace('\\', "/");
                        canonical_id.truncate(canonical_id.len() - extension.ext.len());
                        if let Some((first, _)) = root_ids.insert(
                            canonical_id.clone(),
                            (path.clone(), extension),
                        ) {
                            return Err(invalid(
                                name,
                                format!(
                                    "multiple registered extensions define config `{canonical_id}` in root `{}`: {} and {}",
                                    root.label,
                                    first.display(),
                                    path.display(),
                                ),
                            ));
                        }
                    }
                    let mut root_ids: Vec<_> = root_ids.into_iter().collect();
                    root_ids.sort_by(|left, right| left.0.cmp(&right.0));
                    for (canonical_id, (path, extension)) in root_ids {
                        if *entry_merge == ConfigMergeMode::FirstMatch
                            && grouped.contains_key(&canonical_id)
                        {
                            continue;
                        }
                        let layer = load_layer(
                            &path,
                            &canonical_id,
                            root,
                            extension,
                            allowed_spaces,
                            allowed_trust,
                            parsers,
                            trust_store,
                        )?;
                        grouped.entry(canonical_id).or_default().push(layer);
                    }
                }
                if grouped.is_empty() && *required {
                    return Err(EngineError::LaunchConfigMissing {
                        input: name.clone(),
                        detail: format!("required config catalog `{prefix}` is empty"),
                    });
                }
                if grouped.len() > MAX_CATALOG_ENTRIES {
                    return Err(invalid(name, format!("catalog exceeds {MAX_CATALOG_ENTRIES} entries")));
                }
                let mut entries = BTreeMap::new();
                let mut ids: Vec<_> = grouped.into_iter().collect();
                ids.sort_by(|left, right| left.0.cmp(&right.0));
                for (canonical_id, layers) in ids {
                    let (value, contributors) = merge_layers(layers, *entry_merge, name)?;
                    let digest = value_digest(&value, name, &mut aggregate_bytes)?;
                    entries.insert(canonical_id, LaunchConfigEntryWire {
                        value,
                        value_digest: digest,
                        contributors,
                    });
                }
                LaunchConfigSnapshotWire::Catalog { entries }
            }
        };
        result.insert(name.clone(), snapshot);
    }
    Ok(result)
}

fn item_path<'a>(
    root: &ResolutionRoot,
    id: &str,
    extensions: &'a [ExtensionSpec],
) -> Result<Option<(PathBuf, &'a ExtensionSpec)>, EngineError> {
    let mut found: Option<(PathBuf, &'a ExtensionSpec)> = None;
    for extension in extensions {
        let path = root.ai_root.join("config").join(format!("{id}{}", extension.ext));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                if let Some((first, _)) = &found {
                    return Err(invalid(
                        id,
                        format!(
                            "multiple registered extensions define config `{id}` in root `{}`: {} and {}",
                            root.label,
                            first.display(),
                            path.display(),
                        ),
                    ));
                }
                found = Some((path, extension));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(invalid(id, format!("inspect {}: {error}", path.display())));
            }
        }
    }
    Ok(found)
}

fn load_layer(
    path: &Path,
    canonical_id: &str,
    root: &ResolutionRoot,
    extension: &ExtensionSpec,
    allowed_spaces: &[LaunchItemSpace],
    allowed_trust: &[TrustClass],
    parsers: &ParserDispatcher,
    trust_store: &TrustStore,
) -> Result<Layer, EngineError> {
    validate_config_path(path, &root.ai_root.join("config"), canonical_id)?;
    if !valid_root_label(&root.label) {
        return Err(invalid(
            canonical_id,
            format!("resolution root label `{}` is invalid", root.label),
        ));
    }
    let declared_space = launch_space(root.space);
    if !allowed_spaces.contains(&declared_space) {
        return Err(EngineError::LaunchConfigPolicyDenied {
            code: "launch_config_space_not_allowed".to_owned(),
            input: canonical_id.to_owned(),
            detail: format!("source space {:?} is not allowed", root.space),
        });
    }
    let content = std::fs::read_to_string(path)
        .map_err(|error| invalid(canonical_id, format!("read {}: {error}", path.display())))?;
    let header = parse_signature_header(&content, &extension.signature).ok_or_else(|| {
        invalid(canonical_id, format!("unsigned launch config contributor {}", path.display()))
    })?;
    let content_digest = content_hash_after_signature(&content, &extension.signature).ok_or_else(|| {
        invalid(canonical_id, format!("cannot compute signed content digest for {}", path.display()))
    })?;
    let (contract_trust, _) = verify_item_signature_with_hash(&content_digest, &header, trust_store)
        .map_err(|error| invalid(canonical_id, format!("signature verification failed: {error}")))?;
    let trust_class = match (contract_trust, root.space) {
        (ContractTrustClass::Trusted, ItemSpace::Bundle) => TrustClass::TrustedBundle,
        (ContractTrustClass::Trusted, ItemSpace::Project) => TrustClass::TrustedProject,
        (ContractTrustClass::Untrusted, _) => TrustClass::UntrustedProject,
        (ContractTrustClass::Unsigned, _) => TrustClass::Unsigned,
    };
    if !allowed_trust.contains(&trust_class) {
        return Err(EngineError::LaunchConfigPolicyDenied {
            code: "launch_config_untrusted".to_owned(),
            input: canonical_id.to_owned(),
            detail: format!("trust class {trust_class:?} is not allowed"),
        });
    }
    let value = parsers.dispatch(
        &extension.parser,
        &content,
        Some(path),
        &extension.signature,
    )?;
    Ok(Layer {
        value,
        contributor: LaunchConfigContributorWire {
            space: item_space_wire(root.space),
            root_label: root.label.clone(),
            canonical_id: canonical_id.to_owned(),
            content_digest,
            trust_class: trust_wire(trust_class),
        },
    })
}

fn validate_config_path(
    path: &Path,
    config_root: &Path,
    context: &str,
) -> Result<(), EngineError> {
    let ai_root = config_root.parent().ok_or_else(|| {
        invalid(
            context,
            format!("config root has no declared AI root: {}", config_root.display()),
        )
    })?;
    validate_config_directory(config_root, ai_root, context)?;
    let relative = path.strip_prefix(config_root).map_err(|_| {
        invalid(context, format!("config contributor escaped config root: {}", path.display()))
    })?;
    let canonical_root = std::fs::canonicalize(config_root)
        .map_err(|error| invalid(context, format!("resolve {}: {error}", config_root.display())))?;
    let mut cursor = config_root.to_path_buf();
    for component in relative.components() {
        cursor.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&cursor)
            .map_err(|error| invalid(context, format!("inspect {}: {error}", cursor.display())))?;
        if metadata.file_type().is_symlink() {
            return Err(invalid(context, format!("config paths cannot contain symlinks: {}", cursor.display())));
        }
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| invalid(context, format!("inspect {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(invalid(context, format!("config contributor is not a regular file: {}", path.display())));
    }
    let canonical_path = std::fs::canonicalize(path)
        .map_err(|error| invalid(context, format!("resolve {}: {error}", path.display())))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(invalid(context, format!("config contributor escaped config root: {}", path.display())));
    }
    Ok(())
}

fn validate_config_directory(
    directory: &Path,
    ai_root: &Path,
    context: &str,
) -> Result<(), EngineError> {
    let relative = directory.strip_prefix(ai_root).map_err(|_| {
        invalid(
            context,
            format!("config directory escaped declared AI root: {}", directory.display()),
        )
    })?;
    let root_metadata = std::fs::symlink_metadata(ai_root)
        .map_err(|error| invalid(context, format!("inspect {}: {error}", ai_root.display())))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(invalid(
            context,
            format!("declared AI root must be a real directory: {}", ai_root.display()),
        ));
    }
    let canonical_ai_root = std::fs::canonicalize(ai_root)
        .map_err(|error| invalid(context, format!("resolve {}: {error}", ai_root.display())))?;
    let mut cursor = ai_root.to_path_buf();
    for component in relative.components() {
        cursor.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&cursor)
            .map_err(|error| invalid(context, format!("inspect {}: {error}", cursor.display())))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid(
                context,
                format!("config directories must be real directories: {}", cursor.display()),
            ));
        }
    }
    let canonical_directory = std::fs::canonicalize(directory)
        .map_err(|error| invalid(context, format!("resolve {}: {error}", directory.display())))?;
    if !canonical_directory.starts_with(canonical_ai_root) {
        return Err(invalid(
            context,
            format!("config directory escaped declared AI root: {}", directory.display()),
        ));
    }
    Ok(())
}

fn merge_layers(
    mut layers: Vec<Layer>,
    mode: ConfigMergeMode,
    name: &str,
) -> Result<(Value, Vec<LaunchConfigContributorWire>), EngineError> {
    if layers.len() > MAX_CONTRIBUTORS {
        return Err(invalid(name, format!("config value exceeds {MAX_CONTRIBUTORS} contributors")));
    }
    if mode == ConfigMergeMode::FirstMatch {
        let first = layers.remove(0);
        return Ok((first.value, vec![first.contributor]));
    }
    layers.reverse();
    let mut merged = Value::Object(Map::new());
    let mut contributors = Vec::with_capacity(layers.len());
    for layer in layers {
        merged = crate::config_loading::deep_merge(merged, layer.value);
        contributors.push(layer.contributor);
    }
    Ok((merged, contributors))
}

fn collect_catalog_files(root: &Path, extensions: &[ExtensionSpec]) -> Result<Vec<PathBuf>, EngineError> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&directory)
            .map_err(|error| invalid("catalog", format!("read {}: {error}", directory.display())))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| invalid("catalog", error.to_string()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type().map_err(|error| invalid("catalog", error.to_string()))?;
            if file_type.is_symlink() {
                return Err(invalid("catalog", format!("symlink is forbidden: {}", entry.path().display())));
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() && extension_for(&entry.path(), extensions).is_some() {
                files.push(entry.path());
                if files.len() > MAX_CATALOG_ENTRIES * MAX_CONTRIBUTORS {
                    return Err(invalid("catalog", "catalog contributor cap exceeded"));
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

fn extension_for<'a>(path: &Path, extensions: &'a [ExtensionSpec]) -> Option<&'a ExtensionSpec> {
    let name = path.file_name()?.to_str()?;
    extensions.iter().find(|extension| name.ends_with(&extension.ext))
}

fn value_digest(value: &Value, name: &str, aggregate: &mut usize) -> Result<String, EngineError> {
    let canonical = lillux::canonical_json(value)
        .map_err(|error| invalid(name, format!("config value cannot be canonicalized: {error}")))?;
    if canonical.len() > MAX_VALUE_BYTES {
        return Err(invalid(name, format!("canonical config value exceeds {MAX_VALUE_BYTES} bytes")));
    }
    *aggregate = aggregate.saturating_add(canonical.len());
    if *aggregate > MAX_AGGREGATE_BYTES {
        return Err(invalid(name, format!("aggregate config snapshots exceed {MAX_AGGREGATE_BYTES} bytes")));
    }
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

fn valid_root_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 128
        && label.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

fn launch_space(space: ItemSpace) -> LaunchItemSpace {
    match space {
        ItemSpace::Bundle => LaunchItemSpace::Bundle,
        ItemSpace::Project => LaunchItemSpace::Project,
    }
}

fn item_space_wire(space: ItemSpace) -> ItemSpaceWire {
    match space {
        ItemSpace::Bundle => ItemSpaceWire::Bundle,
        ItemSpace::Project => ItemSpaceWire::Project,
    }
}

fn trust_wire(trust: TrustClass) -> TrustClassWire {
    match trust {
        TrustClass::TrustedBundle => TrustClassWire::TrustedBundle,
        TrustClass::TrustedProject => TrustClassWire::TrustedProject,
        TrustClass::UntrustedProject => TrustClassWire::UntrustedProject,
        TrustClass::Unsigned => TrustClassWire::Unsigned,
    }
}

fn invalid(context: impl Into<String>, reason: impl Into<String>) -> EngineError {
    EngineError::InvalidRuntimeConfig {
        path: context.into(),
        reason: reason.into(),
    }
}

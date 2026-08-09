//! Strict, provenance-bearing configuration snapshots for runtime launch preparation.
//!
//! This loader is deliberately separate from the permissive runtime config loader:
//! every contributor must be signed, trust/space policy is enforced per signed
//! launch contract, paths never cross the handler boundary, and all output is
//! bounded before a preparer can be spawned.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ryeos_handler_protocol::{
    ItemSpaceWire, LaunchConfigContributorWire, LaunchConfigEntryWire, LaunchConfigSnapshotWire,
    TrustClassWire,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::contracts::{ItemSpace, TrustClass as ContractTrustClass};
use crate::error::EngineError;
use crate::item_resolution::{ResolutionRoot, ResolutionRoots, parse_signature_header};
use crate::kind_registry::{ExtensionSpec, KindRegistry};
use crate::parsers::dispatcher::ParserDispatcher;
use crate::resolution::TrustClass;
use crate::runtime_registry::{ConfigMergeMode, LaunchConfigInputDecl, LaunchItemSpace};
use crate::trust::{TrustStore, content_hash_after_signature, verify_item_signature_with_hash};

const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_CONTRIBUTORS: usize = 16;
const MAX_CATALOG_FILES: usize = MAX_CATALOG_ENTRIES * MAX_CONTRIBUTORS;
const MAX_CATALOG_TRAVERSAL_ENTRIES: usize = MAX_CATALOG_FILES * 2;
const MAX_CATALOG_TRAVERSAL_DEPTH: usize = 64;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_AGGREGATE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = MAX_VALUE_BYTES as u64;

#[derive(Debug)]
struct Layer {
    value: Value,
    contributor: LaunchConfigContributorWire,
    source_hash: String,
}

struct LayerLoadPolicy<'a> {
    allowed_spaces: &'a [LaunchItemSpace],
    allowed_trust: &'a [TrustClass],
    parsers: &'a ParserDispatcher,
    node_parsers: &'a ParserDispatcher,
    trust_store: &'a TrustStore,
    node_trust_store: &'a TrustStore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum LaunchConfigDependencyState {
    Absent,
    File {
        source_hash: String,
    },
    Catalog {
        entries: Vec<LaunchConfigCatalogEntryState>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchConfigCatalogEntryState {
    logical_path: String,
    source_hash: String,
}

#[derive(Debug, Clone)]
struct LaunchConfigDependency {
    root_index: usize,
    root_label: String,
    root_space: ItemSpace,
    logical_path: String,
    absolute_path: PathBuf,
    state: LaunchConfigDependencyState,
    catalog_extensions: Vec<String>,
}

/// Complete positive and negative dependency proof for launch configuration.
///
/// Absolute paths are retained only for current-state revalidation and are
/// excluded from the stable identity. Immutable project materializations can
/// therefore share one proof by generation while live callers can still prove
/// that no higher-precedence file or catalog entry appeared.
#[derive(Debug, Clone)]
pub struct LaunchConfigDependencyProof {
    trust_identity: String,
    dependencies: Vec<LaunchConfigDependency>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchConfigProofStatus {
    Current,
    MutableAuthorityChanged,
    ImmutableAuthorityMismatch,
}

impl LaunchConfigDependencyProof {
    pub fn identity_digest(&self) -> Result<String, EngineError> {
        #[derive(Serialize)]
        struct StableDependency<'a> {
            root_index: usize,
            root_label: &'a str,
            root_space: ItemSpace,
            logical_path: &'a str,
            state: &'a LaunchConfigDependencyState,
            catalog_extensions: &'a [String],
        }
        #[derive(Serialize)]
        struct StableProof<'a> {
            schema_version: u32,
            trust_identity: &'a str,
            dependencies: Vec<StableDependency<'a>>,
        }
        let value = serde_json::to_value(StableProof {
            schema_version: 1,
            trust_identity: &self.trust_identity,
            dependencies: self
                .dependencies
                .iter()
                .map(|dependency| StableDependency {
                    root_index: dependency.root_index,
                    root_label: &dependency.root_label,
                    root_space: dependency.root_space,
                    logical_path: &dependency.logical_path,
                    state: &dependency.state,
                    catalog_extensions: &dependency.catalog_extensions,
                })
                .collect(),
        })
        .map_err(|error| invalid("launch_config_proof", error.to_string()))?;
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| invalid("launch_config_proof", error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn revalidate_current(&self) -> bool {
        self.revalidate_dependencies(|dependency| Some(Some(dependency.absolute_path.clone())))
    }

    /// Re-prove this snapshot against a new materialization of the same
    /// logical resolution roots. Root labels and spaces are part of the proof
    /// so a caller cannot rebind a cached snapshot to a different authority.
    pub fn revalidate_against(&self, roots: &ResolutionRoots) -> bool {
        self.revalidate_dependencies(|dependency| {
            let root = roots.ordered.get(dependency.root_index)?;
            if root.label != dependency.root_label || root.space != dependency.root_space {
                return None;
            }
            Some(Some(root.ai_root.join(&dependency.logical_path)))
        })
    }

    /// Revalidate configuration under the exact admitted project-content
    /// authority. Bundle roots are already sealed by the engine generation;
    /// node roots remain current-state dependencies; project roots are checked
    /// against the supplied content closure, including catalog membership and
    /// precedence-affecting absence.
    pub fn revalidate_under_authority(
        &self,
        roots: &ResolutionRoots,
        project: Option<(
            &Path,
            &dyn crate::project_content::AuthoritativeProjectContent,
        )>,
    ) -> bool {
        self.revalidate_under_authority_status(roots, project) == LaunchConfigProofStatus::Current
    }

    /// Classify a failed proof by the authority that changed.
    ///
    /// Node configuration and LiveFs project inputs are deliberately mutable
    /// and may trigger a bounded reload. Bundle generation and
    /// content-addressed project dependencies are immutable for the admitted
    /// launch; disagreement there is an authority invariant, not a retry.
    pub fn revalidate_under_authority_status(
        &self,
        roots: &ResolutionRoots,
        project: Option<(
            &Path,
            &dyn crate::project_content::AuthoritativeProjectContent,
        )>,
    ) -> LaunchConfigProofStatus {
        let mut status = LaunchConfigProofStatus::Current;
        for dependency in &self.dependencies {
            let Some(root) = roots.ordered.get(dependency.root_index) else {
                return LaunchConfigProofStatus::ImmutableAuthorityMismatch;
            };
            if root.label != dependency.root_label || root.space != dependency.root_space {
                return LaunchConfigProofStatus::ImmutableAuthorityMismatch;
            }
            let dependency_status = match root.space {
                ItemSpace::Bundle => LaunchConfigProofStatus::Current,
                ItemSpace::Node => {
                    let path = root.ai_root.join(&dependency.logical_path);
                    if observe_dependency_state(&path, &dependency.catalog_extensions)
                        .as_ref()
                        .is_ok_and(|current| current == &dependency.state)
                    {
                        LaunchConfigProofStatus::Current
                    } else {
                        LaunchConfigProofStatus::MutableAuthorityChanged
                    }
                }
                ItemSpace::Project => match project {
                    None => {
                        let path = root.ai_root.join(&dependency.logical_path);
                        if observe_dependency_state(&path, &dependency.catalog_extensions)
                            .as_ref()
                            .is_ok_and(|current| current == &dependency.state)
                        {
                            LaunchConfigProofStatus::Current
                        } else {
                            LaunchConfigProofStatus::MutableAuthorityChanged
                        }
                    }
                    Some((project_root, content)) => {
                        if root.ai_root != project_root.join(crate::AI_DIR) {
                            LaunchConfigProofStatus::ImmutableAuthorityMismatch
                        } else if authoritative_dependency_matches(content, dependency)
                            .is_ok_and(|current| current)
                        {
                            LaunchConfigProofStatus::Current
                        } else {
                            LaunchConfigProofStatus::ImmutableAuthorityMismatch
                        }
                    }
                },
            };
            match dependency_status {
                LaunchConfigProofStatus::ImmutableAuthorityMismatch => {
                    return dependency_status;
                }
                LaunchConfigProofStatus::MutableAuthorityChanged => {
                    status = dependency_status;
                }
                LaunchConfigProofStatus::Current => {}
            }
        }
        status
    }

    fn revalidate_dependencies(
        &self,
        mut current_path: impl FnMut(&LaunchConfigDependency) -> Option<Option<PathBuf>>,
    ) -> bool {
        self.dependencies.iter().all(|dependency| {
            let Some(path) = current_path(dependency) else {
                return false;
            };
            let Some(path) = path else {
                return true;
            };
            let current = if dependency.catalog_extensions.is_empty() {
                observe_file_state(&path)
            } else {
                observe_catalog_state(&path, &dependency.catalog_extensions)
            };
            current
                .as_ref()
                .is_ok_and(|current| current == &dependency.state)
        })
    }

    fn estimated_bytes(&self) -> usize {
        self.trust_identity
            .capacity()
            .saturating_add(
                self.dependencies
                    .capacity()
                    .saturating_mul(std::mem::size_of::<LaunchConfigDependency>()),
            )
            .saturating_add(self.dependencies.iter().fold(0usize, |total, dependency| {
                total
                    .saturating_add(dependency.root_label.capacity())
                    .saturating_add(dependency.logical_path.capacity())
                    .saturating_add(
                        dependency
                            .absolute_path
                            .as_os_str()
                            .as_encoded_bytes()
                            .len(),
                    )
                    .saturating_add(
                        dependency
                            .catalog_extensions
                            .capacity()
                            .saturating_mul(std::mem::size_of::<String>()),
                    )
                    .saturating_add(
                        dependency
                            .catalog_extensions
                            .iter()
                            .map(String::capacity)
                            .sum::<usize>(),
                    )
                    .saturating_add(
                        serde_json::to_vec(&dependency.state)
                            .map(|serialized| serialized.len())
                            .unwrap_or(usize::MAX),
                    )
            }))
    }
}

#[cfg(test)]
pub(crate) fn node_dependency_proof_test_fixture(
    root_index: usize,
    root: &crate::item_resolution::ResolutionRoot,
    logical_path: &str,
) -> Result<LaunchConfigDependencyProof, EngineError> {
    let absolute_path = root.ai_root.join(logical_path);
    let state = observe_dependency_state(&absolute_path, &[])?;
    Ok(LaunchConfigDependencyProof {
        trust_identity: "test-trust-identity".to_string(),
        dependencies: vec![LaunchConfigDependency {
            root_index,
            root_label: root.label.clone(),
            root_space: root.space,
            logical_path: logical_path.to_string(),
            absolute_path,
            state,
            catalog_extensions: Vec::new(),
        }],
    })
}

fn observe_dependency_state(
    path: &Path,
    catalog_extensions: &[String],
) -> Result<LaunchConfigDependencyState, EngineError> {
    if catalog_extensions.is_empty() {
        observe_file_state(path)
    } else {
        observe_catalog_state(path, catalog_extensions)
    }
}

fn authoritative_dependency_matches(
    content: &dyn crate::project_content::AuthoritativeProjectContent,
    dependency: &LaunchConfigDependency,
) -> Result<bool, EngineError> {
    let project_relative = Path::new(crate::AI_DIR).join(&dependency.logical_path);
    if dependency.catalog_extensions.is_empty() {
        return match &dependency.state {
            LaunchConfigDependencyState::Absent => content.validates_absence(&project_relative),
            LaunchConfigDependencyState::File { source_hash } => {
                content.validates_file(&project_relative, source_hash)
            }
            LaunchConfigDependencyState::Catalog { .. } => Ok(false),
        };
    }

    Ok(observe_catalog_state_from_content(
        content,
        &project_relative,
        &dependency.catalog_extensions,
    )? == dependency.state)
}

#[derive(Debug, Clone)]
pub struct LaunchConfigSnapshotSet {
    pub snapshots: BTreeMap<String, LaunchConfigSnapshotWire>,
    pub dependency_proof: LaunchConfigDependencyProof,
}

impl LaunchConfigSnapshotSet {
    pub fn estimated_bytes(&self) -> usize {
        serde_json::to_vec(&self.snapshots)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
            .saturating_add(self.dependency_proof.estimated_bytes())
    }
}

pub fn load_launch_config_snapshots_with_proof(
    declarations: &BTreeMap<String, LaunchConfigInputDecl>,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    node_parsers: &ParserDispatcher,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
    node_trust_store: &TrustStore,
) -> Result<LaunchConfigSnapshotSet, EngineError> {
    load_launch_config_snapshots_with_proof_from_authority(
        declarations,
        roots,
        parsers,
        node_parsers,
        kinds,
        trust_store,
        node_trust_store,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn load_launch_config_snapshots_with_proof_under_project_authority(
    declarations: &BTreeMap<String, LaunchConfigInputDecl>,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    node_parsers: &ParserDispatcher,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
    node_trust_store: &TrustStore,
    project_root: &Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
) -> Result<LaunchConfigSnapshotSet, EngineError> {
    load_launch_config_snapshots_with_proof_from_authority(
        declarations,
        roots,
        parsers,
        node_parsers,
        kinds,
        trust_store,
        node_trust_store,
        Some((project_root, project_content)),
    )
}

#[allow(clippy::too_many_arguments)]
fn load_launch_config_snapshots_with_proof_from_authority(
    declarations: &BTreeMap<String, LaunchConfigInputDecl>,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    node_parsers: &ParserDispatcher,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
    node_trust_store: &TrustStore,
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<LaunchConfigSnapshotSet, EngineError> {
    let config_schema = kinds.get("config").ok_or_else(|| {
        invalid(
            "launch_contract.config_inputs",
            "config kind is not registered",
        )
    })?;
    let mut result = BTreeMap::new();
    let mut aggregate_bytes = 0usize;
    let mut dependencies = Vec::new();

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
                let policy = LayerLoadPolicy {
                    allowed_spaces,
                    allowed_trust,
                    parsers,
                    node_parsers,
                    trust_store,
                    node_trust_store,
                };
                for (root_index, root) in roots.ordered.iter().enumerate() {
                    observe_item_dependencies(
                        root_index,
                        root,
                        id,
                        &config_schema.extensions,
                        &mut dependencies,
                        project_authority,
                    )?;
                    if let Some((path, extension)) =
                        item_path(root, id, &config_schema.extensions, project_authority)?
                    {
                        if *merge == ConfigMergeMode::FirstMatch && first_match_selected {
                            continue;
                        }
                        let layer =
                            load_layer(&path, id, root, extension, &policy, project_authority)?;
                        validate_loaded_dependency(
                            &dependencies,
                            root_index,
                            &path,
                            &layer.source_hash,
                        )?;
                        layers.push(layer);
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
                let policy = LayerLoadPolicy {
                    allowed_spaces,
                    allowed_trust,
                    parsers,
                    node_parsers,
                    trust_store,
                    node_trust_store,
                };
                for (root_index, root) in roots.ordered.iter().enumerate() {
                    let catalog_root = root.ai_root.join("config").join(prefix);
                    let catalog_extensions = config_schema
                        .extensions
                        .iter()
                        .map(|extension| extension.ext.clone())
                        .collect::<Vec<_>>();
                    let catalog_state = observe_catalog_state_for_root(
                        root,
                        &catalog_root,
                        &catalog_extensions,
                        project_authority,
                    )?;
                    dependencies.push(LaunchConfigDependency {
                        root_index,
                        root_label: root.label.clone(),
                        root_space: root.space,
                        logical_path: format!("config/{prefix}"),
                        absolute_path: catalog_root.clone(),
                        state: catalog_state.clone(),
                        catalog_extensions,
                    });
                    let authoritative_content = project_content_for_root(root, project_authority)?;
                    if authoritative_content.is_none() {
                        match lillux::inspect_optional_entry_no_follow(&catalog_root) {
                            Ok(Some(lillux::secure_fs::PinnedEntryType::Directory)) => {}
                            Ok(Some(_)) => {
                                return Err(invalid(
                                    name,
                                    format!(
                                        "catalog root is not a real directory: {}",
                                        catalog_root.display()
                                    ),
                                ));
                            }
                            Ok(None) => continue,
                            Err(error) => {
                                return Err(invalid(
                                    name,
                                    format!("inspect {}: {error}", catalog_root.display()),
                                ));
                            }
                        }
                        validate_config_directory(&catalog_root, &root.ai_root, name)?;
                    } else if matches!(catalog_state, LaunchConfigDependencyState::Absent) {
                        continue;
                    }
                    let mut root_ids = HashMap::<String, (PathBuf, &ExtensionSpec)>::new();
                    for path in collect_catalog_files_for_root(
                        root,
                        &catalog_root,
                        &config_schema.extensions,
                        project_authority,
                    )? {
                        let extension = extension_for(&path, &config_schema.extensions)
                            .ok_or_else(|| {
                                invalid(
                                    name,
                                    format!("unsupported config extension: {}", path.display()),
                                )
                            })?;
                        let relative =
                            path.strip_prefix(root.ai_root.join("config"))
                                .map_err(|_| {
                                    invalid(
                                        name,
                                        format!(
                                            "catalog entry escaped config root: {}",
                                            path.display()
                                        ),
                                    )
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
                        if let Some((first, _)) =
                            root_ids.insert(canonical_id.clone(), (path.clone(), extension))
                        {
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
                            &policy,
                            project_authority,
                        )?;
                        validate_loaded_dependency(
                            &dependencies,
                            root_index,
                            &path,
                            &layer.source_hash,
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
                    return Err(invalid(
                        name,
                        format!("catalog exceeds {MAX_CATALOG_ENTRIES} entries"),
                    ));
                }
                let mut entries = BTreeMap::new();
                let mut ids: Vec<_> = grouped.into_iter().collect();
                ids.sort_by(|left, right| left.0.cmp(&right.0));
                for (canonical_id, layers) in ids {
                    let (value, contributors) = merge_layers(layers, *entry_merge, name)?;
                    let digest = value_digest(&value, name, &mut aggregate_bytes)?;
                    entries.insert(
                        canonical_id,
                        LaunchConfigEntryWire {
                            value,
                            value_digest: digest,
                            contributors,
                        },
                    );
                }
                LaunchConfigSnapshotWire::Catalog { entries }
            }
        };
        result.insert(name.clone(), snapshot);
    }
    let dependency_proof = LaunchConfigDependencyProof {
        trust_identity: trust_store.fingerprint(),
        dependencies,
    };
    if !dependency_proof.revalidate_under_authority(roots, project_authority) {
        return Err(invalid(
            "launch_config_proof",
            "launch configuration changed while its dependency snapshot was being captured",
        ));
    }
    Ok(LaunchConfigSnapshotSet {
        snapshots: result,
        dependency_proof,
    })
}

fn validate_loaded_dependency(
    dependencies: &[LaunchConfigDependency],
    root_index: usize,
    loaded_path: &Path,
    loaded_source_hash: &str,
) -> Result<(), EngineError> {
    let matches_loaded_source = dependencies
        .iter()
        .filter(|dependency| dependency.root_index == root_index)
        .any(|dependency| match &dependency.state {
            LaunchConfigDependencyState::File { source_hash } => {
                dependency.absolute_path == loaded_path && source_hash == loaded_source_hash
            }
            LaunchConfigDependencyState::Catalog { entries } => loaded_path
                .strip_prefix(&dependency.absolute_path)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .and_then(|relative| entries.iter().find(|entry| entry.logical_path == relative))
                .is_some_and(|entry| entry.source_hash == loaded_source_hash),
            LaunchConfigDependencyState::Absent => false,
        });
    if !matches_loaded_source {
        return Err(invalid(
            "launch_config_proof",
            format!(
                "launch configuration changed while securely loading {}",
                loaded_path.display()
            ),
        ));
    }
    Ok(())
}

fn observe_item_dependencies(
    root_index: usize,
    root: &ResolutionRoot,
    id: &str,
    extensions: &[ExtensionSpec],
    dependencies: &mut Vec<LaunchConfigDependency>,
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<(), EngineError> {
    for extension in extensions {
        let logical_path = format!("config/{id}{}", extension.ext);
        let absolute_path = root.ai_root.join(&logical_path);
        dependencies.push(LaunchConfigDependency {
            root_index,
            root_label: root.label.clone(),
            root_space: root.space,
            logical_path,
            state: observe_file_state_for_root(root, &absolute_path, project_authority)?,
            absolute_path,
            catalog_extensions: Vec::new(),
        });
    }
    Ok(())
}

fn project_content_for_root<'a>(
    root: &ResolutionRoot,
    project_authority: Option<(
        &'a Path,
        &'a dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<Option<&'a dyn crate::project_content::AuthoritativeProjectContent>, EngineError> {
    if root.space != ItemSpace::Project {
        return Ok(None);
    }
    let Some((project_root, content)) = project_authority else {
        return Ok(None);
    };
    if root.ai_root != project_root.join(crate::AI_DIR) {
        return Err(invalid(
            "launch_config_authority",
            format!(
                "project config root {} differs from admitted project root {}",
                root.ai_root.display(),
                project_root.display()
            ),
        ));
    }
    Ok(Some(content))
}

fn project_relative_config_path(
    root: &ResolutionRoot,
    absolute_path: &Path,
) -> Result<PathBuf, EngineError> {
    let relative = absolute_path.strip_prefix(&root.ai_root).map_err(|_| {
        invalid(
            "launch_config_authority",
            format!(
                "config path {} escaped declared project AI root {}",
                absolute_path.display(),
                root.ai_root.display()
            ),
        )
    })?;
    Ok(Path::new(crate::AI_DIR).join(relative))
}

fn observe_file_state_for_root(
    root: &ResolutionRoot,
    path: &Path,
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<LaunchConfigDependencyState, EngineError> {
    let Some(content) = project_content_for_root(root, project_authority)? else {
        return observe_file_state(path);
    };
    let relative = project_relative_config_path(root, path)?;
    match content.read_file(&relative, MAX_SOURCE_BYTES)? {
        Some(source) => Ok(LaunchConfigDependencyState::File {
            source_hash: lillux::sha256_hex(&source),
        }),
        None => Ok(LaunchConfigDependencyState::Absent),
    }
}

fn observe_file_state(path: &Path) -> Result<LaunchConfigDependencyState, EngineError> {
    match lillux::read_optional_regular_file_bounded_no_follow(path, MAX_SOURCE_BYTES) {
        Ok(Some(source)) => Ok(LaunchConfigDependencyState::File {
            source_hash: lillux::sha256_hex(&source),
        }),
        Ok(None) => Ok(LaunchConfigDependencyState::Absent),
        Err(error) => Err(invalid(
            "launch_config_proof",
            format!("securely observe {}: {error:#}", path.display()),
        )),
    }
}

fn observe_catalog_state(
    path: &Path,
    extensions: &[String],
) -> Result<LaunchConfigDependencyState, EngineError> {
    let mut file_count = 0_usize;
    let mut entries = Vec::new();
    let mut aggregate_bytes = 0_u64;
    let present = lillux::visit_regular_files_no_follow_bounded(
        path,
        lillux::DirectoryTraversalBudget::new(
            MAX_CATALOG_TRAVERSAL_ENTRIES,
            MAX_CATALOG_TRAVERSAL_DEPTH,
        ),
        |_relative, _is_directory| Ok(false),
        |relative, mut source| {
            file_count = file_count.saturating_add(1);
            if file_count > MAX_CATALOG_FILES {
                anyhow::bail!("catalog exceeds {MAX_CATALOG_FILES} regular files");
            }
            if extensions.iter().any(|extension| {
                relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(extension))
            }) {
                if entries.len() >= MAX_CATALOG_ENTRIES {
                    anyhow::bail!("catalog exceeds {MAX_CATALOG_ENTRIES} matching entries");
                }
                let source_bytes = source.metadata()?.len();
                if source_bytes > MAX_SOURCE_BYTES {
                    anyhow::bail!(
                        "catalog contributor {} exceeds {MAX_SOURCE_BYTES} bytes",
                        relative.display()
                    );
                }
                aggregate_bytes = aggregate_bytes
                    .checked_add(source_bytes)
                    .ok_or_else(|| anyhow::anyhow!("catalog byte count overflow"))?;
                if aggregate_bytes > MAX_AGGREGATE_BYTES as u64 {
                    anyhow::bail!(
                        "catalog contributors exceed {MAX_AGGREGATE_BYTES} aggregate bytes"
                    );
                }
                let mut bytes = Vec::with_capacity(usize::try_from(source_bytes).unwrap_or(0));
                source
                    .by_ref()
                    .take(MAX_SOURCE_BYTES.saturating_add(1))
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_SOURCE_BYTES {
                    anyhow::bail!(
                        "catalog contributor {} exceeds {MAX_SOURCE_BYTES} bytes",
                        relative.display()
                    );
                }
                entries.push(LaunchConfigCatalogEntryState {
                    logical_path: relative.to_string_lossy().replace('\\', "/"),
                    source_hash: lillux::sha256_hex(&bytes),
                });
            }
            Ok(())
        },
    )
    .map_err(|error| {
        invalid(
            "launch_config_proof",
            format!("securely observe catalog {}: {error:#}", path.display()),
        )
    })?;
    if present && !entries.is_empty() {
        entries.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        Ok(LaunchConfigDependencyState::Catalog { entries })
    } else {
        Ok(LaunchConfigDependencyState::Absent)
    }
}

fn observe_catalog_state_for_root(
    root: &ResolutionRoot,
    path: &Path,
    extensions: &[String],
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<LaunchConfigDependencyState, EngineError> {
    let Some(content) = project_content_for_root(root, project_authority)? else {
        return observe_catalog_state(path, extensions);
    };
    let prefix = project_relative_config_path(root, path)?;
    observe_catalog_state_from_content(content, &prefix, extensions)
}

fn observe_catalog_state_from_content(
    content: &dyn crate::project_content::AuthoritativeProjectContent,
    prefix: &Path,
    extensions: &[String],
) -> Result<LaunchConfigDependencyState, EngineError> {
    let entries = content.list_files(prefix, true, MAX_CATALOG_FILES)?;
    let mut aggregate_bytes = 0_u64;
    let mut observed = Vec::new();
    for entry in entries.into_iter().filter(|entry| {
        extensions.iter().any(|extension| {
            entry
                .relative_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(extension))
        })
    }) {
        if observed.len() >= MAX_CATALOG_ENTRIES {
            return Err(invalid(
                "launch_config_proof",
                format!("catalog exceeds {MAX_CATALOG_ENTRIES} matching entries"),
            ));
        }
        if entry.size > MAX_SOURCE_BYTES {
            return Err(invalid(
                "launch_config_proof",
                format!(
                    "catalog contributor {} exceeds {MAX_SOURCE_BYTES} bytes",
                    entry.relative_path.display()
                ),
            ));
        }
        aggregate_bytes = aggregate_bytes
            .checked_add(entry.size)
            .ok_or_else(|| invalid("launch_config_proof", "catalog byte count overflow"))?;
        if aggregate_bytes > MAX_AGGREGATE_BYTES as u64 {
            return Err(invalid(
                "launch_config_proof",
                format!("catalog contributors exceed {MAX_AGGREGATE_BYTES} aggregate bytes"),
            ));
        }
        observed.push(LaunchConfigCatalogEntryState {
            logical_path: entry.relative_path.to_string_lossy().replace('\\', "/"),
            source_hash: entry.content_hash,
        });
    }
    if observed.is_empty() {
        Ok(LaunchConfigDependencyState::Absent)
    } else {
        observed.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        Ok(LaunchConfigDependencyState::Catalog { entries: observed })
    }
}

fn item_path<'a>(
    root: &ResolutionRoot,
    id: &str,
    extensions: &'a [ExtensionSpec],
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<Option<(PathBuf, &'a ExtensionSpec)>, EngineError> {
    let mut found: Option<(PathBuf, &'a ExtensionSpec)> = None;
    let project_content = project_content_for_root(root, project_authority)?;
    for extension in extensions {
        let path = root
            .ai_root
            .join("config")
            .join(format!("{id}{}", extension.ext));
        let entry_type = match project_content {
            Some(content) => {
                let relative = project_relative_config_path(root, &path)?;
                content
                    .read_file(&relative, MAX_SOURCE_BYTES)?
                    .map(|_| lillux::secure_fs::PinnedEntryType::Regular)
            }
            None => lillux::inspect_optional_entry_no_follow(&path)
                .map_err(|error| invalid(id, format!("inspect {}: {error}", path.display())))?,
        };
        match entry_type {
            Some(lillux::secure_fs::PinnedEntryType::Regular) => {
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
            None => {}
            Some(_) => {
                return Err(invalid(
                    id,
                    format!(
                        "config contributor is not a regular non-symlink file: {}",
                        path.display()
                    ),
                ));
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
    policy: &LayerLoadPolicy<'_>,
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<Layer, EngineError> {
    let project_content = project_content_for_root(root, project_authority)?;
    if project_content.is_none() {
        validate_config_path(path, &root.ai_root.join("config"), canonical_id)?;
    } else {
        path.strip_prefix(root.ai_root.join("config"))
            .map_err(|_| {
                invalid(
                    canonical_id,
                    format!("config contributor escaped config root: {}", path.display()),
                )
            })?;
    }
    if !valid_root_label(&root.label) {
        return Err(invalid(
            canonical_id,
            format!("resolution root label `{}` is invalid", root.label),
        ));
    }
    let declared_space = launch_space(root.space);
    if !policy.allowed_spaces.contains(&declared_space) {
        return Err(EngineError::LaunchConfigPolicyDenied {
            code: "launch_config_space_not_allowed".to_owned(),
            input: canonical_id.to_owned(),
            detail: format!("source space {:?} is not allowed", root.space),
        });
    }
    let source = match project_content {
        Some(content) => {
            let relative = project_relative_config_path(root, path)?;
            content
                .read_file(&relative, MAX_SOURCE_BYTES)?
                .ok_or_else(|| {
                    invalid(
                        canonical_id,
                        format!(
                            "admitted config contributor disappeared: {}",
                            path.display()
                        ),
                    )
                })?
        }
        None => lillux::read_regular_file_bounded_no_follow(path, MAX_SOURCE_BYTES).map_err(
            |error| {
                invalid(
                    canonical_id,
                    format!("securely read {}: {error:#}", path.display()),
                )
            },
        )?,
    };
    let content = String::from_utf8(source).map_err(|error| {
        invalid(
            canonical_id,
            format!(
                "config contributor {} is not UTF-8: {error}",
                path.display()
            ),
        )
    })?;
    let source_hash = lillux::sha256_hex(content.as_bytes());
    let header = parse_signature_header(&content, &extension.signature).ok_or_else(|| {
        invalid(
            canonical_id,
            format!("unsigned launch config contributor {}", path.display()),
        )
    })?;
    let content_digest =
        content_hash_after_signature(&content, &extension.signature).ok_or_else(|| {
            invalid(
                canonical_id,
                format!(
                    "cannot compute signed content digest for {}",
                    path.display()
                ),
            )
        })?;
    let source_trust_store = match root.space {
        ItemSpace::Bundle | ItemSpace::Node => policy.node_trust_store,
        ItemSpace::Project => policy.trust_store,
    };
    let (contract_trust, verified_signer) =
        verify_item_signature_with_hash(&content_digest, &header, source_trust_store).map_err(
            |error| {
                invalid(
                    canonical_id,
                    format!("signature verification failed: {error}"),
                )
            },
        )?;
    let trust_class = match (contract_trust, root.space) {
        (ContractTrustClass::Trusted, ItemSpace::Bundle) => TrustClass::TrustedBundle,
        (ContractTrustClass::Trusted, ItemSpace::Project) => TrustClass::TrustedProject,
        (ContractTrustClass::Trusted, ItemSpace::Node) => TrustClass::TrustedNode,
        (ContractTrustClass::Untrusted, _) => TrustClass::UntrustedProject,
        (ContractTrustClass::Unsigned, _) => TrustClass::Unsigned,
    };
    if !policy.allowed_trust.contains(&trust_class) {
        return Err(EngineError::LaunchConfigPolicyDenied {
            code: "launch_config_untrusted".to_owned(),
            input: canonical_id.to_owned(),
            detail: format!("trust class {trust_class:?} is not allowed"),
        });
    }
    let parser_authority = match root.space {
        ItemSpace::Bundle | ItemSpace::Node => policy.node_parsers,
        ItemSpace::Project => policy.parsers,
    };
    let value = parser_authority.dispatch(
        &extension.parser,
        &content,
        Some(path),
        &extension.signature,
    )?;
    Ok(Layer {
        value,
        source_hash,
        contributor: LaunchConfigContributorWire {
            space: item_space_wire(root.space),
            root_label: root.label.clone(),
            canonical_id: canonical_id.to_owned(),
            content_digest,
            trust_class: trust_wire(trust_class),
            signer_fingerprint: verified_signer
                .ok_or_else(|| invalid(canonical_id, "signed launch config has no signer"))?
                .0,
        },
    })
}

fn validate_config_path(path: &Path, config_root: &Path, context: &str) -> Result<(), EngineError> {
    let _ai_root = config_root.parent().ok_or_else(|| {
        invalid(
            context,
            format!(
                "config root has no declared AI root: {}",
                config_root.display()
            ),
        )
    })?;
    path.strip_prefix(config_root).map_err(|_| {
        invalid(
            context,
            format!("config contributor escaped config root: {}", path.display()),
        )
    })?;
    lillux::read_regular_file_bounded_no_follow(path, MAX_SOURCE_BYTES)
        .map(|_| ())
        .map_err(|error| {
            invalid(
                context,
                format!(
                    "config contributor is not a secure regular file {}: {error:#}",
                    path.display()
                ),
            )
        })
}

fn validate_config_directory(
    directory: &Path,
    ai_root: &Path,
    context: &str,
) -> Result<(), EngineError> {
    directory.strip_prefix(ai_root).map_err(|_| {
        invalid(
            context,
            format!(
                "config directory escaped declared AI root: {}",
                directory.display()
            ),
        )
    })?;
    if !matches!(
        lillux::inspect_optional_entry_no_follow(ai_root),
        Ok(Some(lillux::secure_fs::PinnedEntryType::Directory))
    ) {
        return Err(invalid(
            context,
            format!(
                "declared AI root must be a real directory: {}",
                ai_root.display()
            ),
        ));
    }
    if !matches!(
        lillux::inspect_optional_entry_no_follow(directory),
        Ok(Some(lillux::secure_fs::PinnedEntryType::Directory))
    ) {
        return Err(invalid(
            context,
            format!(
                "config directory must be a descriptor-resolved real directory: {}",
                directory.display()
            ),
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
        return Err(invalid(
            name,
            format!("config value exceeds {MAX_CONTRIBUTORS} contributors"),
        ));
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

fn collect_catalog_files(
    root: &Path,
    extensions: &[ExtensionSpec],
) -> Result<Vec<PathBuf>, EngineError> {
    let mut file_count = 0_usize;
    let mut files = Vec::new();
    let present = lillux::visit_regular_files_no_follow_bounded(
        root,
        lillux::DirectoryTraversalBudget::new(
            MAX_CATALOG_TRAVERSAL_ENTRIES,
            MAX_CATALOG_TRAVERSAL_DEPTH,
        ),
        |_relative, _is_directory| Ok(false),
        |relative, _source| {
            file_count = file_count.saturating_add(1);
            if file_count > MAX_CATALOG_FILES {
                anyhow::bail!("catalog exceeds {MAX_CATALOG_FILES} regular files");
            }
            let path = root.join(relative);
            if extension_for(&path, extensions).is_some() {
                files.push(path);
                if files.len() > MAX_CATALOG_FILES {
                    anyhow::bail!("catalog contributor cap exceeded");
                }
            }
            Ok(())
        },
    )
    .map_err(|error| {
        invalid(
            "catalog",
            format!("securely walk {}: {error:#}", root.display()),
        )
    })?;
    if !present {
        return Ok(Vec::new());
    }
    files.sort();
    Ok(files)
}

fn collect_catalog_files_for_root(
    root: &ResolutionRoot,
    catalog_root: &Path,
    extensions: &[ExtensionSpec],
    project_authority: Option<(
        &Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<Vec<PathBuf>, EngineError> {
    let Some(content) = project_content_for_root(root, project_authority)? else {
        return collect_catalog_files(catalog_root, extensions);
    };
    let prefix = project_relative_config_path(root, catalog_root)?;
    let mut files = content
        .list_files(&prefix, true, MAX_CATALOG_FILES)?
        .into_iter()
        .map(|entry| catalog_root.join(entry.relative_path))
        .filter(|path| extension_for(path, extensions).is_some())
        .collect::<Vec<_>>();
    if files.len() > MAX_CATALOG_FILES {
        return Err(invalid("catalog", "catalog contributor cap exceeded"));
    }
    files.sort();
    Ok(files)
}

fn extension_for<'a>(path: &Path, extensions: &'a [ExtensionSpec]) -> Option<&'a ExtensionSpec> {
    let name = path.file_name()?.to_str()?;
    extensions
        .iter()
        .find(|extension| name.ends_with(&extension.ext))
}

fn value_digest(value: &Value, name: &str, aggregate: &mut usize) -> Result<String, EngineError> {
    let canonical = lillux::canonical_json(value).map_err(|error| {
        invalid(
            name,
            format!("config value cannot be canonicalized: {error}"),
        )
    })?;
    if canonical.len() > MAX_VALUE_BYTES {
        return Err(invalid(
            name,
            format!("canonical config value exceeds {MAX_VALUE_BYTES} bytes"),
        ));
    }
    *aggregate = aggregate.saturating_add(canonical.len());
    if *aggregate > MAX_AGGREGATE_BYTES {
        return Err(invalid(
            name,
            format!("aggregate config snapshots exceed {MAX_AGGREGATE_BYTES} bytes"),
        ));
    }
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

fn valid_root_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 128
        && label.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

fn launch_space(space: ItemSpace) -> LaunchItemSpace {
    match space {
        ItemSpace::Bundle => LaunchItemSpace::Bundle,
        ItemSpace::Project => LaunchItemSpace::Project,
        ItemSpace::Node => LaunchItemSpace::Node,
    }
}

fn item_space_wire(space: ItemSpace) -> ItemSpaceWire {
    match space {
        ItemSpace::Bundle => ItemSpaceWire::Bundle,
        ItemSpace::Project => ItemSpaceWire::Project,
        ItemSpace::Node => ItemSpaceWire::Node,
    }
}

fn trust_wire(trust: TrustClass) -> TrustClassWire {
    match trust {
        TrustClass::TrustedBundle => TrustClassWire::TrustedBundle,
        TrustClass::TrustedProject => TrustClassWire::TrustedProject,
        TrustClass::TrustedNode => TrustClassWire::TrustedNode,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_content::{AuthoritativeProjectContent, ProjectContentEntry};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct TestProjectContent {
        files: BTreeMap<PathBuf, Vec<u8>>,
    }

    impl TestProjectContent {
        fn with_file(mut self, path: &str, bytes: &[u8]) -> Self {
            self.files.insert(PathBuf::from(path), bytes.to_vec());
            self
        }
    }

    impl AuthoritativeProjectContent for TestProjectContent {
        fn list_files(
            &self,
            prefix: &Path,
            recursive: bool,
            max_entries: usize,
        ) -> Result<Vec<ProjectContentEntry>, EngineError> {
            let mut entries = Vec::new();
            for (path, bytes) in &self.files {
                let Ok(relative) = path.strip_prefix(prefix) else {
                    continue;
                };
                if relative.as_os_str().is_empty()
                    || (!recursive && relative.components().count() != 1)
                {
                    continue;
                }
                if entries.len() >= max_entries {
                    return Err(EngineError::Internal(
                        "test project content entry bound exceeded".to_string(),
                    ));
                }
                entries.push(ProjectContentEntry {
                    relative_path: relative.to_path_buf(),
                    content_hash: lillux::sha256_hex(bytes),
                    size: bytes.len() as u64,
                    normalized_mode: 0o644,
                });
            }
            entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            Ok(entries)
        }

        fn read_file(
            &self,
            relative_path: &Path,
            max_bytes: u64,
        ) -> Result<Option<Vec<u8>>, EngineError> {
            Ok(self
                .files
                .get(relative_path)
                .filter(|bytes| bytes.len() as u64 <= max_bytes)
                .cloned())
        }

        fn validates_file(
            &self,
            relative_path: &Path,
            content_hash: &str,
        ) -> Result<bool, EngineError> {
            Ok(self
                .files
                .get(relative_path)
                .is_some_and(|bytes| lillux::sha256_hex(bytes) == content_hash))
        }

        fn validates_absence(&self, relative_path: &Path) -> Result<bool, EngineError> {
            Ok(!self.files.contains_key(relative_path))
        }
    }

    fn project_roots(project_root: &Path) -> ResolutionRoots {
        ResolutionRoots {
            ordered: vec![ResolutionRoot {
                space: ItemSpace::Project,
                identity: crate::contracts::ItemSourceRoot::Project,
                label: "project".to_string(),
                ai_root: project_root.join(crate::AI_DIR),
                content_root: Some(project_root.to_path_buf()),
            }],
        }
    }

    #[test]
    fn admitted_project_config_proof_checks_positive_absent_and_catalog_cas_state() {
        let project_root = PathBuf::from("/not-opened/project");
        let item_bytes = b"name: exact\n";
        let catalog_bytes = b"name: catalog\n";
        let item_hash = lillux::sha256_hex(item_bytes);
        let catalog_hash = lillux::sha256_hex(catalog_bytes);
        let proof = LaunchConfigDependencyProof {
            trust_identity: "trust".to_string(),
            dependencies: vec![
                LaunchConfigDependency {
                    root_index: 0,
                    root_label: "project".to_string(),
                    root_space: ItemSpace::Project,
                    logical_path: "config/test.yaml".to_string(),
                    absolute_path: PathBuf::from("/must/not/be/opened/item"),
                    state: LaunchConfigDependencyState::File {
                        source_hash: item_hash,
                    },
                    catalog_extensions: Vec::new(),
                },
                LaunchConfigDependency {
                    root_index: 0,
                    root_label: "project".to_string(),
                    root_space: ItemSpace::Project,
                    logical_path: "config/missing.yaml".to_string(),
                    absolute_path: PathBuf::from("/must/not/be/opened/missing"),
                    state: LaunchConfigDependencyState::Absent,
                    catalog_extensions: Vec::new(),
                },
                LaunchConfigDependency {
                    root_index: 0,
                    root_label: "project".to_string(),
                    root_space: ItemSpace::Project,
                    logical_path: "config/providers".to_string(),
                    absolute_path: PathBuf::from("/must/not/be/opened/catalog"),
                    state: LaunchConfigDependencyState::Catalog {
                        entries: vec![LaunchConfigCatalogEntryState {
                            logical_path: "one.yaml".to_string(),
                            source_hash: catalog_hash,
                        }],
                    },
                    catalog_extensions: vec![".yaml".to_string()],
                },
            ],
        };
        let roots = project_roots(&project_root);
        let exact = TestProjectContent::default()
            .with_file(".ai/config/test.yaml", item_bytes)
            .with_file(".ai/config/providers/one.yaml", catalog_bytes);
        assert!(proof.revalidate_under_authority(
            &roots,
            Some((&project_root, &exact as &dyn AuthoritativeProjectContent)),
        ));

        let shadowed = TestProjectContent::default()
            .with_file(".ai/config/test.yaml", item_bytes)
            .with_file(".ai/config/missing.yaml", b"new shadow\n")
            .with_file(".ai/config/providers/one.yaml", catalog_bytes);
        assert!(!proof.revalidate_under_authority(
            &roots,
            Some((&project_root, &shadowed as &dyn AuthoritativeProjectContent,)),
        ));

        let changed_catalog = TestProjectContent::default()
            .with_file(".ai/config/test.yaml", item_bytes)
            .with_file(".ai/config/providers/one.yaml", b"changed\n");
        assert!(!proof.revalidate_under_authority(
            &roots,
            Some((
                &project_root,
                &changed_catalog as &dyn AuthoritativeProjectContent,
            )),
        ));
    }

    #[test]
    fn admitted_project_config_proof_rejects_root_rebinding() {
        let project_root = PathBuf::from("/not-opened/project");
        let proof = LaunchConfigDependencyProof {
            trust_identity: "trust".to_string(),
            dependencies: vec![LaunchConfigDependency {
                root_index: 0,
                root_label: "project".to_string(),
                root_space: ItemSpace::Project,
                logical_path: "config/missing.yaml".to_string(),
                absolute_path: PathBuf::from("/must/not/be/opened"),
                state: LaunchConfigDependencyState::Absent,
                catalog_extensions: Vec::new(),
            }],
        };
        let content = TestProjectContent::default();
        let mut roots = project_roots(&project_root);
        roots.ordered[0].label = "different".to_string();
        assert!(!proof.revalidate_under_authority(
            &roots,
            Some((&project_root, &content as &dyn AuthoritativeProjectContent,)),
        ));
    }
}

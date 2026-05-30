use std::path::{Path, PathBuf};

use anyhow::Context;
use ryeos_runtime::alias_registry::{AliasDef, PositionalForm, ProjectResolution};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct LoadedAliasDescriptor {
    pub def: AliasDef,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct LoadedVerbDescriptor {
    pub description: String,
    pub execute: String,
}

#[derive(Debug, Deserialize)]
struct VerbYaml {
    name: String,
    #[serde(default)]
    description: String,
    execute: Option<String>,
    #[serde(default)]
    aliases: Vec<VerbAliasYaml>,
}

#[derive(Debug, Deserialize)]
struct VerbAliasYaml {
    tokens: Vec<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    deprecated: Option<bool>,
    #[serde(default)]
    replacement_tokens: Option<Vec<String>>,
    #[serde(default)]
    removed_in: Option<String>,
    #[serde(default)]
    positional_field: Option<String>,
    #[serde(default)]
    positional_forms: Vec<PositionalForm>,
    #[serde(default)]
    project_resolution: ProjectResolution,
}

pub fn direct_bundle_roots(system_space_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let bundles_dir = system_space_dir.join(".ai").join("bundles");
    let Ok(bundle_entries) = std::fs::read_dir(&bundles_dir) else {
        return roots;
    };

    for bundle_entry in bundle_entries.flatten() {
        let name = bundle_entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str.ends_with(".backup.prev") {
            continue;
        }
        roots.push(bundle_entry.path());
    }

    roots
}

pub fn load_alias_descriptors(
    bundle_roots: &[PathBuf],
) -> anyhow::Result<Vec<LoadedAliasDescriptor>> {
    let mut out = Vec::new();

    for verb in load_verb_yamls(bundle_roots)? {
        if verb.execute.is_none() {
            continue;
        }
        for alias in verb.aliases {
            out.push(LoadedAliasDescriptor {
                description: alias
                    .description
                    .clone()
                    .unwrap_or_else(|| verb.description.clone()),
                def: AliasDef {
                    tokens: alias.tokens,
                    verb: verb.name.clone(),
                    deprecated: alias.deprecated.unwrap_or(false),
                    replacement_tokens: alias.replacement_tokens,
                    removed_in: alias.removed_in,
                    positional_field: alias.positional_field,
                    positional_forms: alias.positional_forms,
                    project_resolution: alias.project_resolution,
                },
            });
        }
    }

    Ok(out)
}

pub fn load_verb_descriptor(
    bundle_roots: &[PathBuf],
    verb_name: &str,
) -> anyhow::Result<Option<LoadedVerbDescriptor>> {
    for verb in load_verb_yamls(bundle_roots)? {
        if verb.name != verb_name {
            continue;
        }
        let Some(execute) = verb.execute else {
            continue;
        };
        return Ok(Some(LoadedVerbDescriptor {
            description: verb.description,
            execute,
        }));
    }

    Ok(None)
}

fn load_verb_yamls(bundle_roots: &[PathBuf]) -> anyhow::Result<Vec<VerbYaml>> {
    let mut out = Vec::new();
    for root in bundle_roots {
        let verbs_dir = root.join(ryeos_engine::AI_DIR).join("node").join("verbs");
        if !verbs_dir.is_dir() {
            continue;
        }
        for path in yaml_files_recursive(&verbs_dir)? {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("read verb {}", path.display()))?;
            let body = lillux::signature::strip_signature_lines(&content);
            let verb: VerbYaml = serde_yaml::from_str(&body)
                .with_context(|| format!("parse verb YAML {}", path.display()))?;
            out.push(verb);
        }
    }
    Ok(out)
}

fn yaml_files_recursive(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_yaml_files(dir, &mut out)?;
    Ok(out)
}

fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("read descriptor directory {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_files(&path, out)?;
        } else if is_yaml_file(&path) {
            out.push(path);
        }
    }

    Ok(())
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("yaml") | Some("yml")
    )
}

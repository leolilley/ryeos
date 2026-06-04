use std::path::Path;

use anyhow::Context;
use ryeos_app::node_config::loader::BootstrapLoader;
use ryeos_app::node_config::sections::alias as node_alias;
use ryeos_app::node_config::{NodeConfigSnapshot, SectionTable};
use ryeos_runtime::alias_registry as runtime_alias;
use ryeos_runtime::alias_registry::AliasDef;

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

pub fn load_verified_snapshot(system_space_dir: &Path) -> anyhow::Result<NodeConfigSnapshot> {
    let user_root = ryeos_engine::roots::user_root().ok();
    let trust_store =
        ryeos_engine::trust::TrustStore::load_three_tier(None, user_root.as_deref(), &[])
            .context("load trust store for verified node config")?;
    let loader = BootstrapLoader {
        system_space_dir,
        trust_store: &trust_store,
    };
    let bundles = loader
        .load_bundle_section()
        .context("load verified node bundle registrations")?;
    loader
        .load_full(&SectionTable::new(), &bundles)
        .context("load verified node config")
}

pub fn load_alias_descriptors_from_snapshot(
    snapshot: &NodeConfigSnapshot,
) -> Vec<LoadedAliasDescriptor> {
    let mut out = Vec::new();

    for alias in &snapshot.aliases {
        out.push(LoadedAliasDescriptor {
            description: alias.description.clone(),
            def: AliasDef {
                tokens: alias.tokens.clone(),
                verb: alias.verb.clone(),
                deprecated: alias.deprecated.unwrap_or(false),
                replacement_tokens: alias.replacement_tokens.clone(),
                removed_in: alias.removed_in.clone(),
                positional_forms: alias
                    .positional_forms
                    .iter()
                    .map(convert_positional_form)
                    .collect(),
                project_resolution: convert_project_resolution(alias.project_resolution),
            },
        });
    }

    out
}

pub fn load_alias_descriptors(
    system_space_dir: &Path,
) -> anyhow::Result<Vec<LoadedAliasDescriptor>> {
    let snapshot = load_verified_snapshot(system_space_dir)?;
    Ok(load_alias_descriptors_from_snapshot(&snapshot))
}

pub fn load_verb_descriptor_from_snapshot(
    snapshot: &NodeConfigSnapshot,
    verb_name: &str,
) -> Option<LoadedVerbDescriptor> {
    for verb in &snapshot.verbs {
        if verb.name != verb_name {
            continue;
        }
        let Some(execute) = &verb.execute else {
            continue;
        };
        return Some(LoadedVerbDescriptor {
            description: verb.description.clone(),
            execute: execute.clone(),
        });
    }

    None
}

fn convert_project_resolution(
    value: node_alias::ProjectResolution,
) -> runtime_alias::ProjectResolution {
    match value {
        node_alias::ProjectResolution::None => runtime_alias::ProjectResolution::None,
        node_alias::ProjectResolution::Required => runtime_alias::ProjectResolution::Required,
        node_alias::ProjectResolution::Optional => runtime_alias::ProjectResolution::Optional,
    }
}

fn convert_positional_form(value: &node_alias::PositionalForm) -> runtime_alias::PositionalForm {
    runtime_alias::PositionalForm {
        slots: value
            .slots
            .iter()
            .map(|slot| runtime_alias::PositionalSlot {
                field: slot.field.clone(),
                matcher: convert_positional_matcher(slot.matcher),
            })
            .collect(),
    }
}

fn convert_positional_matcher(
    value: node_alias::PositionalMatcher,
) -> runtime_alias::PositionalMatcher {
    match value {
        node_alias::PositionalMatcher::Any => runtime_alias::PositionalMatcher::Any,
        node_alias::PositionalMatcher::CanonicalRef => {
            runtime_alias::PositionalMatcher::CanonicalRef
        }
    }
}

pub fn find_alias(
    snapshot: &NodeConfigSnapshot,
    verb_tokens: &[String],
) -> Option<LoadedAliasDescriptor> {
    load_alias_descriptors_from_snapshot(snapshot)
        .into_iter()
        .find(|alias| alias.def.tokens == verb_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn converts_snapshot_aliases_to_runtime_aliases() {
        let snapshot = NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
            verbs: vec![],
            aliases: vec![node_alias::AliasRecord {
                category: "aliases".into(),
                section: "aliases".into(),
                tokens: vec!["bundle".into(), "sign".into()],
                verb: "bundle-sign".into(),
                description: "Sign bundle".into(),
                deprecated: Some(false),
                replacement_tokens: None,
                removed_in: None,
                positional_forms: vec![node_alias::PositionalForm {
                    slots: vec![node_alias::PositionalSlot {
                        field: "source".into(),
                        matcher: node_alias::PositionalMatcher::Any,
                    }],
                }],
                project_resolution: node_alias::ProjectResolution::Optional,
                source_file: PathBuf::from("/tmp/verb.yaml"),
            }],
            hosted_node_policies: vec![],
        };

        let aliases = load_alias_descriptors_from_snapshot(&snapshot);
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].def.tokens, ["bundle", "sign"]);
        assert_eq!(aliases[0].def.verb, "bundle-sign");
        assert_eq!(aliases[0].def.positional_forms[0].slots[0].field, "source");
        assert_eq!(
            aliases[0].def.project_resolution,
            runtime_alias::ProjectResolution::Optional
        );
    }
}

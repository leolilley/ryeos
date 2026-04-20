use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct LimitsConfig {
    pub defaults: Value,
    pub caps: Value,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub execution: Value,
    pub limits: LimitsConfig,
    pub routing: Value,
    pub hooks: Vec<Value>,
    pub risk: Value,
    pub errors: Value,
    pub providers: HashMap<String, Value>,
}

pub fn load_runtime_config(config: &Config, project_path: Option<&Path>) -> Result<RuntimeConfig> {
    let system_roots = config.all_system_roots();
    let user_root = discover_user_root();

    let config_roots: Vec<PathBuf> = {
        let mut roots = Vec::new();
        for sr in &system_roots {
            roots.push(sr.join(".ai/config/rye-runtime"));
        }
        if let Some(ref ur) = user_root {
            roots.push(ur.join(".ai/config/rye-runtime"));
        }
        if let Some(pp) = project_path {
            roots.push(pp.join(".ai/config/rye-runtime"));
        }
        roots
    };

    let schemas = discover_and_load_schemas(&config_roots)?;

    let execution = load_and_merge_config(
        &config_roots,
        "execution.yaml",
        &schemas,
    )?;

    let limits_raw = load_and_merge_config(
        &config_roots,
        "limits.yaml",
        &schemas,
    )?;
    let limits = parse_limits(&limits_raw);

    let routing = load_and_merge_config(
        &config_roots,
        "model_routing.yaml",
        &schemas,
    )?;

    let hooks = load_hooks(&config_roots)?;

    let risk = load_and_merge_config(
        &config_roots,
        "capability_risk.yaml",
        &schemas,
    )?;

    let errors = load_and_merge_config(
        &config_roots,
        "error_classification.yaml",
        &schemas,
    )?;

    let providers = load_providers(&config_roots)?;

    Ok(RuntimeConfig {
        execution,
        limits,
        routing,
        hooks,
        risk,
        errors,
        providers,
    })
}

fn discover_user_root() -> Option<PathBuf> {
    std::env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

fn discover_and_load_schemas(
    config_roots: &[PathBuf],
) -> Result<HashMap<String, Value>> {
    let mut schemas: HashMap<String, Value> = HashMap::new();

    for root in config_roots {
        let schema_dir = root.join("schemas");
        if !schema_dir.is_dir() {
            continue;
        }

        for entry in std::fs::read_dir(&schema_dir)
            .with_context(|| format!("failed to read {}", schema_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            if !name.ends_with(".config-schema.yaml") {
                continue;
            }

            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let doc: serde_yaml::Value = serde_yaml::from_str(&content)
                .with_context(|| format!("failed to parse {}", path.display()))?;

            let kind = doc.get("kind").and_then(|v| v.as_str());
            if kind != Some("config-schema") {
                continue;
            }

            let target_config = doc
                .get("target_config")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if target_config.contains("..") || target_config.starts_with('/') {
                bail!(
                    "invalid target_config '{}' in {} (must be relative, no ..)",
                    target_config,
                    path.display()
                );
            }

            let schema = doc
                .get("schema")
                .cloned()
                .unwrap_or(serde_yaml::Value::Null);
            let schema_json = serde_json::to_value(&schema)
                .context("failed to convert schema to JSON")?;

            if let Some(existing) = schemas.get(target_config) {
                let existing_path = existing.get("_source_file").and_then(|v| v.as_str()).unwrap_or("?");
                bail!(
                    "duplicate schema match for '{}' in {} (already defined in {})",
                    target_config,
                    path.display(),
                    existing_path,
                );
            }

            let mut schema_with_meta = schema_json;
            if let Value::Object(ref mut map) = schema_with_meta {
                map.insert("_source_file".to_string(), Value::String(path.to_string_lossy().to_string()));
            }

            schemas.insert(target_config.to_string(), schema_with_meta);
        }
    }

    Ok(schemas)
}

fn load_and_merge_config(
    config_roots: &[PathBuf],
    filename: &str,
    _schemas: &HashMap<String, Value>,
) -> Result<Value> {
    let mut merged = serde_json::Map::new();

    for root in config_roots {
        let path = root.join(filename);
        if !path.is_file() {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let cleaned = lillux::signature::strip_signature_lines(&content);
        let doc: serde_yaml::Value = serde_yaml::from_str(&cleaned)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let json_val = serde_json::to_value(&doc)
            .with_context(|| format!("failed to convert {} to JSON", path.display()))?;

        deep_merge(&mut merged, &json_val);
    }

    Ok(Value::Object(merged))
}

fn load_hooks(config_roots: &[PathBuf]) -> Result<Vec<Value>> {
    let mut all_hooks = Vec::new();

    for root in config_roots {
        let path = root.join("hook_conditions.yaml");
        if !path.is_file() {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let cleaned = lillux::signature::strip_signature_lines(&content);
        let doc: Value = serde_yaml::from_str(&cleaned)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        if let Some(hooks) = doc.get("hooks").and_then(|h| h.as_array()) {
            all_hooks.extend(hooks.iter().cloned());
        }
    }

    Ok(all_hooks)
}

fn load_providers(config_roots: &[PathBuf]) -> Result<HashMap<String, Value>> {
    let mut providers = HashMap::new();

    for root in config_roots {
        let providers_dir = root.join("model_providers");
        if !providers_dir.is_dir() {
            continue;
        }

        for entry in std::fs::read_dir(&providers_dir)
            .with_context(|| format!("failed to read {}", providers_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.extension().map(|e| e == "yaml").unwrap_or(false) {
                continue;
            }

            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let cleaned = lillux::signature::strip_signature_lines(&content);
            let doc: Value = serde_yaml::from_str(&cleaned)
                .with_context(|| format!("failed to parse {}", path.display()))?;

            providers.insert(stem, doc);
        }
    }

    Ok(providers)
}

fn parse_limits(raw: &Value) -> LimitsConfig {
    let defaults = raw.get("defaults").cloned().unwrap_or(Value::Object(Default::default()));
    let caps = raw.get("caps").cloned().unwrap_or(Value::Object(Default::default()));
    LimitsConfig { defaults, caps }
}

fn deep_merge(target: &mut serde_json::Map<String, Value>, source: &Value) {
    if let Value::Object(source_map) = source {
        for (key, value) in source_map {
            if let (Some(Value::Object(existing)), Value::Object(incoming)) =
                (target.get(key), value)
            {
                let mut merged = existing.clone();
                deep_merge(&mut merged, &Value::Object(incoming.clone()));
                target.insert(key.clone(), Value::Object(merged));
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_merge_merges_objects() {
        let mut target = serde_json::Map::new();
        target.insert("a".into(), Value::Number(1.into()));
        target.insert("b".into(), Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("x".into(), Value::Number(10.into()));
            m
        }));

        let source = serde_json::json!({
            "b": { "y": 20 },
            "c": 3,
        });

        deep_merge(&mut target, &source);

        assert_eq!(target["a"], 1);
        assert_eq!(target["b"]["x"], 10);
        assert_eq!(target["b"]["y"], 20);
        assert_eq!(target["c"], 3);
    }

    #[test]
    fn parse_limits_extracts_defaults_and_caps() {
        let raw = serde_json::json!({
            "defaults": { "turns": 25, "tokens": 200000 },
            "caps": { "turns": 100 },
        });
        let limits = parse_limits(&raw);
        assert_eq!(limits.defaults["turns"], 25);
        assert_eq!(limits.caps["turns"], 100);
    }

    #[test]
    fn parse_limits_defaults_when_empty() {
        let raw = serde_json::json!({});
        let limits = parse_limits(&raw);
        assert!(limits.defaults.is_object());
        assert!(limits.caps.is_object());
    }
}

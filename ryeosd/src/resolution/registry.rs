use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::{bail, Result};
use serde_json::Value;

use super::config_loader::RuntimeConfig;
use crate::state::AppState;

pub type ResolverFn = fn(&mut Value, &RuntimeConfig, &rye_engine::contracts::ResolvedItem, &AppState) -> Result<()>;

pub struct ResolverSpec {
    pub name: &'static str,
    pub requires: &'static [&'static str],
    pub produces: &'static [&'static str],
    pub func: ResolverFn,
}

pub struct ResolverRegistry {
    resolvers: HashMap<&'static str, ResolverSpec>,
}

impl ResolverRegistry {
    pub fn new() -> Self {
        Self {
            resolvers: HashMap::new(),
        }
    }

    pub fn register(&mut self, spec: ResolverSpec) {
        self.resolvers.insert(spec.name, spec);
    }

    pub fn get(&self, name: &str) -> Option<&ResolverSpec> {
        self.resolvers.get(name)
    }

    pub fn validate_resolution_list(&self, resolution: &[String]) -> Result<()> {
        for name in resolution {
            if !self.resolvers.contains_key(name.as_str()) {
                bail!("unknown resolver: '{}'", name);
            }
        }

        let mut produced: HashSet<&str> = ["item", "capabilities"].iter().copied().collect();

        for name in resolution {
            let spec = self.resolvers.get(name.as_str()).unwrap();

            for req in spec.requires {
                if !produced.contains(req) {
                    bail!(
                        "resolver '{}' requires '{}' but no prior resolver produces it",
                        name,
                        req
                    );
                }
            }

            for prod in spec.produces {
                produced.insert(prod);
            }
        }

        Ok(())
    }

    pub fn run_resolution(
        &self,
        resolution: &[String],
        blob: &mut Value,
        config: &RuntimeConfig,
        item: &rye_engine::contracts::ResolvedItem,
        state: &AppState,
    ) -> Result<()> {
        for name in resolution {
            let spec = self.resolvers.get(name.as_str()).unwrap();
            (spec.func)(blob, config, item, state)?;
        }
        Ok(())
    }
}

pub fn build_registry() -> ResolverRegistry {
    let mut reg = ResolverRegistry::new();

    reg.register(ResolverSpec {
        name: "resolve_limits",
        requires: &["item"],
        produces: &["limits"],
        func: resolvers::resolve_limits,
    });

    reg.register(ResolverSpec {
        name: "merge_hooks",
        requires: &["item"],
        produces: &["hooks"],
        func: resolvers::merge_hooks,
    });

    reg.register(ResolverSpec {
        name: "validate_env_requires",
        requires: &["item"],
        produces: &[],
        func: resolvers::validate_env_requires,
    });

    reg
}

mod resolvers {
    use anyhow::Result;
    use serde_json::{json, Value};

    use crate::resolution::config_loader::RuntimeConfig;
    use crate::state::AppState;

    pub(crate) fn effective_limit(
        requested: Option<&Value>,
        default: &Value,
        cap: &Value,
        key: &str,
    ) -> Value {
        let req_val = requested.and_then(|v| v.get(key));
        let def_val = default.get(key);
        let cap_val = cap.get(key);

        let effective = match req_val {
            Some(v) => v.clone(),
            None => def_val.cloned().unwrap_or(Value::Null),
        };

        if let Some(cv) = cap_val {
            match (&effective, cv) {
                (Value::Number(e), Value::Number(c)) => {
                    if let (Some(ev), Some(cv2)) = (e.as_f64(), c.as_f64()) {
                        return Value::from(ev.min(cv2) as i64);
                    }
                }
                _ => {}
            }
        }

        effective
    }

    pub fn resolve_limits(
        blob: &mut Value,
        config: &RuntimeConfig,
        _item: &rye_engine::contracts::ResolvedItem,
        _state: &AppState,
    ) -> Result<()> {
        let item_limits = blob.get("item").and_then(|i| i.get("limits"));
        let defaults = &config.limits.defaults;
        let caps = &config.limits.caps;

        let keys = ["turns", "tokens", "spend_usd", "spawns", "depth", "duration_seconds"];
        let mut limits = serde_json::Map::new();
        for key in &keys {
            limits.insert(
                key.to_string(),
                effective_limit(item_limits, defaults, caps, key),
            );
        }

        if let Value::Object(ref mut map) = blob {
            map.insert("limits".to_string(), Value::Object(limits));
        }

        Ok(())
    }

    pub fn merge_hooks(
        blob: &mut Value,
        config: &RuntimeConfig,
        _item: &rye_engine::contracts::ResolvedItem,
        _state: &AppState,
    ) -> Result<()> {
        if let Value::Object(ref mut map) = blob {
            map.insert("hooks".to_string(), json!(config.hooks));
        }
        Ok(())
    }

    pub fn validate_env_requires(
        _blob: &mut Value,
        _config: &RuntimeConfig,
        item: &rye_engine::contracts::ResolvedItem,
        _state: &AppState,
    ) -> Result<()> {
        let env_requires = item
            .metadata
            .extra
            .get("env_requires")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut missing = Vec::new();
        for var in &env_requires {
            if std::env::var(var).is_err() {
                missing.push(var.clone());
            }
        }

        if missing.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("missing required env vars: {}", missing.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_resolution_accepts_valid_chain() {
        let reg = build_registry();
        let resolution = vec![
            "resolve_limits".to_string(),
            "merge_hooks".to_string(),
            "validate_env_requires".to_string(),
        ];
        assert!(reg.validate_resolution_list(&resolution).is_ok());
    }

    #[test]
    fn validate_resolution_rejects_unknown_resolver() {
        let reg = build_registry();
        let resolution = vec!["nonexistent_resolver".to_string()];
        let err = reg.validate_resolution_list(&resolution).unwrap_err();
        assert!(err.to_string().contains("unknown resolver"));
    }

    #[test]
    fn validate_resolution_rejects_missing_dependency() {
        let mut reg = ResolverRegistry::new();
        reg.register(ResolverSpec {
            name: "needs_stuff",
            requires: &["nonexistent_section"],
            produces: &[],
            func: |_, _, _, _| Ok(()),
        });
        let resolution = vec!["needs_stuff".to_string()];
        let err = reg.validate_resolution_list(&resolution).unwrap_err();
        assert!(err.to_string().contains("requires"));
    }

    #[test]
    fn effective_limit_clamps_to_cap() {
        let result = resolvers::effective_limit(
            Some(&serde_json::json!({"turns": 200})),
            &serde_json::json!({"turns": 25}),
            &serde_json::json!({"turns": 100}),
            "turns",
        );
        assert_eq!(result, 100);
    }

    #[test]
    fn effective_limit_uses_default_when_no_request() {
        let result = resolvers::effective_limit(
            None,
            &serde_json::json!({"turns": 25}),
            &serde_json::json!({"turns": 100}),
            "turns",
        );
        assert_eq!(result, 25);
    }
}

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadKindProfile {
    pub root_executable: bool,
    pub supports_interrupt: bool,
    pub supports_continuation: bool,
}

#[derive(Debug, Clone)]
pub struct KindProfileRegistry {
    profiles: HashMap<String, ThreadKindProfile>,
}

impl KindProfileRegistry {
    /// Load profiles from the daemon config file, falling back to defaults.
    pub fn load_from_config(config: &Config) -> Self {
        if let Some(config_path) = Self::find_config_path(config) {
            if let Ok(contents) = fs::read_to_string(&config_path) {
                if let Ok(parsed) = serde_yaml::from_str::<serde_yaml::Value>(&contents) {
                    if let Some(profiles_val) = parsed.get("thread_kind_profiles") {
                        if let Ok(profiles) = serde_yaml::from_value::<
                            HashMap<String, ThreadKindProfile>,
                        >(
                            profiles_val.clone()
                        ) {
                            if !profiles.is_empty() {
                                return Self { profiles };
                            }
                        }
                    }
                }
            }
        }
        Self::load_defaults()
    }

    fn find_config_path(config: &Config) -> Option<PathBuf> {
        let path = config.state_dir.join("config.yaml");
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Load with default profiles.
    pub fn load_defaults() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert(
            "directive_run".to_string(),
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: true,
                supports_continuation: true,
            },
        );
        profiles.insert(
            "tool_run".to_string(),
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
        profiles.insert(
            "graph_run".to_string(),
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: true,
                supports_continuation: true,
            },
        );
        profiles.insert(
            "graph_step".to_string(),
            ThreadKindProfile {
                root_executable: false,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
        profiles.insert(
            "system_task".to_string(),
            ThreadKindProfile {
                root_executable: false,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
        profiles.insert(
            "service_run".to_string(),
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
        // V5.4 SSE seam: `runtime_run` is the schema-declared
        // `thread_profile` for `kind: runtime` items. In V5.3 it
        // mirrors `tool_run` exactly (same lifecycle — no interrupt,
        // no continuation) because every shipped runtime spawns via
        // the V5.2 native machinery. V5.4 streaming runtimes will
        // diverge this profile (e.g. `supports_interrupt: true`)
        // without touching the dispatch core.
        profiles.insert(
            "runtime_run".to_string(),
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
        Self { profiles }
    }

    /// Get profile for a kind. Returns None if kind is unknown.
    pub fn get(&self, kind: &str) -> Option<&ThreadKindProfile> {
        self.profiles.get(kind)
    }

    /// Check if a kind is registered.
    pub fn is_valid(&self, kind: &str) -> bool {
        self.profiles.contains_key(kind)
    }

    /// Check if a kind can be used as a root execution.
    pub fn is_root_executable(&self, kind: &str) -> bool {
        self.profiles.get(kind).is_some_and(|p| p.root_executable)
    }

    /// Derive the list of allowed actions for a thread based on kind profile, status, and process state.
    pub fn allowed_actions(&self, kind: &str, status: &str, has_process: bool) -> Vec<String> {
        let Some(profile) = self.get(kind) else {
            return Vec::new();
        };

        match status {
            "created" | "running" => {
                let mut actions = vec!["cancel".to_string()];
                if has_process {
                    actions.push("kill".to_string());
                }
                if profile.supports_interrupt {
                    actions.push("interrupt".to_string());
                }
                actions
            }
            "completed" | "failed" | "cancelled" | "killed" | "timed_out" => {
                if profile.supports_continuation {
                    vec!["continue".to_string()]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_run_is_root_executable() {
        let reg = KindProfileRegistry::load_defaults();
        assert!(reg.is_root_executable("service_run"));
    }

    #[test]
    fn service_run_no_interrupt_or_continuation() {
        let reg = KindProfileRegistry::load_defaults();
        let profile = reg.get("service_run").unwrap();
        assert!(!profile.supports_interrupt);
        assert!(!profile.supports_continuation);
    }

    #[test]
    fn kind_profile_default_runtime_run_exists() {
        let reg = KindProfileRegistry::load_defaults();
        let profile = reg
            .get("runtime_run")
            .expect("`runtime_run` must be a default profile (A3 SSE seam)");
        assert!(profile.root_executable);
        assert!(!profile.supports_interrupt);
        assert!(!profile.supports_continuation);
    }

    #[test]
    fn service_run_allowed_actions() {
        let reg = KindProfileRegistry::load_defaults();
        // Running: cancel + kill (has_process=true)
        let actions = reg.allowed_actions("service_run", "running", true);
        assert_eq!(actions, vec!["cancel", "kill"]);
        // Running: cancel only (no process)
        let actions = reg.allowed_actions("service_run", "running", false);
        assert_eq!(actions, vec!["cancel"]);
        // Completed: no continuation
        let actions = reg.allowed_actions("service_run", "completed", false);
        assert!(actions.is_empty());
    }
}

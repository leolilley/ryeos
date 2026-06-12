//! Thread-kind profile registry — built from kind schema declarations.
//!
//! Each kind schema's `execution.thread_profile` declares the thread kind
//! name AND its lifecycle flags (interrupt, continuation, root-executable).
//! This registry is built by scanning those declarations at boot — no
//! hardcoded Rust profile map.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use ryeos_engine::kind_registry::ThreadProfileDecl;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadKindProfile {
    pub root_executable: bool,
    pub supports_interrupt: bool,
    pub supports_continuation: bool,
}

impl From<&ThreadProfileDecl> for ThreadKindProfile {
    fn from(tp: &ThreadProfileDecl) -> Self {
        Self {
            root_executable: tp.root_executable,
            supports_interrupt: tp.supports_interrupt,
            supports_continuation: tp.supports_continuation,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KindProfileRegistry {
    profiles: HashMap<String, ThreadKindProfile>,
}

impl KindProfileRegistry {
    /// Build the profile registry from the loaded `KindRegistry`.
    ///
    /// Scans every kind schema for an `execution.thread_profile` declaration
    /// and registers its lifecycle flags.
    ///
    /// If `kind_registry` is `None` (test fixtures, minimal boot), returns
    /// an empty registry.
    pub fn build(kind_registry: Option<&ryeos_engine::kind_registry::KindRegistry>) -> Self {
        let mut profiles = HashMap::new();

        // Scan kind schemas for thread_profile declarations.
        if let Some(kinds) = kind_registry {
            for kind_name in kinds.kinds() {
                let Some(schema) = kinds.get(kind_name) else {
                    continue;
                };
                let Some(exec) = schema.execution() else {
                    continue;
                };
                if let Some(tp) = &exec.thread_profile {
                    profiles.insert(tp.name.clone(), ThreadKindProfile::from(tp));
                }
            }
        }

        // Daemon-internal profile for child threads spawned by
        // launch augmentations (e.g. compose_context_positions).
        // These threads use the target kind's thread_profile when
        // available, falling back to "system_task" otherwise.
        Self::insert_internal_profiles(&mut profiles);

        tracing::info!(
            count = profiles.len(),
            names = ?profiles.keys().collect::<Vec<_>>(),
            "thread kind profiles loaded"
        );

        Self { profiles }
    }

    fn insert_internal_profiles(profiles: &mut HashMap<String, ThreadKindProfile>) {
        // system_task: non-root, non-interruptible, non-continuable.
        // Used for daemon-internal maintenance threads (e.g. launch
        // augmentation child threads when no target kind profile exists).
        profiles.insert(
            "system_task".to_string(),
            ThreadKindProfile {
                root_executable: false,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
        // seat_session: the seat is itself a thread — braided, owned,
        // replayable. Opened/settled via the seat services, never
        // root-executed; reattach (continuation) arrives later.
        profiles.insert(
            "seat_session".to_string(),
            ThreadKindProfile {
                root_executable: false,
                supports_interrupt: false,
                supports_continuation: false,
            },
        );
    }

    /// Get profile for a thread kind. Returns None if unknown.
    pub fn get(&self, kind: &str) -> Option<&ThreadKindProfile> {
        self.profiles.get(kind)
    }

    /// Check if a thread kind is registered.
    pub fn is_valid(&self, kind: &str) -> bool {
        self.profiles.contains_key(kind)
    }

    /// Check if a thread kind can be used as a root execution.
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
    fn build_with_no_kind_registry_has_internal_profiles() {
        let reg = KindProfileRegistry::build(None);
        assert!(reg.is_valid("system_task"));
        assert!(!reg.is_valid("tool_run")); // no kind registry → no schema-derived profiles
    }

    #[test]
    fn system_task_not_root_executable() {
        let reg = KindProfileRegistry::build(None);
        assert!(!reg.is_root_executable("system_task"));
    }

    #[test]
    fn allowed_actions_running_with_process() {
        let reg = KindProfileRegistry::build(None);
        let actions = reg.allowed_actions("system_task", "running", true);
        assert_eq!(actions, vec!["cancel", "kill"]);
    }

    #[test]
    fn allowed_actions_completed_no_continuation() {
        let reg = KindProfileRegistry::build(None);
        let actions = reg.allowed_actions("system_task", "completed", false);
        assert!(actions.is_empty());
    }

    #[test]
    fn build_from_kind_registry() {
        // Test that ThreadKindProfile::from(ThreadProfileDecl) works.
        let decl = ThreadProfileDecl {
            name: "test_kind_run".to_string(),
            root_executable: true,
            supports_interrupt: true,
            supports_continuation: false,
        };
        let profile = ThreadKindProfile::from(&decl);
        assert!(profile.root_executable);
        assert!(profile.supports_interrupt);
        assert!(!profile.supports_continuation);
    }
}

//! Thread-kind profile registry — built from kind schema declarations.
//!
//! Each kind schema's `execution.thread_profile` declares the thread kind
//! name AND its lifecycle flags (interrupt, continuation, root-executable).
//! This registry is built by scanning those declarations at boot — no
//! hardcoded Rust profile map.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use ryeos_engine::kind_registry::ThreadProfileDecl;
use ryeos_state::FencedChainKinds;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadKindProfile {
    pub root_executable: bool,
    pub supports_interrupt: bool,
    pub supports_continuation: bool,
    pub supports_operator_followup: bool,
    /// Terminal chains of this kind retire through a fenced retained-evidence
    /// manifest instead of immediate removal. Declared by the kind's signed
    /// profile; the registry never names a kind.
    #[serde(default)]
    pub retirement_fence: bool,
}

impl From<&ThreadProfileDecl> for ThreadKindProfile {
    fn from(tp: &ThreadProfileDecl) -> Self {
        Self {
            root_executable: tp.root_executable,
            supports_interrupt: tp.supports_interrupt,
            supports_continuation: tp.supports_continuation,
            supports_operator_followup: tp.supports_operator_followup,
            retirement_fence: tp.retirement_fence,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KindProfileRegistry {
    profiles: HashMap<String, ThreadKindProfile>,
    /// Built once from the profiles that declare `retirement_fence`.
    fenced_chain_kinds: OnceLock<FencedChainKinds>,
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

        // Add the small set of non-schema daemon bookkeeping profiles. Actual
        // executable workers, including launch augmentations, cross root
        // admission under their verified runtime kind's declared profile.
        Self::insert_internal_profiles(&mut profiles);

        tracing::info!(
            count = profiles.len(),
            names = ?profiles.keys().collect::<Vec<_>>(),
            "thread kind profiles loaded"
        );

        Self {
            profiles,
            fenced_chain_kinds: OnceLock::new(),
        }
    }

    /// Add a synthetic profile to a test harness that deliberately does not
    /// load signed kind schemas. Production registries remain schema-derived.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn with_test_profile(
        mut self,
        name: impl Into<String>,
        profile: ThreadKindProfile,
    ) -> Self {
        self.profiles.insert(name.into(), profile);
        self.fenced_chain_kinds = OnceLock::new();
        self
    }

    /// The thread kinds whose terminal chains retire through a fence, taken
    /// from the profiles' declared flag and built once. A kind name the
    /// fence set refuses is a refusal here, never a skipped kind.
    pub fn fenced_chain_kinds(&self) -> anyhow::Result<&FencedChainKinds> {
        if let Some(kinds) = self.fenced_chain_kinds.get() {
            return Ok(kinds);
        }
        let kinds = FencedChainKinds::new(
            self.profiles
                .iter()
                .filter(|(_, profile)| profile.retirement_fence)
                .map(|(name, _)| name.clone()),
        )?;
        Ok(self.fenced_chain_kinds.get_or_init(|| kinds))
    }

    fn insert_internal_profiles(profiles: &mut HashMap<String, ThreadKindProfile>) {
        // system_task: non-root, non-interruptible, non-continuable. Reserved
        // for daemon-internal bookkeeping rows that do not execute an item.
        profiles.insert(
            "system_task".to_string(),
            ThreadKindProfile {
                root_executable: false,
                supports_interrupt: false,
                supports_continuation: false,
                supports_operator_followup: false,
                retirement_fence: false,
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
                // Terminal "continue" is an OPERATOR action — it must gate on
                // operator follow-up, not mere (machine) continuation. A graph
                // self-continues by machine from its running segment path, never
                // by an operator continuing a settled turn.
                if profile.supports_continuation && profile.supports_operator_followup {
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
            supports_operator_followup: false,
            retirement_fence: true,
        };
        let profile = ThreadKindProfile::from(&decl);
        assert!(profile.root_executable);
        assert!(profile.supports_interrupt);
        assert!(!profile.supports_continuation);
        assert!(!profile.supports_operator_followup);
        assert!(profile.retirement_fence);
    }

    #[test]
    fn test_profile_can_model_a_schema_kind_without_hardcoding_production_registry() {
        let reg = KindProfileRegistry::build(None).with_test_profile(
            "graph_run",
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: false,
                supports_continuation: true,
                supports_operator_followup: false,
                retirement_fence: false,
            },
        );
        let profile = reg.get("graph_run").expect("synthetic graph profile");
        assert!(profile.root_executable);
        assert!(!profile.supports_interrupt);
        assert!(profile.supports_continuation);
        assert!(!profile.supports_operator_followup);
    }

    fn fenced_profile() -> ThreadKindProfile {
        ThreadKindProfile {
            root_executable: false,
            supports_interrupt: false,
            supports_continuation: false,
            supports_operator_followup: false,
            retirement_fence: true,
        }
    }

    #[test]
    fn fenced_kinds_come_only_from_the_declared_flag() {
        let reg = KindProfileRegistry::build(None);
        assert!(reg.fenced_chain_kinds().unwrap().is_empty());
        let reg = reg.with_test_profile("fenced_kind", fenced_profile());
        let kinds = reg.fenced_chain_kinds().unwrap();
        assert!(kinds.contains("fenced_kind"));
        assert!(!kinds.contains("system_task"));
        // Built once: the same set answers every later call.
        assert!(std::ptr::eq(kinds, reg.fenced_chain_kinds().unwrap()));
    }

    #[test]
    fn no_kind_is_named_by_the_registry_itself() {
        let reg = KindProfileRegistry::build(None);
        assert!(!reg.is_valid("seat_session"));
    }

    #[test]
    fn a_padded_fenced_kind_name_is_refused() {
        let reg = KindProfileRegistry::build(None).with_test_profile(" padded", fenced_profile());
        assert!(reg.fenced_chain_kinds().is_err());
    }

    #[test]
    fn a_predecessor_profile_shape_with_unknown_fields_is_refused() {
        let refused = serde_json::from_str::<ThreadKindProfile>(
            r#"{"root_executable":false,"supports_interrupt":false,"supports_continuation":false,"supports_operator_followup":false,"seat":true}"#,
        );
        assert!(refused.is_err());
        let defaulted = serde_json::from_str::<ThreadKindProfile>(
            r#"{"root_executable":false,"supports_interrupt":false,"supports_continuation":false,"supports_operator_followup":false}"#,
        )
        .unwrap();
        assert!(!defaulted.retirement_fence);
    }
}

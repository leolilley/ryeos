//! `SubprocessRole` — role the subprocess plays in the dispatch loop.
//!
//! Drives B1 cap-gate enforcement. The role is INDEPENDENT of the protocol
//! the subprocess speaks — a future kind using `protocol: runtime_v1`
//! without being a runtime MUST NOT trigger the `runtime.execute` cap check.
//! Only `SubprocessRole::RuntimeTarget` does that, and the role is set
//! EXACTLY ONCE based on the user's original `item_ref` at the top of
//! `dispatch_loop`.

use ryeos_engine::runtime_registry::VerifiedRuntime;

/// Role the subprocess plays in the dispatch loop. Drives B1 cap
/// enforcement; INDEPENDENT of the protocol the subprocess speaks.
#[derive(Debug, Clone)]
pub enum SubprocessRole {
    /// Default; tool path, service path, alias-chain leaf.
    Regular,
    /// Direct user invocation of a `runtime:*` root ref. Triggers
    /// the `runtime.execute` cap gate.
    RuntimeTarget {
        verified_runtime: Box<VerifiedRuntime>,
    },
}

/// Capabilities required for the runtime target role.
pub const RUNTIME_EXECUTE_CAP: &str = "runtime.execute";

/// B1 enforcement: only direct `runtime:*` root invocation triggers
/// the cap check. Alias chains (directive → registry → runtime) do NOT
/// inherit the cap. This is a ROLE check, not a protocol check.
pub fn enforce_runtime_target_caps(
    role: &SubprocessRole,
    acting_caps: &[String],
) -> Result<(), super::dispatch_error::DispatchError> {
    if !matches!(role, SubprocessRole::RuntimeTarget { .. }) {
        return Ok(());
    }
    if !acting_caps.iter().any(|c| c == RUNTIME_EXECUTE_CAP || c == "*") {
        return Err(super::dispatch_error::DispatchError::MissingCap {
            required: RUNTIME_EXECUTE_CAP.to_string(),
        });
    }
    Ok(())
}

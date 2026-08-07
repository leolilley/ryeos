//! Content-addressed identity of the execution substrate.
//!
//! The effective definition digest names the program; this object names
//! what computes it — device, kernel stack, numerics, interpreter — as a
//! coordinate beside the program digest, never inside it. Recorded-class
//! evidence is portable across execution identities by construction;
//! sealed-class claims are scoped to (program digest, execution identity)
//! and degrade to recorded where the identities differ. A boundary with no
//! honest identity to claim (a remote provider) carries none, which is
//! exactly why it caps at the recorded class.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXECUTION_IDENTITY_KIND: &str = "execution_identity";
pub const EXECUTION_IDENTITY_SCHEMA_VERSION: u32 = 1;

/// Ceiling for the serialized identity. An execution identity is a
/// description, not a payload; anything approaching this bound is smuggling
/// content that belongs in a realization.
pub const MAX_EXECUTION_IDENTITY_BYTES: usize = 64 * 1024;

/// The device tranche: what silicon, described no further than codegen
/// cares. Self-attested by the node's own probe — no confidential-computing
/// claim is made or implied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDeviceIdentity {
    /// Coarse class: `cpu` today, `gpu` when local inference brings one.
    pub class: String,
    /// Architecture string as codegen sees it (e.g. `x86_64`,
    /// `gfx1201`).
    pub arch: String,
    /// Feature flags and model detail that reach codegen, when the probe
    /// can name them. Open by design: identity is the canonical bytes,
    /// not this module's interpretation of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// The interpreter tranche: the ambient `python-interpreter` residue,
/// absorbed into named identity instead of remaining a footnote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionInterpreterIdentity {
    /// Reported version string (e.g. `3.13.5`).
    pub version: String,
    /// Content digest of the interpreter binary the node resolves.
    pub binary_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIdentity {
    pub schema: u32,
    pub kind: String,
    pub device: ExecutionDeviceIdentity,
    /// Interpreter tranche; absent on nodes that resolve none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<ExecutionInterpreterIdentity>,
    /// Kernel-stack tranche — tinygrad tree, compiler/driver versions,
    /// compiled kernel set and BEAM cache digests. Values arrive with the
    /// sealed-local runtime; the slot exists now so the object's shape
    /// never migrates when they do. Open: identity is the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_stack: Option<Value>,
    /// Numerics tranche — the policy flags that change bits. Same
    /// arrival story as `kernel_stack`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numerics: Option<Value>,
}

fn require_hex64(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("execution identity {field} must be 64 lowercase hex characters");
    }
    Ok(())
}

impl ExecutionIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != EXECUTION_IDENTITY_KIND {
            bail!("unexpected execution identity kind: {}", self.kind);
        }
        if self.schema != EXECUTION_IDENTITY_SCHEMA_VERSION {
            bail!(
                "unexpected execution identity schema: {} (current {})",
                self.schema,
                EXECUTION_IDENTITY_SCHEMA_VERSION
            );
        }
        for (field, value) in [
            ("device class", &self.device.class),
            ("device arch", &self.device.arch),
        ] {
            super::validate_trimmed_control_free(
                &format!("execution identity {field}"),
                value,
                false,
            )?;
        }
        if let Some(interpreter) = &self.interpreter {
            super::validate_trimmed_control_free(
                "execution identity interpreter version",
                &interpreter.version,
                false,
            )?;
            require_hex64("interpreter binary_sha256", &interpreter.binary_sha256)?;
        }
        let bytes = serde_json::to_vec(self)?.len();
        if bytes > MAX_EXECUTION_IDENTITY_BYTES {
            bail!(
                "execution identity is {bytes} bytes; the bound is \
                 {MAX_EXECUTION_IDENTITY_BYTES} — identities describe, they do not carry"
            );
        }
        Ok(())
    }

    /// The coordinate itself: sha256 over the canonical object. Everything
    /// that scopes a sealed claim compares this digest, never fields.
    pub fn identity_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
    }

    /// Decode only the exact current wire contract, rejecting other schemas
    /// before serde interprets any field.
    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("execution identity has no string kind"))?;
        if kind != EXECUTION_IDENTITY_KIND {
            bail!("unexpected execution identity kind: {kind}");
        }
        let schema = value
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("execution identity has no numeric schema"))?;
        if schema != u64::from(EXECUTION_IDENTITY_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "execution identity",
                schema,
                EXECUTION_IDENTITY_SCHEMA_VERSION,
            )
            .into());
        }
        let identity: Self = serde_json::from_value(value.clone())?;
        identity.validate()?;
        Ok(identity)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> ExecutionIdentity {
        ExecutionIdentity {
            schema: EXECUTION_IDENTITY_SCHEMA_VERSION,
            kind: EXECUTION_IDENTITY_KIND.to_string(),
            device: ExecutionDeviceIdentity {
                class: "cpu".to_string(),
                arch: "x86_64".to_string(),
                detail: Some(json!({"features": ["avx2"]})),
            },
            interpreter: Some(ExecutionInterpreterIdentity {
                version: "3.13.5".to_string(),
                binary_sha256: "a".repeat(64),
            }),
            kernel_stack: None,
            numerics: None,
        }
    }

    #[test]
    fn an_identity_round_trips_through_the_current_contract() {
        let value = identity().to_value().unwrap();
        let decoded = ExecutionIdentity::from_current_value(&value).unwrap();
        assert_eq!(decoded, identity());
    }

    #[test]
    fn the_digest_is_deterministic_and_moves_with_any_tranche() {
        let base = identity().identity_digest().unwrap();
        assert_eq!(base, identity().identity_digest().unwrap());

        let mut other_arch = identity();
        other_arch.device.arch = "aarch64".to_string();
        assert_ne!(base, other_arch.identity_digest().unwrap());

        let mut other_interp = identity();
        other_interp.interpreter.as_mut().unwrap().version = "3.14.0".to_string();
        assert_ne!(base, other_interp.identity_digest().unwrap());

        let mut with_kernels = identity();
        with_kernels.kernel_stack = Some(json!({"tinygrad": "b".repeat(64)}));
        assert_ne!(base, with_kernels.identity_digest().unwrap());
    }

    #[test]
    fn a_predecessor_schema_is_rejected_before_field_interpretation() {
        let mut value = identity().to_value().unwrap();
        value["schema"] = json!(0);
        value["device"] = json!("garbage");
        let error = ExecutionIdentity::from_current_value(&value).unwrap_err();
        assert!(error.to_string().contains("schema"));
    }

    #[test]
    fn a_malformed_interpreter_digest_fails_closed() {
        let mut invalid = identity();
        invalid.interpreter.as_mut().unwrap().binary_sha256 = "junk".to_string();
        assert!(invalid.validate().is_err());
    }
}

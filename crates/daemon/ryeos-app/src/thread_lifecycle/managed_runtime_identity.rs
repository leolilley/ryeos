//! Current installed identity for daemon-managed runtimes.
//!
//! This module owns the common, non-launching identity projection used by a
//! fresh managed launch and by callers which must prove what that same launch
//! would select now. It does not materialize or execute the native executor;
//! the executor crate retains the complete-root ambiguity proof, sealed cache,
//! and process-launch authority.

use anyhow::{Context as _, Result, anyhow, bail};
use ryeos_engine::kind_registry::TerminatorDecl;
use ryeos_engine::protocol_vocabulary::{
    CallbackChannel, LifecycleMode, StdinShape, StdoutMode, StdoutShape,
};
use ryeos_engine::resolution::TrustClass;

/// Exact signed runtime and protocol selected from one installed generation.
#[derive(Debug, Clone)]
pub struct CurrentManagedRuntimeSelection {
    pub runtime: ryeos_engine::runtime_registry::VerifiedRuntime,
    pub protocol: ryeos_engine::protocols::VerifiedProtocol,
    pub executor_ref: String,
}

/// Exact signed bundle identity of the executor named by a verified runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentManagedExecutorIdentity {
    pub executor_ref: String,
    pub content_hash: String,
    pub bundle_manifest_hash: String,
    pub bundle_signer_fingerprint: String,
}

impl CurrentManagedExecutorIdentity {
    /// Compare all behavior-bearing fields supplied by the executor's stronger
    /// complete-root verification and descriptor-bound materialization owner.
    pub fn matches_materialized(
        &self,
        executor_ref: &str,
        content_hash: &str,
        bundle_manifest_hash: &str,
        bundle_signer_fingerprint: &str,
    ) -> bool {
        self.executor_ref == executor_ref
            && self.content_hash == content_hash
            && self.bundle_manifest_hash == bundle_manifest_hash
            && self.bundle_signer_fingerprint == bundle_signer_fingerprint
    }
}

fn resolve_selection_in_generation(
    engine: &ryeos_engine::engine::Engine,
    runtime_ref: Option<&str>,
    served_kind: &str,
) -> Result<CurrentManagedRuntimeSelection> {
    let runtime = engine
        .runtimes
        .resolve_for_launch(runtime_ref, served_kind)
        .map_err(|error| anyhow!(error))?
        .clone();
    if runtime.yaml.serves != served_kind {
        bail!(
            "managed runtime '{}' serves kind '{}', not requested kind '{}'",
            runtime.canonical_ref,
            runtime.yaml.serves,
            served_kind,
        );
    }
    if runtime.trust_class != TrustClass::TrustedBundle {
        bail!(
            "managed runtime '{}' does not have installed TrustedBundle provenance",
            runtime.canonical_ref
        );
    }
    if engine
        .kinds
        .get(served_kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.method_dispatch.as_ref())
        .is_some()
    {
        bail!(
            "managed runtime '{}' serves a method-dispatch-only kind",
            runtime.canonical_ref
        );
    }

    let runtime_schema = engine
        .kinds
        .get(&runtime.canonical_ref.kind)
        .ok_or_else(|| {
            anyhow!(
                "verified runtime '{}' has no registered kind schema",
                runtime.canonical_ref
            )
        })?;
    let protocol_ref = match runtime_schema
        .execution()
        .and_then(|execution| execution.terminator.as_ref())
    {
        Some(TerminatorDecl::Subprocess { protocol }) => {
            protocol.static_ref().ok_or_else(|| {
                anyhow!(
                    "runtime '{}' protocol selection was not resolved to a static ref",
                    runtime.canonical_ref
                )
            })?
        }
        Some(other) => {
            bail!(
                "runtime '{}' declares non-subprocess terminator {other:?}",
                runtime.canonical_ref
            )
        }
        None => bail!(
            "runtime '{}' has no subprocess terminator",
            runtime.canonical_ref
        ),
    };
    let protocol = engine
        .protocols
        .require(protocol_ref)
        .with_context(|| format!("resolve managed runtime protocol '{protocol_ref}'"))?
        .clone();
    if protocol.trust_class != TrustClass::TrustedBundle {
        bail!(
            "managed runtime protocol '{}' does not have installed TrustedBundle provenance",
            protocol.canonical_ref
        );
    }
    if protocol.descriptor.callback_channel == CallbackChannel::None
        || protocol.descriptor.stdin.shape != StdinShape::LaunchEnvelope
        || protocol.descriptor.stdout.shape != StdoutShape::RuntimeResult
        || protocol.descriptor.stdout.mode != StdoutMode::Terminal
        || protocol.descriptor.lifecycle.mode != LifecycleMode::Managed
    {
        bail!(
            "managed runtime protocol '{}' is not the callback launch-envelope/runtime-result contract",
            protocol.canonical_ref
        );
    }

    let executor_ref = runtime
        .native_executor_ref()
        .context("derive managed native executor ref")?;
    Ok(CurrentManagedRuntimeSelection {
        runtime,
        protocol,
        executor_ref,
    })
}

/// Select a current signed runtime and callback protocol under one coherent
/// installed-bundle generation. This is read-only and does not prepare or
/// launch an execution.
pub fn resolve_current_managed_runtime_selection(
    engine: &ryeos_engine::engine::Engine,
    runtime_ref: Option<&str>,
    served_kind: &str,
) -> Result<CurrentManagedRuntimeSelection> {
    engine.with_checked_bundle_generation(|_| {
        resolve_selection_in_generation(engine, runtime_ref, served_kind)
    })
}

/// Re-resolve the executor named by `expected` from the verified runtime's
/// exact source bundle. The re-selection check prevents a caller from pairing
/// an earlier runtime/protocol projection with a different installed
/// generation. Hashing the binary is intentionally synchronous so launch
/// callers can place this operation inside their existing blocking boundary.
pub fn resolve_current_managed_executor_identity(
    engine: &ryeos_engine::engine::Engine,
    expected: &CurrentManagedRuntimeSelection,
) -> Result<CurrentManagedExecutorIdentity> {
    engine.with_checked_bundle_generation(|_| {
        let expected_runtime_ref = expected.runtime.canonical_ref.to_string();
        let current = resolve_selection_in_generation(
            engine,
            Some(&expected_runtime_ref),
            &expected.runtime.yaml.serves,
        )?;
        for (label, expected_value, current_value) in [
            (
                "runtime content hash",
                expected.runtime.raw_content_digest.as_str(),
                current.runtime.raw_content_digest.as_str(),
            ),
            (
                "runtime signer",
                expected.runtime.signer_fingerprint.as_str(),
                current.runtime.signer_fingerprint.as_str(),
            ),
            (
                "protocol ref",
                expected.protocol.canonical_ref.as_str(),
                current.protocol.canonical_ref.as_str(),
            ),
            (
                "protocol content hash",
                expected.protocol.raw_content_digest.as_str(),
                current.protocol.raw_content_digest.as_str(),
            ),
            (
                "protocol signer",
                expected.protocol.signer_fingerprint.as_str(),
                current.protocol.signer_fingerprint.as_str(),
            ),
            (
                "executor ref",
                expected.executor_ref.as_str(),
                current.executor_ref.as_str(),
            ),
        ] {
            if expected_value != current_value {
                bail!(
                    "managed {label} changed across installed-generation selection: expected={expected_value}, current={current_value}"
                );
            }
        }
        if expected.runtime.bundle_root != current.runtime.bundle_root {
            bail!("managed runtime source bundle changed across installed-generation selection");
        }

        // Node bundle admission has already verified unique executor ownership
        // across the complete registered root set. This generation guard pins
        // those signed manifest identities, so current inspection reuses that
        // proof rather than adding another all-bundle scan here. Actual spawn
        // still owns descriptor-bound materialization and cache attestation.
        let binary = ryeos_engine::binary_resolver::resolve_bundle_binary_ref(
            &current.runtime.yaml.binary_ref,
            &current.runtime.bundle_root,
            |fingerprint| {
                engine
                    .node_trust_store
                    .get(fingerprint)
                    .map(|signer| signer.verifying_key)
            },
            TrustClass::TrustedBundle,
        )
        .with_context(|| {
            format!(
                "resolve managed executor '{}' from runtime source bundle",
                current.executor_ref
            )
        })?;
        Ok(CurrentManagedExecutorIdentity {
            executor_ref: current.executor_ref,
            content_hash: binary.content_hash,
            bundle_manifest_hash: binary.manifest_hash,
            bundle_signer_fingerprint: binary.signer_fingerprint,
        })
    })
}

/// Construct the one canonical managed artifact identity after the caller has
/// additionally matched the executor crate's complete-root verification and
/// descriptor-bound materialization to `executor`.
pub fn managed_runtime_artifact_identity(
    selection: &CurrentManagedRuntimeSelection,
    executor: &CurrentManagedExecutorIdentity,
) -> Result<ryeos_state::objects::AdmittedLaunchArtifactIdentity> {
    if selection.executor_ref != executor.executor_ref {
        bail!(
            "managed executor identity ref '{}' does not match selected runtime ref '{}'",
            executor.executor_ref,
            selection.executor_ref
        );
    }
    let identity = ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
        runtime_ref: selection.runtime.canonical_ref.to_string(),
        runtime_content_hash: selection.runtime.raw_content_digest.clone(),
        runtime_signer_fingerprint: selection.runtime.signer_fingerprint.clone(),
        protocol_ref: selection.protocol.canonical_ref.clone(),
        protocol_content_hash: selection.protocol.raw_content_digest.clone(),
        protocol_signer_fingerprint: selection.protocol.signer_fingerprint.clone(),
        executor_ref: executor.executor_ref.clone(),
        executor_content_hash: executor.content_hash.clone(),
        executor_bundle_manifest_hash: executor.bundle_manifest_hash.clone(),
        executor_bundle_signer_fingerprint: executor.bundle_signer_fingerprint.clone(),
    };
    identity.validate()?;
    Ok(identity)
}

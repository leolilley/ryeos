//! Internal execution primitives shared by release and authority-calibration flows.
//!
//! These helpers retain the normal pinned-project admission, terminal product
//! acceptance, and qualification-publication gates.  Callers select a purpose-
//! owned Graph and recipes; they cannot inject accepted evidence directly.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::Context as _;
use ryeos_app::{handler_context::HandlerContext, state::AppState};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub(crate) struct PinnedGraphExecution {
    pub graph_ref: &'static str,
    pub source_project: PathBuf,
    pub expected_source_hash: String,
    pub signed_build_recipe: String,
    pub build_recipe_filename: &'static str,
    pub build_recipe_must_match_source: bool,
    pub capture_recipe_filename: &'static str,
    pub signed_capture_recipe: Option<String>,
    pub recipe_ref: &'static str,
    pub expected_recipe_raw_digest: String,
    pub parameters: Value,
    pub product_selections:
        Vec<ryeos_state::external_content::products::composition::ProductSelectionInput>,
    pub pre_minted_thread_id: Option<String>,
}

pub(crate) fn materialize_execution_project(
    source: &Path,
    expected_source_hash: &str,
    signed_build_recipe: &str,
    build_recipe_filename: &str,
    build_recipe_must_match_source: bool,
    capture_recipe_filename: &str,
    signed_capture_recipe: Option<&str>,
) -> anyhow::Result<tempfile::TempDir> {
    struct HashingReader<R> {
        inner: R,
        digest: Sha256,
    }
    impl<R: std::io::Read> std::io::Read for HashingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.inner.read(buffer)?;
            self.digest.update(&buffer[..count]);
            Ok(count)
        }
    }

    let workspace = tempfile::Builder::new()
        .prefix("ryeos-bundle-release-")
        .tempdir()?;
    let mut archive = Command::new("git")
        .args(["archive", "--format=tar", "HEAD"])
        .current_dir(source)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start exact release source materialization")?;
    let stdout = archive
        .stdout
        .take()
        .context("release source materializer has no stdout")?;
    let mut reader = HashingReader {
        inner: stdout,
        digest: Sha256::new(),
    };
    {
        let mut tar = tar::Archive::new(&mut reader);
        tar.unpack(workspace.path())
            .context("materialize exact release source archive")?;
    }
    std::io::copy(&mut reader, &mut std::io::sink())
        .context("finish hashing exact release source archive")?;
    let output = archive
        .wait_with_output()
        .context("wait for release source materialization")?;
    anyhow::ensure!(
        output.status.success(),
        "release source materialization failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(
        format!("{:x}", reader.digest.finalize()) == expected_source_hash,
        "release source HEAD changed after its admitted snapshot was authorized"
    );

    let mut directory = workspace.path().to_path_buf();
    for component in [".ai", "config", "bundle-release"] {
        directory.push(component);
        match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "release recipe overlay crosses an unsafe source entry"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    for (name, contents) in [
        (build_recipe_filename, Some(signed_build_recipe)),
        (capture_recipe_filename, signed_capture_recipe),
    ] {
        let Some(contents) = contents else { continue };
        let recipe_path = directory.join(name);
        if name == build_recipe_filename && build_recipe_must_match_source {
            let archived = lillux::read_regular_file_bounded_no_follow(&recipe_path, 256 * 1024)
                .context("read fixed recipe from verified source snapshot")?;
            anyhow::ensure!(
                archived == contents.as_bytes(),
                "fixed release recipe differs from the verified source snapshot"
            );
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&recipe_path) {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "release source has an unsafe fixed recipe entry"
            );
        }
        lillux::atomic_write_private(&recipe_path, contents.as_bytes())?;
    }
    Ok(workspace)
}

pub(crate) async fn execute_pinned_graph(
    request: PinnedGraphExecution,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        context.verified,
        "bundle release execution requires an authenticated caller"
    );
    let PinnedGraphExecution {
        graph_ref,
        source_project,
        expected_source_hash,
        signed_build_recipe,
        build_recipe_filename,
        build_recipe_must_match_source,
        capture_recipe_filename,
        signed_capture_recipe,
        recipe_ref,
        expected_recipe_raw_digest,
        parameters,
        product_selections,
        pre_minted_thread_id,
    } = request;
    let workspace = tokio::task::spawn_blocking(move || {
        materialize_execution_project(
            &source_project,
            &expected_source_hash,
            &signed_build_recipe,
            build_recipe_filename,
            build_recipe_must_match_source,
            capture_recipe_filename,
            signed_capture_recipe.as_deref(),
        )
    })
    .await
    .context("release execution project materializer stopped")??;

    let mut policy = ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
        ryeos_app::execution_policy::ExecutionResponse::Wait,
    );
    policy.ownership = ryeos_app::execution_policy::ExecutionOwnership::RequestScoped;
    policy.recovery = ryeos_app::execution_policy::ExecutionRecovery::None;
    policy.validate()?;
    let project_source =
        ryeos_executor::execution::project_source::ProjectSource::CaptureLiveFullProject;
    let mut project_ctx =
        crate::routes::response_modes::execute_mode::resolve_project_context_off_thread(
            crate::routes::response_modes::execute_mode::ResolveProjectContextRequest {
                state: state.as_ref().clone(),
                source: project_source.clone(),
                project_path: workspace.path().to_path_buf(),
                principal_id: context.fingerprint.clone(),
                checkout_id: format!("bundle-release-{}", uuid::Uuid::new_v4()),
                pinned_realization: Some(
                    ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
                ),
                normalization: crate::routes::response_modes::execute_mode::ProjectRootNormalization::CanonicalizeLive,
                launch_timings: None,
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("capture release execution project: {error}"))?;
    let resolved = crate::routes::response_modes::execute_mode::resolve_execution_contract(
        &policy,
        &project_source,
        &project_ctx,
        None,
        None,
        &context.fingerprint,
        &context.scopes,
        &state,
    )?;
    let effective_path = project_ctx.effective_path.clone();
    let site_id = state.threads.site_id().to_owned();
    let plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: context.fingerprint.clone(),
                scopes: context.scopes.clone(),
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: effective_path.clone(),
        },
        subject_resolution_authority: resolved.provenance.subject_resolution_authority(),
        current_site_id: site_id.clone(),
        origin_site_id: context.execution_origin(&site_id),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let recipe_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(recipe_ref)?;
    let resolved_recipe = project_ctx.request_engine.resolve(&plan_ctx, &recipe_ref)?;
    anyhow::ensure!(
        resolved_recipe.raw_content_digest == expected_recipe_raw_digest,
        "admitted native build recipe differs from publisher-authorized bytes"
    );
    project_ctx
        .request_engine
        .verify(&plan_ctx, resolved_recipe)
        .context("publisher-authorized native build recipe is not trusted")?;
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: context.fingerprint.clone(),
        caller_scopes: context.scopes.clone(),
        engine: Arc::clone(&project_ctx.request_engine),
        plan_ctx,
        requested_call: None,
    };
    let acting_principal = context.fingerprint.clone();
    let dispatch_request = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params: parameters,
        ref_bindings: Default::default(),
        product_selections,
        acting_principal: &acting_principal,
        project_path: &effective_path,
        provenance: resolved.provenance,
        lifecycle_authority: resolved.lifecycle_authority,
        launch_timings: None,
        original_root_kind: if graph_ref.starts_with("tool:") {
            "tool"
        } else {
            "graph"
        },
        pre_minted_thread_id,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };
    let result = ryeos_executor::dispatch::dispatch_with_handler_context(
        graph_ref,
        context,
        &dispatch_request,
        &exec_ctx,
        &state,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{graph_ref} dispatch failed: {error}"));
    // Captured roots outlive dispatch publication and no longer.
    drop(project_ctx.take_captured_generation());
    drop(workspace);
    result
}

pub(crate) fn successful_dispatch_result(envelope: &Value) -> anyhow::Result<&Value> {
    let terminal = envelope
        .get("result")
        .context("release execution omitted its terminal result envelope")?;
    anyhow::ensure!(
        terminal.get("outcome_code").and_then(Value::as_str) == Some("success"),
        "release execution did not complete successfully"
    );
    terminal
        .get("result")
        .filter(|value| !value.is_null())
        .context("release execution omitted its successful result")
}

pub(crate) fn accept_dispatch_products(
    envelope: &Value,
    state: &AppState,
) -> anyhow::Result<(
    String,
    ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
)> {
    let thread_id = envelope
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .context("release producer omitted its terminal thread identity")?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let capsule = state
        .state_store
        .admitted_launch_capsule(thread_id)?
        .context("release producer has no admitted launch capsule")?;
    let accepted = ryeos_app::operator_external_content::product_build::accept_terminal(
        state, &capsule, thread_id, &guard,
    )?;
    ryeos_app::operator_external_content::product_build::verify_current(
        state,
        &guard,
        &accepted.to_value()?,
        &capsule,
    )?;
    let hash = authority.cas_store()?.store_object(&accepted.to_value()?)?;
    Ok((hash, accepted))
}

pub(crate) async fn publish_qualification(
    state: Arc<AppState>,
    context: HandlerContext,
    witness_hash: String,
    relationship_name: String,
    verifier_chain_root_id: String,
    verifier_thread_id: String,
) -> anyhow::Result<
    ryeos_app::operator_external_content::product_qualification::ProductQualificationResponse,
> {
    ryeos_app::operator_external_content::product_qualification::qualify(
        state,
        context,
        ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
            witness_hash,
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            relationship_name,
            verifier_chain_root_id,
            verifier_thread_id,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::successful_dispatch_result;
    use serde_json::json;

    #[test]
    fn successful_result_rejects_failed_or_empty_terminal_envelopes() {
        assert!(successful_dispatch_result(&json!({"result":{"outcome_code":"failed"}})).is_err());
        assert!(
            successful_dispatch_result(&json!({"result":{"outcome_code":"success","result":null}}))
                .is_err()
        );
        assert_eq!(
            successful_dispatch_result(
                &json!({"result":{"outcome_code":"success","result":{"schema":"ok"}}})
            )
            .unwrap()["schema"],
            "ok"
        );
    }
}

//! Origin-side driver that exports one exact local qualification as a signed
//! testimony and submits it to a hosted node.
//!
//! The daemon holds no operator key. It derives the exact unsigned testimony
//! and returns it; the operator signs it where the key lives and submits the
//! signed testimony back, which the daemon verifies against the operator
//! identity the node pins before it forwards the testimony. The hosted node
//! decides under its own acceptance policy whether to accept it. The testimony
//! is also returned so it can be carried to the hosted node by other means.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_external_content::qualification_testimony::{
    ContentExportSelection, TestimonyPreparation, finish_testimony_export,
    prepare_content_testimony_blocking, prepare_testimony_blocking, testimony_issue_time,
};
use ryeos_app::state::AppState;
use ryeos_node_runtime::operator_identity::OperatorPublicIdentity;
use ryeos_state::objects::Attestation;
use serde::Serialize;
use serde_json::Value;

use crate::remote::client::{QualificationTestimonyAcceptRequest, RemoteClient};

#[derive(Debug, Clone)]
pub struct QualificationTestimonyExportRequest {
    pub remote: String,
    pub qualification_hash: String,
    pub acceptance_policy_ref: String,
    /// `Some` selects the content lane and names the activated content; `None`
    /// is the product lane.
    pub content: Option<ContentExportSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualificationTestimonyExportOutcome {
    pub remote: String,
    pub qualification_hash: String,
    pub acceptance_policy_ref: String,
    pub testimony_hash: String,
    pub acceptance_hash: String,
    pub coordinate_id: String,
    pub reused_existing: bool,
    /// The signed testimony exactly as submitted, for offline carry.
    pub testimony: Value,
}

/// Derive the unsigned testimony the configured operator is asked to sign.
/// `now` is the time the statement is issued at; a signed submission carries
/// it back so the daemon re-derives the identical statement.
async fn prepare(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    request: &QualificationTestimonyExportRequest,
    now: String,
) -> Result<TestimonyPreparation> {
    let state = Arc::clone(state);
    let request = request.clone();
    let ctx = ctx.clone();
    tokio::task::spawn_blocking(move || {
        let operator = OperatorPublicIdentity::load(&state.config.app_root)
            .context("load configured operator identity for testimony export")?;
        match &request.content {
            None => prepare_testimony_blocking(
                &state,
                &ctx,
                &operator,
                &request.qualification_hash,
                &request.acceptance_policy_ref,
                &now,
            ),
            Some(selection) => prepare_content_testimony_blocking(
                &state,
                &ctx,
                &operator,
                selection,
                &request.qualification_hash,
                &request.acceptance_policy_ref,
                &now,
            ),
        }
    })
    .await
    .context("qualification testimony preparation task stopped")?
}

/// The unsigned testimony for the operator to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualificationTestimonyPreparedOutcome {
    pub remote: String,
    pub qualification_hash: String,
    pub acceptance_policy_ref: String,
    pub coordinate_id: String,
    /// The exact statement to sign: the operator signs the canonical JSON of
    /// this attestation without its `signature` field.
    pub unsigned_testimony: Value,
}

/// First step: derive and return the unsigned testimony.
pub async fn prepare_qualification_testimony(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    request: &QualificationTestimonyExportRequest,
) -> Result<QualificationTestimonyPreparedOutcome> {
    let remotes = crate::remote::config::load_remotes(&state.config.app_root)?;
    crate::remote::config::get_remote(&remotes, &request.remote)?;
    let prepared = prepare(state, ctx, request, lillux::time::iso8601_now()).await?;
    Ok(QualificationTestimonyPreparedOutcome {
        remote: request.remote.clone(),
        qualification_hash: request.qualification_hash.clone(),
        acceptance_policy_ref: request.acceptance_policy_ref.clone(),
        coordinate_id: prepared.coordinate_id,
        unsigned_testimony: prepared.unsigned.to_value(),
    })
}

/// Second step: verify the operator's signature over the statement this node
/// derives and submit the signed testimony to the remote.
pub async fn export_qualification_testimony(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    request: &QualificationTestimonyExportRequest,
    signed: &Attestation,
) -> Result<QualificationTestimonyExportOutcome> {
    let remotes = crate::remote::config::load_remotes(&state.config.app_root)?;
    let remote = crate::remote::config::get_remote(&remotes, &request.remote)?;
    let client = RemoteClient::from_remote_cfg_as_delegated(state, &remote, ctx)?;

    let issued_at = testimony_issue_time(signed, lillux::time::unix_secs_now().try_into()?)?;
    let prepared = prepare(state, ctx, request, issued_at).await?;
    let export = {
        let state = Arc::clone(state);
        let signed = signed.clone();
        tokio::task::spawn_blocking(move || {
            let operator = OperatorPublicIdentity::load(&state.config.app_root)
                .context("load configured operator identity for testimony export")?;
            finish_testimony_export(&state, &operator, prepared, &signed)
        })
        .await
        .context("qualification testimony verification task stopped")??
    };
    let testimony = export.testimony.to_value();
    tracing::info!(
        remote = %request.remote,
        testimony = %export.testimony_hash,
        coordinate = %export.coordinate_id,
        "qualification testimony: exported"
    );

    let accepted = client
        .qualification_testimony_accept(
            &QualificationTestimonyAcceptRequest {
                testimony: testimony.clone(),
            },
            &export.testimony_hash,
            &export.coordinate_id,
        )
        .await
        .with_context(|| {
            format!(
                "qualification testimony submission to {} failed; the signed testimony can be carried to that node offline",
                request.remote
            )
        })?;
    Ok(QualificationTestimonyExportOutcome {
        remote: request.remote.clone(),
        qualification_hash: request.qualification_hash.clone(),
        acceptance_policy_ref: request.acceptance_policy_ref.clone(),
        testimony_hash: accepted.testimony_hash,
        acceptance_hash: accepted.acceptance_hash,
        coordinate_id: accepted.coordinate_id,
        reused_existing: accepted.reused_existing,
        testimony,
    })
}

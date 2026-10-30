//! Origin-side driver that exports one exact local qualification as a signed
//! testimony and submits it to a hosted node.
//!
//! The configured operator key signs the testimony; the hosted node decides
//! under its own acceptance policy whether to accept it. The testimony is also
//! returned so it can be carried to the hosted node by other means.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_external_content::qualification_testimony::{
    ContentExportSelection, export_content_testimony_blocking, export_testimony_blocking,
};
use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::NodeIdentity;
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

pub async fn export_qualification_testimony(
    state: &Arc<AppState>,
    ctx: &HandlerContext,
    request: &QualificationTestimonyExportRequest,
) -> Result<QualificationTestimonyExportOutcome> {
    let remotes = crate::remote::config::load_remotes_layered(&state.config.app_root, None)?;
    let remote = crate::remote::config::get_remote(&remotes, &request.remote)?;
    let client = RemoteClient::from_remote_cfg_as_configured_operator(state, &remote, ctx)?;

    let export = {
        let state = Arc::clone(state);
        let request = request.clone();
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            let operator = NodeIdentity::load(&state.config.operator_signing_key_path)
                .context("load configured operator identity for testimony export")?;
            match &request.content {
                None => export_testimony_blocking(
                    &state,
                    &ctx,
                    &operator,
                    &request.qualification_hash,
                    &request.acceptance_policy_ref,
                    &lillux::time::iso8601_now(),
                ),
                Some(selection) => export_content_testimony_blocking(
                    &state,
                    &ctx,
                    &operator,
                    selection,
                    &request.qualification_hash,
                    &request.acceptance_policy_ref,
                    &lillux::time::iso8601_now(),
                ),
            }
        })
        .await
        .context("qualification testimony export task stopped")??
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

//! The terminal effect executor: each `RyeOsEffect` from the core maps
//! to one daemon call; results come home over the loop's effect channel
//! and fold back into the core as `EffectResult` events. No ryeos state
//! lives here — this is the boundary where engine intent becomes
//! transport calls, and the boundary where the render loop stops
//! waiting: a round trip in flight never blocks a frame.

use std::sync::Arc;

use ryeos_client_base::ui::{
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind,
};

use crate::transport::daemon::{ClientError, DaemonClient};

/// Launch one generation of effects as a concurrent batch off the loop —
/// a startup burst of independent fetches costs one round trip, not one
/// per view. The joined results arrive as a single message in emission
/// order; the loop folds them and spawns any follow-up generation the
/// folds emit. Freshness is the core's job (per-key epochs), so batches
/// from different generations may resolve in any order.
pub fn spawn_effects(
    client: &Arc<DaemonClient>,
    effects: Vec<RyeOsEffect>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<RyeOsEffectResult>>,
) {
    if effects.is_empty() {
        return;
    }
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let results = futures_util::future::join_all(
            effects.iter().map(|effect| run_effect(&client, effect)),
        )
        .await;
        let _ = tx.send(results);
    });
}

async fn run_effect(client: &DaemonClient, effect: &RyeOsEffect) -> RyeOsEffectResult {
    let kind = result_kind_for(&effect.kind);
    match effect_data(client, &effect.kind).await {
        Ok(data) => RyeOsEffectResult {
            id: effect.id,
            ok: true,
            kind,
            data: Some(data),
            error: None,
        },
        Err(error) => RyeOsEffectResult {
            id: effect.id,
            ok: false,
            kind,
            data: None,
            error: Some(effect_error(&effect.kind, error)),
        },
    }
}

fn effect_error(kind: &RyeOsEffectKind, error: ClientError) -> ryeos_client_base::ui::RyeOsUiError {
    let mutation = matches!(
        kind,
        RyeOsEffectKind::InvokeBinding { .. } | RyeOsEffectKind::ReplaceSession { .. }
    );
    let contact_unknown = matches!(
        &error,
        ClientError::Transport(_)
            | ClientError::DaemonDown { .. }
            | ClientError::Io(_)
            | ClientError::Json(_)
    );
    if mutation && contact_unknown {
        let code = if matches!(kind, RyeOsEffectKind::ReplaceSession { .. }) {
            "session_replacement_outcome_unknown"
        } else {
            "invocation_outcome_unknown"
        };
        ryeos_client_base::ui::RyeOsUiError::outcome_unknown(
            code, error.to_string(),
        )
    } else {
        ryeos_client_base::ui::RyeOsUiError::definite("platform_effect_failed", error.to_string())
    }
}

async fn effect_data(
    client: &DaemonClient,
    kind: &RyeOsEffectKind,
) -> Result<serde_json::Value, ClientError> {
    match kind {
        RyeOsEffectKind::FetchSource {
            request,
            request_bounds,
            ..
        } => {
            // Polling sources must not enter ordinary execution admission.
            // That path pins a project snapshot before it can inspect a
            // service's `record_thread: false` contract, so a live threads
            // view can otherwise starve real work with snapshot/store churn.
            // The UI invocation lane enforces the resolved service's
            // read-only policy without admitting a service thread.
            request
                .validate_bounds(*request_bounds)
                .map_err(|error| ClientError::UiBindingRequest(error.to_string()))?;
            let body = serde_json::to_value(request)?;
            let envelope = client
                .signed_post("/ui/api/invocations/dispatch", &body)
                .await?;
            Ok(envelope
                .pointer("/result/result")
                .or_else(|| envelope.get("result"))
                .cloned()
                .unwrap_or(envelope))
        }
        RyeOsEffectKind::InvokeBinding {
            request,
            request_bounds,
            ..
        } => {
            request
                .validate_bounds(*request_bounds)
                .map_err(|error| ClientError::UiBindingRequest(error.to_string()))?;
            let body = serde_json::to_value(request)?;
            let envelope = client
                .signed_post("/ui/api/invocations/dispatch", &body)
                .await?;
            Ok(envelope
                .pointer("/result/result")
                .or_else(|| envelope.get("result"))
                .cloned()
                .unwrap_or(envelope))
        }
        RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. } => Ok(serde_json::Value::Null),
        RyeOsEffectKind::ReplaceSession {
            session_id,
            launch_url,
        } => Ok(serde_json::to_value(
            client.redeem_ui_session(session_id, launch_url).await?,
        )?),
    }
}

fn result_kind_for(kind: &RyeOsEffectKind) -> RyeOsEffectResultKind {
    match kind {
        RyeOsEffectKind::FetchSource { .. } => RyeOsEffectResultKind::SourceData,
        RyeOsEffectKind::InvokeBinding { .. } => RyeOsEffectResultKind::BindingInvoked,
        RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. }
        | RyeOsEffectKind::ReplaceSession { .. } => RyeOsEffectResultKind::BrowserOnly,
    }
}

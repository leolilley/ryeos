//! `ui.ryeos.node.usage` — settled token and spend usage for the executions
//! visible to the exact caller. Discovery is a projection; each chain is
//! reached only through a thread whose signed subject the caller may inspect.
//! Totals describe those observed chains, never node-wide spend.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::queries::ThreadUsageTotals;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRequest {}

/// One observed chain with usage, ready to project.
struct ChainUsage {
    chain_root_id: String,
    item_ref: String,
    status: String,
    updated_at: String,
    totals: ThreadUsageTotals,
}

fn summary_text(totals: &ThreadUsageTotals) -> String {
    format!(
        "{} in \u{b7} {} out \u{b7} ${:.4} \u{b7} {} turns",
        totals.input_tokens, totals.output_tokens, totals.spend_usd, totals.completed_turns
    )
}

/// The response body for the observed chains: label/value total rows, one row
/// per chain (largest spend first), and the coverage statement.
fn usage_response(mut chains: Vec<ChainUsage>, observed_threads: usize, partial: bool) -> Value {
    chains.sort_by(|a, b| {
        b.totals
            .spend_usd
            .total_cmp(&a.totals.spend_usd)
            .then_with(|| a.chain_root_id.cmp(&b.chain_root_id))
    });
    let mut sum = ThreadUsageTotals::default();
    for chain in &chains {
        sum.thread_count += chain.totals.thread_count;
        sum.completed_turns += chain.totals.completed_turns;
        sum.input_tokens += chain.totals.input_tokens;
        sum.output_tokens += chain.totals.output_tokens;
        sum.spend_usd += chain.totals.spend_usd;
    }
    let row = |label: &str, value: String| serde_json::json!({"label": label, "value": value});
    let totals = vec![
        row("input tokens", sum.input_tokens.to_string()),
        row("output tokens", sum.output_tokens.to_string()),
        row("spend (USD)", format!("{:.4}", sum.spend_usd)),
        row("completed turns", sum.completed_turns.to_string()),
        row("chains with usage", chains.len().to_string()),
    ];
    let chains: Vec<Value> = chains
        .iter()
        .map(|chain| {
            serde_json::json!({
                "chain_root_id": chain.chain_root_id,
                "item_ref": chain.item_ref,
                "status": chain.status,
                "updated_at": chain.updated_at,
                "summary": summary_text(&chain.totals),
                "input_tokens": chain.totals.input_tokens,
                "output_tokens": chain.totals.output_tokens,
                "spend_usd": chain.totals.spend_usd,
                "completed_turns": chain.totals.completed_turns,
                "thread_count": chain.totals.thread_count,
            })
        })
        .collect();
    serde_json::json!({
        "schema_version": "ryeos-ui.node.usage.v1",
        "totals": totals,
        "chains": chains,
        "coverage": {
            "state": if partial { "partial" } else { "observed" },
            "reason": "settled_usage_projection_for_authorized_observed_chains_not_node_totals",
            "observed_threads": observed_threads,
        },
    })
}

pub async fn handle_usage(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let _req: UsageRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let (subjects, observed, scanned) =
        super::ui_node::observe_authorized_subjects(&ctx, &state, &caller)?;
    let partial = observed == super::ui_node::MAX_ACTIVITY_THREADS
        || scanned == super::ui_threads::MAX_THREAD_LIST_CANDIDATES_SCANNED;

    // One row per chain, labelled by the chain root thread when observed and
    // otherwise by the most recently updated observed thread of that chain.
    let mut by_chain: BTreeMap<&str, &ryeos_app::state_store::AuthoritativeThreadSubject> =
        BTreeMap::new();
    for signed in subjects.values() {
        let slot = by_chain
            .entry(signed.chain_root_id.as_str())
            .or_insert(signed);
        let is_root = signed.thread_id == signed.chain_root_id;
        if is_root || (slot.thread_id != slot.chain_root_id && signed.updated_at > slot.updated_at)
        {
            *slot = signed;
        }
    }
    let mut chains = Vec::new();
    for (chain_root_id, signed) in by_chain {
        let totals = state.state_store.chain_usage_totals(chain_root_id)?;
        if totals.thread_count == 0 {
            continue;
        }
        chains.push(ChainUsage {
            chain_root_id: chain_root_id.to_string(),
            item_ref: signed.item_ref.clone(),
            status: signed.status.as_str().to_string(),
            updated_at: signed.updated_at.clone(),
            totals,
        });
    }
    Ok(usage_response(chains, subjects.len(), partial))
}

pub const USAGE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/node/usage",
    endpoint: "ui.ryeos.node.usage",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_usage(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(id: &str, spend: f64, input: i64) -> ChainUsage {
        ChainUsage {
            chain_root_id: id.into(),
            item_ref: format!("directive:ops/{id}"),
            status: "completed".into(),
            updated_at: "2026-11-17T00:00:00Z".into(),
            totals: ThreadUsageTotals {
                thread_count: 1,
                completed_turns: 2,
                input_tokens: input,
                output_tokens: 10,
                spend_usd: spend,
            },
        }
    }

    #[test]
    fn totals_sum_chains_and_rows_lead_with_the_largest_spend() {
        let response = usage_response(
            vec![chain("T-a", 0.25, 100), chain("T-b", 0.75, 300)],
            3,
            false,
        );
        let totals: Vec<(&str, &str)> = response["totals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(
            totals,
            vec![
                ("input tokens", "400"),
                ("output tokens", "20"),
                ("spend (USD)", "1.0000"),
                ("completed turns", "4"),
                ("chains with usage", "2"),
            ]
        );
        assert_eq!(response["chains"][0]["chain_root_id"], "T-b");
        assert_eq!(
            response["chains"][0]["summary"],
            "300 in \u{b7} 10 out \u{b7} $0.7500 \u{b7} 2 turns"
        );
        assert_eq!(response["coverage"]["state"], "observed");
        assert_eq!(response["coverage"]["observed_threads"], 3);
    }

    #[test]
    fn partial_observation_is_reported_not_hidden() {
        let response = usage_response(Vec::new(), 128, true);
        assert_eq!(response["coverage"]["state"], "partial");
        assert!(response["chains"].as_array().unwrap().is_empty());
        assert_eq!(response["totals"][2]["value"], "0.0000");
    }
}

//! Doctor rows `external_capacity` and `credential_profiles`: what the running
//! daemon says still holds external capacity or a credential-profile lock.
//!
//! The runtime store belongs to the running daemon, so these rows ask it
//! through `service:external-execution/status` with a short deadline. A
//! stopped node, or a daemon that does not answer, reports the rows
//! `not_checked`. A row FAILs when something waits on the operator, WARNs while
//! the daemon's recovery is still settling something, and is OK when only
//! live work holds capacity; `--json` lists every row either way.

use std::path::Path;
use std::time::Duration;

use ryeos_app::external_execution_status::{Attention, ExternalExecutionStatus};
use ryeos_core_tools::actions::doctor::{CheckResult, FAIL, NA, OK, WARN};
use serde_json::{Value, json};

pub(crate) const CAPACITY_ROW: &str = "external_capacity";
pub(crate) const PROFILES_ROW: &str = "credential_profiles";
const STATUS_REF: &str = "service:external-execution/status";
const DEADLINE: Duration = Duration::from_secs(10);

fn row(name: &str, status: &str, detail: Value) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        status: status.to_owned(),
        detail,
    }
}

/// Both rows for a node whose daemon was not asked.
pub(crate) fn not_checked(note: &str, fix: Option<&str>) -> Vec<CheckResult> {
    [CAPACITY_ROW, PROFILES_ROW]
        .into_iter()
        .map(|name| {
            let mut detail = json!({ "state": "not_checked", "note": note });
            if let Some(fix) = fix {
                detail["fix"] = json!(fix);
            }
            row(name, NA, detail)
        })
        .collect()
}

/// Both rows for a running node, asked live.
pub(crate) async fn checks(app_root: &Path) -> Vec<CheckResult> {
    match tokio::time::timeout(DEADLINE, fetch(app_root)).await {
        Ok(Ok(status)) => rows(&status),
        Ok(Err(error)) => unanswered(&format!("{error:#}")),
        Err(_) => unanswered(&format!(
            "the daemon did not answer within {} s",
            DEADLINE.as_secs()
        )),
    }
}

fn unanswered(error: &str) -> Vec<CheckResult> {
    [CAPACITY_ROW, PROFILES_ROW]
        .into_iter()
        .map(|name| {
            row(
                name,
                WARN,
                json!({
                    "state": "not_checked",
                    "error": error,
                    "note": "the running daemon did not answer the external-execution status call",
                    "fix": "run `ryeos external-execution status` to see the refusal",
                }),
            )
        })
        .collect()
}

async fn fetch(app_root: &Path) -> anyhow::Result<ExternalExecutionStatus> {
    let daemon_url = crate::transport::http::resolve_daemon_url(app_root).await?;
    let discovered = crate::transport::discovery::discover_audience(&daemon_url).await?;
    let signer = crate::transport::signing::Signer::resolve(app_root)?;
    let body = serde_json::to_vec(&json!({
        "item_ref": STATUS_REF,
        "ref_bindings": {},
        "product_selections": [],
        "parameters": {},
    }))?;
    let headers = signer.sign("POST", "/execute", &body, &discovered.principal_id)?;
    let response = crate::transport::http::post_json(
        &format!("{}/execute", discovered.base_url),
        &headers,
        &body,
    )
    .await?;
    let result = response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("status response carries no result: {response}"))?;
    Ok(serde_json::from_value(result)?)
}

fn attention_status(attention: Option<Attention>) -> &'static str {
    match attention {
        None | Some(Attention::InUse) => OK,
        Some(Attention::Settling) => WARN,
        Some(Attention::Operator) => FAIL,
    }
}

fn count(attentions: impl Iterator<Item = Attention>) -> Value {
    let (mut in_use, mut settling, mut operator) = (0u64, 0u64, 0u64);
    for attention in attentions {
        match attention {
            Attention::InUse => in_use += 1,
            Attention::Settling => settling += 1,
            Attention::Operator => operator += 1,
        }
    }
    json!({ "in_use": in_use, "settling": settling, "operator": operator })
}

/// The two rows from one status answer.
pub(crate) fn rows(status: &ExternalExecutionStatus) -> Vec<CheckResult> {
    let capacity = row(
        CAPACITY_ROW,
        attention_status(status.capacity_attention()),
        json!({
            "counts": count(
                status
                    .allocations
                    .iter()
                    .map(|row| row.attention)
                    .chain(status.qualification_local_starts.iter().map(|row| row.attention)),
            ),
            "allocations": status.allocations,
            "qualification_local_starts": status.qualification_local_starts,
        }),
    );
    let profiles = row(
        PROFILES_ROW,
        attention_status(status.credential_attention()),
        json!({
            "counts": count(status.credential_profiles.iter().map(|row| row.attention)),
            "held": status.credential_profiles,
        }),
    );
    vec![capacity, profiles]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(value: Value) -> ExternalExecutionStatus {
        serde_json::from_value(value).unwrap()
    }

    fn empty() -> Value {
        json!({
            "schema": ryeos_app::external_execution_status::STATUS_SCHEMA,
            "observed_at_ms": 1,
            "allocations": [],
            "qualification_local_starts": [],
            "credential_profiles": [],
        })
    }

    fn held_profile(attention: &str) -> Value {
        json!({
            "profile_id": "personal",
            "state": "active",
            "credential_generation": 1,
            "login_expires_at_ms": null,
            "held_since_ms": 1,
            "owner": {
                "kind": "dedicated_session",
                "chain_root_id": "T-chain",
                "placement_thread_id": "T-chain",
                "session_state": "outcome_unknown",
            },
            "attention": attention,
            "fix": "`ryeos worker session terminate T-chain <reason>` releases it",
        })
    }

    fn quarantined_allocation() -> Value {
        json!({
            "placement_thread_id": "T-placement",
            "phase": "quarantined",
            "owner_kind": "direct_thread",
            "capacity_owner": "f".repeat(64),
            "occurrence_bound": false,
            "age_ms": 5,
            "updated_at_ms": 1,
            "attention": "operator",
            "fix": "settle it",
        })
    }

    #[test]
    fn doctor_rows_na_when_stopped() {
        let rows = not_checked("the node is not running", Some("ryeos start"));
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            [CAPACITY_ROW, PROFILES_ROW]
        );
        for row in rows {
            assert_eq!(row.status, NA);
            assert_eq!(row.detail["state"], "not_checked");
            assert_eq!(row.detail["fix"], "ryeos start");
        }
    }

    #[test]
    fn doctor_rows_ok_when_nothing_is_held() {
        for row in rows(&status(empty())) {
            assert_eq!(row.status, OK, "{}", row.detail);
        }
    }

    #[test]
    fn doctor_rows_fail_on_held_capacity_that_waits_on_the_operator() {
        let mut value = empty();
        value["allocations"] = json!([quarantined_allocation()]);
        value["credential_profiles"] = json!([held_profile("operator")]);
        let rows = rows(&status(value));
        assert_eq!(rows[0].status, FAIL);
        assert_eq!(rows[0].detail["counts"]["operator"], 1);
        assert_eq!(rows[0].detail["allocations"][0]["fix"], "settle it");
        assert_eq!(rows[1].status, FAIL);
        assert_eq!(
            rows[1].detail["held"][0]["owner"]["chain_root_id"],
            "T-chain"
        );
    }

    #[test]
    fn doctor_rows_warn_while_settling_and_pass_live_work() {
        let mut value = empty();
        value["credential_profiles"] = json!([held_profile("settling")]);
        assert_eq!(rows(&status(value.clone()))[1].status, WARN);
        value["credential_profiles"] = json!([held_profile("in_use")]);
        let rows = rows(&status(value));
        assert_eq!(rows[1].status, OK);
        assert_eq!(rows[1].detail["counts"]["in_use"], 1);
    }

    #[test]
    fn unanswered_rows_warn_not_checked() {
        for row in unanswered("connection refused") {
            assert_eq!(row.status, WARN);
            assert_eq!(row.detail["state"], "not_checked");
            assert_eq!(row.detail["error"], "connection refused");
        }
    }
}

//! Doctor row `public_routes`: the public-page routes the running node left
//! out at its start.
//!
//! A route signed `public: true` is served only when the node's
//! `public_routes` policy names its id. One it does not name is left out of
//! the route table at start, its path answered as any unknown path is, and the
//! rest of the node starts. The daemon decides this when it builds the table
//! and reports it through `service:system/routes`; this row asks it, with a
//! short deadline. A stopped node, or one that does not answer, reports the
//! row `not_checked`. The row WARNs while any public route is left out.

use std::path::Path;
use std::time::Duration;

use ryeos_core_tools::actions::doctor::{CheckResult, NA, OK, WARN};
use serde_json::{Value, json};

pub(crate) const ROW: &str = "public_routes";
const ROUTES_REF: &str = "service:system/routes";
const DEADLINE: Duration = Duration::from_secs(10);

fn row(status: &str, detail: Value) -> CheckResult {
    CheckResult {
        name: ROW.to_owned(),
        status: status.to_owned(),
        detail,
    }
}

/// The row for a node whose daemon was not asked.
pub(crate) fn not_checked(note: &str, fix: Option<&str>) -> CheckResult {
    let mut detail = json!({ "state": "not_checked", "note": note });
    if let Some(fix) = fix {
        detail["fix"] = json!(fix);
    }
    row(NA, detail)
}

/// The row for a running node, asked live.
pub(crate) async fn check(app_root: &Path) -> CheckResult {
    match tokio::time::timeout(
        DEADLINE,
        crate::dispatcher::execute_service(app_root, ROUTES_REF, json!({})),
    )
    .await
    {
        Ok(Ok(answer)) => from_answer(&answer),
        Ok(Err(error)) => unanswered(&format!("{error:#}")),
        Err(_) => unanswered(&format!(
            "the daemon did not answer within {} s",
            DEADLINE.as_secs()
        )),
    }
}

fn unanswered(error: &str) -> CheckResult {
    row(
        WARN,
        json!({
            "state": "not_checked",
            "error": error,
            "note": "the running daemon did not answer the routes call",
        }),
    )
}

fn from_answer(answer: &Value) -> CheckResult {
    let skipped: Vec<Value> = answer["skipped_public_routes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if skipped.is_empty() {
        return row(OK, json!({ "skipped": [] }));
    }
    row(
        WARN,
        json!({
            "code": "public_route_not_named",
            "skipped": skipped,
            "note": "these routes are signed public, but the node's public_routes policy does not name them, so the node does not serve them",
            "fix": "name each route id in the public_routes policy section with its limits, then restart; or remove the bundle that carries them",
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_skipped_public_route_is_ok() {
        let answer = json!({ "routes": [], "skipped_public_routes": [] });
        assert_eq!(from_answer(&answer).status, OK);
    }

    #[test]
    fn a_skipped_public_route_warns_and_names_it() {
        let answer = json!({
            "skipped_public_routes": [
                { "id": "public/front", "path": "/", "code": "public_route_not_named" }
            ]
        });
        let check = from_answer(&answer);
        assert_eq!(check.status, WARN);
        assert_eq!(check.detail["code"], "public_route_not_named");
        assert_eq!(check.detail["skipped"][0]["id"], "public/front");
    }
}

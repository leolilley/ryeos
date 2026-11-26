//! The client's reader for the shared read envelope.
//!
//! A service that answers with the envelope tags its answer with a `schema`
//! of the form `ryeos.read.<family>.<name>/v<N>`, a `revision` token and a
//! `coverage` of `{state, reason}`, then carries its listing under `rows` (or
//! a single record under `record`), an optional `page`, and a side table of
//! signers. A caller that sends `if_revision` equal to the answer's revision
//! gets `{ "not_modified": true, "revision": ... }` and no rows.
//!
//! This module only reads those fields. A source whose answer is not an
//! envelope is not touched by anything here and keeps its own shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema prefix every envelope answer carries.
const SCHEMA_PREFIX: &str = "ryeos.read.";

/// The closed coverage states an envelope may report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeCoverageState {
    Complete,
    Observed,
    Partial,
    Failed,
    Unavailable,
}

impl EnvelopeCoverageState {
    /// The state code the signed coverage wording is keyed by.
    pub fn code(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Observed => "observed",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        Some(match code {
            "complete" => Self::Complete,
            "observed" => Self::Observed,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            "unavailable" => Self::Unavailable,
            _ => return None,
        })
    }
}

/// What `page.total` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeTotalBasis {
    Exact,
    AtLeast,
    Unknown,
}

/// The page facts an envelope reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopePage {
    pub next_cursor: Option<String>,
    pub total: Option<u64>,
    pub total_basis: EnvelopeTotalBasis,
}

/// One envelope answer, borrowed from the stored response.
#[derive(Debug, Clone)]
pub struct ReadEnvelope<'a> {
    pub revision: &'a str,
    pub state: EnvelopeCoverageState,
    /// The reason code, only when it is a plain code.
    pub reason: Option<&'a str>,
    rows: Option<&'a [Value]>,
    record: Option<&'a Value>,
    pub page: Option<EnvelopePage>,
}

/// The envelope a response is, when it is one. A response that names an
/// envelope schema but has no readable coverage state reads as `failed`: its
/// rows are never shown as authoritative.
pub fn read_envelope(response: &Value) -> Option<ReadEnvelope<'_>> {
    let schema = response.get("schema")?.as_str()?;
    if !schema.starts_with(SCHEMA_PREFIX) {
        return None;
    }
    let coverage = response.get("coverage");
    let parsed_state = coverage
        .and_then(|coverage| coverage.get("state"))
        .and_then(Value::as_str)
        .and_then(EnvelopeCoverageState::parse);
    let state = parsed_state.unwrap_or(EnvelopeCoverageState::Failed);
    let reason = coverage
        .and_then(|coverage| coverage.get("reason"))
        .and_then(Value::as_str)
        .filter(|reason| is_code(reason));
    Some(ReadEnvelope {
        revision: response
            .get("revision")
            .and_then(Value::as_str)
            .unwrap_or(""),
        state,
        reason,
        rows: response
            .get("rows")
            .and_then(Value::as_array)
            .map(Vec::as_slice),
        record: response.get("record").filter(|record| record.is_object()),
        page: response
            .get("page")
            .and_then(|page| serde_json::from_value(page.clone()).ok()),
    })
}

fn is_code(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= 80
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl<'a> ReadEnvelope<'a> {
    /// The records the answer carries: its rows, or its one record. A failed
    /// answer carries no authoritative rows, so it yields none.
    pub fn records(&self) -> &'a [Value] {
        if self.state == EnvelopeCoverageState::Failed {
            return &[];
        }
        match (self.rows, self.record) {
            (Some(rows), _) => rows,
            (None, Some(record)) => std::slice::from_ref(record),
            (None, None) => &[],
        }
    }
}

/// The answer a source reads as when its response is not the contract the
/// view was written for: a failed envelope with the reason `contract_mismatch`
/// and no rows. `None` when the response is that contract.
pub fn contract_mismatch_answer(contract: &str, response: &Value) -> Option<Value> {
    if response.get("schema").and_then(Value::as_str) == Some(contract) {
        return None;
    }
    Some(serde_json::json!({
        "schema": CONTRACT_MISMATCH_SCHEMA,
        "revision": "",
        "coverage": { "state": "failed", "reason": CONTRACT_MISMATCH_REASON },
        "rows": [],
    }))
}

/// The schema of the answer a contract mismatch is read as.
const CONTRACT_MISMATCH_SCHEMA: &str = "ryeos.read.contract_mismatch/v1";

/// The coverage reason, and the key of its signed sentence.
pub const CONTRACT_MISMATCH_REASON: &str = "contract_mismatch";

/// Whether a response is a `not_modified` reply: the server's statement that
/// the revision the client sent is still current.
pub fn is_not_modified(response: &Value) -> bool {
    response.get("not_modified").and_then(Value::as_bool) == Some(true)
        && response.get("schema").is_none()
}

/// The revision of a stored envelope response, to send back as `if_revision`.
pub fn stored_revision(response: &Value) -> Option<&str> {
    let envelope = read_envelope(response)?;
    (!envelope.revision.is_empty()).then_some(envelope.revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(state: &str) -> Value {
        json!({
            "schema": "ryeos.read.test.rows/v1", "as_of": "t", "revision": "rev1:a",
            "coverage": {"state": state, "reason": null},
            "rows": [{"id": 1}, {"id": 2}],
            "page": {"next_cursor": "c", "total": 9, "total_basis": "exact"},
            "signers": {}
        })
    }

    #[test]
    fn an_envelope_yields_its_rows_page_and_revision() {
        let response = envelope("observed");
        let read = read_envelope(&response).unwrap();
        assert_eq!(read.records().len(), 2);
        assert_eq!(read.revision, "rev1:a");
        assert_eq!(read.state, EnvelopeCoverageState::Observed);
        let page = read.page.unwrap();
        assert_eq!(page.total, Some(9));
        assert_eq!(page.total_basis, EnvelopeTotalBasis::Exact);
        assert_eq!(page.next_cursor.as_deref(), Some("c"));
    }

    #[test]
    fn a_record_answer_is_one_row() {
        let response = json!({
            "schema": "ryeos.read.test.one/v1", "revision": "r",
            "coverage": {"state": "complete"}, "record": {"id": 7}
        });
        let read = read_envelope(&response).unwrap();
        assert_eq!(read.records(), &[json!({"id": 7})]);
        assert!(read.page.is_none());
    }

    #[test]
    fn a_failed_or_unreadable_coverage_draws_no_rows() {
        assert!(
            read_envelope(&envelope("failed"))
                .unwrap()
                .records()
                .is_empty()
        );
        let mut broken = envelope("complete");
        broken["coverage"]["state"] = json!("great");
        let read = read_envelope(&broken).unwrap();
        assert_eq!(read.state, EnvelopeCoverageState::Failed);
        assert!(read.records().is_empty());
    }

    #[test]
    fn other_shapes_are_not_envelopes() {
        assert!(read_envelope(&json!({"threads": []})).is_none());
        assert!(read_envelope(&json!({"schema": "something.else/v1"})).is_none());
        assert!(read_envelope(&json!([1])).is_none());
    }

    #[test]
    fn not_modified_is_only_the_bare_reply() {
        assert!(is_not_modified(
            &json!({"not_modified": true, "revision": "r"})
        ));
        assert!(!is_not_modified(&envelope("complete")));
        assert!(!is_not_modified(&json!({"not_modified": false})));
        assert_eq!(stored_revision(&envelope("complete")), Some("rev1:a"));
        assert_eq!(stored_revision(&json!({"x": 1})), None);
    }

    #[test]
    fn a_response_of_another_contract_reads_as_a_failed_answer_with_no_rows() {
        let held = envelope("complete");
        let schema = held["schema"].as_str().unwrap().to_owned();
        assert!(contract_mismatch_answer(&schema, &held).is_none());
        let answer = contract_mismatch_answer("ryeos.read.other.rows/v1", &held)
            .expect("a different schema is a mismatch");
        let read = read_envelope(&answer).expect("the answer is an envelope");
        assert_eq!(read.state, EnvelopeCoverageState::Failed);
        assert_eq!(read.reason, Some(CONTRACT_MISMATCH_REASON));
        assert!(read.records().is_empty());
        // A bare, non-envelope response is a mismatch too.
        assert!(contract_mismatch_answer(&schema, &json!({"rows": [1]})).is_some());
    }
}

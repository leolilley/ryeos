//! `ryeos external-content runtime-snapshot-run`: one client operation over
//! the node's own runtime-snapshot services.
//!
//! bootstrap source → source ready → produce → snapshot ready → terminate
//! source → create qualification occurrence → occurrence running → verify →
//! terminate occurrence (unless `keep_occurrence`).
//!
//! There is no state file. Every step is a node service whose operation id
//! derives from its coordinates, so a rerun with the same input reads the
//! node's own journal back and continues from the first step not yet done.
//! A step whose record is uncertain (an attempt in flight, quarantined, late
//! or rejected) stops the run and names the read verb for that record; the
//! run never repeats a provider contact the node has not settled.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::CliError;

/// Most readiness polls of one step before the run stops and reports.
const MAX_POLLS: u32 = 120;
const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunRequest {
    production_binding: String,
    qualification_binding: String,
    /// The bootstrap source: `{kind: captured_product, witness_hash,
    /// witness_source}` or `{kind: bundle_materialization,
    /// materialization_binding_id, coordinate_digest, attestation_hash}`.
    source: Value,
    maximum_lifetime_seconds: u32,
    #[serde(default)]
    keep_occurrence: bool,
}

impl RunRequest {
    pub(crate) fn parse(parameters: &Value) -> Result<Self, CliError> {
        let request: Self = serde_json::from_value(parameters.clone())
            .map_err(|error| local(format!("runtime-snapshot run input: {error}")))?;
        match request.source.get("kind").and_then(Value::as_str) {
            Some("captured_product" | "bundle_materialization") => Ok(request),
            _ => Err(local(
                "runtime-snapshot run source.kind is `captured_product` or `bundle_materialization`",
            )),
        }
    }

    /// The produce source: the bootstrap source joined to its operation.
    fn produce_source(&self, bootstrap_operation_id: &str) -> Value {
        let mut source = self.source.clone();
        source["bootstrap_operation_id"] = json!(bootstrap_operation_id);
        source
    }
}

/// The steps of one run, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Bootstrap,
    SourceReady,
    Produce,
    SnapshotReady,
    TerminateSource,
    CreateOccurrence,
    OccurrenceRunning,
    Verify,
    TerminateOccurrence,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::SourceReady => "source_ready",
            Self::Produce => "produce",
            Self::SnapshotReady => "snapshot_ready",
            Self::TerminateSource => "terminate_source",
            Self::CreateOccurrence => "create_occurrence",
            Self::OccurrenceRunning => "occurrence_running",
            Self::Verify => "verify",
            Self::TerminateOccurrence => "terminate_occurrence",
        }
    }

    /// The read-only verb that shows this step's record without contact.
    fn read_verb(self) -> &'static str {
        match self {
            Self::Bootstrap | Self::SourceReady => {
                "ryeos external-content bootstrap-runtime-source-operation"
            }
            Self::Produce | Self::SnapshotReady => "ryeos external-content runtime-snapshot",
            Self::TerminateSource => "ryeos external-content bootstrap-runtime-source-termination",
            Self::CreateOccurrence | Self::OccurrenceRunning | Self::Verify => {
                "ryeos external-content runtime-snapshot-qualification"
            }
            Self::TerminateOccurrence => {
                "ryeos external-content runtime-snapshot-qualification-termination"
            }
        }
    }

    /// The phase a step's record must reach for the run to go on.
    fn settled_phase(self) -> Option<&'static str> {
        match self {
            Self::Bootstrap | Self::CreateOccurrence => Some("occurrence_bound"),
            Self::Produce => Some("bound"),
            Self::Verify => Some("observed"),
            Self::TerminateSource | Self::TerminateOccurrence => Some("terminal"),
            Self::SourceReady | Self::SnapshotReady | Self::OccurrenceRunning => None,
        }
    }
}

/// What the run does after one step's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Next {
    Continue,
    /// Not ready yet: ask again.
    Poll,
    /// The node has not settled this step; the run stops without contact.
    Stop {
        phase: String,
    },
}

/// Decide from a record only. Pure, so a rerun over the node's journal makes
/// the same decision the first run did.
pub(crate) fn classify(step: Step, record: &Value) -> Next {
    let phase = record.get("phase").and_then(Value::as_str);
    if let Some(settled) = step.settled_phase() {
        return match phase {
            Some(phase) if phase == settled => Next::Continue,
            // A termination whose acknowledgement is pending is observed again,
            // never posted again: the service replays observation only.
            Some("reserved" | "attempt_pending")
                if matches!(step, Step::TerminateSource | Step::TerminateOccurrence) =>
            {
                Next::Poll
            }
            Some(phase) => Next::Stop {
                phase: phase.to_owned(),
            },
            None => Next::Stop {
                phase: "absent".to_owned(),
            },
        };
    }
    let ready = match step {
        Step::SourceReady | Step::SnapshotReady => record
            .get("readiness")
            .is_some_and(|value| !value.is_null()),
        Step::OccurrenceRunning => record.get("running").and_then(Value::as_bool) == Some(true),
        _ => unreachable!("settled steps return above"),
    };
    if ready {
        Next::Continue
    } else if step == Step::OccurrenceRunning
        && record
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "terminated" | "absent" | "errored"))
    {
        Next::Stop {
            phase: record["status"].as_str().unwrap_or_default().to_owned(),
        }
    } else {
        Next::Poll
    }
}

/// Run every step, calling `call(service, parameters)` for each node service.
pub(crate) async fn run_with<F, Fut>(
    request: &RunRequest,
    mut call: F,
    poll_interval: Duration,
) -> Result<Value, CliError>
where
    F: FnMut(&'static str, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, CliError>>,
{
    let mut report = serde_json::Map::new();
    let mut step_record = async |step: Step,
                                 service: &'static str,
                                 parameters: Value,
                                 report: &mut serde_json::Map<String, Value>|
           -> Result<Option<Value>, CliError> {
        let mut polls = 0;
        loop {
            let record = call(service, parameters.clone()).await?;
            match classify(step, &record) {
                Next::Continue => {
                    report.insert(step.label().to_owned(), record.clone());
                    return Ok(Some(record));
                }
                Next::Poll if polls < MAX_POLLS => {
                    polls += 1;
                    tokio::time::sleep(poll_interval).await;
                }
                Next::Poll => {
                    report.insert(
                        "stopped".to_owned(),
                        json!({
                            "step": step.label(),
                            "reason": "not ready within the run's polling bound",
                            "read": step.read_verb(),
                            "record": record,
                        }),
                    );
                    return Ok(None);
                }
                Next::Stop { phase } => {
                    report.insert(
                        "stopped".to_owned(),
                        json!({
                            "step": step.label(),
                            "phase": phase,
                            "reason": "the node has not settled this step; it is not repeated",
                            "read": step.read_verb(),
                            "record": record,
                        }),
                    );
                    return Ok(None);
                }
            }
        }
    };

    let Some(bootstrap) = step_record(
        Step::Bootstrap,
        "service:external-content/bootstrap-runtime-source",
        json!({
            "binding_id": request.production_binding,
            "source": request.source,
            "maximum_lifetime_seconds": request.maximum_lifetime_seconds,
        }),
        &mut report,
    )
    .await?
    else {
        return Ok(Value::Object(report));
    };
    let bootstrap_id = operation_id(&bootstrap, "bootstrap")?;
    if step_record(
        Step::SourceReady,
        "service:external-content/observe-bootstrap-runtime-source",
        json!({ "operation_id": bootstrap_id }),
        &mut report,
    )
    .await?
    .is_none()
    {
        return Ok(Value::Object(report));
    }
    let Some(snapshot) = step_record(
        Step::Produce,
        "service:external-content/produce-runtime-snapshot",
        json!({
            "binding_id": request.production_binding,
            "source": request.produce_source(&bootstrap_id),
        }),
        &mut report,
    )
    .await?
    else {
        return Ok(Value::Object(report));
    };
    let snapshot_id = operation_id(&snapshot, "snapshot")?;
    if step_record(
        Step::SnapshotReady,
        "service:external-content/observe-runtime-snapshot-readiness",
        json!({ "operation_id": snapshot_id }),
        &mut report,
    )
    .await?
    .is_none()
    {
        return Ok(Value::Object(report));
    }
    // The snapshot is taken; the source sandbox has no further use.
    if step_record(
        Step::TerminateSource,
        "service:external-content/terminate-bootstrap-runtime-source",
        json!({ "operation_id": bootstrap_id }),
        &mut report,
    )
    .await?
    .is_none()
    {
        return Ok(Value::Object(report));
    }
    let Some(qualification) = step_record(
        Step::CreateOccurrence,
        "service:external-content/create-runtime-snapshot-qualification",
        json!({
            "qualification_binding_id": request.qualification_binding,
            "snapshot_operation_id": snapshot_id,
        }),
        &mut report,
    )
    .await?
    else {
        return Ok(Value::Object(report));
    };
    let qualification_id = operation_id(&qualification, "qualification")?;
    if step_record(
        Step::OccurrenceRunning,
        "service:external-content/observe-runtime-snapshot-qualification",
        json!({ "qualification_operation_id": qualification_id }),
        &mut report,
    )
    .await?
    .is_none()
    {
        return Ok(Value::Object(report));
    }
    if step_record(
        Step::Verify,
        "service:external-content/verify-runtime-snapshot-qualification",
        json!({ "qualification_operation_id": qualification_id }),
        &mut report,
    )
    .await?
    .is_none()
    {
        return Ok(Value::Object(report));
    }
    if request.keep_occurrence {
        let lifetime_ms = qualification["intent"]["maximum_lifetime_seconds"]
            .as_i64()
            .unwrap_or_default()
            * 1_000;
        report.insert(
            "kept_occurrence".to_owned(),
            json!({
                "qualification_operation_id": qualification_id,
                "deadline_ms": qualification["created_at_ms"].as_i64().unwrap_or_default() + lifetime_ms,
                "terminate": format!(
                    "ryeos external-content terminate-runtime-snapshot-qualification {qualification_id}"
                ),
            }),
        );
    } else {
        step_record(
            Step::TerminateOccurrence,
            "service:external-content/terminate-runtime-snapshot-qualification",
            json!({ "qualification_operation_id": qualification_id }),
            &mut report,
        )
        .await?;
    }
    Ok(Value::Object(report))
}

fn operation_id(record: &Value, what: &str) -> Result<String, CliError> {
    record["intent"]["operation_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| local(format!("{what} record names no operation id")))
}

/// Refuse before any provider spend when either binding fails doctor's pin
/// coherence check.
fn preflight_bindings(
    app_root: &Path,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    request: &RunRequest,
) -> Result<(), CliError> {
    let trust = ryeos_engine::trust::TrustStore::load(
        None,
        &ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).config(),
    )
    .map_err(|error| local(format!("load node trust for the binding check: {error:#}")))?;
    let report = ryeos_app::node_config::coherence::inspect_installed_binding_coherence(
        app_root, &trust, snapshot,
    )
    .map_err(|error| local(format!("binding coherence check: {error:#}")))?;
    let failing: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| {
            finding.severity == ryeos_app::node_config::coherence::FindingSeverity::Fail
                && ((finding.binding.section == "runtime_snapshot_production"
                    && finding.binding.id == request.production_binding)
                    || (finding.binding.section == "runtime_snapshot_qualification"
                        && finding.binding.id == request.qualification_binding))
        })
        .map(|finding| {
            format!(
                "{}/{} {} {}: {}",
                finding.binding.section,
                finding.binding.id,
                finding.code,
                finding.field,
                finding.fix
            )
        })
        .collect();
    if failing.is_empty() {
        Ok(())
    } else {
        Err(local(format!(
            "runtime-snapshot run refused before any provider contact: {}",
            failing.join("; ")
        )))
    }
}

pub(crate) async fn run(
    app_root: &Path,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    parameters: &Value,
) -> Result<Value, CliError> {
    let request = RunRequest::parse(parameters)?;
    preflight_bindings(app_root, snapshot, &request)?;
    run_with(
        &request,
        |service, parameters| crate::dispatcher::execute_service(app_root, service, parameters),
        POLL_INTERVAL,
    )
    .await
}

fn local(detail: impl Into<String>) -> CliError {
    CliError::Local {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    fn request() -> RunRequest {
        RunRequest::parse(&json!({
            "production_binding": "render-snapshot",
            "qualification_binding": "render-snapshot-qualification",
            "source": {"kind": "captured_product", "witness_hash": "a".repeat(64), "witness_source": {"kind": "local_capture"}},
            "maximum_lifetime_seconds": 600,
        }))
        .unwrap()
    }

    fn record(id: &str, phase: &str) -> Value {
        json!({"intent": {"operation_id": id, "maximum_lifetime_seconds": 900}, "phase": phase, "readiness": {"ok": true}, "created_at_ms": 1})
    }

    /// A node journal: each service answers from what it has recorded.
    struct Journal {
        answers: BTreeMap<&'static str, Value>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Journal {
        fn settled() -> Self {
            let answers = BTreeMap::from([
                (
                    "service:external-content/bootstrap-runtime-source",
                    record("b", "occurrence_bound"),
                ),
                (
                    "service:external-content/observe-bootstrap-runtime-source",
                    record("b", "occurrence_bound"),
                ),
                (
                    "service:external-content/produce-runtime-snapshot",
                    record("s", "bound"),
                ),
                (
                    "service:external-content/observe-runtime-snapshot-readiness",
                    record("s", "bound"),
                ),
                (
                    "service:external-content/terminate-bootstrap-runtime-source",
                    record("bt", "terminal"),
                ),
                (
                    "service:external-content/create-runtime-snapshot-qualification",
                    record("q", "occurrence_bound"),
                ),
                (
                    "service:external-content/observe-runtime-snapshot-qualification",
                    json!({"running": true, "status": "running"}),
                ),
                (
                    "service:external-content/verify-runtime-snapshot-qualification",
                    record("v", "observed"),
                ),
                (
                    "service:external-content/terminate-runtime-snapshot-qualification",
                    record("qt", "terminal"),
                ),
            ]);
            Self {
                answers,
                calls: RefCell::new(Vec::new()),
            }
        }

        async fn call(&self, service: &'static str, _parameters: Value) -> Result<Value, CliError> {
            self.calls.borrow_mut().push(service);
            Ok(self.answers[service].clone())
        }
    }

    #[tokio::test]
    async fn a_settled_journal_runs_every_step_once() {
        let journal = Journal::settled();
        let report = run_with(&request(), |s, p| journal.call(s, p), Duration::ZERO)
            .await
            .unwrap();
        assert!(report.get("stopped").is_none(), "{report}");
        assert_eq!(journal.calls.borrow().len(), 9);
        assert!(report.get("terminate_occurrence").is_some());
    }

    #[tokio::test]
    async fn run_resumes_from_node_journal_without_repeating_uncertain_step() {
        let mut journal = Journal::settled();
        journal.answers.insert(
            "service:external-content/produce-runtime-snapshot",
            record("s", "quarantined"),
        );
        let report = run_with(&request(), |s, p| journal.call(s, p), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(report["stopped"]["step"], "produce");
        assert_eq!(report["stopped"]["phase"], "quarantined");
        assert_eq!(
            report["stopped"]["read"],
            "ryeos external-content runtime-snapshot"
        );
        // The uncertain produce was asked once and nothing after it ran.
        let calls = journal.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .filter(|s| s.ends_with("produce-runtime-snapshot"))
                .count(),
            1
        );
        assert!(!calls.iter().any(|s| s.contains("qualification")));
    }

    #[tokio::test]
    async fn keep_occurrence_reports_the_deadline_and_never_terminates_it() {
        let journal = Journal::settled();
        let mut request = request();
        request.keep_occurrence = true;
        let report = run_with(&request, |s, p| journal.call(s, p), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(report["kept_occurrence"]["deadline_ms"], 900_001);
        assert!(
            !journal
                .calls
                .borrow()
                .iter()
                .any(|s| s.ends_with("terminate-runtime-snapshot-qualification"))
        );
    }

    #[test]
    fn a_stopped_occurrence_is_not_polled_forever() {
        assert_eq!(
            classify(
                Step::OccurrenceRunning,
                &json!({"running": false, "status": "creating"})
            ),
            Next::Poll
        );
        assert_eq!(
            classify(
                Step::OccurrenceRunning,
                &json!({"running": false, "status": "terminated"})
            ),
            Next::Stop {
                phase: "terminated".into()
            }
        );
    }

    #[test]
    fn the_source_kind_is_checked() {
        assert!(
            RunRequest::parse(&json!({
                "production_binding": "p", "qualification_binding": "q",
                "source": {"kind": "worker"}, "maximum_lifetime_seconds": 600,
            }))
            .is_err()
        );
    }
}

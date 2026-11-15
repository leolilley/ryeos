//! `ryeos external-content product publish`: produce a guest product and ship
//! it, as one client composite.
//!
//! It only sequences verified node commands the operator could run one by
//! one: activate the producer's external content (when named), execute the
//! producer, follow its thread to a terminal state, capture the named
//! product, admit the witness under the node's local admission policy, and
//! push it to a remote when asked. It keeps no state file: every step after
//! production is keyed by node coordinates (the chain, thread and product a
//! capture names; the subject and policy an admission names; the staging a
//! push resumes), so a run that stopped after production resumes with
//! `--from-thread <thread-id>`.

use std::path::Path;
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};

use crate::error::CliError;

pub(crate) const TOKENS: [&str; 3] = ["external-content", "product", "publish"];

/// Thread states after which a thread does not change.
const TERMINAL: &[&str] = &[
    "completed",
    "failed",
    "cancelled",
    "killed",
    "timed_out",
    "continued",
];

pub(crate) fn matches(rest: &[String]) -> bool {
    rest.len() >= TOKENS.len() && rest.iter().zip(TOKENS).all(|(token, want)| token == want)
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos external-content product publish",
    about = "Produce a named product and ship it: activate, execute the producer, wait for it, \
             capture the product, admit the witness, and push it to a remote when asked.",
    no_binary_name = true
)]
struct Args {
    /// The producer to execute, for example `graph:codex/guest-runtime-production`.
    producer_ref: String,

    /// The product name the producer declares, for example `runtime`.
    product_name: String,

    /// Activate this external-content activation recipe (online) before
    /// producing, waiting until the activation completes.
    #[arg(long)]
    activation: Option<String>,

    /// Resume after production: capture from this completed producer thread
    /// instead of executing the producer again.
    #[arg(long)]
    from_thread: Option<String>,

    /// Push the admitted product to this configured remote, under the
    /// declaration's signed byte bound.
    #[arg(long)]
    push_to: Option<String>,

    /// Execute the producer from this exact published project snapshot.
    #[arg(long, conflicts_with = "current_head")]
    project_snapshot: Option<String>,

    /// Execute the producer from the current principal project HEAD.
    #[arg(long)]
    current_head: bool,

    /// Give up waiting for the activation or the producer after this many
    /// seconds.
    #[arg(long, default_value_t = 3600)]
    timeout_secs: u64,

    /// Emit the final result as JSON.
    #[arg(long)]
    json: bool,
}

/// How the producer's project is selected for the launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectSelector {
    /// Capture the project at admission (`--pin-project`), the default.
    PinAtAdmission,
    CurrentHead,
    Snapshot(String),
}

pub(crate) struct PublishPlan {
    pub producer_ref: String,
    pub product_name: String,
    pub activation: Option<String>,
    pub from_thread: Option<String>,
    pub push_to: Option<String>,
    pub project: ProjectSelector,
}

pub(crate) struct Timing {
    pub poll: Duration,
    pub timeout: Duration,
}

/// The node commands the composite sends, one verified command per call.
pub(crate) trait NodeCommands {
    async fn call(&mut self, argv: Vec<String>) -> Result<Value, CliError>;
    async fn pause(&mut self, duration: Duration);
    fn progress(&mut self, message: &str) -> Result<(), CliError>;
}

pub(crate) async fn run(
    argv: &[String],
    app_root: &Path,
    project: Option<&Path>,
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    command_registration_policy: &ryeos_runtime::CommandRegistrationPolicy,
    console: &crate::tty::Console,
) -> Result<(), CliError> {
    use clap::error::ErrorKind;
    let args = match Args::try_parse_from(argv) {
        Ok(args) => args,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                console.text(&error.render().to_string())?;
                return Ok(());
            }
            _ => return Err(local(error.to_string())),
        },
    };
    let plan = PublishPlan {
        producer_ref: args.producer_ref,
        product_name: args.product_name,
        activation: args.activation,
        from_thread: args.from_thread,
        push_to: args.push_to,
        project: match (args.project_snapshot, args.current_head) {
            (Some(snapshot), _) => ProjectSelector::Snapshot(snapshot),
            (None, true) => ProjectSelector::CurrentHead,
            (None, false) => ProjectSelector::PinAtAdmission,
        },
    };
    let mut node = DaemonCommands {
        app_root,
        project,
        snapshot,
        command_registration_policy,
        console,
    };
    let timing = Timing {
        poll: Duration::from_secs(2),
        timeout: Duration::from_secs(args.timeout_secs),
    };
    let result = publish(&mut node, &plan, &timing).await?;
    if args.json {
        crate::tty::write_json(&result)?;
        return Ok(());
    }
    let mut banner = crate::tty::StatusBanner::new(crate::tty::Tone::Success, "PRODUCT PUBLISHED");
    for key in [
        "product_name",
        "thread_id",
        "witness_hash",
        "manifest_hash",
        "admission_hash",
        "remote",
        "acceptance_hash",
    ] {
        if let Some(value) = result.get(key).and_then(Value::as_str) {
            banner
                .rows
                .push(crate::tty::Row::key_value(key.replace('_', " "), value));
        }
    }
    console.status(&banner)?;
    Ok(())
}

/// The whole flow. Every step after production is idempotent on the node, so
/// the error of a failed step names the `--from-thread` that resumes it.
pub(crate) async fn publish<N: NodeCommands>(
    node: &mut N,
    plan: &PublishPlan,
    timing: &Timing,
) -> Result<Value, CliError> {
    let thread_id = match &plan.from_thread {
        Some(thread_id) => thread_id.clone(),
        None => {
            if let Some(activation) = &plan.activation {
                activate(node, timing, activation).await?;
            }
            launch(node, plan).await?
        }
    };
    let resume = |error: CliError| {
        local(format!(
            "{error}; the producer thread is {thread_id}: resume with --from-thread {thread_id}"
        ))
    };
    let thread = wait_for_thread(node, timing, &thread_id)
        .await
        .map_err(resume)?;
    let chain_root_id = thread
        .get("chain_root_id")
        .and_then(Value::as_str)
        .unwrap_or(&thread_id)
        .to_owned();

    node.progress(&format!("producer {thread_id} completed; capturing"))?;
    let captured = node
        .call(argv(&[
            "external-content",
            "capture-product",
            &chain_root_id,
            &thread_id,
            &plan.product_name,
        ]))
        .await
        .map_err(resume)?;
    let witness = match (
        captured.get("state").and_then(Value::as_str),
        hash_field(&captured, "witness_hash"),
    ) {
        (Some("captured"), Some(witness)) => witness,
        (state, _) => {
            return Err(resume(local(format!(
                "capture of product '{}' ended {}, not captured with a witness",
                plan.product_name,
                state.unwrap_or("without a state")
            ))));
        }
    };
    let manifest_hash = find_key(&captured, "manifest_hash");

    node.progress(&format!("captured witness {witness}; admitting it"))?;
    let admitted = node
        .call(argv(&["admission", "submit", &witness]))
        .await
        .map_err(resume)?;
    if find_key(&admitted, "subject_hash").as_deref() != Some(witness.as_str())
        || find_key(&admitted, "claim").as_deref() != Some("accepted")
    {
        return Err(resume(local(format!(
            "the admission does not name witness {witness} as accepted: {admitted}"
        ))));
    }
    let admission = find_key(&admitted, "attestation_hash")
        .filter(|hash| is_hash(hash))
        .ok_or_else(|| resume(local("the admission result has no attestation_hash")))?;

    let mut result = json!({
        "producer_ref": plan.producer_ref,
        "product_name": plan.product_name,
        "chain_root_id": chain_root_id,
        "thread_id": thread_id,
        "witness_hash": witness,
        "manifest_hash": manifest_hash,
        "admission_hash": admission,
    });
    let Some(remote) = &plan.push_to else {
        return Ok(result);
    };
    node.progress(&format!("admitted as {admission}; pushing to {remote}"))?;
    let pushed = node
        .call(argv(&[
            "external-content",
            "push-product",
            remote,
            &witness,
            &admission,
        ]))
        .await
        .map_err(resume)?;
    let acceptance = hash_field(&pushed, "acceptance_hash")
        .ok_or_else(|| resume(local("the push returned no acceptance_hash")))?;
    if let Some(accepted) = hash_field(&pushed, "witness_hash")
        && accepted != witness
    {
        return Err(resume(local(format!(
            "remote {remote} accepted witness {accepted}, not {witness}"
        ))));
    }
    result["remote"] = json!(remote);
    result["acceptance_hash"] = json!(acceptance);
    result["maximum_bytes"] = pushed.get("maximum_bytes").cloned().unwrap_or(Value::Null);
    Ok(result)
}

/// Submit the activation until the node reports it completed. Activation is
/// durable on the node, so submitting it again observes the same one.
async fn activate<N: NodeCommands>(
    node: &mut N,
    timing: &Timing,
    activation: &str,
) -> Result<(), CliError> {
    let mut waited = Duration::ZERO;
    loop {
        let out = node
            .call(argv(&[
                "external-content",
                "activate",
                activation,
                "online",
            ]))
            .await?;
        let state = out.get("state").and_then(Value::as_str).unwrap_or("");
        if state == "completed" && out.get("receipt_hash").is_some() {
            node.progress(&format!("activation {activation} completed"))?;
            return Ok(());
        }
        if matches!(state, "failed" | "cancelled") {
            return Err(local(format!(
                "activation {activation} ended {state}: {out}"
            )));
        }
        if waited >= timing.timeout {
            return Err(local(format!(
                "activation {activation} did not complete within {}s (state {state})",
                timing.timeout.as_secs()
            )));
        }
        node.pause(timing.poll).await;
        waited += timing.poll;
    }
}

async fn launch<N: NodeCommands>(node: &mut N, plan: &PublishPlan) -> Result<String, CliError> {
    let mut command = vec!["execute".to_owned(), "--async".to_owned()];
    match &plan.project {
        ProjectSelector::PinAtAdmission => command.push("--pin-project".to_owned()),
        ProjectSelector::CurrentHead => command.push("--current-head".to_owned()),
        ProjectSelector::Snapshot(snapshot) => {
            command.push("--project-snapshot".to_owned());
            command.push(snapshot.clone());
        }
    }
    command.push(plan.producer_ref.clone());
    let launched = node.call(command).await?;
    let thread_id = launched
        .get("thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            local(format!(
                "the producer launch was accepted without a thread_id: {launched}"
            ))
        })?
        .to_owned();
    node.progress(&format!(
        "producer {} launched as {thread_id}",
        plan.producer_ref
    ))?;
    Ok(thread_id)
}

/// Poll the thread until it is terminal. Only `completed` is a product.
async fn wait_for_thread<N: NodeCommands>(
    node: &mut N,
    timing: &Timing,
    thread_id: &str,
) -> Result<Value, CliError> {
    let mut waited = Duration::ZERO;
    loop {
        let raw = node.call(argv(&["thread", "get", thread_id])).await?;
        let view = match raw.get("thread") {
            Some(thread) if thread.is_object() => thread.clone(),
            _ => raw.clone(),
        };
        let status = view
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if TERMINAL.contains(&status.as_str()) {
            if status != "completed" {
                return Err(local(format!(
                    "producer thread {thread_id} ended {status}, so there is no product to capture"
                )));
            }
            return Ok(view);
        }
        if waited >= timing.timeout {
            return Err(local(format!(
                "producer thread {thread_id} is not terminal after {}s (status {status})",
                timing.timeout.as_secs()
            )));
        }
        node.pause(timing.poll).await;
        waited += timing.poll;
    }
}

fn argv(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_owned()).collect()
}

fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hash_field(value: &Value, key: &str) -> Option<String> {
    find_key(value, key).filter(|hash| is_hash(hash))
}

/// The first string under `key` anywhere in `value`, depth first.
fn find_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| map.values().find_map(|child| find_key(child, key))),
        Value::Array(items) => items.iter().find_map(|child| find_key(child, key)),
        _ => None,
    }
}

fn local(detail: impl Into<String>) -> CliError {
    CliError::Local {
        detail: detail.into(),
    }
}

struct DaemonCommands<'a> {
    app_root: &'a Path,
    project: Option<&'a Path>,
    snapshot: &'a ryeos_app::node_config::NodeConfigSnapshot,
    command_registration_policy: &'a ryeos_runtime::CommandRegistrationPolicy,
    console: &'a crate::tty::Console,
}

impl NodeCommands for DaemonCommands<'_> {
    async fn call(&mut self, argv: Vec<String>) -> Result<Value, CliError> {
        crate::dispatcher::execute_verified_command_in(
            self.app_root,
            self.snapshot,
            self.command_registration_policy,
            &argv,
            self.project,
        )
        .await
    }

    async fn pause(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn progress(&mut self, message: &str) -> Result<(), CliError> {
        self.console
            .info(&crate::tty::Diagnostic::info(message.to_owned()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const WITNESS: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const ADMISSION: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const ACCEPTANCE: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const MANIFEST: &str = "4444444444444444444444444444444444444444444444444444444444444444";

    struct FakeNode {
        calls: Vec<Vec<String>>,
        replies: VecDeque<(&'static str, Result<Value, CliError>)>,
    }

    impl FakeNode {
        fn new(replies: Vec<(&'static str, Result<Value, CliError>)>) -> Self {
            Self {
                calls: Vec::new(),
                replies: replies.into(),
            }
        }
    }

    impl NodeCommands for FakeNode {
        async fn call(&mut self, argv: Vec<String>) -> Result<Value, CliError> {
            let (expected, reply) = self
                .replies
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected call {argv:?}"));
            let verb = argv[..2].join(" ");
            assert_eq!(verb, expected, "call {argv:?}");
            self.calls.push(argv);
            reply
        }

        async fn pause(&mut self, _duration: Duration) {}

        fn progress(&mut self, _message: &str) -> Result<(), CliError> {
            Ok(())
        }
    }

    fn timing() -> Timing {
        Timing {
            poll: Duration::from_secs(2),
            timeout: Duration::from_secs(10),
        }
    }

    fn plan(from_thread: Option<&str>, push_to: Option<&str>, activation: bool) -> PublishPlan {
        PublishPlan {
            producer_ref: "graph:codex/guest-runtime-production".into(),
            product_name: "runtime".into(),
            activation: activation.then(|| "config:codex/guest-runtime-activation".to_owned()),
            from_thread: from_thread.map(str::to_owned),
            push_to: push_to.map(str::to_owned),
            project: ProjectSelector::PinAtAdmission,
        }
    }

    fn completed_thread() -> Result<Value, CliError> {
        Ok(json!({ "thread": { "status": "completed", "chain_root_id": "T-root" } }))
    }

    fn captured() -> Result<Value, CliError> {
        Ok(
            json!({ "state": "captured", "witness_hash": WITNESS, "evidence": { "manifest_hash": MANIFEST } }),
        )
    }

    fn admitted() -> Result<Value, CliError> {
        Ok(
            json!({ "attestation_hash": ADMISSION, "subject_hash": WITNESS, "policy": "local-node-v2", "claim": "accepted" }),
        )
    }

    #[tokio::test]
    async fn publish_runs_every_step_and_pushes_without_a_bound() {
        let mut node = FakeNode::new(vec![
            (
                "external-content activate",
                Ok(json!({ "state": "running" })),
            ),
            (
                "external-content activate",
                Ok(json!({ "state": "completed", "receipt_hash": "r" })),
            ),
            ("execute --async", Ok(json!({ "thread_id": "T-producer" }))),
            (
                "thread get",
                Ok(json!({ "thread": { "status": "running" } })),
            ),
            ("thread get", completed_thread()),
            ("external-content capture-product", captured()),
            ("admission submit", admitted()),
            (
                "external-content push-product",
                Ok(
                    json!({ "acceptance_hash": ACCEPTANCE, "witness_hash": WITNESS, "maximum_bytes": 8 }),
                ),
            ),
        ]);
        let result = publish(&mut node, &plan(None, Some("hosted"), true), &timing())
            .await
            .unwrap();
        assert_eq!(result["acceptance_hash"], ACCEPTANCE);
        assert_eq!(result["manifest_hash"], MANIFEST);
        assert_eq!(
            node.calls[2],
            [
                "execute",
                "--async",
                "--pin-project",
                "graph:codex/guest-runtime-production"
            ]
        );
        assert_eq!(
            node.calls[5],
            [
                "external-content",
                "capture-product",
                "T-root",
                "T-producer",
                "runtime"
            ]
        );
        // The push names no byte bound: the declaration's applies.
        assert_eq!(
            node.calls[7],
            [
                "external-content",
                "push-product",
                "hosted",
                WITNESS,
                ADMISSION
            ]
        );
    }

    #[tokio::test]
    async fn publish_resumes_from_thread() {
        let mut node = FakeNode::new(vec![
            ("thread get", completed_thread()),
            ("external-content capture-product", captured()),
            ("admission submit", admitted()),
        ]);
        let result = publish(&mut node, &plan(Some("T-producer"), None, true), &timing())
            .await
            .unwrap();
        assert_eq!(result["thread_id"], "T-producer");
        assert_eq!(result["admission_hash"], ADMISSION);
        // Neither activation nor the producer ran again.
        assert!(node.calls.iter().all(|call| call[0] != "execute"));
    }

    #[tokio::test]
    async fn publish_stops_on_failed_capture() {
        let mut node = FakeNode::new(vec![
            ("thread get", completed_thread()),
            (
                "external-content capture-product",
                Ok(json!({ "state": "refused" })),
            ),
        ]);
        let error = publish(
            &mut node,
            &plan(Some("T-producer"), Some("hosted"), false),
            &timing(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("ended refused"), "{error}");
        assert!(error.contains("--from-thread T-producer"), "{error}");
        assert_eq!(node.calls.len(), 2, "nothing after the failed capture");
    }

    #[tokio::test]
    async fn publish_skips_push_without_remote() {
        let mut node = FakeNode::new(vec![
            ("execute --async", Ok(json!({ "thread_id": "T-producer" }))),
            ("thread get", completed_thread()),
            ("external-content capture-product", captured()),
            ("admission submit", admitted()),
        ]);
        let result = publish(&mut node, &plan(None, None, false), &timing())
            .await
            .unwrap();
        assert!(result.get("acceptance_hash").is_none());
        assert!(result.get("remote").is_none());
    }

    #[tokio::test]
    async fn a_failed_producer_has_no_product() {
        let mut node = FakeNode::new(vec![
            ("execute --async", Ok(json!({ "thread_id": "T-producer" }))),
            (
                "thread get",
                Ok(json!({ "thread": { "status": "failed" } })),
            ),
        ]);
        let error = publish(&mut node, &plan(None, None, false), &timing())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("ended failed"), "{error}");
    }

    #[test]
    fn the_tokens_select_only_the_composite() {
        let rest = |tokens: &[&str]| tokens.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
        assert!(matches(&rest(&[
            "external-content",
            "product",
            "publish",
            "graph:x",
            "runtime"
        ])));
        assert!(!matches(&rest(&["external-content", "product", "T-1"])));
    }
}

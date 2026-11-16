//! `ryeos codex login device`: the whole Codex device-login ceremony as one
//! client composite.
//!
//! It only sequences verified node commands the operator could run one by
//! one: open the projectless login session, start the device login and show
//! its code, wait for the session to observe the account, end the session so
//! its worker releases the profile, show the observed account, and confirm
//! that exact login epoch and account digest. The node keeps the two-step
//! authority: observation by the login worker, confirmation by the operator.

use std::io::{BufRead as _, IsTerminal as _};
use std::path::Path;
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};

use crate::error::CliError;

pub(crate) const TOKENS: [&str; 3] = ["codex", "login", "device"];

/// The device code window is at most 15 minutes on the node.
const MAX_LOGIN_WINDOW_MS: i64 = 15 * 60 * 1000;

pub(crate) fn matches(rest: &[String]) -> bool {
    rest.len() >= TOKENS.len() && rest.iter().zip(TOKENS).all(|(token, want)| token == want)
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos codex login device",
    about = "Log a Codex credential profile in with a device code: open the login session, \
             show the code, wait for the account, end the session, and confirm the account.",
    no_binary_name = true
)]
struct Args {
    /// Credential profile to log in. It must be unauthenticated and unlocked.
    profile_id: String,

    /// Create the profile first. Creating an existing owned profile is a no-op.
    #[arg(long)]
    create: bool,

    /// Open the login on an explicitly trusted process-group node
    /// (`codex login open-trusted`) instead of the hard-contained one.
    #[arg(long)]
    trusted: bool,

    /// Confirm the observed account without asking. Required when stdin is
    /// not a terminal.
    #[arg(long)]
    yes: bool,

    /// Emit the final result as JSON.
    #[arg(long)]
    json: bool,
}

pub(crate) struct DeviceLoginPlan {
    pub profile_id: String,
    pub create: bool,
    pub trusted: bool,
}

pub(crate) struct Timing {
    pub poll: Duration,
    pub session_ready_timeout: Duration,
    pub lock_release_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(5),
            session_ready_timeout: Duration::from_secs(180),
            lock_release_timeout: Duration::from_secs(60),
        }
    }
}

/// The node commands the ceremony sends, one verified command per call.
pub(crate) trait NodeCommands {
    async fn call(&mut self, argv: Vec<String>) -> Result<Value, CliError>;
    async fn pause(&mut self, duration: Duration);
    fn now_ms(&self) -> i64;
}

/// The person at the terminal.
pub(crate) trait Operator {
    fn progress(&mut self, message: &str) -> Result<(), CliError>;
    fn show_device_code(&mut self, verification_url: &str, user_code: &str)
    -> Result<(), CliError>;
    fn approve_account(&mut self, account: &Value, account_digest: &str) -> Result<bool, CliError>;
}

pub(crate) async fn run(
    argv: &[String],
    app_root: &Path,
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
    if !args.yes && !std::io::stdin().is_terminal() {
        return Err(local(
            "stdin is not a terminal, so the observed account cannot be confirmed interactively; \
             pass --yes to confirm it without asking",
        ));
    }
    let mut node = DaemonCommands {
        app_root,
        snapshot,
        command_registration_policy,
    };
    let mut operator = TerminalOperator {
        console,
        assume_yes: args.yes,
    };
    let plan = DeviceLoginPlan {
        profile_id: args.profile_id,
        create: args.create,
        trusted: args.trusted,
    };
    let result = device_login(&mut node, &mut operator, &plan, &Timing::default()).await?;
    if args.json {
        crate::tty::write_json(&result)?;
        return Ok(());
    }
    let mut banner =
        crate::tty::StatusBanner::new(crate::tty::Tone::Success, "CODEX PROFILE LOGGED IN");
    for key in [
        "profile_id",
        "state",
        "credential_generation",
        "login_epoch",
        "confirmed_account_digest",
        "login_session",
    ] {
        if let Some(value) = result.get(key) {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            banner
                .rows
                .push(crate::tty::Row::key_value(key.replace('_', " "), value));
        }
    }
    console.status(&banner)?;
    Ok(())
}

/// The whole ceremony. Any failure while the login session is open ends that
/// session so its worker releases the profile; the node's session cleanup
/// cancels a login that was never observed.
pub(crate) async fn device_login<N: NodeCommands, O: Operator>(
    node: &mut N,
    operator: &mut O,
    plan: &DeviceLoginPlan,
    timing: &Timing,
) -> Result<Value, CliError> {
    let profile_id = plan.profile_id.as_str();
    if plan.create {
        node.call(argv(&["codex", "profile", "create", profile_id]))
            .await?;
    }
    let profile = node
        .call(argv(&["codex", "profile", "get", profile_id]))
        .await?;
    require_ready_for_login(&profile, profile_id)?;

    let open = if plan.trusted { "open-trusted" } else { "open" };
    let launch = node
        .call(argv(&["codex", "login", open, profile_id, "--async"]))
        .await?;
    let session = launch
        .get("thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| local("the login launch was accepted without a thread_id"))?
        .to_owned();
    operator.progress(&format!("login session {session} opened"))?;

    if let Err(error) = observe_account(node, operator, timing, &session, profile_id).await {
        let ended = node.call(terminate_argv(&session)).await;
        return Err(local(match ended {
            Ok(_) => format!("{error}; login session {session} was ended"),
            Err(end_error) => format!(
                "{error}; ending login session {session} also failed ({end_error}); run `ryeos codex session terminate {session} cancelled`"
            ),
        }));
    }

    // Confirmation bumps the credential generation, so it needs no live
    // writer: end the session and wait for its worker to release the profile.
    node.call(terminate_argv(&session)).await?;
    operator.progress("account observed; login session ended, waiting for the profile lock")?;
    let profile = wait_for_released_profile(node, timing, profile_id).await?;
    let state = profile.get("state").and_then(Value::as_str).unwrap_or("");
    let login_epoch = profile
        .get("login_epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| local("the profile has no login epoch"))?;
    if state != "confirming" {
        return Err(local(format!(
            "credential profile '{profile_id}' is {state} after the login, not confirming; read it with `ryeos codex profile get {profile_id}`"
        )));
    }
    let account = profile
        .get("sanitized_account")
        .filter(|account| !account.is_null())
        .ok_or_else(|| local("the profile has no observed account"))?;
    let digest = profile
        .get("sanitized_account_digest")
        .and_then(Value::as_str)
        .ok_or_else(|| local("the profile has no observed account digest"))?;
    if !operator.approve_account(account, digest)? {
        return Err(local(format!(
            "account not confirmed. Credential profile '{profile_id}' stays confirming; confirm it with `ryeos codex profile confirm {profile_id} {login_epoch} {digest}` before the window closes, or after it closes run `ryeos codex profile abandon-login {profile_id} {login_epoch}`"
        )));
    }
    let login_epoch_text = login_epoch.to_string();
    let confirmed = node
        .call(argv(&[
            "codex",
            "profile",
            "confirm",
            profile_id,
            login_epoch_text.as_str(),
            digest,
        ]))
        .await?;
    Ok(json!({
        "profile_id": profile_id,
        "state": confirmed.get("state").cloned().unwrap_or(Value::Null),
        "credential_generation": confirmed
            .get("credential_generation")
            .cloned()
            .unwrap_or(Value::Null),
        "login_epoch": login_epoch,
        "confirmed_account_digest": digest,
        "login_session": session,
    }))
}

fn require_ready_for_login(profile: &Value, profile_id: &str) -> Result<(), CliError> {
    let state = profile.get("state").and_then(Value::as_str).unwrap_or("");
    let epoch = profile
        .get("login_epoch")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if let Some(holder) = profile.get("lock_owner").and_then(Value::as_str) {
        return Err(local(format!(
            "credential profile '{profile_id}' is held by {holder}; end the session using it before logging in"
        )));
    }
    match state {
        "unauthenticated" => Ok(()),
        "enrolling" | "confirming" => Err(local(format!(
            "credential profile '{profile_id}' already has login epoch {epoch} {state}; finish it, or once its window closes run `ryeos codex profile abandon-login {profile_id} {epoch}`"
        ))),
        "active" => Err(local(format!(
            "credential profile '{profile_id}' is already active; revoke it before logging in again"
        ))),
        other => Err(local(format!(
            "credential profile '{profile_id}' is {other} and cannot log in"
        ))),
    }
}

async fn observe_account<N: NodeCommands, O: Operator>(
    node: &mut N,
    operator: &mut O,
    timing: &Timing,
    session: &str,
    profile_id: &str,
) -> Result<(), CliError> {
    wait_for_ready_session(node, timing, session).await?;
    let started = node
        .call(session_command_argv(
            session,
            "device-login-start",
            "credential.login.start",
        ))
        .await?;
    let device = &started["result"]["response"]["result"];
    let (Some(url), Some(code)) = (
        device.get("verificationUrl").and_then(Value::as_str),
        device.get("userCode").and_then(Value::as_str),
    ) else {
        return Err(local("credential.login.start returned no device code"));
    };
    operator.show_device_code(url, code)?;

    // The node enforces the real window at confirmation; this bound only
    // stops polling once no observation can still count.
    let profile = node
        .call(argv(&["codex", "profile", "get", profile_id]))
        .await?;
    let window_closes_at_ms = profile
        .get("login_expires_at_ms")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| node.now_ms().saturating_add(MAX_LOGIN_WINDOW_MS));
    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        let key = format!("device-account-read-{attempt}");
        let read = node
            .call(session_command_argv(
                session,
                &key,
                "credential.account.read",
            ))
            .await?;
        let account = &read["result"]["response"]["result"]["account"];
        if !account.is_null() {
            return Ok(());
        }
        if node.now_ms() >= window_closes_at_ms {
            return Err(local(format!(
                "the device login window closed before the account was observed; run `ryeos codex login device {profile_id}` again"
            )));
        }
        node.pause(timing.poll).await;
    }
}

async fn wait_for_ready_session<N: NodeCommands>(
    node: &mut N,
    timing: &Timing,
    session: &str,
) -> Result<(), CliError> {
    let deadline = node
        .now_ms()
        .saturating_add(duration_ms(timing.session_ready_timeout));
    let mut last: String;
    loop {
        match node
            .call(argv(&["codex", "session", "status", session]))
            .await
        {
            Ok(status) => {
                let state = status.get("state").and_then(Value::as_str).unwrap_or("");
                let attached = status
                    .get("worker_instance_id")
                    .and_then(Value::as_str)
                    .is_some();
                if state == "idle" && attached {
                    return Ok(());
                }
                if state == "terminal" {
                    return Err(local(format!(
                        "login session {session} ended before it was ready"
                    )));
                }
                last = format!("state {state}");
            }
            // The session row appears only after admission; keep reading.
            Err(error) => last = error.to_string(),
        }
        if node.now_ms() >= deadline {
            return Err(local(format!(
                "login session {session} was not ready in time ({last})"
            )));
        }
        node.pause(timing.poll).await;
    }
}

async fn wait_for_released_profile<N: NodeCommands>(
    node: &mut N,
    timing: &Timing,
    profile_id: &str,
) -> Result<Value, CliError> {
    let deadline = node
        .now_ms()
        .saturating_add(duration_ms(timing.lock_release_timeout));
    loop {
        let profile = node
            .call(argv(&["codex", "profile", "get", profile_id]))
            .await?;
        let holder = profile
            .get("lock_owner")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(holder) = holder else {
            return Ok(profile);
        };
        if node.now_ms() >= deadline {
            return Err(local(format!(
                "credential profile '{profile_id}' is still held by {holder} after the login session ended; confirm with `ryeos codex profile confirm` once it is released"
            )));
        }
        node.pause(timing.poll).await;
    }
}

fn argv(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_owned()).collect()
}

fn session_command_argv(session: &str, idempotency_key: &str, route_id: &str) -> Vec<String> {
    argv(&[
        "codex",
        "session",
        "command",
        session,
        idempotency_key,
        route_id,
        "--input",
        r#"{"payload":{}}"#,
    ])
}

fn terminate_argv(session: &str) -> Vec<String> {
    argv(&["codex", "session", "terminate", session, "cancelled"])
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn local(detail: impl Into<String>) -> CliError {
    CliError::Local {
        detail: detail.into(),
    }
}

struct DaemonCommands<'a> {
    app_root: &'a Path,
    snapshot: &'a ryeos_app::node_config::NodeConfigSnapshot,
    command_registration_policy: &'a ryeos_runtime::CommandRegistrationPolicy,
}

impl NodeCommands for DaemonCommands<'_> {
    async fn call(&mut self, argv: Vec<String>) -> Result<Value, CliError> {
        crate::dispatcher::execute_verified_command(
            self.app_root,
            self.snapshot,
            self.command_registration_policy,
            &argv,
        )
        .await
    }

    async fn pause(&mut self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn now_ms(&self) -> i64 {
        lillux::time::timestamp_millis() as i64
    }
}

struct TerminalOperator<'a> {
    console: &'a crate::tty::Console,
    assume_yes: bool,
}

impl Operator for TerminalOperator<'_> {
    fn progress(&mut self, message: &str) -> Result<(), CliError> {
        self.console
            .info(&crate::tty::Diagnostic::info(message.to_owned()))?;
        Ok(())
    }

    fn show_device_code(
        &mut self,
        verification_url: &str,
        user_code: &str,
    ) -> Result<(), CliError> {
        // The code is ephemeral: show it, never journal it.
        let mut banner =
            crate::tty::StatusBanner::new(crate::tty::Tone::Neutral, "CODEX DEVICE LOGIN");
        banner.rows.push(crate::tty::Row::key_value(
            "open",
            crate::tty::sanitize_terminal_inline(verification_url),
        ));
        banner.rows.push(crate::tty::Row::key_value(
            "code",
            crate::tty::sanitize_terminal_inline(user_code),
        ));
        banner.rows.push(crate::tty::Row::text(
            "Sign in and approve the device; this command waits for the account.",
        ));
        self.console.status(&banner)?;
        Ok(())
    }

    fn approve_account(&mut self, account: &Value, account_digest: &str) -> Result<bool, CliError> {
        let rendered = serde_json::to_string_pretty(account)
            .map_err(|error| local(format!("render observed account: {error}")))?;
        self.console.info(&crate::tty::Diagnostic::info(format!(
            "Observed account (digest {account_digest}): {}",
            crate::tty::sanitize_terminal_inline(&rendered.replace('\n', " "))
        )))?;
        if self.assume_yes {
            self.console
                .info(&crate::tty::Diagnostic::info("Confirming it (--yes)."))?;
            return Ok(true);
        }
        self.console
            .prompt("Confirm this account for the profile? [y/N] ")?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeNode {
        calls: Vec<Vec<String>>,
        replies: VecDeque<(&'static str, Result<Value, CliError>)>,
        now_ms: i64,
    }

    impl FakeNode {
        fn new(replies: Vec<(&'static str, Result<Value, CliError>)>) -> Self {
            Self {
                calls: Vec::new(),
                replies: replies.into(),
                now_ms: 1_000,
            }
        }

        fn verbs(&self) -> Vec<String> {
            self.calls
                .iter()
                .map(|call| {
                    let verb = call[..3].join(" ");
                    match call.get(5) {
                        Some(route) if verb == "codex session command" => {
                            format!("{verb} {route}")
                        }
                        _ => verb,
                    }
                })
                .collect()
        }
    }

    impl NodeCommands for FakeNode {
        async fn call(&mut self, argv: Vec<String>) -> Result<Value, CliError> {
            let (expected, reply) = self
                .replies
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected call {argv:?}"));
            let verb = argv[..3].join(" ");
            assert_eq!(verb, expected, "call {argv:?}");
            self.calls.push(argv);
            reply
        }

        async fn pause(&mut self, duration: Duration) {
            self.now_ms += duration_ms(duration);
        }

        fn now_ms(&self) -> i64 {
            self.now_ms
        }
    }

    #[derive(Default)]
    struct FakeOperator {
        shown: Option<(String, String)>,
        approved: Option<String>,
        approve: bool,
    }

    impl Operator for FakeOperator {
        fn progress(&mut self, _message: &str) -> Result<(), CliError> {
            Ok(())
        }

        fn show_device_code(
            &mut self,
            verification_url: &str,
            user_code: &str,
        ) -> Result<(), CliError> {
            self.shown = Some((verification_url.to_owned(), user_code.to_owned()));
            Ok(())
        }

        fn approve_account(
            &mut self,
            _account: &Value,
            account_digest: &str,
        ) -> Result<bool, CliError> {
            self.approved = Some(account_digest.to_owned());
            Ok(self.approve)
        }
    }

    fn timing() -> Timing {
        Timing {
            poll: Duration::from_secs(5),
            session_ready_timeout: Duration::from_secs(30),
            lock_release_timeout: Duration::from_secs(30),
        }
    }

    fn plan(create: bool) -> DeviceLoginPlan {
        DeviceLoginPlan {
            profile_id: "personal".into(),
            create,
            trusted: true,
        }
    }

    fn device_code() -> Value {
        json!({"result": {"response": {"result": {
            "verificationUrl": "https://example.invalid/device",
            "userCode": "ABCD-EFGHI"
        }}}})
    }

    fn account_read(present: bool) -> Value {
        let account = if present {
            json!({"type": "chatgpt"})
        } else {
            Value::Null
        };
        json!({"result": {"response": {"result": {"account": account}}}})
    }

    fn opening(create: bool) -> Vec<(&'static str, Result<Value, CliError>)> {
        let mut replies = Vec::new();
        if create {
            replies.push((
                "codex profile create",
                Ok(json!({"state": "unauthenticated"})),
            ));
        }
        replies.extend([
            (
                "codex profile get",
                Ok(json!({"state": "unauthenticated", "login_epoch": 0, "lock_owner": null})),
            ),
            (
                "codex login open-trusted",
                Ok(json!({"status": "accepted", "thread_id": "T-login"})),
            ),
            (
                "codex session status",
                Err(local("HTTP 404 before the session row exists")),
            ),
            (
                "codex session status",
                Ok(json!({"state": "idle", "worker_instance_id": "W-1"})),
            ),
            ("codex session command", Ok(device_code())),
            (
                "codex profile get",
                Ok(json!({"state": "enrolling", "login_epoch": 1, "login_expires_at_ms": 60_000})),
            ),
        ]);
        replies
    }

    #[tokio::test]
    async fn device_login_sequence_order() {
        let mut replies = opening(true);
        replies.extend([
            ("codex session command", Ok(account_read(false))),
            ("codex session command", Ok(account_read(true))),
            ("codex session terminate", Ok(json!({"state": "terminal"}))),
            (
                "codex profile get",
                Ok(json!({"state": "confirming", "lock_owner": "W-1", "login_epoch": 1})),
            ),
            (
                "codex profile get",
                Ok(json!({
                    "state": "confirming",
                    "lock_owner": null,
                    "login_epoch": 1,
                    "sanitized_account": {"type": "chatgpt"},
                    "sanitized_account_digest": "d".repeat(64),
                })),
            ),
            (
                "codex profile confirm",
                Ok(json!({"state": "active", "credential_generation": 2})),
            ),
        ]);
        let mut node = FakeNode::new(replies);
        let mut operator = FakeOperator {
            approve: true,
            ..Default::default()
        };
        let result = device_login(&mut node, &mut operator, &plan(true), &timing())
            .await
            .unwrap();
        assert_eq!(
            node.verbs(),
            [
                "codex profile create",
                "codex profile get",
                "codex login open-trusted",
                "codex session status",
                "codex session status",
                "codex session command credential.login.start",
                "codex profile get",
                "codex session command credential.account.read",
                "codex session command credential.account.read",
                "codex session terminate",
                "codex profile get",
                "codex profile get",
                "codex profile confirm",
            ]
        );
        assert_eq!(
            node.calls[2],
            argv(&["codex", "login", "open-trusted", "personal", "--async"])
        );
        assert_eq!(
            node.calls[9],
            argv(&["codex", "session", "terminate", "T-login", "cancelled"])
        );
        let digest = "d".repeat(64);
        assert_eq!(
            node.calls[12],
            argv(&["codex", "profile", "confirm", "personal", "1", &digest])
        );
        assert_eq!(
            operator.shown,
            Some(("https://example.invalid/device".into(), "ABCD-EFGHI".into()))
        );
        assert_eq!(operator.approved.as_deref(), Some(digest.as_str()));
        assert_eq!(result["state"], "active");
        assert_eq!(result["credential_generation"], 2);
        assert_eq!(result["login_session"], "T-login");
    }

    #[tokio::test]
    async fn device_login_stops_on_expired_window() {
        let mut replies = opening(false);
        // 1_000 ms start, 5 s per poll, window closes at 60_000 ms: twelve
        // empty reads, then the session is ended and nothing is confirmed.
        for _ in 0..12 {
            replies.push(("codex session command", Ok(account_read(false))));
        }
        replies.push(("codex session terminate", Ok(json!({"state": "terminal"}))));
        let mut node = FakeNode::new(replies);
        let mut operator = FakeOperator::default();
        let error = device_login(&mut node, &mut operator, &plan(false), &timing())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("window closed"), "{error}");
        assert!(error.contains("T-login was ended"), "{error}");
        assert!(node.replies.is_empty());
        assert!(
            !node
                .verbs()
                .iter()
                .any(|verb| verb == "codex profile confirm"),
            "an expired window must never confirm"
        );
        assert_eq!(operator.approved, None);
    }

    #[tokio::test]
    async fn device_login_refuses_a_profile_left_mid_login() {
        let mut node = FakeNode::new(vec![(
            "codex profile get",
            Ok(json!({"state": "confirming", "login_epoch": 4, "lock_owner": null})),
        )]);
        let error = device_login(
            &mut node,
            &mut FakeOperator::default(),
            &plan(false),
            &timing(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("ryeos codex profile abandon-login personal 4"));
        assert_eq!(node.calls.len(), 1, "nothing is opened for a stuck profile");
    }

    #[tokio::test]
    async fn declined_account_is_not_confirmed() {
        let mut replies = opening(false);
        replies.extend([
            ("codex session command", Ok(account_read(true))),
            ("codex session terminate", Ok(json!({"state": "terminal"}))),
            (
                "codex profile get",
                Ok(json!({
                    "state": "confirming",
                    "lock_owner": null,
                    "login_epoch": 1,
                    "sanitized_account": {"type": "chatgpt"},
                    "sanitized_account_digest": "e".repeat(64),
                })),
            ),
        ]);
        let mut node = FakeNode::new(replies);
        let error = device_login(
            &mut node,
            &mut FakeOperator::default(),
            &plan(false),
            &timing(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("account not confirmed"));
        assert!(node.replies.is_empty());
    }

    #[test]
    fn composite_tokens_match_only_the_device_verb() {
        let rest = |tokens: &[&str]| argv(tokens);
        assert!(matches(&rest(&["codex", "login", "device", "personal"])));
        assert!(!matches(&rest(&["codex", "login", "open", "personal"])));
        assert!(!matches(&rest(&["codex", "login"])));
    }
}

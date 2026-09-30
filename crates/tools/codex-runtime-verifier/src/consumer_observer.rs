//! Single-threaded preparation for the enclosing consumer observer.
//!
//! The authenticated adapter run must select this exact delivered executable
//! and challenge. These local checks corroborate bytes and account identity;
//! they do not authenticate the provider channel or qualify a runtime. No
//! worker, session, contact journal or candidate authority is created here.

use std::{ffi::OsStr, path::Path};

use anyhow::{Context as _, Result, ensure};
use lillux::{PinnedDirectory, time::MonotonicDeadline};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeChallenge, RESTORATION_VERIFIER_REMOTE_DIRECTORY,
};

use crate::consumer_record::{
    CONSUMER_INPUT_RECORD_NAME, ConsumerInputRecord, ImportedConsumerInputs,
    MAX_CONSUMER_INPUT_RECORD_BYTES,
};

pub const CONSUMER_STARTUP_FLAG: &str = "--consumer-challenge-b64";

/// Exact bounded argument decoding, not authorization of the run channel.
pub fn decode_startup_arguments(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<ConsumerRuntimeChallenge> {
    use base64::Engine as _;
    let mut arguments = arguments.into_iter();
    ensure!(
        arguments.next().as_deref() == Some(OsStr::new(CONSUMER_STARTUP_FLAG)),
        "consumer observer requires its exact startup flag"
    );
    let encoded = arguments
        .next()
        .context("consumer observer challenge absent")?;
    ensure!(
        arguments.next().is_none(),
        "consumer observer received extra arguments"
    );
    let encoded = encoded
        .to_str()
        .context("consumer observer challenge is not UTF-8")?;
    ensure!(
        !encoded.is_empty() && encoded.len() <= 8192,
        "consumer observer challenge exceeds startup bound"
    );
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?;
    ensure!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes) == encoded,
        "consumer observer challenge is not canonical base64url"
    );
    ConsumerRuntimeChallenge::parse(&bytes)
}

/// Failure custody only, not an additional execution journal or workflow.
/// An uncertain cleanup returns this same value with its owners intact. The
/// enclosing attempt must quarantine and settle the provider occurrence.
#[must_use = "retain observer failure owners until cleanup or provider death"]
pub struct ConsumerObservationFailure {
    error: anyhow::Error,
    app: Option<crate::app_server::AppServerObservation>,
    launch: Option<crate::app_server::AppServerLaunchFailure>,
    peer: Option<crate::scripted_peer::RunningScriptedPeer>,
    active_deadline: MonotonicDeadline,
    cleanup_deadline: MonotonicDeadline,
}

impl ConsumerObservationFailure {
    fn before_contact(
        error: anyhow::Error,
        active_deadline: MonotonicDeadline,
        cleanup_deadline: MonotonicDeadline,
    ) -> Self {
        Self {
            error,
            app: None,
            launch: None,
            peer: None,
            active_deadline,
            cleanup_deadline,
        }
    }

    /// Settle only the retained local owners, never attest provider death or
    /// namespace settlement from forceful exact-child cleanup. Both deadlines
    /// are retained from the original attempt and cannot be renewed on retry.
    pub fn settle(mut self) -> std::result::Result<anyhow::Error, Self> {
        let active = self.active_deadline;
        let cleanup = self.cleanup_deadline;
        if let Some(mut app) = self.app.take() {
            if app.stop_until(active, cleanup).is_err() {
                self.app = Some(app);
            }
        }
        if let Some(launch) = self.launch.take() {
            match launch.settle(cleanup) {
                Ok(error) => {
                    self.error = self.error.context(format!("owner startup failed: {error}"))
                }
                Err(launch) => self.launch = Some(launch),
            }
        }
        if let Some(peer) = self.peer.take() {
            match peer.cancel_until(cleanup) {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => {
                    self.error = self
                        .error
                        .context("scripted peer refused during failure cleanup");
                }
                Err(peer) => self.peer = Some(peer),
            }
        }
        if self.app.is_some() || self.launch.is_some() || self.peer.is_some() {
            Err(self)
        } else {
            Ok(self.error)
        }
    }
}

impl std::fmt::Debug for ConsumerObservationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsumerObservationFailure")
            .field("error", &self.error)
            .field("app_retained", &self.app.is_some())
            .field("launch_retained", &self.launch.is_some())
            .field("peer_retained", &self.peer.is_some())
            .finish()
    }
}

/// One-use private preparation, not another execution lifecycle. The existing
/// enclosing attempt owns admission, contact and final provider termination.
pub struct PreparedConsumerObserver {
    pub record: ConsumerInputRecord,
    pub imported: ImportedConsumerInputs,
    pub work: PinnedDirectory,
    pub active_deadline: MonotonicDeadline,
    pub cleanup_deadline: MonotonicDeadline,
}

impl PreparedConsumerObserver {
    /// Execute the single prepared observation using the existing exact-child
    /// and scripted-peer owners. Returning evidence does not qualify a runtime;
    /// returning a failure retains all unsettled local ownership obligations.
    pub fn observe(
        &self,
        challenge: &ConsumerRuntimeChallenge,
    ) -> std::result::Result<Vec<u8>, ConsumerObservationFailure> {
        use crate::app_server::AppServerObservation;
        use crate::consumer_protocol::{
            ConsumerObservationInputs, ConsumerScriptedExpectation, collect_owned_consumer_protocol,
        };
        let before = |error| {
            ConsumerObservationFailure::before_contact(
                error,
                self.active_deadline,
                self.cleanup_deadline,
            )
        };
        self.imported
            .require_record_challenge(&self.record, challenge)
            .map_err(before)?;
        self.imported
            .require_current_verifier_image()
            .map_err(before)?;
        let home = self
            .work
            .create_child(OsStr::new("home"), 0o700)
            .map_err(before)?;
        let control = self
            .work
            .create_child(OsStr::new("native-control"), 0o700)
            .map_err(before)?;
        let owner = self
            .work
            .create_child(OsStr::new("outer-owner"), 0o700)
            .map_err(before)?;
        let expectation = ConsumerScriptedExpectation::prepare(&self.record, challenge, &self.work)
            .map_err(before)?;
        // All staging and exact launch checks happen before the peer task or
        // subordinate begins. Parent preparation authors home exactly once.
        let expected = self
            .imported
            .prepare_codex_outer_request(&self.record, challenge, &home, &control, &owner)
            .map_err(before)?;
        let launch = self
            .imported
            .prepare_outer_owner_launch(&self.record, challenge, &home, &control, &owner)
            .map_err(before)?;
        if self.active_deadline.has_elapsed() {
            return Err(before(anyhow::anyhow!("consumer prepared launch expired")));
        }
        let bound = expectation
            .bind_peer(self.active_deadline)
            .map_err(before)?;
        let mut peer = Some(bound.start().map_err(before)?);
        let mut app = match AppServerObservation::launch(launch, self.active_deadline) {
            Ok(app) => app,
            Err(launch) => {
                return Err(ConsumerObservationFailure {
                    error: anyhow::anyhow!("consumer outer owner startup failed"),
                    app: None,
                    launch: Some(launch),
                    peer,
                    active_deadline: self.active_deadline,
                    cleanup_deadline: self.cleanup_deadline,
                });
            }
        };
        let result = collect_owned_consumer_protocol(
            &mut app,
            &mut peer,
            ConsumerObservationInputs {
                imported: &self.imported,
                record: &self.record,
                challenge,
                expectation: &expectation,
                fresh_home: &home,
                native_control: &control,
                protected_owner: &owner,
                expected_request: expected.request(),
            },
            self.active_deadline,
            self.cleanup_deadline,
        )
        .and_then(|observed| {
            observed.canonical_evidence(&self.record, challenge, self.active_deadline)
        });
        match result {
            Ok(bytes) => Ok(bytes),
            Err(error) => Err(ConsumerObservationFailure {
                error,
                app: Some(app),
                launch: None,
                peer,
                active_deadline: self.active_deadline,
                cleanup_deadline: self.cleanup_deadline,
            }),
        }
    }
}

/// Call only at exclusive synchronous startup, before any thread or child.
/// The root account transition is irreversible. Any partial grant or failure
/// makes this occurrence unsuitable for reuse; never repair or relaunch it.
pub fn prepare_startup(
    challenge: &ConsumerRuntimeChallenge,
    active_deadline: MonotonicDeadline,
    cleanup_deadline: MonotonicDeadline,
) -> Result<PreparedConsumerObserver> {
    challenge.validate()?;
    let active_deadline = active_deadline.min(cleanup_deadline);
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer startup deadlines expired"
    );
    let runtime_root = PinnedDirectory::open(Path::new("/ryeos/guest-runtime"))?
        .context("consumer observer installed runtime absent")?;
    let runtime =
        crate::consumer_outer_owner::require_selected_owner_runtime(&runtime_root, challenge)?;
    let parent = PinnedDirectory::open(Path::new(RESTORATION_VERIFIER_REMOTE_DIRECTORY))?
        .context("consumer observer qualification parent absent")?;
    parent.require_owner(0)?;
    parent.ensure_path_binding()?;
    let source_path = challenge.intent.remote_upload_directory()?;
    let source = PinnedDirectory::open(Path::new(&source_path))?
        .context("consumer observer exact delivered root absent")?;
    source.require_owner(0)?;
    source.ensure_path_binding()?;
    runtime.require_disjoint_directory_tree(&parent)?;
    runtime.require_disjoint_directory_tree(&source)?;
    // This is the provider-created private delivery wrapper, not an admitted
    // product member. Product contents/modes are verified below, never repaired.
    source.tighten_owner_private_directory()?;
    let file = source
        .open_pinned_regular(OsStr::new(CONSUMER_INPUT_RECORD_NAME), false)?
        .context("consumer observer input record absent")?;
    let bytes =
        file.read_stable_bounded(&file.observation()?, MAX_CONSUMER_INPUT_RECORD_BYTES as u64)?;
    let record = ConsumerInputRecord::parse(&bytes)?;
    let imported = record.open_imported_products(&source, challenge)?;
    imported.require_current_verifier_image()?;
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer startup import expired"
    );
    // Fixed attempt name and create-only semantics refuse an ambiguous second
    // invocation. Do not mint a new random workspace to bypass that refusal.
    let name = format!("consumer-observer-{}", challenge.intent.operation_id);
    let work = parent.create_child(OsStr::new(&name), 0o700)?;
    work.require_owner_private_directory()?;
    work.require_disjoint_directory_tree(&source)?;
    runtime.require_disjoint_directory_tree(&work)?;
    imported.transfer_private_delivery_account(&record, challenge, &runtime, active_deadline)?;
    let account = &runtime.profile().account;
    // Root retains mutation of this non-secret parent. Only exact private
    // attempt roots/files are transferred to the selected installed account.
    account.grant_readonly_host_directory(&parent)?;
    account.grant_private_directory(&work)?;
    runtime.recheck()?;
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer account transition expired"
    );
    account.drop_current_process()?;
    account.require_current_process()?;
    runtime.recheck()?;
    parent.ensure_path_binding()?;
    work.require_owner_private_directory()?;
    source.require_owner_private_directory()?;
    // Reopen the complete exact closure under the relinquished account. The
    // pre-transition successful read is not proof that post-drop inputs work.
    let imported = record.open_imported_products(&source, challenge)?;
    imported.require_current_verifier_image()?;
    ensure!(
        !active_deadline.has_elapsed(),
        "consumer post-drop import expired"
    );
    Ok(PreparedConsumerObserver {
        record,
        imported,
        work,
        active_deadline,
        cleanup_deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_arguments_refuse_missing_extra_aliased_and_invalid_encodings() {
        for args in [
            vec![],
            vec![CONSUMER_STARTUP_FLAG],
            vec!["--challenge-b64", "e30"],
            vec![CONSUMER_STARTUP_FLAG, "e30", "extra"],
            vec![CONSUMER_STARTUP_FLAG, "%%%"],
            vec![CONSUMER_STARTUP_FLAG, "e30="],
            vec![CONSUMER_STARTUP_FLAG, "e30"],
        ] {
            assert!(
                decode_startup_arguments(args.into_iter().map(std::ffi::OsString::from)).is_err()
            );
        }
        assert!(
            decode_startup_arguments([CONSUMER_STARTUP_FLAG.into(), "x".repeat(8193).into(),])
                .is_err()
        );
    }

    #[test]
    fn pre_contact_failure_preserves_original_error_without_live_owners() {
        let active = MonotonicDeadline::after(lillux::time::Duration::ZERO);
        let cleanup = MonotonicDeadline::after(lillux::time::Duration::ZERO);
        let failure = ConsumerObservationFailure::before_contact(
            anyhow::anyhow!("exact preflight refused"),
            active,
            cleanup,
        );
        assert!(failure.active_deadline.has_elapsed());
        assert!(failure.cleanup_deadline.has_elapsed());
        assert!(failure.app.is_none() && failure.launch.is_none() && failure.peer.is_none());
        let error = failure
            .settle()
            .expect("no born process or peer requires cleanup");
        assert_eq!(error.to_string(), "exact preflight refused");
    }
}

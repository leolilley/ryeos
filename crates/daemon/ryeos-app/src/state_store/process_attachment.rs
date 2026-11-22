//! Thread process attachment, stop requests and process identity.
use super::*;

#[derive(Clone, Copy)]
pub(super) enum ProcessAttachmentMode<'a> {
    Idempotent {
        launch_owner: Option<&'a str>,
    },
    New {
        launch_owner: Option<&'a str>,
        rearm_resume_budget: bool,
    },
}

impl<'a> ProcessAttachmentMode<'a> {
    pub(super) fn launch_owner(self) -> Option<&'a str> {
        match self {
            Self::Idempotent { launch_owner } | Self::New { launch_owner, .. } => launch_owner,
        }
    }

    pub(super) fn requires_empty(self) -> bool {
        matches!(self, Self::New { .. })
    }

    pub(super) fn rearms_resume_budget(self) -> bool {
        matches!(
            self,
            Self::New {
                rearm_resume_budget: true,
                ..
            }
        )
    }
}

impl StateStore {
    /// Persist a daemon-held process at the attachment-before-execution
    /// boundary. The runtime cannot have self-attached before this call, so an
    /// existing identity (including an exact repeat) is rejected.
    pub fn attach_new_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        self.attach_thread_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            ProcessAttachmentMode::New {
                launch_owner: expected_launch_owner,
                rearm_resume_budget: false,
            },
        )
    }

    /// Persist a held continuation process and atomically transfer recovery
    /// ownership from its predecessor-to-successor launch budget to its own
    /// same-thread resume budget.
    pub fn attach_new_thread_process_rearming_resume_budget(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        self.attach_thread_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            ProcessAttachmentMode::New {
                launch_owner: expected_launch_owner,
                rearm_resume_budget: true,
            },
        )
    }

    pub(super) fn attach_thread_process_with_mode(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        mode: ProcessAttachmentMode<'_>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if let Some(expected) = mode.launch_owner() {
            let claim = g
                .runtime_db
                .get_launch_claim(thread_id)?
                .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no launch owner"))?;
            if claim.claimed_by != expected {
                anyhow::bail!("stale launch owner cannot attach process to {thread_id}");
            }
        }
        // The projection row is the authoritative lifecycle identity. A bare
        // runtime row must never acquire a process that reconcile/drain cannot
        // subsequently account for.
        let thread = g.state_db.get_thread(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("thread not found before process attach: {thread_id}")
        })?;
        let exact_repeat = !mode.requires_empty()
            && g.runtime_db
                .get_runtime_info(thread_id)?
                .is_some_and(|runtime| {
                    runtime.pid == Some(pid)
                        && runtime.pgid == Some(pgid)
                        && runtime.process_identity.as_ref() == Some(process_identity)
                });
        if !exact_repeat
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            anyhow::bail!("process attachment admission is closed for daemon shutdown");
        }
        // Defensive: skip attach if the thread was already finalized
        // (e.g. cancelled while the runner was between spawn and attach). An
        // exact identity repeat remains safe and idempotent: fast callback
        // runtimes can self-attach and finalize before their in-process owner
        // reaches the same attach call.
        if is_terminal_status(&thread.status) && !exact_repeat {
            tracing::warn!(
                thread_id,
                status = %thread.status,
                pid,
                pgid,
                "skipping attach_process — thread already terminal"
            );
            anyhow::bail!(
                "refusing to attach process {pid}/{pgid} to terminal thread {thread_id} ({})",
                thread.status
            );
        }
        if !exact_repeat
            && g.runtime_db
                .launch_window_is_cancelled(&thread.chain_root_id)?
        {
            anyhow::bail!(
                "refusing to attach process {pid}/{pgid} to cancelled launch-window member {thread_id}"
            );
        }
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        Self::authorize_thread_workspace_contact_locked(&g, thread_id)?;
        if mode.requires_empty() {
            if mode.rearms_resume_budget() {
                g.runtime_db.attach_new_process_rearming_resume_budget(
                    thread_id,
                    pid,
                    pgid,
                    process_identity,
                    launch_metadata,
                )
            } else {
                g.runtime_db.attach_new_process(
                    thread_id,
                    pid,
                    pgid,
                    process_identity,
                    launch_metadata,
                )
            }
        } else {
            g.runtime_db
                .attach_process(thread_id, pid, pgid, process_identity, launch_metadata)
        }?;
        self.attached_process_registry().insert(
            thread_id.to_string(),
            AttachedProcess {
                thread_id: thread_id.to_string(),
                process_identity: Some(process_identity.clone()),
                cancellation_mode: launch_metadata.cancellation_mode,
            },
        );
        Ok(())
    }

    /// Linearization point between durable process attachment and target
    /// release. A stop that wins this lock prevents release; a stop that lands
    /// afterward observes the exact attached identity and terminates it.
    pub fn authorize_attached_process_release(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        let g = self.lock()?;
        if let Some(expected) = expected_launch_owner {
            let claim = g
                .runtime_db
                .get_launch_claim(thread_id)?
                .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no launch owner"))?;
            if claim.claimed_by != expected {
                anyhow::bail!("stale launch owner cannot release process for {thread_id}");
            }
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("process release is fenced during daemon shutdown");
        }
        let thread = g.state_db.get_thread(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("thread not found before process release: {thread_id}")
        })?;
        if is_terminal_status(&thread.status) {
            anyhow::bail!(
                "refusing to release process for terminal thread {thread_id} ({})",
                thread.status
            );
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing before process release: {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            anyhow::bail!(
                "process release is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        if runtime.pid != Some(process_identity.target_pid)
            || runtime.pgid != Some(process_identity.group_leader_pid)
            || runtime.process_identity.as_ref() != Some(process_identity)
        {
            anyhow::bail!(
                "process release identity does not match durable attachment for thread {thread_id}"
            );
        }
        Self::authorize_thread_workspace_contact_locked(&g, thread_id)?;
        g.runtime_db
            .consume_process_release_fence(thread_id, process_identity)?;
        Ok(())
    }

    /// Close process attachment admission at the shutdown serialization point.
    /// The volatile owner registry and StateStore lock jointly serialize both
    /// subprocess attachment and in-process owner registration: every owner
    /// registered first is visible to the drain, and every later registration
    /// observes the closed gate.
    pub fn close_process_attachment_admission(&self) -> Result<()> {
        // Deny scheduler metadata before any fallible registry/Store lock.
        // A shutdown refusal cannot reopen this one-use startup boundary.
        if let Some(original) = &self.live_native_open {
            original.close_scheduler_admission();
        }
        let _active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let _g = self.lock()?;
        self.process_attachment_admission_open
            .store(false, Ordering::Release);
        self.process_attachment_admission_closed.notify_waiters();
        Ok(())
    }

    /// Whether this daemon instance still admits/finalizes live executions.
    /// Shutdown closes the gate before draining; process owners that wake from
    /// a shutdown-owned signal use this to preserve resumable rows instead of
    /// misclassifying the interruption as an execution failure.
    pub fn process_attachment_admission_is_open(&self) -> bool {
        self.process_attachment_admission_open
            .load(Ordering::Acquire)
    }

    /// Pushed shutdown fence for long-lived execution controllers. The atomic
    /// predicate closes the notify-before-subscribe race, so callers never
    /// need to poll SQLite merely to notice daemon shutdown.
    pub async fn wait_for_process_attachment_admission_close(&self) {
        loop {
            let notified = self.process_attachment_admission_closed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.process_attachment_admission_is_open() {
                return;
            }
            notified.await;
        }
    }

    /// Fail closed before a runtime callback authors or dispatches new work.
    /// Durable child-link/continuation mutations repeat this check or inherit
    /// the stop under the same store lock; this front-door check avoids doing
    /// expensive resolution after the authoring fence is already closed.
    pub fn ensure_running_runtime_mutation_allowed(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("runtime authoring is closed for daemon shutdown");
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing for callback thread {thread_id}")
        })?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback thread not found: {thread_id}"))?;
        if thread.status != ThreadStatus::Running.as_str() {
            anyhow::bail!(
                "runtime mutation requires a running thread; {thread_id} is {}",
                thread.status
            );
        }
        if let Some(intent) = runtime.stop_intent {
            anyhow::bail!(
                "runtime mutation is closed for stop-requested thread {thread_id} ({})",
                intent.as_str()
            );
        }
        Ok(())
    }

    /// Atomically admit a non-read runtime callback against authoritative
    /// lifecycle, stop, and shutdown state. This is the callback
    /// linearization point: a request admitted here may finish, while every
    /// request that arrives after terminal/stop/shutdown is refused.
    pub fn ensure_runtime_callback_mutation_allowed(
        &self,
        thread_id: &str,
        stop_completion: bool,
    ) -> Result<()> {
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback thread not found: {thread_id}"))?;
        if is_terminal_status(&thread.status) {
            anyhow::bail!(
                "runtime callback mutation is fenced for terminal thread {thread_id} ({})",
                thread.status
            );
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing for callback thread {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            if stop_completion {
                return Ok(());
            }
            anyhow::bail!(
                "runtime callback mutation is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("runtime callback mutation is fenced during daemon shutdown");
        }
        Ok(())
    }

    /// Atomically tombstone an explicit stop against process attachment and
    /// return the identity (if any) that won the attach race before the stop.
    pub fn request_thread_stop(
        &self,
        thread_id: &str,
        intent: runtime_db::StopIntent,
    ) -> Result<RuntimeInfo> {
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let g = self.lock()?;
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing before stop request: {thread_id}"))?;
        if let Some(refusal) = in_process_stop_refusal_locked(&active, &runtime, thread_id) {
            bail!("{}", refusal.message(thread_id));
        }
        g.runtime_db.request_thread_stop(thread_id, intent)
    }

    /// Request a stop only if ordinary execution ownership is still open.
    ///
    /// The gate check and tombstone share the StateStore lock with shutdown's
    /// gate close. Cancellation cleanup therefore either wins before shutdown
    /// and owns the durable stop, or observes the closed gate and leaves the row
    /// untouched for the shutdown coordinator.
    pub fn request_thread_stop_if_admission_open(
        &self,
        thread_id: &str,
        intent: runtime_db::StopIntent,
    ) -> Result<StopIfAdmissionOpenOutcome> {
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found before owner-drop stop: {thread_id}"))?;
        if is_terminal_status(&thread.status) {
            return Ok(StopIfAdmissionOpenOutcome::AlreadyTerminal);
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing before owner-drop stop: {thread_id}"))?;
        if let Some(refusal) = in_process_stop_refusal_locked(&active, &runtime, thread_id) {
            bail!("{}", refusal.message(thread_id));
        }
        // A durable follow waiter transfers lifecycle ownership to the daemon.
        // Keep this check under the same store lock as the stop tombstone so a
        // request drop cannot race waiter reservation and kill the new chain.
        if g.runtime_db
            .get_follow_waiter_by_parent_thread(thread_id)?
            .is_some()
            || g.runtime_db
                .get_follow_waiter_by_successor(thread_id)?
                .is_some()
        {
            return Ok(StopIfAdmissionOpenOutcome::PreservedForFollow);
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            return Ok(StopIfAdmissionOpenOutcome::PreservedForShutdown);
        }
        g.runtime_db
            .request_thread_stop(thread_id, intent)
            .map(|runtime| StopIfAdmissionOpenOutcome::Requested(Box::new(runtime)))
    }

    pub fn clear_thread_process_if_matches(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<bool> {
        let g = self.lock()?;
        let cleared = g
            .runtime_db
            .clear_process_if_matches(thread_id, process_identity)?;
        if cleared {
            self.attached_process_registry().remove(thread_id);
        }
        Ok(cleared)
    }

    pub fn clear_thread_process_if_matches_owned(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_owner: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot detach process from {thread_id}");
        }
        let settlement = runtime_db::ThreadProcessSettlement::new(
            thread_id,
            &claim.owner,
            process_identity,
            None,
            if process_identity.process_scope.is_some() {
                runtime_db::ThreadProcessSettlementKind::ReapedScopeEmpty
            } else {
                runtime_db::ThreadProcessSettlementKind::ReapedGroupAbsent
            },
        )?;
        let cleared = g.runtime_db.clear_process_if_matches_with_settlement(
            thread_id,
            process_identity,
            Some(&settlement),
        )?;
        if cleared {
            self.attached_process_registry().remove(thread_id);
        }
        Ok(cleared)
    }

    /// Record that recovery holds this exact attached identity in
    /// quarantine. False when the row no longer carries the identity.
    pub fn record_process_quarantine(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        reason: runtime_db::ProcessQuarantineReason,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.record_process_quarantine(
            thread_id,
            process_identity,
            reason,
            lillux::time::timestamp_millis(),
        )
    }

    /// Every thread whose attached identity recovery holds in quarantine.
    pub fn list_process_quarantines(&self) -> Result<Vec<runtime_db::QuarantinedProcess>> {
        let g = self.lock()?;
        g.runtime_db.list_process_quarantines()
    }

    /// Read the exact operational settlement retained after compare-clear.
    /// Product qualification must also bind it to a signed terminal event and
    /// independently recheck Lillux scope emptiness before using it.
    pub fn latest_thread_process_settlement(
        &self,
        thread_id: &str,
    ) -> Result<Option<runtime_db::ThreadProcessSettlement>> {
        let g = self.lock()?;
        g.runtime_db.latest_thread_process_settlement(thread_id)
    }

    pub fn list_attached_thread_ids(&self) -> Result<Vec<String>> {
        let attached = self.attached_process_registry();
        Ok(attached.keys().cloned().collect())
    }

    /// Prove under the same owner-fenced StateStore lock that the supervised
    /// process identity has been compare-cleared. Post-exit workspace capture
    /// must not infer this boundary from a PID probe or an earlier wait result.
    pub fn assert_execution_process_detached_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        if g.runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|runtime| runtime.process_identity)
            .is_some()
        {
            bail!("thread {thread_id} still has an attached process identity");
        }
        Ok(())
    }
}

//! Commands and launch windows.
use super::*;

impl StateStore {
    pub fn submit_command(&self, cmd: &NewCommandRecord) -> Result<CommandRecord> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, &cmd.thread_id)?;
        g.runtime_db.submit_command(cmd)
    }

    pub fn claim_commands(
        &self,
        thread_id: &str,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .claim_commands(thread_id, limit, max_serialized_bytes)
    }

    pub fn reset_resume_attempts(&self, thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.reset_resume_attempts(thread_id)
    }

    /// Enqueue a detached child chain into a launch window and admit as many
    /// queued members as the window width (and optional global live ceiling)
    /// allow. Returns the chain roots admitted NOW — the caller launches
    /// them; the enqueued child is queued iff its id is absent.
    pub fn launch_window_enqueue(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = g.state_db.authorize_runtime_pin(child_chain_root_id)?;
        g.runtime_db
            .launch_window_insert(child_chain_root_id, window_key, width, now_ms)?;
        g.runtime_db
            .launch_window_admit_global(global_live_limit, now_ms)
    }

    /// Repair membership without admitting it; used when launch metadata proves
    /// the child was originally windowed but the membership write was lost.
    pub fn launch_window_insert_only(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        now_ms: i64,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = g.state_db.authorize_runtime_pin(child_chain_root_id)?;
        g.runtime_db
            .launch_window_insert(child_chain_root_id, window_key, width, now_ms)
    }

    /// Release a window slot for a chain that reached a hard terminal and
    /// admit the window's next queued members (returned for launching).
    pub fn launch_window_release(
        &self,
        child_chain_root_id: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_release(child_chain_root_id, global_live_limit, now_ms)
    }

    pub fn launch_window_is_queued(&self, child_chain_root_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.launch_window_is_queued(child_chain_root_id)
    }

    pub fn launch_window_cancel_queued(
        &self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let mut g = self.lock()?;
        g.runtime_db
            .launch_window_cancel_queued(chain_roots, now_ms)
    }

    pub fn launch_window_cancel_members(
        &self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .launch_window_cancel_members(chain_roots, now_ms)
    }

    pub fn launch_window_is_cancelled(&self, chain_root: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .launch_window_is_cancelled(chain_root)
    }

    pub fn list_cancelled_window_members(&self) -> Result<Vec<String>> {
        self.lock()?.runtime_db.launch_window_cancelled_members()
    }

    pub fn discard_window_member(&self, chain_root: &str) -> Result<()> {
        self.lock()?
            .runtime_db
            .launch_window_discard_member(chain_root)
    }

    pub fn launch_window_is_member(&self, child_chain_root_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.launch_window_is_member(child_chain_root_id)
    }

    pub fn launch_window_launched_members(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.launch_window_launched_members()
    }

    pub fn launch_window_keys_with_queue(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.launch_window_keys_with_queue()
    }

    pub fn launch_window_admit_global(
        &self,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_admit_global(global_live_limit, now_ms)
    }

    pub fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Option<&Value>,
    ) -> Result<CommandRecord> {
        let g = self.lock()?;
        g.runtime_db.complete_command(command_id, status, result)
    }

    /// Read one command by id, or `None` if it does not exist.
    pub fn get_command(&self, command_id: i64) -> Result<Option<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.get_command(command_id)
    }

    /// Whether a `kill` command was ever submitted for `thread_id` (the launcher's
    /// kill-intent marker). See `RuntimeDb::thread_has_kill_command`.
    pub fn thread_has_kill_command(&self, thread_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.thread_has_kill_command(thread_id)
    }

    /// Settle every still-open command for a finalized thread (fulfilled →
    /// `completed`, else `rejected`), returning the affected records so waiters
    /// can be woken. See `RuntimeDb::settle_open_commands`.
    pub fn settle_open_commands(
        &self,
        thread_id: &str,
        terminal_status: &str,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .settle_open_commands(thread_id, terminal_status)
    }

    /// Record that `parent_thread_id` spawned `child_thread_id` (operational
    /// lineage for cancel/kill cascade). An exact replay is idempotent;
    /// conflicting parent or relation authority is rejected. If the parent was
    /// already stop-tombstoned, atomically inherit that monotonic intent on the
    /// child before releasing the store lock, closing the late-link race. An
    /// in-process chain member is linked, and inherits its parent's stop, by
    /// its own birth; for it this is the exact replay of that link, which
    /// inherits nothing more: a parent stop landed since the birth reaches the
    /// member only through [`Self::link_unspawned_in_process_child`].
    pub fn record_child_link(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        relation: &str,
    ) -> Result<Option<StopIntent>> {
        let _permit = self.acquire_write_permit()?;
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        let g = self.lock()?;
        if let Some(chain_root_id) = in_process_chain_member_root_locked(&g, child_thread_id)? {
            return replay_in_process_chain_member_link_locked(
                &g,
                &chain_root_id,
                parent_thread_id,
                child_thread_id,
                relation,
                false,
            );
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            let runtime = g
                .runtime_db
                .get_runtime_info(child_thread_id)?
                .ok_or_else(|| {
                    anyhow!("child runtime row missing before shutdown stop: {child_thread_id}")
                })?;
            if let Some(refusal) =
                in_process_stop_refusal_locked(&active, &runtime, child_thread_id)
            {
                bail!("{}", refusal.message(child_thread_id));
            }
            // The shutdown gate may close after a callback created its child but
            // before it reached this second durable mutation. Tombstone the child
            // first so no later attach can win, then preserve operational lineage
            // on a best-effort basis.
            let stopped = g
                .runtime_db
                .request_thread_stop(child_thread_id, StopIntent::Cancel)?;
            if let Err(error) =
                g.runtime_db
                    .record_child_link(parent_thread_id, child_thread_id, relation)
            {
                tracing::warn!(
                    parent_thread_id,
                    child_thread_id,
                    relation,
                    error = %error,
                    "failed to preserve child lineage after shutdown tombstone"
                );
            }
            return Ok(Some(stopped.stop_intent.unwrap_or(StopIntent::Cancel)));
        }
        let parent = g
            .state_db
            .get_thread(parent_thread_id)?
            .ok_or_else(|| anyhow!("parent thread {parent_thread_id} does not exist"))?;
        let child = g
            .state_db
            .get_thread(child_thread_id)?
            .ok_or_else(|| anyhow!("child thread {child_thread_id} does not exist"))?;
        let mut chain_roots = vec![parent.chain_root_id, child.chain_root_id];
        chain_roots.sort();
        chain_roots.dedup();
        let mut _admissions = Vec::with_capacity(chain_roots.len());
        for chain_root_id in chain_roots {
            _admissions.push(g.state_db.authorize_runtime_pin(&chain_root_id)?);
        }
        let inherited_stop = g
            .runtime_db
            .get_runtime_info(parent_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("parent runtime row missing: {parent_thread_id}"))?
            .stop_intent;
        // A dispatch admitted immediately before its parent finalized may reach
        // this linkage point afterward. Preserve the lineage and tombstone the
        // newly-created child in the same runtime-DB transaction. An exact
        // replay is not a late child and must not synthesize a new stop merely
        // because the parent has since continued. Follow children are linked
        // before the generic launch path reaches this point, so its later
        // `dispatch` registration must replay the authoritative `follow`
        // relation rather than contradict it. Continuations are different:
        // their predecessor is intentionally terminal/continued.
        let existing_relation = g.runtime_db.child_link_relation(child_thread_id)?;
        let replays_follow_relation =
            relation == "dispatch" && existing_relation.as_deref() == Some("follow");
        let effective_relation = if replays_follow_relation {
            "follow"
        } else {
            relation
        };
        let stop_policy = if let Some(intent) = inherited_stop {
            runtime_db::ChildLinkStopPolicy::Always(intent)
        } else if relation == "dispatch"
            && !replays_follow_relation
            && is_terminal_status(&parent.status)
        {
            runtime_db::ChildLinkStopPolicy::IfInserted(StopIntent::Cancel)
        } else {
            runtime_db::ChildLinkStopPolicy::None
        };
        if stop_policy != runtime_db::ChildLinkStopPolicy::None {
            let runtime = g
                .runtime_db
                .get_runtime_info(child_thread_id)?
                .ok_or_else(|| {
                    anyhow!("child runtime row missing before inherited stop: {child_thread_id}")
                })?;
            if let Some(refusal) =
                in_process_stop_refusal_locked(&active, &runtime, child_thread_id)
            {
                bail!("{}", refusal.message(child_thread_id));
            }
        }
        let (_, effective_stop) = g.runtime_db.record_child_link_with_stop_policy(
            parent_thread_id,
            child_thread_id,
            effective_relation,
            stop_policy,
        )?;
        Ok(effective_stop)
    }
}

//! Terminal and fenced chain retirement.
use super::*;

impl StateStore {
    pub(super) fn inspect_terminal_chain_pins<F>(
        g: &Inner,
        chain: &ryeos_state::AuthoritativeTerminalChain,
        pin_count: &F,
    ) -> Result<runtime_db::ChainRecoveryPins>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let mut pins = g
            .runtime_db
            .inspect_chain_recovery_pins(&chain.chain_root_id, &chain.thread_ids)?;
        pins.candidate_evidence = g
            .runtime_db
            .candidate_evidence_pin_count(&chain.chain_root_id)?;
        let members = chain
            .thread_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        for (parent_thread_id, child_thread_id) in
            g.runtime_db.chain_child_links(&chain.thread_ids)?
        {
            let parent_is_member = members.contains(parent_thread_id.as_str());
            let child_is_member = members.contains(child_thread_id.as_str());
            if parent_is_member && child_is_member {
                continue;
            }
            let counterpart = if parent_is_member {
                &child_thread_id
            } else {
                &parent_thread_id
            };
            // A missing counterpart while its operational edge remains is an
            // inconsistent read, not proof of safety. Fail closed by retaining
            // the chain. A terminal counterpart no longer needs cancellation
            // cascade state and therefore does not pin this history.
            if g.state_db
                .get_thread(counterpart)?
                .is_none_or(|thread| !is_terminal_status(&thread.status))
            {
                pins.child_links = pins
                    .child_links
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("child-link recovery pin count overflow"))?;
            }
        }
        pins.scheduler_fires = pin_count(&chain.thread_ids)?;
        Ok(pins)
    }

    pub(super) fn finish_terminal_chain_removal(
        &self,
        g: &StateStoreGuard<'_>,
        chain: &ryeos_state::AuthoritativeTerminalChain,
        chain_lock: &ChainLock,
        runtime_paths: &[ThreadRuntimeRemoval],
        head_already_absent: bool,
        result: &mut TerminalChainRetirement,
    ) -> Result<()> {
        let selected_permit = self.selected_mutation_permit(
            g,
            SelectedMutationOperation::DeleteChain(&chain.chain_root_id, &chain.thread_ids),
        )?;
        if !head_already_absent
            && !g
                .state_db
                .remove_chain_head_ref(&chain.chain_root_id, chain_lock)?
        {
            bail!(
                "authoritative chain head disappeared during retirement: {}",
                chain.chain_root_id
            );
        }
        result.deleted_runtime_rows += g.runtime_db.delete_chain_runtime(
            &selected_permit,
            &chain.chain_root_id,
            &chain.thread_ids,
        )?;
        self.refresh_attached_process_registry_locked(g)?;
        result.deleted_runtime_files += delete_thread_runtime_files(runtime_paths)?;
        result.deleted_projection_rows += g
            .state_db
            .delete_chain_projection(&chain.chain_root_id, chain_lock)?;
        if !g
            .state_db
            .acknowledge_chain_removal_cleanup(&chain.chain_root_id, chain_lock)?
        {
            bail!(
                "pending chain removal disappeared before acknowledgement: {}",
                chain.chain_root_id
            );
        }
        Ok(())
    }

    pub(super) fn recover_pending_terminal_chain_removals_with<F>(
        &self,
        now: &str,
        dry_run: bool,
        pin_count: &F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let mut result = TerminalChainRetirement::default();
        let mut pending_removals = {
            let g = self.lock()?;
            g.state_db.pending_remove_cursor()?
        };
        while let Some(observed) = pending_removals.next_transition()? {
            let chain_root_id = observed.chain_root_id.clone();
            let _permit = self.acquire_recovery_cleanup_permit(dry_run)?;
            let g = self.lock()?;
            let chain_lock = if dry_run {
                g.state_db.acquire_existing_chain_lock(&chain_root_id)?
            } else {
                g.state_db.acquire_chain_lock(&chain_root_id)?
            };
            let still_pending = g
                .state_db
                .pending_chain_transition(&chain_root_id)?
                .is_some_and(|pending| {
                    pending.transition_id == observed.transition_id
                        && pending.operation == ryeos_state::HeadOperation::Remove
                });
            if !still_pending {
                continue;
            }
            // Includes HeadAbsent contradictions: retain SAME Remove and all
            // cleanup owners rather than cancel/repair/acknowledge through them.
            self.require_selected_bulk_exclusion(&g)?;

            match g.state_db.pending_chain_removal_head_state(
                &chain_root_id,
                &observed.transition_id,
                &chain_lock,
            )? {
                ryeos_state::PendingRemoveHeadState::HeadAbsent => {
                    let chain = g
                        .state_db
                        .pending_removed_terminal_chain_under_lock(&chain_root_id, &chain_lock)?
                        .ok_or_else(|| {
                            anyhow!(
                                "pending Remove for {chain_root_id} names a nonterminal or invalid closure"
                            )
                        })?;
                    // The absent trusted head proves that this Remove crossed
                    // its authoritative deletion boundary. Operational rows,
                    // scheduler fires, follow state, and runtime files left by
                    // a crash at that boundary are cleanup targets, not fresh
                    // vetoes: new pin admission is already forbidden once the
                    // Remove publishes. Rechecking them here would strand the
                    // exact partial cleanup this replay owns forever.
                    let runtime_paths = inspect_thread_runtime_files(
                        self.thread_runtime_authority()?,
                        &chain.thread_ids,
                    )?;
                    result.pending_retirements_recovered += 1;
                    if !dry_run {
                        self.finish_terminal_chain_removal(
                            &g,
                            &chain,
                            &chain_lock,
                            &runtime_paths,
                            true,
                            &mut result,
                        )?;
                    }
                    continue;
                }
                ryeos_state::PendingRemoveHeadState::AdvancedHeadVisible { current_head_hash } => {
                    result.pending_retirements_recovered += 1;
                    tracing::warn!(
                        chain_root_id,
                        current_head_hash,
                        "pending terminal-history Remove is stale against an advanced signed head; repairing the current head"
                    );
                    if dry_run {
                        g.state_db.verify_advanced_head_after_stale_chain_removal(
                            &chain_root_id,
                            &observed.transition_id,
                            &current_head_hash,
                            &chain_lock,
                        )?;
                    } else {
                        g.state_db.repair_advanced_head_after_stale_chain_removal(
                            &chain_root_id,
                            &observed.transition_id,
                            &chain_lock,
                        )?;
                    }
                    continue;
                }
                ryeos_state::PendingRemoveHeadState::ExpectedHeadVisible => {}
            }

            let Some(chain) = g
                .state_db
                .authoritative_terminal_chain_under_lock(&chain_root_id, &chain_lock)?
            else {
                result.pending_retirements_recovered += 1;
                if !dry_run {
                    g.state_db
                        .cancel_pending_chain_removal(&chain_root_id, &chain_lock)?;
                }
                continue;
            };
            let due = chain.is_due_at(now)?;
            let pins = Self::inspect_terminal_chain_pins(&g, &chain, pin_count)?;
            if !due || !pins.is_empty() {
                result.pending_retirements_recovered += 1;
                if !dry_run {
                    g.state_db
                        .cancel_pending_chain_removal(&chain_root_id, &chain_lock)?;
                }
                continue;
            }
            let runtime_paths =
                inspect_thread_runtime_files(self.thread_runtime_authority()?, &chain.thread_ids)?;
            result.pending_retirements_recovered += 1;
            result.retired_chains += 1;
            if !dry_run {
                self.finish_terminal_chain_removal(
                    &g,
                    &chain,
                    &chain_lock,
                    &runtime_paths,
                    false,
                    &mut result,
                )?;
            }
        }
        Ok(result)
    }

    /// Complete or cancel only the bounded durable Remove records left by an
    /// interrupted retention transaction. Startup calls this before readiness;
    /// it never scans current chain heads or selects new GC candidates.
    pub fn recover_pending_terminal_chain_removals<F>(
        &self,
        now: &str,
        dry_run: bool,
        pin_count: F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let result = self.recover_pending_terminal_chain_removals_with(now, dry_run, &pin_count);
        match self
            .lock()
            .and_then(|g| Ok(g.state_db.pending_chain_transitions()?.len()))
        {
            Ok(pending) => self.projection_health.observe_pending_transitions(pending),
            Err(error) => self
                .projection_health
                .observe_pending_transition_error(&error),
        }
        result
    }

    /// Retire terminal chain history solely from the signed policy captured on
    /// its root. Candidate rows are a bounded read-model optimization; every
    /// destructive decision is repeated from the trust-verified CAS closure
    /// under the global guard/permit/mutex/chain-lock hierarchy.
    pub fn retire_due_terminal_chains<F>(
        &self,
        now: &str,
        dry_run: bool,
        pin_count: F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        const CANDIDATE_BATCH: usize = 128;
        let mut result =
            self.recover_pending_terminal_chain_removals_with(now, dry_run, &pin_count)?;
        let mut cursor: Option<ryeos_state::DueTerminalChainCursor> = None;
        loop {
            let candidates = {
                let g = self.lock()?;
                self.require_selected_bulk_exclusion(&g)?;
                g.state_db
                    .list_due_terminal_chains(now, CANDIDATE_BATCH, cursor.as_ref())?
            };
            if candidates.is_empty() {
                break;
            }
            result.candidate_chains += candidates.len();
            cursor = candidates
                .last()
                .map(|candidate| ryeos_state::DueTerminalChainCursor {
                    retire_after: candidate.retire_after,
                    chain_root_id: candidate.chain_root_id.clone(),
                });

            for candidate in &candidates {
                let _permit = if dry_run {
                    self.acquire_gc_inspection_permit()?
                } else {
                    self.acquire_write_permit()?
                };
                let g = self.lock()?;
                self.require_selected_bulk_exclusion(&g)?;
                let chain_lock = if dry_run {
                    g.state_db
                        .acquire_existing_chain_lock(&candidate.chain_root_id)?
                } else {
                    g.state_db.acquire_chain_lock(&candidate.chain_root_id)?
                };
                // Any unresolved transition owns the chain's publication
                // slot. In particular, a Prepared Set may still name a newer
                // closure even while the old head/projection match this stale
                // candidate row; retirement must never try to converge or
                // overwrite that publication intent.
                if g.state_db
                    .pending_chain_transition(&candidate.chain_root_id)?
                    .is_some()
                {
                    continue;
                }
                if let Some(fenced) = self.fenced_chain_kinds.get() {
                    let root = g
                        .state_db
                        .get_thread(&candidate.chain_root_id)?
                        .ok_or_else(|| {
                            anyhow!(
                                "due chain {} has no root thread row",
                                candidate.chain_root_id
                            )
                        })?;
                    if fenced.contains(&root.kind) {
                        continue;
                    }
                }
                let Some(chain) = g.state_db.authoritative_terminal_chain_under_lock(
                    &candidate.chain_root_id,
                    &chain_lock,
                )?
                else {
                    continue;
                };
                if chain.head_hash != candidate.indexed_chain_state_hash
                    || chain.terminal_at != candidate.terminal_at
                    || !chain.is_due_at(now)?
                {
                    continue;
                }
                let pins = Self::inspect_terminal_chain_pins(&g, &chain, &pin_count)?;
                if !pins.is_empty() {
                    continue;
                }
                let runtime_paths = inspect_thread_runtime_files(
                    self.thread_runtime_authority()?,
                    &chain.thread_ids,
                )?;
                result.retired_chains += 1;
                if !dry_run {
                    self.finish_terminal_chain_removal(
                        &g,
                        &chain,
                        &chain_lock,
                        &runtime_paths,
                        false,
                        &mut result,
                    )?;
                }
            }
            if candidates.len() < CANDIDATE_BATCH {
                break;
            }
        }
        Ok(result)
    }

    /// Install the kinds that retire through a fence. Once per Store: the
    /// same set again is accepted, a different one is refused.
    pub fn install_fenced_chain_kinds(&self, kinds: &ryeos_state::FencedChainKinds) -> Result<()> {
        let installed = self.fenced_chain_kinds.get_or_init(|| kinds.clone());
        anyhow::ensure!(
            installed == kinds,
            "fenced chain kinds were already installed with a different set"
        );
        Ok(())
    }

    /// The installed fenced chain kinds.
    pub fn fenced_chain_kinds(&self) -> Result<&ryeos_state::FencedChainKinds> {
        self.fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))
    }

    /// One bounded keyset slice of fenced-kind chains whose retention has
    /// elapsed. A scheduling hint only: `retire_fenced_chain` requalifies.
    pub fn due_fenced_chain_retirement_slice(
        &self,
        now: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        let kinds = self
            .fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))?;
        let g = self.lock()?;
        self.require_selected_bulk_exclusion(&g)?;
        g.state_db
            .projection()
            .due_fenced_chain_retirement_slice(kinds, now, after, limit)
    }

    /// Finish the head half of every interrupted fenced retirement: a fence
    /// whose chain still has a head gets its Remove prepared and the head
    /// unlinked. The caller holds the exclusive retention exclusion.
    pub fn replay_fenced_chain_retirements(
        &self,
    ) -> Result<ryeos_state::state_db::FencedRetirementReplay> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        self.require_selected_bulk_exclusion(&g)?;
        g.state_db
            .replay_fenced_chain_retirements(permit.cas_guard())
    }

    /// Retire one fenced-kind chain to a retained closure, or finish an
    /// interrupted retirement of it. The caller decides which counterparts and
    /// evidence events the closure keeps; State derives every coordinate, and
    /// the composition's installed validator judges each retained event. The
    /// caller holds the retention exclusion whose pin count it passes.
    pub fn retire_fenced_chain<F>(
        &self,
        chain_root_id: &str,
        now: &str,
        plan: &FencedChainRetirementPlan,
        pin_count: F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let kinds = self
            .fenced_chain_kinds
            .get()
            .ok_or_else(|| anyhow!("fenced chain kinds are not installed"))?;
        let validator = crate::object_contracts::retained_evidence_validator()?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        self.require_selected_bulk_exclusion(&g)?;
        let root = g
            .state_db
            .get_thread(chain_root_id)?
            .ok_or_else(|| anyhow!("chain {chain_root_id} has no root thread row"))?;
        anyhow::ensure!(
            kinds.contains(&root.kind),
            "chain {chain_root_id} is not of a fenced kind"
        );
        // Owner and evidence counterparts are locked together in sorted
        // order, the one order every multi-chain path uses.
        let mut lock_ids: BTreeSet<&str> = plan
            .evidence
            .iter()
            .map(|reference| reference.counterpart_root.as_str())
            .filter(|counterpart| *counterpart != chain_root_id)
            .collect();
        anyhow::ensure!(
            !plan.counterpart_roots.contains(chain_root_id),
            "a retired chain cannot be its own counterpart"
        );
        lock_ids.insert(chain_root_id);
        let mut owner_lock = None;
        let mut counterpart_locks = Vec::new();
        for id in lock_ids {
            let lock = g.state_db.acquire_chain_lock(id)?;
            if id == chain_root_id {
                owner_lock = Some(lock);
            } else {
                counterpart_locks.push(lock);
            }
        }
        let owner_lock = owner_lock.ok_or_else(|| anyhow!("owner chain lock was not acquired"))?;
        let counterpart_refs: Vec<&ChainLock> = counterpart_locks.iter().collect();
        let chain = match g
            .state_db
            .authoritative_terminal_chain_under_lock(chain_root_id, &owner_lock)?
        {
            Some(chain) => {
                anyhow::ensure!(
                    chain.is_due_at(now)?,
                    "chain {chain_root_id} is not due for retirement"
                );
                let pins = Self::inspect_terminal_chain_pins(&g, &chain, &pin_count)?;
                anyhow::ensure!(
                    pins.is_empty(),
                    "chain {chain_root_id} is still pinned: {pins:?}"
                );
                chain
            }
            None => g
                .state_db
                .pending_removed_terminal_chain_under_lock(chain_root_id, &owner_lock)?
                .ok_or_else(|| {
                    anyhow!("chain {chain_root_id} is neither live nor a pending Remove")
                })?,
        };
        let runtime_paths =
            inspect_thread_runtime_files(self.thread_runtime_authority()?, &chain.thread_ids)?;
        let deleted_rows = std::cell::Cell::new(0usize);
        let deleted_files = std::cell::Cell::new(0usize);
        let cleanup = || -> Result<()> {
            let selected_permit = self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::DeleteChain(&chain.chain_root_id, &chain.thread_ids),
            )?;
            deleted_rows.set(
                deleted_rows.get()
                    + g.runtime_db.delete_chain_runtime(
                        &selected_permit,
                        &chain.chain_root_id,
                        &chain.thread_ids,
                    )?,
            );
            self.refresh_attached_process_registry_locked(&g)?;
            deleted_files.set(deleted_files.get() + delete_thread_runtime_files(&runtime_paths)?);
            Ok(())
        };
        let report = g.state_db.retire_chain(
            chain_root_id,
            &owner_lock,
            permit.cas_guard(),
            ryeos_state::state_db::ChainRetirement {
                counterpart_roots: plan.counterpart_roots.clone(),
                evidence: plan.evidence.clone(),
                counterpart_locks: &counterpart_refs,
                validator: &validator,
                runtime_cleanup: &cleanup,
            },
        )?;
        tracing::info!(
            chain_root_id,
            manifest_hash = report.manifest_hash,
            stored_manifest = report.stored_manifest,
            "fenced chain retired"
        );
        Ok(TerminalChainRetirement {
            candidate_chains: 1,
            retired_chains: 1,
            deleted_projection_rows: 0,
            deleted_runtime_rows: deleted_rows.get(),
            deleted_runtime_files: deleted_files.get(),
            pending_retirements_recovered: usize::from(!report.stored_manifest),
        })
    }
}

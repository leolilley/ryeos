//! Credential profile generations, enrollment and revocation.
use super::*;

pub(super) fn operational_credential_profile(
    profile: &runtime_db::CredentialProfileRecord,
) -> ryeos_state::OperationalCredentialProfileRecord {
    ryeos_state::OperationalCredentialProfileRecord {
        profile_id: profile.profile_id.clone(),
        owner_principal: profile.owner_principal.clone(),
        home_id: profile.home_id.clone(),
        authority_revision: profile.authority_revision,
        credential_generation: profile.credential_generation,
        state: profile.state.clone(),
        active_login_id: profile.active_login_id.clone(),
        login_epoch: profile.login_epoch,
        login_expires_at_ms: profile.login_expires_at_ms,
        sanitized_account: profile.sanitized_account.clone(),
        created_at_ms: profile.created_at_ms,
        updated_at_ms: profile.updated_at_ms,
    }
}

pub(super) fn reconcile_credential_profile_authority(
    state_db: &StateDb,
    runtime_db: &runtime_db::RuntimeDb,
) -> Result<()> {
    // StateStore construction owns both pinned database namespaces exclusively.
    // Re-read the complete stable set before changing RuntimeDb so the repair
    // below never acts on a cross-database observation that changed in between.
    let stable_profiles = state_db.operational_credential_profiles()?;
    if state_db.operational_credential_profiles()? != stable_profiles {
        anyhow::bail!("stable credential-profile authority changed during reconciliation");
    }
    let stable_by_id = stable_profiles
        .iter()
        .map(|profile| (profile.profile_id.as_str(), profile))
        .collect::<BTreeMap<_, _>>();
    let runtime_profiles = runtime_db.credential_profile_projections()?;
    let mut seen = BTreeSet::new();
    for mut runtime_profile in runtime_profiles {
        seen.insert(runtime_profile.profile_id.clone());
        if let Some(stable) = stable_by_id.get(runtime_profile.profile_id.as_str())
            && runtime_db.reconcile_terminal_credential_profile_authority(stable)?
        {
            runtime_profile = runtime_db
                .credential_profile(&runtime_profile.profile_id)?
                .ok_or_else(|| {
                    anyhow!("reconciled terminal credential profile projection disappeared")
                })?;
        }
        let stable = state_db.merge_operational_credential_profile(
            &operational_credential_profile(&runtime_profile),
        )?;
        runtime_db.reconcile_credential_profile_projection(&stable)?;
    }
    for stable in stable_profiles {
        if seen.insert(stable.profile_id.clone()) {
            runtime_db.reconcile_credential_profile_projection(&stable)?;
        }
    }
    Ok(())
}

pub(super) fn persist_credential_profile_authority_locked(
    inner: &Inner,
    profile_id: &str,
) -> Result<()> {
    let profile = inner
        .runtime_db
        .credential_profile_projections()?
        .into_iter()
        .find(|profile| profile.profile_id == profile_id)
        .ok_or_else(|| anyhow!("credential profile projection disappeared before persistence"))?;
    let stable = inner
        .state_db
        .merge_operational_credential_profile(&operational_credential_profile(&profile))?;
    if stable.authority_revision != profile.authority_revision {
        anyhow::bail!(
            "stable credential authority advanced beyond the committed runtime transition"
        );
    }
    Ok(())
}

impl StateStore {
    pub fn reserve_credential_profile_generation(
        &self,
        reservation: NewCredentialProfileReservation<'_>,
    ) -> Result<CredentialProfileReservationRecord> {
        let g = self.lock()?;
        let current = g
            .state_db
            .read_project_head(reservation.project_principal_key, reservation.project_hash)?;
        if current.as_deref() != Some(reservation.target_project_head_hash) {
            bail!("target project HEAD changed before handoff reservation");
        }
        g.runtime_db
            .reserve_credential_profile_generation(reservation)
    }

    pub fn credential_profile_reservation_for_successor(
        &self,
        successor_thread_id: &str,
    ) -> Result<Option<CredentialProfileReservationRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .credential_profile_reservation_for_successor(successor_thread_id)
    }

    /// Read the typed remote-continuation authority that admitted one exact
    /// successor. This follows the authoritative braid, never an operational
    /// job result or caller-provided checkpoint hash.
    pub fn remote_continuation_authority(
        &self,
        chain_root_id: &str,
        successor_thread_id: &str,
    ) -> Result<Option<ryeos_state::objects::RemoteContinuationAuthority>> {
        let g = self.lock()?;
        let successor = g
            .state_db
            .read_authoritative_thread_snapshot(chain_root_id, successor_thread_id)?
            .ok_or_else(|| anyhow!("remote continuation successor is absent"))?;
        let Some(source_thread_id) = successor.upstream_thread_id.as_deref() else {
            return Ok(None);
        };
        let source = g
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(chain_root_id, source_thread_id)?
            .ok_or_else(|| anyhow!("remote continuation source is absent"))?;
        let Some((_hash, event)) = source.last_event else {
            return Ok(None);
        };
        if event.event_type != "thread_continued" {
            return Ok(None);
        }
        let Some(value) = event.payload.get("remote_adoption") else {
            return Ok(None);
        };
        let authority: ryeos_state::objects::RemoteContinuationAuthority =
            serde_json::from_value(value.clone())
                .context("decode authoritative remote continuation edge")?;
        authority.validate()?;
        if authority.successor_thread_id != successor_thread_id
            || event
                .payload
                .get("successor_thread_id")
                .and_then(Value::as_str)
                != Some(successor_thread_id)
        {
            bail!("remote continuation authority contradicts its successor edge");
        }
        Ok(Some(authority))
    }

    pub fn release_credential_profile_reservation(&self, reservation_id: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .release_credential_profile_reservation(reservation_id)
    }

    pub fn dedicated_sessions_for_credential_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<DedicatedSessionRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .dedicated_sessions_for_credential_profile(profile_id)
    }

    pub fn reconcile_unattached_credential_profile_locks(&self) -> Result<(usize, usize)> {
        let g = self.lock()?;
        g.runtime_db.reconcile_unattached_credential_profile_locks()
    }

    pub fn create_credential_profile(&self, profile: NewCredentialProfile<'_>) -> Result<()> {
        let g = self.lock()?;
        let profile_id = profile.profile_id.to_owned();
        g.runtime_db.create_credential_profile(profile)?;
        persist_credential_profile_authority_locked(&g, &profile_id)
    }

    pub fn credential_profile(&self, profile_id: &str) -> Result<Option<CredentialProfileRecord>> {
        let g = self.lock()?;
        g.runtime_db.credential_profile(profile_id)
    }

    pub fn list_credential_profiles_for_owner(
        &self,
        owner_principal: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<runtime_db::CredentialProfilePage> {
        self.lock()?
            .runtime_db
            .list_credential_profiles_for_owner(owner_principal, after, limit)
    }

    pub fn acquire_credential_profile(
        &self,
        profile_id: &str,
        owner_principal: &str,
        lock_owner: &str,
    ) -> Result<u64> {
        let g = self.lock()?;
        g.runtime_db
            .acquire_credential_profile(profile_id, owner_principal, lock_owner)
    }

    pub fn release_credential_profile(&self, profile_id: &str, lock_owner: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .release_credential_profile(profile_id, lock_owner)
    }

    pub fn begin_credential_enrollment(
        &self,
        profile_id: &str,
        lock_owner: &str,
        login_id: &str,
        expires_at_ms: i64,
    ) -> Result<u64> {
        let g = self.lock()?;
        let epoch = g.runtime_db.begin_credential_enrollment(
            profile_id,
            lock_owner,
            login_id,
            expires_at_ms,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)?;
        Ok(epoch)
    }

    pub fn complete_credential_enrollment(
        &self,
        profile_id: &str,
        lock_owner: &str,
        login_id: &str,
        login_epoch: u64,
        sanitized_account: &Value,
    ) -> Result<u64> {
        let g = self.lock()?;
        let generation = g.runtime_db.complete_credential_enrollment(
            profile_id,
            lock_owner,
            login_id,
            login_epoch,
            sanitized_account,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)?;
        Ok(generation)
    }

    pub fn observe_session_credential_enrollment(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        worker_boot_epoch: u64,
        sanitized_account: &Value,
    ) -> Result<u64> {
        let g = self.lock()?;
        let login_epoch = g.runtime_db.observe_session_credential_enrollment(
            placement_thread_id,
            worker_instance_id,
            worker_boot_epoch,
            sanitized_account,
        )?;
        let profile_id = g
            .runtime_db
            .dedicated_session(placement_thread_id)?
            .ok_or_else(|| anyhow!("credential login session disappeared"))?
            .credential_profile_id;
        persist_credential_profile_authority_locked(&g, &profile_id)?;
        Ok(login_epoch)
    }

    pub fn confirm_credential_enrollment(
        &self,
        profile_id: &str,
        owner_principal: &str,
        login_epoch: u64,
        expected_account_digest: &str,
    ) -> Result<u64> {
        let g = self.lock()?;
        let generation = g.runtime_db.confirm_credential_enrollment(
            profile_id,
            owner_principal,
            login_epoch,
            expected_account_digest,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)?;
        Ok(generation)
    }

    pub fn cancel_credential_enrollment(
        &self,
        profile_id: &str,
        lock_owner: &str,
        login_id: &str,
        login_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .cancel_credential_enrollment(profile_id, lock_owner, login_id, login_epoch)?;
        persist_credential_profile_authority_locked(&g, profile_id)
    }

    pub fn revoke_credential_profile(
        &self,
        profile_id: &str,
        owner_principal: &str,
        expected_generation: u64,
    ) -> Result<u64> {
        let g = self.lock()?;
        let generation = g.runtime_db.revoke_credential_profile(
            profile_id,
            owner_principal,
            expected_generation,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)?;
        Ok(generation)
    }

    pub fn finish_credential_profile_revocation(
        &self,
        profile_id: &str,
        owner_principal: &str,
        revoking_generation: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.finish_credential_profile_revocation(
            profile_id,
            owner_principal,
            revoking_generation,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)
    }

    pub fn begin_credential_profile_deletion(
        &self,
        profile_id: &str,
        owner_principal: &str,
        expected_generation: u64,
    ) -> Result<u64> {
        let g = self.lock()?;
        let generation = g.runtime_db.begin_credential_profile_deletion(
            profile_id,
            owner_principal,
            expected_generation,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)?;
        Ok(generation)
    }

    pub fn finish_credential_profile_deletion(
        &self,
        profile_id: &str,
        owner_principal: &str,
        deleting_generation: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.finish_credential_profile_deletion(
            profile_id,
            owner_principal,
            deleting_generation,
        )?;
        persist_credential_profile_authority_locked(&g, profile_id)
    }
}

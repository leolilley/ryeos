//! Actual offline physical/controller custody, outside the resident SQLite
//! graph. The authentic signed claim/clock/namespace remain with the caller.
use anyhow::{Context, Result, ensure};
use std::sync::Arc;
use lillux_sqlite::{OriginalNativeBudget, OriginalNativeBudgetOwner};
use lillux_sqlite::original_host_owner::{OriginalNativeRegionOwner, OriginalNativeRegionHandle,
    OriginalNativeRegionInitializationFailure, OriginalNativeRegionReleaseExclusion,
    OriginalNativeRegionSealFailure};
use lillux_sqlite::original_host_owner::backing::{HostStorage, HostStorageLayout,
    HostReleaseCredit, HostStorageReleased};

pub(super) const HOST_RELEASE_ATTEMPTS: u64 = 8;
/// All fields are retained before the corresponding effect. No Drop performs
/// native cleanup, backing release, quota settlement or authority renewal.
pub(super) struct OfflineResident {
    process: u32,
    generation: u64,
    layout: HostStorageLayout,
    entered: bool,
    raw: Option<OriginalNativeBudget>,
    initialization_failure: Option<OriginalNativeRegionInitializationFailure>,
    backing: Option<HostStorage>,
    owner: Option<OriginalNativeRegionOwner>,
    seal_failure: Option<OriginalNativeRegionSealFailure>,
    exclusion: Option<OriginalNativeRegionReleaseExclusion>,
    release_credit: Option<HostReleaseCredit>,
    budget: Option<Arc<OriginalNativeBudgetOwner>>,
    physical_release: Option<HostStorageReleased>,
}
impl OfflineResident {
    pub(super) fn empty(generation: u64, layout: HostStorageLayout) -> Self {
        Self { process: std::process::id(), generation, layout, entered: false,
            raw: None, initialization_failure: None, backing: None, owner: None,
            seal_failure: None, exclusion: None, release_credit: None, budget: None,
            physical_release: None }
    }
    pub(super) fn require_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "inherited offline resident controller refused");
        Ok(())
    }
    pub(super) fn bytes(&self) -> u64 { self.layout.bytes() as u64 }
    pub(super) fn initialize(&mut self, budget: OriginalNativeBudget,
        mut checkpoint: impl FnMut(bool) -> Result<()>) -> Result<Arc<OriginalNativeBudgetOwner>> {
        self.require_process()?;
        ensure!(!self.entered && self.physical_release.is_none(),
            "offline resident preparation already entered/released; retain original failure");
        self.raw = Some(budget);
        self.entered = true;
        checkpoint(false)?;
        // Precharge physical preparation and finite release attempts against
        // this same raw counter before it moves into resident memory.
        let raw = self.raw.as_ref().context("actual offline raw budget absent")?;
        raw.debit_steps(self.layout.work.checked_add(2).context("resident placement work overflow")?, false)?;
        raw.debit_steps(HOST_RELEASE_ATTEMPTS, true)?;
        self.release_credit = Some(unsafe { HostReleaseCredit::prepaid(HOST_RELEASE_ATTEMPTS) });
        checkpoint(false)?;
        self.backing = Some(unsafe { HostStorage::map(self.layout) }.map_err(anyhow::Error::msg)?);
        // SAFETY: the same retained raw counter prepaid the exact page work
        // above after the authentic durable claim/debit checkpoint.
        unsafe { self.backing.as_ref().context("retained offline mapping absent")?
            .make_resident(&self.layout, || checkpoint(false).map_err(|_| "original offline residency authority refused")) }
            .map_err(anyhow::Error::msg)?;
        checkpoint(false)?;
        let (address, bytes) = self.backing.as_ref().context("retained offline backing absent")?
            .resident_region().map_err(anyhow::Error::msg)?;
        let raw = self.raw.take().context("sole offline raw budget already transferred")?;
        // SAFETY: the actual signed offline reservation already durably debited
        // the exact quoted region within its memory cap. This sole controller
        // retained the real map before residency and owns it through every
        // failure/disposal/settlement; original PID/generation were bound before
        // publication. No other controller or published pointer can free it.
        match unsafe { OriginalNativeRegionOwner::initialize(address, bytes, raw, self.process, self.generation) } {
            Ok(owner) => self.owner = Some(owner),
            Err(failure) => {
                let refusal = failure.refusal;
                self.initialization_failure = Some(failure);
                return Err(anyhow::Error::new(refusal));
            }
        }
        let facade = OriginalNativeBudgetOwner::from_region(self.handle()?);
        self.budget = Some(facade.clone());
        Ok(facade)
    }
    pub(super) fn handle(&self) -> Result<OriginalNativeRegionHandle> {
        self.require_process()?;
        self.owner.as_ref().context("actual offline resident region absent")?
            .handle().map_err(anyhow::Error::new)
    }
    pub(super) fn begin_close(&self) -> Result<()> {
        self.require_process()?;
        if let Some(owner) = &self.owner { owner.begin_close()?; }
        Ok(())
    }
    /// Only after authentic admission closure and actual native terminal
    /// obligations have been joined. Real remaining handles make seal refuse.
    pub(super) fn release(&mut self, mut checkpoint: impl FnMut() -> Result<()>) -> Result<()> {
        self.require_process()?;
        if self.physical_release.is_some() { return Ok(()); }
        checkpoint()?;
        if let Some(failure) = self.seal_failure.take() {
            match failure.into_owner() {
                Ok(owner) => self.owner = Some(owner),
                Err(failure) => { self.seal_failure = Some(failure); anyhow::bail!("offline seal custody transfer refused"); }
            }
        }
        if self.exclusion.is_none() {
            if let Some(owner) = &self.owner {
                owner.begin_close()?;
                drop(self.budget.take());
                owner.retire_pending()?;
                let owner = self.owner.take().context("sole offline region owner absent")?;
                match owner.seal() {
                    Ok(exclusion) => self.exclusion = Some(exclusion),
                    Err(failure) => {
                        let refusal = failure.refusal;
                        self.seal_failure = Some(failure);
                        return Err(anyhow::Error::new(refusal));
                    }
                }
            }
        }
        if let Some(exclusion) = &mut self.exclusion {
            ensure!(exclusion.original_generation()? == self.generation, "offline resident release selects foreign generation");
            exclusion.dispose()?;
        } else {
            ensure!(self.owner.is_none() && self.budget.is_none(), "offline pre-region release has published references");
            // Failed initialization retained the sole raw value; transfer it
            // before releasing backing, with process refusal preserving failure.
            if let Some(failure) = self.initialization_failure.take() {
                match failure.into_budget() {
                    Ok(raw) => self.raw = Some(raw),
                    Err(failure) => { self.initialization_failure = Some(failure); anyhow::bail!("offline raw initialization custody refused"); }
                }
            }
        }
        let mut physical_checkpoint = || -> std::result::Result<(), &'static str> {
            checkpoint().map_err(|_| "original offline physical-release authority refused")
        };
        let released = if let Some(backing) = &mut self.backing {
            let credit = self.release_credit.as_mut().context("offline physical release lacks original prepaid credit")?;
            // SAFETY: this sole outside controller retains the same map/owner
            // or genuine never-published initialization custody. Exclusion was
            // sealed/disposed above and matches this authentic generation.
            unsafe {
                match &self.exclusion {
                    Some(exclusion) => backing.release(exclusion, credit, &mut physical_checkpoint),
                    None => backing.release_before_region(credit, &mut physical_checkpoint),
                }
            }.map_err(anyhow::Error::msg)?
        } else {
            // SAFETY: actual retained state has no acquired map, initialized
            // region, facade or escaped pointer; no numeric record substitutes.
            unsafe { HostStorageReleased::never_mapped() }
        };
        self.physical_release = Some(released);
        Ok(())
    }
    pub(super) fn require_released(&self) -> Result<()> {
        self.require_process()?;
        self.physical_release.as_ref().context("offline resident physical release incomplete")?
            .require_original_process().map_err(anyhow::Error::msg)
    }
    pub(super) fn original_region_release_fact(&self, generation: u64, bytes: u64)
        -> Result<lillux_sqlite::original_host_owner::backing::HostStorageRegionReleaseFact> {
        self.require_process()?;
        ensure!(self.generation == generation && u64::try_from(self.layout.bytes())? == bytes,
            "actual offline resident differs from signed original generation or extent");
        let fact = self.physical_release.as_ref().context("actual offline physical release incomplete")?
            .admitted_region().map_err(anyhow::Error::msg)?
            .context("never-mapped or pre-region release cannot acknowledge admitted original storage")?;
        ensure!(fact.generation().map_err(anyhow::Error::msg)? == generation
            && u64::try_from(fact.bytes().map_err(anyhow::Error::msg)?)? == bytes,
            "physical receipt differs from actual original admitted region");
        Ok(fact)
    }
}

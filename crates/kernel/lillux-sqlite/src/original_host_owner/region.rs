//! One outside controller for one resident SQLite region: funding retention,
//! mapping, residency, initialization and the resumable seal/dispose/release
//! sequence. Authority checks, installation, initialization of the backend
//! and its finish stay with each owner; checkpoints carry that authority in.
use anyhow::{Context, Result, ensure};
use std::sync::Arc;
use crate::native::{OriginalNativeBudget, OriginalNativeBudgetOwner};
use super::{OriginalNativeRegionCensus, OriginalNativeRegionHandle, OriginalNativeRegionInitializationFailure,
    OriginalNativeRegionOwner, OriginalNativeRegionReleaseExclusion, OriginalNativeRegionSealFailure};
use super::backing::{HostReleaseCredit, HostStorage, HostStorageLayout, HostStorageRegionReleaseFact,
    HostStorageReleased};

/// Every field is retained before the corresponding effect. No Drop performs
/// native cleanup, backing release, quota settlement or authority renewal.
pub struct NativeHostRegion {
    process: u32,
    generation: Option<u64>,
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

impl NativeHostRegion {
    /// No effect; `layout` is the pure region quote.
    pub fn new(layout: HostStorageLayout) -> Self {
        Self { process: std::process::id(), generation: None, layout, entered: false,
            raw: None, initialization_failure: None, backing: None, owner: None,
            seal_failure: None, exclusion: None, release_credit: None, budget: None,
            physical_release: None }
    }
    pub fn require_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "inherited native host region controller refused");
        Ok(())
    }
    pub fn bytes(&self) -> u64 { self.layout.bytes() as u64 }
    /// Retains `raw` first, then checkpoints, debits the placement work
    /// (normal) and `release_steps` (cleanup) from that same counter, mints
    /// the matching release credit and checkpoints again.
    pub fn enter(&mut self, raw: OriginalNativeBudget, release_steps: u64,
        mut checkpoint: impl FnMut() -> Result<()>) -> Result<()> {
        self.require_process()?;
        ensure!(!self.entered && self.physical_release.is_none(),
            "native host region preparation already entered/released; retain original failure");
        self.raw = Some(raw);
        self.entered = true;
        checkpoint()?;
        let raw = self.raw.as_ref().context("retained raw native budget absent")?;
        raw.debit_steps(self.layout.work.checked_add(2).context("resident placement work overflow")?, false)?;
        raw.debit_steps(release_steps, true)?;
        // SAFETY: exactly these cleanup steps were irreversibly debited from
        // the retained raw counter above, once.
        self.release_credit = Some(unsafe { HostReleaseCredit::prepaid(release_steps) });
        checkpoint()
    }
    /// Further pre-map debits against the retained raw counter.
    pub fn debit_raw(&self, steps: u64, cleanup: bool) -> Result<()> {
        self.require_process()?;
        ensure!(self.entered && self.backing.is_none() && self.physical_release.is_none(),
            "native host region raw debit outside its pre-map window");
        self.raw.as_ref().context("retained raw native budget absent")?.debit_steps(steps, cleanup)
    }
    /// Maps and makes the layout resident; `checkpoint` runs inside residency
    /// and once after it.
    pub fn map_resident(&mut self, mut checkpoint: impl FnMut() -> Result<()>) -> Result<()> {
        self.require_process()?;
        ensure!(self.entered && self.release_credit.is_some() && self.backing.is_none()
            && self.physical_release.is_none(), "native host region mapping outside its prepaid window");
        // SAFETY: enter debited the placement work from the retained raw
        // counter; this controller retains the map before every later effect.
        self.backing = Some(unsafe { HostStorage::map(self.layout) }.map_err(anyhow::Error::msg)?);
        // SAFETY: the same retained raw counter prepaid the exact page work.
        unsafe { self.backing.as_ref().context("retained native host mapping absent")?
            .make_resident(&self.layout, || checkpoint().map_err(|_| "original native residency authority refused")) }
            .map_err(anyhow::Error::msg)?;
        checkpoint()
    }
    /// Moves the retained raw counter into the resident region. A refusal
    /// retains the initialization failure, and with it the counter, until
    /// release.
    /// # Safety
    /// The caller funded this exact region layout from one authentic original
    /// admission, durably or process-exclusively under the node StateLock,
    /// and `generation` is an authentic nonrecycled incarnation binding, never
    /// a decoded grant.
    pub unsafe fn initialize(&mut self, generation: u64) -> Result<Arc<OriginalNativeBudgetOwner>> {
        self.require_process()?;
        ensure!(self.generation.is_none() && self.owner.is_none() && self.initialization_failure.is_none(),
            "native host region already initialized");
        let (address, bytes) = self.backing.as_ref().context("retained native host backing absent")?
            .resident_region().map_err(anyhow::Error::msg)?;
        let raw = self.raw.take().context("sole raw native budget already transferred")?;
        self.generation = Some(generation);
        // SAFETY: this sole controller retained the real map before residency
        // and owns it through every failure, disposal and settlement; funding
        // and generation are the caller's obligations above.
        match unsafe { OriginalNativeRegionOwner::initialize(address, bytes, raw, self.process, generation) } {
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
    pub fn handle(&self) -> Result<OriginalNativeRegionHandle> {
        self.require_process()?;
        self.owner.as_ref().context("actual native host region absent")?
            .handle().map_err(anyhow::Error::new)
    }
    pub fn census(&self) -> Result<OriginalNativeRegionCensus> {
        self.require_process()?;
        self.owner.as_ref().context("actual native host region absent")?
            .census().map_err(anyhow::Error::new)
    }
    pub fn begin_close(&self) -> Result<()> {
        self.require_process()?;
        if let Some(owner) = &self.owner { owner.begin_close()?; }
        Ok(())
    }
    /// Only after authentic admission closure and actual native terminal
    /// obligations have been joined. Real remaining handles make seal refuse.
    /// Resumable: each completed stage is retained before the next.
    pub fn release(&mut self, mut checkpoint: impl FnMut() -> Result<()>) -> Result<()> {
        self.require_process()?;
        if self.physical_release.is_some() { return Ok(()); }
        checkpoint()?;
        if let Some(failure) = self.seal_failure.take() {
            match failure.into_owner() {
                Ok(owner) => self.owner = Some(owner),
                Err(failure) => { self.seal_failure = Some(failure); anyhow::bail!("native host region seal custody transfer refused"); }
            }
        }
        if self.exclusion.is_none() {
            if let Some(owner) = &self.owner {
                owner.begin_close()?;
                drop(self.budget.take());
                owner.retire_pending()?;
                let owner = self.owner.take().context("sole native host region owner absent")?;
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
            ensure!(Some(exclusion.original_generation()?) == self.generation,
                "native host region release selects foreign generation");
            exclusion.dispose()?;
        } else {
            ensure!(self.owner.is_none() && self.budget.is_none(), "native host pre-region release has published references");
            // Failed initialization retained the sole raw value; transfer it
            // before releasing backing, with process refusal preserving failure.
            if let Some(failure) = self.initialization_failure.take() {
                match failure.into_budget() {
                    Ok(raw) => self.raw = Some(raw),
                    Err(failure) => { self.initialization_failure = Some(failure); anyhow::bail!("native host raw initialization custody refused"); }
                }
            }
        }
        let mut physical_checkpoint = || -> std::result::Result<(), &'static str> {
            checkpoint().map_err(|_| "original native host physical-release authority refused")
        };
        let released = if let Some(backing) = &mut self.backing {
            let credit = self.release_credit.as_mut().context("native host physical release lacks original prepaid credit")?;
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
    pub fn require_released(&self) -> Result<()> {
        self.require_process()?;
        self.physical_release.as_ref().context("native host physical release incomplete")?
            .require_original_process().map_err(anyhow::Error::msg)
    }
    pub fn region_release_fact(&self, generation: u64, bytes: u64) -> Result<HostStorageRegionReleaseFact> {
        self.require_process()?;
        ensure!(self.generation == Some(generation) && u64::try_from(self.layout.bytes())? == bytes,
            "actual native host region differs from original generation or extent");
        let fact = self.physical_release.as_ref().context("actual native host physical release incomplete")?
            .admitted_region().map_err(anyhow::Error::msg)?
            .context("never-mapped or pre-region release cannot acknowledge admitted original storage")?;
        ensure!(fact.generation().map_err(anyhow::Error::msg)? == generation
            && u64::try_from(fact.bytes().map_err(anyhow::Error::msg)?)? == bytes,
            "physical receipt differs from actual original admitted region");
        Ok(fact)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    const RELEASE_STEPS: u64 = 8;

    fn raw() -> OriginalNativeBudget {
        // SAFETY: test-only funding of a region that is never installed.
        unsafe { OriginalNativeBudget::prepaid_by_original_issuer(16 * 1024 * 1024, 1_000_000, 1, 64) }.unwrap()
    }
    fn debits(region: &NativeHostRegion) -> (u64, u64) {
        let snapshot = region.raw.as_ref().expect("raw retained before initialization").snapshot();
        (snapshot.allocator_steps, snapshot.cleanup_steps)
    }

    #[test]
    fn checkpoints_bracket_debits_and_residency_and_refusals_keep_custody() {
        let layout = HostStorage::quote_native_region().unwrap();
        // First checkpoint precedes every debit; the raw counter is retained.
        let mut region = NativeHostRegion::new(layout);
        let mut calls = 0;
        assert!(region.enter(raw(), RELEASE_STEPS, || { calls += 1; anyhow::bail!("refused") }).is_err());
        assert_eq!((calls, debits(&region)), (1, (0, 0)));
        assert!(region.release_credit.is_none());
        region.release(|| Ok(())).unwrap();
        region.require_released().unwrap();
        // Second checkpoint follows both debits and the minted credit.
        let mut region = NativeHostRegion::new(layout);
        let mut calls = 0;
        assert!(region.enter(raw(), RELEASE_STEPS, || { calls += 1; if calls == 2 { anyhow::bail!("refused") } Ok(()) }).is_err());
        assert_eq!((calls, debits(&region)), (2, (layout.work + 2, RELEASE_STEPS)));
        assert!(region.release_credit.is_some() && region.backing.is_none());
        assert!(region.debit_raw(1, false).is_ok());
        region.release(|| Ok(())).unwrap();
        // Residency runs checkpoints inside the mapping and once after it; a
        // refusal after residency keeps the mapping for pre-region release.
        let mut region = NativeHostRegion::new(layout);
        region.enter(raw(), RELEASE_STEPS, || Ok(())).unwrap();
        let mut inside = 0;
        region.map_resident(|| { inside += 1; Ok(()) }).unwrap();
        assert!(inside >= 3);
        assert!(region.debit_raw(1, false).is_err(), "raw debit refused after mapping");
        region.release(|| Ok(())).unwrap();
        region.require_released().unwrap();
        let mut region = NativeHostRegion::new(layout);
        region.enter(raw(), RELEASE_STEPS, || Ok(())).unwrap();
        let mut calls = 0;
        assert!(region.map_resident(|| { calls += 1; if calls == inside { anyhow::bail!("refused") } Ok(()) }).is_err());
        assert!(region.backing.is_some() && region.raw.is_some());
        region.release(|| Ok(())).unwrap();
        // Initialized region: the release fact binds generation and extent.
        let generation = 0x7265_6769_6f6e_0001;
        let mut region = NativeHostRegion::new(layout);
        region.enter(raw(), RELEASE_STEPS, || Ok(())).unwrap();
        region.map_resident(|| Ok(())).unwrap();
        // SAFETY: test-only funding; the generation is unique to this test.
        let facade = unsafe { region.initialize(generation) }.unwrap();
        assert!(region.release(|| Ok(())).is_err(), "facade alias makes seal refuse");
        drop(facade);
        region.release(|| Ok(())).unwrap();
        region.require_released().unwrap();
        assert!(region.region_release_fact(generation + 1, region.bytes()).is_err());
        region.region_release_fact(generation, region.bytes()).unwrap();
    }
}

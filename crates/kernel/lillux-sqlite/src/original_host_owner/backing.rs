//! Actual host backing after durable debit. No allocator credit is inferred
//! from this mapping for unrelated Arc/Vec/error allocations or bootstrap.
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct HostStorage {
    release_generation: Option<u64>,
    process: u32,
    address: usize,
    length: usize,
    resident: AtomicBool,
    locked: AtomicBool,
    residency_started: AtomicBool,
    // Which proof kept this mapping resident, set once residency completes.
    residency: std::sync::OnceLock<lillux::process_control::ResidencyProof>,
    unlocked: bool,
    unmapped: bool,
}

/// Affine prepaid work held outside the resident graph. Only the reservation
/// constructs this after irreversibly debiting the same original raw counter.
pub struct HostReleaseCredit { process: u32, remaining: u64 }
impl HostReleaseCredit {
    /// # Safety
    /// The genuine outside issuer irreversibly debited exactly these cleanup
    /// steps from the same original counter once, before this affine transfer.
    /// No replacement, renewal or refund may reissue consumed attempt credit.
    pub unsafe fn prepaid(steps: u64) -> Self {
        Self { process: std::process::id(), remaining: steps }
    }
    fn consume_attempt(&mut self) -> Result<(), &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign host release credit"); }
        self.remaining = self.remaining.checked_sub(1).ok_or("original host release work exhausted")?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct HostStorageLayout { length: usize, page: usize, pub work: u64 }

impl HostStorageLayout {
    pub fn bytes(&self) -> usize { self.length }
}
impl HostStorage {
    /// Pure concrete region/page quote, before any mapping or native effect.
    pub fn quote_native_region() -> Result<HostStorageLayout, &'static str> {
        let concrete = super::original_native_region_layout().map_err(|_| "native region quote refused")?;
        #[cfg(not(target_os = "linux"))]
        { let _ = concrete; Err("host backing requires Linux") }
        #[cfg(target_os = "linux")]
        {
            let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                .map_err(|_| "invalid host page size")?;
            if page == 0 || concrete.layout.align() > page { return Err("native region page alignment refused"); }
            let length = concrete.layout.size().checked_add(page - 1)
                .map(|bytes| bytes / page * page).ok_or("native region page quote overflow")?;
            Self::quote(u64::try_from(length).map_err(|_| "native region size overflow")?)
        }
    }
    /// Only the original reservation calls this after durable host-byte debit.
    /// No Drop unmaps this retained resource or invents settlement.
    pub fn quote(bytes: u64) -> Result<HostStorageLayout, &'static str> {
        #[cfg(not(target_os = "linux"))]
        { let _ = bytes; return Err("host backing requires Linux"); }
        #[cfg(target_os = "linux")]
        {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            let page = usize::try_from(page).map_err(|_| "invalid host page size")?;
            let length = usize::try_from(bytes).map_err(|_| "host backing size overflow")?;
            if page == 0 || length == 0 || length % page != 0 || length > isize::MAX as usize {
                return Err("host allowance must cover an exact page-aligned mapping");
            }
            let work = u64::try_from(length / page).map_err(|_| "host page work overflow")?
                .checked_add(2).ok_or("host mapping work overflow")?;
            Ok(HostStorageLayout { length, page, work })
        }
    }

    /// # Safety
    /// The original outside issuer already durably reserved this exact byte
    /// layout in its genuine bounded envelope and prepaid actual mapping work.
    /// The returned sole controller is retained before every subsequent effect.
    pub unsafe fn map(layout: HostStorageLayout) -> Result<Self, &'static str> {
        #[cfg(not(target_os = "linux"))]
        { let _ = layout; return Err("host backing requires Linux"); }
        #[cfg(target_os = "linux")]
        {
            let address = unsafe { libc::mmap(std::ptr::null_mut(), layout.length,
                libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1, 0) };
            if address == libc::MAP_FAILED { return Err("host backing mapping refused"); }
            Ok(Self { release_generation: None, process: std::process::id(), unlocked: false, unmapped: false, address: address as usize, length: layout.length, resident: AtomicBool::new(false), locked: AtomicBool::new(false), residency_started: AtomicBool::new(false), residency: std::sync::OnceLock::new() })
        }
    }

    /// The caller retains the mapped owner before this fallible physical
    /// residency effect. A refusal cannot discard a partially backed mapping.
    ///
    /// Residency is proven by locking the pages when this process may lock
    /// them, or otherwise by touching every page while no swap is reachable
    /// for this process. If neither can be proven this refuses with the
    /// observed limits; it never keeps an unproven mapping as resident.
    /// # Safety
    /// This same genuine outside issuer prepaid the complete mlock/page-touch
    /// work once from its original counter and retains mapping/debit custody.
    /// Checkpoint verifies that original authority and nonrenewing clock at
    /// each boundary; it does not create funding or substitute another owner.
    pub unsafe fn make_resident(
        &self,
        layout: &HostStorageLayout,
        mut checkpoint: impl FnMut() -> Result<(), &'static str>,
    ) -> Result<lillux::process_control::ResidencyProof, String> {
        #[cfg(not(target_os = "linux"))]
        { let _ = (layout, checkpoint); return Err("host residency requires Linux".to_owned()); }
        #[cfg(target_os = "linux")]
        {
            // Check the cached process before changing inherited state or
            // touching a copied mapping. The outside owner never replays a
            // released or already-published physical residency attempt.
            if crate::foreign_process(self.process) || self.unlocked || self.unmapped
                || self.resident.load(Ordering::Acquire)
                || self.address == 0 || self.length != layout.length {
                return Err("host residency requires the original empty mapping".to_owned());
            }
            self.residency_started.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| "original host residency already attempted; no replay")?;
            checkpoint()?;
            let length = u64::try_from(self.length).map_err(|_| "host residency size overflow")?;
            let planned = lillux::process_control::ResidencyProof::plan(length)
                .map_err(|reason| format!("host physical residency refused: {reason}; original mapping retained"))?;
            let proof = match planned {
                lillux::process_control::ResidencyProof::Locked { basis } => {
                    if unsafe { libc::mlock(self.address as *const libc::c_void, self.length) } == 0 {
                        // Retain this irreversible completion before a page
                        // checkpoint can refuse. A failed mlock is still
                        // conservatively munlocked over the entire actual
                        // span; false here never certifies partial absence.
                        self.locked.store(true, Ordering::Release);
                        lillux::process_control::ResidencyProof::Locked { basis }
                    } else {
                        let error = std::io::Error::last_os_error();
                        // The limits looked sufficient but the kernel refused
                        // the lock for lack of lockable memory or privilege.
                        // A swap proof is the alternative, observed afresh;
                        // any other failure refuses outright. It can be a
                        // weaker basis than the lock that was planned, so
                        // the node re-decides its capability floor from the
                        // proof returned here before admitting work.
                        if !matches!(error.raw_os_error(), Some(libc::ENOMEM) | Some(libc::EPERM)) {
                            return Err(format!(
                                "host physical residency refused: mlock of {} bytes failed ({error}) although {basis}; original mapping retained",
                                self.length
                            ));
                        }
                        lillux::process_control::ResidencyProof::plan_without_lock(length).map_err(|swap| {
                            format!(
                                "host physical residency refused: mlock of {} bytes failed ({error}) although {basis}, and {swap}; original mapping retained",
                                self.length
                            )
                        })?
                    }
                }
                proof @ (lillux::process_control::ResidencyProof::SwapLimit { basis: _ }
                | lillux::process_control::ResidencyProof::SwapAbsent { basis: _ }) => {
                    // Observed again right before the touch: the pages stay
                    // resident only while nothing can page them out.
                    proof.confirm_swap_unreachable().map_err(|reason| {
                        format!("host physical residency refused: {reason}; original mapping retained")
                    })?;
                    proof
                }
                lillux::process_control::ResidencyProof::Unknown(_) => {
                    return Err("host physical residency refused: unknown residency method; original mapping retained".to_owned());
                }
            };
            // Touch each page before publishing slots. Every page was prepaid;
            // the original authority is checked at each bounded page effect.
            for offset in (0..self.length).step_by(layout.page) {
                checkpoint()?;
                unsafe { (self.address as *mut u8).add(offset).write_volatile(0); }
            }
            checkpoint()?;
            self.residency.set(proof.clone()).map_err(|_| "host residency proof already recorded")?;
            self.resident.store(true, Ordering::Release);
            Ok(proof)
        }
    }

    /// The proof that keeps this mapping resident, once residency completed.
    pub fn residency_proof(&self) -> Option<&lillux::process_control::ResidencyProof> {
        if crate::foreign_process(self.process) { return None; }
        self.residency.get()
    }

    /// Re-check, before new native work is admitted against this region,
    /// that its residency proof still holds. A lock holds for the mapping's
    /// life; a swap-unreachable proof is observed again, since swap enabled
    /// after startup would let the kernel page the region out.
    pub fn confirm_residency(&self) -> Result<(), String> {
        if crate::foreign_process(self.process) || self.unmapped || !self.resident.load(Ordering::Acquire) {
            return Err("host storage has no completed physical residency".to_owned());
        }
        self.residency
            .get()
            .ok_or_else(|| "host residency proof is absent".to_owned())?
            .confirm_swap_unreachable()
            .map_err(|reason| format!("native residency no longer holds: {reason}"))
    }

    /// Supply only the complete concrete SQLite region, never a generic slot
    /// or an escaping mapped reference. The outside reservation retains this
    /// physical mapping until the region's explicit release exclusion.
    pub fn resident_region(&self) -> Result<(NonNull<u8>, usize), &'static str> {
        if crate::foreign_process(self.process) || self.unmapped || !self.resident.load(Ordering::Acquire) {
            return Err("host storage has no completed physical residency");
        }
        let pointer = NonNull::new(self.address as *mut u8).ok_or("host region address is null")?;
        Ok((pointer, self.length))
    }
    /// Release only after the same graph's concrete destructor and zero-loan
    /// seal. Each completed syscall is retained before the next fallible step.
    /// # Safety
    /// The exclusion belongs to THIS actual mapping. The sole outside
    /// controller retained backing custody through every region publication,
    /// raw access, zero-loan seal and disposal. No other owner can free it.
    pub unsafe fn release(
        &mut self,
        exclusion: &super::OriginalNativeRegionReleaseExclusion,
        credit: &mut HostReleaseCredit,
        checkpoint: impl FnMut() -> Result<(), &'static str>,
    ) -> Result<HostStorageReleased, &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign host mapping process"); }
        exclusion.require_physical_mapping(self.address, self.length).map_err(|_| "physical release selects another actual mapping")?;
        exclusion.ready_for_physical_release().map_err(|_| "native region disposal is incomplete")?;
        let generation = exclusion.original_generation().map_err(|_| "foreign original region release")?;
        if self.release_generation.is_some_and(|original| original != generation) { return Err("physical release generation changed"); }
        self.release_generation = Some(generation);
        self.release_mapping(credit, checkpoint)
    }

    /// # Safety
    /// The actual sole outside controller establishes that no region was
    /// initialized/published here and no raw pointer/loan escaped. It retains
    /// the actual raw budget or initialization failure and original authority,
    /// debit, account and clocks through physical release and settlement.
    pub unsafe fn release_before_region(&mut self, credit: &mut HostReleaseCredit,
        checkpoint: impl FnMut() -> Result<(), &'static str>) -> Result<HostStorageReleased, &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign host mapping process"); }
        if self.release_generation.is_some() { return Err("admitted region cannot use pre-region release"); }
        self.release_mapping(credit, checkpoint)
    }

    fn release_mapping(&mut self, credit: &mut HostReleaseCredit, mut checkpoint: impl FnMut() -> Result<(), &'static str>)
        -> Result<HostStorageReleased, &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign host mapping process"); }
        #[cfg(not(target_os = "linux"))]
        { return Err("host release requires Linux"); }
        #[cfg(target_os = "linux")]
        {
            if !self.unlocked {
                checkpoint()?;
                credit.consume_attempt()?;
                if unsafe { libc::munlock(self.address as *const libc::c_void, self.length) } != 0 {
                    return Err("original host unlock refused; mapping retained");
                }
                self.unlocked = true;
                self.locked.store(false, Ordering::Release);
                self.resident.store(false, Ordering::Release);
            }
            if !self.unmapped {
                checkpoint()?;
                credit.consume_attempt()?;
                if unsafe { libc::munmap(self.address as *mut libc::c_void, self.length) } != 0 {
                    return Err("original host unmap refused; unlocked mapping retained");
                }
                self.unmapped = true;
            }
            Ok(HostStorageReleased { process: self.process,
                kind: match self.release_generation {
                    Some(generation) => HostStorageReleaseKind::Region { generation, bytes: self.length },
                    None => HostStorageReleaseKind::BeforeRegion { bytes: self.length },
                } })
        }
    }

}

/// Constructed by actual physical release or the sole outside owner's
/// verified never-acquired mapping disposition.
/// Private observation, never a caller-provided terminal flag.
pub struct HostStorageReleased { process: u32, kind: HostStorageReleaseKind }
#[derive(Clone, Copy)]
enum HostStorageReleaseKind { Region { generation: u64, bytes: usize }, BeforeRegion { bytes: usize }, NeverMapped }
#[derive(Clone, Copy)]
pub struct HostStorageRegionReleaseFact { process: u32, generation: u64, bytes: usize }
impl HostStorageRegionReleaseFact {
    pub fn generation(&self) -> Result<u64, &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign admitted region release fact"); }
        Ok(self.generation)
    }
    pub fn bytes(&self) -> Result<usize, &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign admitted region release fact"); }
        Ok(self.bytes)
    }
}
impl HostStorageReleased {
    /// # Safety
    /// Only the actual original outside controller may observe this after
    /// verifying its retained acquisition state contains no mapping, region,
    /// publication or escaped pointer. Numeric records alone are insufficient.
    pub unsafe fn never_mapped() -> Self {
        Self { process: std::process::id(), kind: HostStorageReleaseKind::NeverMapped }
    }
    pub fn require_original_process(&self) -> Result<(), &'static str> {
        if crate::foreign_process(self.process) { return Err("foreign physical release observation"); }
        Ok(())
    }
    pub fn admitted_region(&self) -> Result<Option<HostStorageRegionReleaseFact>, &'static str> {
        self.require_original_process()?;
        Ok(match self.kind {
            HostStorageReleaseKind::Region { generation, bytes } => Some(HostStorageRegionReleaseFact { process: self.process, generation, bytes }),
            HostStorageReleaseKind::BeforeRegion { .. } | HostStorageReleaseKind::NeverMapped => None,
        })
    }
}

#[cfg(test)]
mod release_credit_controls {
    use super::HostReleaseCredit;

    #[test]
    fn failed_syscall_attempt_credit_is_not_refunded_or_renewed() {
        let _serial = crate::test_host::serial_guard();
        // Mechanism-only credit control; no native/account grant is issued.
        let mut credit = unsafe { HostReleaseCredit::prepaid(2) };
        // The charge precedes the syscall, so success and refusal consume the
        // same irreversible allowance; no completion callback refunds it.
        credit.consume_attempt().unwrap();
        credit.consume_attempt().unwrap();
        assert_eq!(credit.consume_attempt(), Err("original host release work exhausted"));
        assert_eq!(credit.consume_attempt(), Err("original host release work exhausted"));
    }

    #[test]
    fn inherited_credit_refuses_before_counter_mutation() {
        let _serial = crate::test_host::serial_guard();
        let mut credit = HostReleaseCredit { process: std::process::id().wrapping_add(1), remaining: 2 };
        assert_eq!(credit.consume_attempt(), Err("foreign host release credit"));
        assert_eq!(credit.remaining, 2);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod controller_lock_ceiling {
    use super::HostStorage;

    #[test]
    fn resident_region_fits_the_supervised_controller_lock_ceiling() {
        let _serial = crate::test_host::serial_guard();
        // A supervised controller's locked-memory limit is a fixed host
        // constant; the region it locks must stay well inside it.
        let bytes = HostStorage::quote_native_region().unwrap().bytes() as u64;
        assert!(
            bytes.checked_mul(2).unwrap() <= lillux::process_control::CONTROLLER_MEMLOCK_CEILING_BYTES,
            "resident region {bytes} bytes outgrew the controller lock ceiling"
        );
    }
}

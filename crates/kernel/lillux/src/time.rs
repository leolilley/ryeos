use std::thread;
pub use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Subcommand;
use serde::{Deserialize, Serialize};

pub const OCCUPANCY_CLOCK_CONTRACT_VERSION: u32 = 1;

/// Platform-neutral behavior promised by Lillux's durable occupancy clock.
/// The host-specific clock identifier and syscall never cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccupancyClockContract {
    pub version: u32,
    pub monotonic: bool,
    pub includes_host_suspend: bool,
    pub resets_with_host_incarnation: bool,
    pub tick_unit: OccupancyTickUnit,
    pub contract_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OccupancyTickUnit {
    Nanoseconds,
}

/// One durable coordinate in the Lillux occupancy-clock domain. Consumers may
/// compare coordinates only when both opaque identities match exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccupancyCoordinate {
    pub contract_digest: String,
    pub incarnation_digest: String,
    pub tick_ns: u64,
}

/// A finite occupancy interval rooted in one durable Lillux clock domain.
/// Consumers retain this as opaque process-lifecycle authority; all clock
/// arithmetic and host sampling remain inside Lillux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccupancyLimit {
    pub start: OccupancyCoordinate,
    pub maximum_occupancy_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccupancyWindowState {
    Service { remaining: Duration },
    Cleanup { remaining: Duration },
    Expired,
}

impl OccupancyClockContract {
    pub fn current() -> Result<Self, String> {
        let mut contract = Self {
            version: OCCUPANCY_CLOCK_CONTRACT_VERSION,
            monotonic: true,
            includes_host_suspend: true,
            resets_with_host_incarnation: true,
            tick_unit: OccupancyTickUnit::Nanoseconds,
            contract_digest: String::new(),
        };
        contract.contract_digest = occupancy_contract_digest(&contract)?;
        Ok(contract)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != OCCUPANCY_CLOCK_CONTRACT_VERSION
            || !self.monotonic
            || !self.includes_host_suspend
            || !self.resets_with_host_incarnation
            || self.tick_unit != OccupancyTickUnit::Nanoseconds
        {
            return Err("unsupported Lillux occupancy-clock contract".to_string());
        }
        if occupancy_contract_digest(self)? != self.contract_digest {
            return Err("Lillux occupancy-clock contract digest mismatch".to_string());
        }
        Ok(())
    }
}

impl OccupancyCoordinate {
    pub fn validate(&self) -> Result<(), String> {
        if !crate::valid_hash(&self.contract_digest) || !crate::valid_hash(&self.incarnation_digest)
        {
            return Err("occupancy coordinate identities are not canonical digests".to_string());
        }
        Ok(())
    }

    pub fn elapsed_nanoseconds_since(&self, earlier: &Self) -> Result<u64, String> {
        self.validate()?;
        earlier.validate()?;
        if self.contract_digest != earlier.contract_digest
            || self.incarnation_digest != earlier.incarnation_digest
        {
            return Err("occupancy coordinates belong to different clock domains".to_string());
        }
        self.tick_ns
            .checked_sub(earlier.tick_ns)
            .ok_or_else(|| "occupancy clock moved backwards".to_string())
    }
}

impl OccupancyLimit {
    pub fn new(start: OccupancyCoordinate, maximum_occupancy_ns: u64) -> Result<Self, String> {
        start.validate()?;
        if maximum_occupancy_ns == 0 {
            return Err("occupancy limit must be positive".to_string());
        }
        start
            .tick_ns
            .checked_add(maximum_occupancy_ns)
            .ok_or_else(|| "occupancy limit overflows its clock domain".to_string())?;
        Ok(Self {
            start,
            maximum_occupancy_ns,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        self.start.validate()?;
        OccupancyClockContract::current()?.validate()?;
        if self.start.contract_digest != OccupancyClockContract::current()?.contract_digest {
            return Err("occupancy limit uses an unsupported clock contract".to_string());
        }
        Self::new(self.start.clone(), self.maximum_occupancy_ns).map(|_| ())
    }

    /// Observe the remaining service/cleanup window without exposing a host
    /// clock identifier or timestamp arithmetic to the caller.
    pub fn window(&self, cleanup_allowance: Duration) -> Result<OccupancyWindowState, String> {
        self.window_at(&occupancy_now()?, cleanup_allowance)
    }

    pub fn is_expired(&self) -> Result<bool, String> {
        Ok(matches!(
            self.window(Duration::ZERO)?,
            OccupancyWindowState::Expired
        ))
    }

    pub fn window_at(
        &self,
        now: &OccupancyCoordinate,
        cleanup_allowance: Duration,
    ) -> Result<OccupancyWindowState, String> {
        let elapsed = now.elapsed_nanoseconds_since(&self.start)?;
        let cleanup_ns = u64::try_from(cleanup_allowance.as_nanos())
            .map_err(|_| "occupancy cleanup allowance overflows nanoseconds".to_string())?;
        if cleanup_ns >= self.maximum_occupancy_ns {
            return Err("occupancy cleanup allowance consumes the complete limit".to_string());
        }
        let service_ns = self.maximum_occupancy_ns - cleanup_ns;
        if elapsed < service_ns {
            return Ok(OccupancyWindowState::Service {
                remaining: Duration::from_nanos(service_ns - elapsed),
            });
        }
        if elapsed < self.maximum_occupancy_ns {
            return Ok(OccupancyWindowState::Cleanup {
                remaining: Duration::from_nanos(self.maximum_occupancy_ns - elapsed),
            });
        }
        Ok(OccupancyWindowState::Expired)
    }
}

/// Sample the host's durable occupancy clock. OS-specific selection remains
/// wholly inside Lillux; RyeOS receives only the reviewed semantic contract,
/// an opaque incarnation identity and an integer coordinate.
pub fn occupancy_now() -> Result<OccupancyCoordinate, String> {
    occupancy_now_platform()
}

fn occupancy_contract_digest(contract: &OccupancyClockContract) -> Result<String, String> {
    let mut value = serde_json::to_value(contract).map_err(|error| error.to_string())?;
    value
        .as_object_mut()
        .ok_or_else(|| "occupancy clock contract must encode as an object".to_string())?
        .remove("contract_digest");
    let canonical = crate::canonical_json(&value).map_err(|error| error.to_string())?;
    Ok(crate::sha256_hex(canonical.as_bytes()))
}

#[cfg(target_os = "linux")]
fn occupancy_now_platform() -> Result<OccupancyCoordinate, String> {
    let contract = OccupancyClockContract::current()?;
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` points to a valid writable timespec for the duration of
    // the call. The chosen clock and its semantics remain private to Lillux.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(format!(
            "sample durable occupancy clock: {}",
            std::io::Error::last_os_error()
        ));
    }
    let seconds = u64::try_from(value.tv_sec)
        .map_err(|_| "occupancy clock returned a negative second coordinate".to_string())?;
    let nanoseconds = u64::try_from(value.tv_nsec)
        .map_err(|_| "occupancy clock returned a negative nanosecond coordinate".to_string())?;
    let tick_ns = seconds
        .checked_mul(1_000_000_000)
        .and_then(|total| total.checked_add(nanoseconds))
        .ok_or_else(|| "occupancy clock coordinate overflow".to_string())?;
    let incarnation = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("read occupancy clock incarnation: {error}"))?;
    let incarnation = incarnation.trim();
    if incarnation.is_empty() {
        return Err("occupancy clock incarnation is empty".to_string());
    }
    Ok(OccupancyCoordinate {
        contract_digest: contract.contract_digest,
        incarnation_digest: crate::sha256_hex(incarnation.as_bytes()),
        tick_ns,
    })
}

#[cfg(not(target_os = "linux"))]
fn occupancy_now_platform() -> Result<OccupancyCoordinate, String> {
    Err("durable occupancy clock is unavailable on this platform".to_string())
}

/// One process-local monotonic measurement owned by the Lillux host-time
/// boundary. The value is intentionally opaque: callers may retain it only to
/// measure elapsed duration in the same process and clock domain.
#[derive(Debug)]
pub struct MonotonicTimer {
    started_at: Instant,
}

/// Opaque process-local monotonic expiry authority.
///
/// Callers can ask whether the deadline has elapsed, but never receive the
/// host `Instant` or compare it as durable identity. This keeps callback and
/// lease expiry mechanics inside the Lillux time boundary.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicDeadline {
    deadline: Instant,
}

impl MonotonicDeadline {
    pub fn after(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
        }
    }

    pub fn has_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Narrow two process-local expiry authorities without restarting either
    /// clock or exposing the underlying host instant.
    pub fn min(self, other: Self) -> Self {
        Self {
            deadline: self.deadline.min(other.deadline),
        }
    }

    /// Compare process-local expiries without sampling, exporting or renewing time.
    pub(crate) fn is_before(&self, other: &Self) -> bool {
        self.deadline < other.deadline
    }

    /// Remaining duration in this process-local monotonic clock domain.
    /// Returning zero is the only expiry representation exposed to callers;
    /// the host `Instant` never escapes Lillux.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

#[cfg(test)]
mod monotonic_deadline_ordering_tests {
    use super::*;

    #[test]
    fn deadline_order_uses_exact_endpoints_and_accepts_equal_copies() {
        let anchor = Instant::now();
        let early = MonotonicDeadline { deadline: anchor };
        let equal = early;
        let late = MonotonicDeadline {
            deadline: anchor + Duration::from_secs(1),
        };
        assert!(!early.is_before(&equal));
        assert!(!equal.is_before(&early));
        assert!(early.is_before(&late));
        assert!(!late.is_before(&early));
    }

    #[test]
    fn expired_endpoint_order_does_not_depend_on_remaining_samples() {
        let anchor = Instant::now();
        let earlier = anchor.checked_sub(Duration::from_secs(2));
        let early = MonotonicDeadline {
            deadline: earlier.unwrap_or(anchor),
        };
        let late = MonotonicDeadline { deadline: anchor };
        assert!(early.has_elapsed());
        assert!(late.has_elapsed());
        assert_eq!(early.is_before(&late), earlier.is_some());
        assert!(!late.is_before(&early));
    }
}

impl MonotonicTimer {
    pub fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn elapsed_millis(&self) -> u64 {
        u64::try_from(self.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn elapsed_micros(&self) -> u64 {
        u64::try_from(self.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// Sleep the current thread for one process-local duration. Raw host timing
/// remains below the Lillux boundary even for synchronous protocol loops.
pub fn sleep(duration: Duration) {
    thread::sleep(duration);
}

pub fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_unix_secs(now.as_secs())
}

/// Format an absolute Unix timestamp (seconds) as ISO-8601 UTC — same
/// encoding `iso8601_now` emits, for computing comparable cutoffs.
pub fn iso8601_from_unix_secs(secs: u64) -> String {
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Current wall-clock time as milliseconds since Unix epoch.
pub fn timestamp_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[derive(Subcommand)]
pub enum TimeAction {
    /// Current wall-clock time
    Now,
    /// Sleep for N milliseconds
    After {
        #[arg(long)]
        ms: u64,
    },
}

pub fn run(action: TimeAction) -> serde_json::Value {
    match action {
        TimeAction::Now => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            serde_json::json!({
                "timestamp_ns": now.as_nanos() as u64,
                "timestamp_ms": now.as_millis() as u64,
            })
        }
        TimeAction::After { ms } => {
            let start = Instant::now();
            thread::sleep(Duration::from_millis(ms));
            let elapsed = start.elapsed().as_millis() as u64;
            serde_json::json!({ "elapsed_ms": elapsed })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_clock_exposes_only_the_portable_contract() {
        let contract = OccupancyClockContract::current().unwrap();
        contract.validate().unwrap();
        let first = occupancy_now().unwrap();
        let second = occupancy_now().unwrap();
        assert_eq!(first.contract_digest, contract.contract_digest);
        assert_eq!(second.contract_digest, contract.contract_digest);
        assert_eq!(first.incarnation_digest, second.incarnation_digest);
        second.elapsed_nanoseconds_since(&first).unwrap();
    }

    #[test]
    fn occupancy_coordinates_refuse_cross_incarnation_comparison() {
        let mut first = occupancy_now().unwrap();
        let second = first.clone();
        first.incarnation_digest = crate::sha256_hex(b"different-incarnation");
        assert!(second.elapsed_nanoseconds_since(&first).is_err());
    }

    #[test]
    fn occupancy_limit_separates_service_cleanup_and_expiry() {
        let start = occupancy_now().unwrap();
        let limit = OccupancyLimit::new(start.clone(), 10_000).unwrap();
        let at = |tick_ns| OccupancyCoordinate {
            tick_ns,
            ..start.clone()
        };
        assert_eq!(
            limit
                .window_at(&at(start.tick_ns + 4_000), Duration::from_nanos(2_000))
                .unwrap(),
            OccupancyWindowState::Service {
                remaining: Duration::from_nanos(4_000)
            }
        );
        assert_eq!(
            limit
                .window_at(&at(start.tick_ns + 9_000), Duration::from_nanos(2_000))
                .unwrap(),
            OccupancyWindowState::Cleanup {
                remaining: Duration::from_nanos(1_000)
            }
        );
        assert_eq!(
            limit
                .window_at(&at(start.tick_ns + 10_000), Duration::from_nanos(2_000))
                .unwrap(),
            OccupancyWindowState::Expired
        );
    }

    #[test]
    fn deadline_intersection_preserves_the_original_earliest_instant() {
        let earlier = MonotonicDeadline::after(Duration::ZERO);
        let later = MonotonicDeadline::after(Duration::from_secs(60));
        assert_eq!(earlier.min(later).deadline, earlier.deadline);
        assert_eq!(later.min(earlier).deadline, earlier.deadline);
        assert!(later.min(earlier).has_elapsed());
        assert_eq!(earlier.min(earlier).deadline, earlier.deadline);
    }
}

/// A sealed transfer of an existing local window, not financial, node,
/// release or physical-retirement authority. No raw/serde/default constructor.
/// The selected adapter must inherit BOTH exact descriptor roles once.
pub struct OriginalNamespaceOccupancyHandoff {
    original_deadline: MonotonicDeadline,
    window: crate::InheritedDescriptorAuthority,
    clock_namespace: crate::InheritedDescriptorAuthority,
}

impl OriginalNamespaceOccupancyHandoff {
    /// The original local deadline retained by the exporting owner.
    /// This is never reconstructed from remaining duration or wire data.
    pub fn original_deadline(&self) -> MonotonicDeadline {
        self.original_deadline
    }

    /// Duplicate the same sealed window and pinned clock as one opaque owner.
    /// This preserves the original deadline and grants no contact or target
    /// permission. A selected caller must separately authorize any forwarding.
    #[cfg(target_os = "linux")]
    pub fn duplicate_retained_pair(&self) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt as _;
        let _lease = crate::exec::retain_fork_sensitive_descriptors_until(self.original_deadline)
            .map_err(|error| format!("retain original window duplication: {error}"))?;
        self.require_live_pair()?;
        // The outer deadline-bound lease makes these nested registered leases
        // reentrant, even when a fork is waiting. No second barrier wait.
        let duplicate = Self {
            original_deadline: self.original_deadline,
            window: self.window.duplicate_at_or_above(3)?,
            clock_namespace: self.clock_namespace.duplicate_at_or_above(3)?,
        };
        let original_fds = [
            self.window.inherited_descriptor()?,
            self.clock_namespace.inherited_descriptor()?,
        ];
        for (original, copy) in [
            (&self.window, &duplicate.window),
            (&self.clock_namespace, &duplicate.clock_namespace),
        ] {
            if original_fds.contains(&copy.inherited_descriptor()?) {
                return Err("original window duplicate overlaps retained roles".into());
            }
            let source = original
                .file()
                .metadata()
                .map_err(|error| error.to_string())?;
            let target = copy.file().metadata().map_err(|error| error.to_string())?;
            if (source.dev(), source.ino()) != (target.dev(), target.ino()) {
                return Err("original window duplicate changed object identity".into());
            }
        }
        duplicate.require_live_pair()?;
        self.require_live_pair()?;
        Ok(duplicate)
    }

    #[cfg(target_os = "linux")]
    fn require_live_pair(&self) -> Result<(), String> {
        let window_fd = self.window.inherited_descriptor()?;
        let clock_fd = self.clock_namespace.inherited_descriptor()?;
        if window_fd <= 2 || clock_fd <= 2 || window_fd == clock_fd {
            return Err("original-window roles must be distinct and above stdio".into());
        }
        original_window::require_same_clock_namespace(self.clock_namespace.file())?;
        let bytes = crate::sandbox::read_sealed_inherited_descriptor(
            window_fd,
            original_window::MAX_BYTES,
        )?;
        let wire: original_window::Wire = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode strict original occupancy handoff: {error}"))?;
        wire.require_clock_namespace(self.clock_namespace.file())?;
        wire.validate(&occupancy_now()?)?;
        original_window::require_window(
            &wire.limit,
            Duration::from_nanos(wire.cleanup_ns),
            self.original_deadline,
        )
    }

    #[cfg(not(target_os = "linux"))]
    pub fn duplicate_retained_pair(&self) -> Result<Self, String> {
        Err("original native occupancy handoff is unavailable on this platform".into())
    }

    #[cfg(target_os = "linux")]
    pub fn pin_original_clock_namespace(
        original_deadline: MonotonicDeadline,
    ) -> Result<crate::InheritedDescriptorAuthority, String> {
        let lease = crate::exec::retain_fork_sensitive_descriptors_until(original_deadline)
            .map_err(|error| format!("retain original clock namespace: {error}"))?;
        original_window::pin_current_clock_namespace(&lease)
    }

    #[cfg(target_os = "linux")]
    pub fn require_original_clock_namespace(
        original: &crate::InheritedDescriptorAuthority,
        original_deadline: MonotonicDeadline,
    ) -> Result<(), String> {
        let _lease = crate::exec::retain_fork_sensitive_descriptors_until(original_deadline)
            .map_err(|error| format!("retain original clock comparison: {error}"))?;
        original_window::require_same_clock_namespace(original.file())
    }

    #[cfg(target_os = "linux")]
    pub fn from_original(
        limit: &OccupancyLimit,
        cleanup: Duration,
        original_deadline: MonotonicDeadline,
    ) -> Result<Self, String> {
        let clock_namespace = Self::pin_original_clock_namespace(original_deadline)?;
        Self::from_original_in_retained_clock_namespace(
            limit,
            cleanup,
            original_deadline,
            &clock_namespace,
        )
    }

    /// Caller must derive the original limit/deadline from its existing funded
    /// owner. This retains THAT clock namespace, not a newly observed substitute.
    #[cfg(target_os = "linux")]
    pub fn from_original_in_retained_clock_namespace(
        limit: &OccupancyLimit,
        cleanup: Duration,
        original_deadline: MonotonicDeadline,
        original_clock_namespace: &crate::InheritedDescriptorAuthority,
    ) -> Result<Self, String> {
        let _lease = crate::exec::retain_fork_sensitive_descriptors_until(original_deadline)
            .map_err(|error| format!("retain original window transfer: {error}"))?;
        Self::require_original_clock_namespace(original_clock_namespace, original_deadline)?;
        let clock_namespace = original_clock_namespace.clone();
        // Sample BOOTTIME BEFORE remaining Instant duration. Any delay between
        // samples only narrows the original deadline; suspend also narrows it.
        let before = occupancy_now()?;
        original_window::require_window(limit, cleanup, original_deadline)?;
        let remaining = original_deadline.remaining();
        let contact_expiry_tick_ns = before
            .tick_ns
            .checked_add(
                u64::try_from(remaining.as_nanos())
                    .map_err(|_| "original contact expiry overflow")?,
            )
            .ok_or("original contact expiry overflow")?;
        let service_expiry = limit
            .start
            .tick_ns
            .checked_add(limit.maximum_occupancy_ns)
            .and_then(|expiry| expiry.checked_sub(u64::try_from(cleanup.as_nanos()).ok()?))
            .ok_or("original service expiry overflow")?;
        let contact_expiry_tick_ns = contact_expiry_tick_ns.min(service_expiry);
        use std::os::unix::fs::MetadataExt as _;
        let namespace_identity = clock_namespace
            .file()
            .metadata()
            .map_err(|error| error.to_string())?;
        let wire = original_window::Wire {
            version: 1,
            clock_namespace_dev: namespace_identity.dev(),
            clock_namespace_ino: namespace_identity.ino(),
            limit: limit.clone(),
            cleanup_ns: u64::try_from(cleanup.as_nanos())
                .map_err(|_| "original cleanup overflow")?,
            contact_expiry_tick_ns,
        };
        wire.validate(&before)?;
        let bytes = serde_json::to_vec(&wire).map_err(|error| error.to_string())?;
        if bytes.is_empty() || bytes.len() > original_window::MAX_BYTES {
            return Err("original occupancy handoff exceeds its fixed bound".into());
        }
        let window = crate::sealed_memfd(c"original-native-occupancy-window", &bytes)?;
        original_window::require_same_clock_namespace(clock_namespace.file())?;
        original_window::require_window(limit, cleanup, original_deadline)?;
        Ok(Self {
            original_deadline,
            window,
            clock_namespace,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn pin_original_clock_namespace(
        _original_deadline: MonotonicDeadline,
    ) -> Result<crate::InheritedDescriptorAuthority, String> {
        Err("original native clock namespace is unavailable on this platform".into())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn require_original_clock_namespace(
        _original: &crate::InheritedDescriptorAuthority,
        _original_deadline: MonotonicDeadline,
    ) -> Result<(), String> {
        Err("original native clock namespace is unavailable on this platform".into())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn from_original_in_retained_clock_namespace(
        _limit: &OccupancyLimit,
        _cleanup: Duration,
        _original_deadline: MonotonicDeadline,
        _original_clock_namespace: &crate::InheritedDescriptorAuthority,
    ) -> Result<Self, String> {
        Err("original native occupancy handoff is unavailable on this platform".into())
    }

    /// Consume into the existing inherited-FD compiler. Descriptor coordinates
    /// are transport only; no reconstructed numeric PID or fresh timer.
    pub fn into_authorities(
        self,
    ) -> (
        crate::InheritedDescriptorAuthority,
        crate::InheritedDescriptorAuthority,
    ) {
        (self.window, self.clock_namespace)
    }
}

/// Adopted once in the dedicated adapter BEFORE USER/PID transition. Keeps the
/// exact clock namespace pinned until the existing native owner consumes it.
/// A restart must not recreate this from elapsed duration or a recovered row.
#[cfg(target_os = "linux")]
pub struct AdoptedOriginalNamespaceOccupancy {
    wire: original_window::Wire,
    deadline: MonotonicDeadline,
    clock_namespace: crate::InheritedDescriptorAuthority,
}

#[cfg(target_os = "linux")]
impl AdoptedOriginalNamespaceOccupancy {
    /// # Safety
    /// Both distinct descriptors must be uniquely owned, deliberately inherited
    /// for this exact request, and have no other Rust owner in this process.
    pub unsafe fn take_inherited(window_fd: u32, clock_namespace_fd: u32) -> Result<Self, String> {
        if window_fd <= 2 || clock_namespace_fd <= 2 || window_fd == clock_namespace_fd {
            return Err("original-window roles must be distinct and above stdio".into());
        }
        let window = unsafe { crate::take_inherited_descriptor_authority(window_fd) }?;
        let clock_namespace =
            unsafe { crate::take_inherited_descriptor_authority(clock_namespace_fd) }?;
        Self::from_owned_original_roles(window, clock_namespace, None)
    }

    /// Adopt the same original roles while checking a protected terminal writer
    /// against the complete live inherited inventory BEFORE those roles close.
    /// The original wire alone supplies the deadline; no caller clock is accepted.
    /// This is role/object validation, not funding/native-generation admission.
    ///
    /// # Safety
    /// Window and clock must be distinct uniquely owned inherited coordinates,
    /// with no existing Rust owner. Writer and every companion must remain live
    /// in the dedicated adapter. Writer is already owned and is only borrowed.
    pub unsafe fn take_inherited_with_terminal_writer(
        window_fd: u32,
        clock_namespace_fd: u32,
        writer: &crate::InheritedDescriptorAuthority,
        companion_fds: &[u32],
    ) -> Result<Self, String> {
        let writer_fd = writer.inherited_descriptor()?;
        if window_fd <= 2
            || clock_namespace_fd <= 2
            || window_fd == clock_namespace_fd
            || window_fd == writer_fd
            || clock_namespace_fd == writer_fd
        {
            return Err(
                "original-window roles overlap stdio, each other or terminal writer".into(),
            );
        }
        let window = unsafe { crate::take_inherited_descriptor_authority(window_fd) }?;
        let clock_namespace =
            unsafe { crate::take_inherited_descriptor_authority(clock_namespace_fd) }?;
        Self::from_owned_original_roles(window, clock_namespace, Some((writer, companion_fds)))
    }

    fn from_owned_original_roles(
        window: crate::InheritedDescriptorAuthority,
        clock_namespace: crate::InheritedDescriptorAuthority,
        terminal_writer: Option<(&crate::InheritedDescriptorAuthority, &[u32])>,
    ) -> Result<Self, String> {
        original_window::require_same_clock_namespace(clock_namespace.file())?;
        let bytes = crate::sandbox::read_sealed_inherited_descriptor(
            window.inherited_descriptor()?,
            original_window::MAX_BYTES,
        )?;
        let wire: original_window::Wire = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode strict original occupancy handoff: {error}"))?;
        // Anchor BEFORE observing BOOTTIME. Never add stale remaining duration
        // to a later Instant: that would extend the exported original expiry.
        let anchor = Instant::now();
        let now = occupancy_now()?;
        wire.validate(&now)?;
        wire.require_clock_namespace(clock_namespace.file())?;
        let remaining = Duration::from_nanos(wire.contact_expiry_tick_ns - now.tick_ns);
        let deadline = MonotonicDeadline {
            deadline: anchor
                .checked_add(remaining)
                .ok_or("original contact deadline overflow")?,
        };
        original_window::require_window(
            &wire.limit,
            Duration::from_nanos(wire.cleanup_ns),
            deadline,
        )?;
        if let Some((writer, companion_fds)) = terminal_writer {
            // All actual peers must still be live here. The original sealed
            // window and actual clock are owned above, not closed/reopened.
            let window_fd = window.inherited_descriptor()?;
            let clock_fd = clock_namespace.inherited_descriptor()?;
            for required in [0, 1, 2, window_fd, clock_fd] {
                if !companion_fds.contains(&required) {
                    return Err(
                        "native terminal peer inventory omits stdio or original roles".into(),
                    );
                }
            }
            writer.require_distinct_nonblocking_pipe_writer_from_inherited_coordinates(
                companion_fds,
                deadline,
            )?;
        }
        // The sealed input has served its one-use role; no FD reaches target.
        drop(window);
        Ok(Self {
            wire,
            deadline,
            clock_namespace,
        })
    }

    /// Borrow the original service bound while retaining clock custody.
    /// Repeated observations return the same stored local deadline, never a
    /// renewed timer. Callers must also re-observe BOOTTIME after bounded waits.
    pub fn original_service_parts(
        &self,
    ) -> Result<(OccupancyLimit, Duration, MonotonicDeadline), String> {
        original_window::require_same_clock_namespace(self.clock_namespace.file())?;
        self.wire.validate(&occupancy_now()?)?;
        self.wire
            .require_clock_namespace(self.clock_namespace.file())?;
        let cleanup = Duration::from_nanos(self.wire.cleanup_ns);
        original_window::require_window(&self.wire.limit, cleanup, self.deadline)?;
        Ok((self.wire.limit.clone(), cleanup, self.deadline))
    }

    /// Observe the reserve within the SAME original occupancy maximum.
    /// Contact expiry closes service but must not hide the cleanup interval.
    /// A Service observation alone does not authorize service: callers must
    /// also pass original_service_parts. Parent cleanup policy may narrow this
    /// bound. Remaining duration grants neither a new timer nor settlement.
    pub fn original_cleanup_window(&self) -> Result<OccupancyWindowState, String> {
        self.wire
            .require_clock_namespace(self.clock_namespace.file())?;
        if self.wire.version != 1
            || self.wire.cleanup_ns == 0
            || self.wire.contact_expiry_tick_ns == 0
        {
            return Err("original cleanup handoff has unsupported version or shape".into());
        }
        self.wire.limit.validate()?;
        // One fresh durable observation validates the original boot/clock and
        // supplies both phase and remaining time; no local Instant is created.
        let now = occupancy_now()?;
        self.wire
            .limit
            .window_at(&now, Duration::from_nanos(self.wire.cleanup_ns))
    }

    /// Consume the exact adopted coordinates. Recheck original expiry and both
    /// actual clock-namespace views immediately before watcher preparation.
    pub fn into_original_parts(
        self,
    ) -> Result<(OccupancyLimit, Duration, MonotonicDeadline), String> {
        original_window::require_same_clock_namespace(self.clock_namespace.file())?;
        self.wire.validate(&occupancy_now()?)?;
        self.wire
            .require_clock_namespace(self.clock_namespace.file())?;
        let cleanup = Duration::from_nanos(self.wire.cleanup_ns);
        original_window::require_window(&self.wire.limit, cleanup, self.deadline)?;
        // self is consumed; its pinned namespace FD closes before target fork.
        Ok((self.wire.limit, cleanup, self.deadline))
    }
}

#[cfg(target_os = "linux")]
mod original_window {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;
    pub(super) const MAX_BYTES: usize = 4096;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Wire {
        pub(super) version: u32,
        pub(super) clock_namespace_dev: u64,
        pub(super) clock_namespace_ino: u64,
        pub(super) limit: OccupancyLimit,
        pub(super) cleanup_ns: u64,
        pub(super) contact_expiry_tick_ns: u64,
    }
    impl Wire {
        pub(super) fn require_clock_namespace(
            &self,
            original: &std::fs::File,
        ) -> Result<(), String> {
            require_same_clock_namespace(original)?;
            let identity = original.metadata().map_err(|error| error.to_string())?;
            if identity.dev() != self.clock_namespace_dev
                || identity.ino() != self.clock_namespace_ino
            {
                return Err(
                    "sealed original window disagrees with retained clock namespace".into(),
                );
            }
            Ok(())
        }
        pub(super) fn validate(&self, now: &OccupancyCoordinate) -> Result<(), String> {
            if self.version != 1
                || self.cleanup_ns == 0
                || self.contact_expiry_tick_ns <= now.tick_ns
            {
                return Err("original contact handoff is expired or unsupported".into());
            }
            self.limit.validate()?;
            now.elapsed_nanoseconds_since(&self.limit.start)?; // same actual boot/clock contract
            if !matches!(
                self.limit
                    .window_at(now, Duration::from_nanos(self.cleanup_ns))?,
                OccupancyWindowState::Service { .. }
            ) {
                return Err("original occupancy handoff has no service/cleanup window".into());
            }
            Ok(())
        }
    }

    pub(super) fn require_window(
        limit: &OccupancyLimit,
        cleanup: Duration,
        deadline: MonotonicDeadline,
    ) -> Result<(), String> {
        if deadline.has_elapsed()
            || !matches!(limit.window(cleanup)?, OccupancyWindowState::Service { .. })
        {
            return Err("original contact or service window expired".into());
        }
        Ok(())
    }

    pub(super) fn pin_current_clock_namespace(
        lease: &crate::exec::ForkSensitiveDescriptorLease,
    ) -> Result<crate::InheritedDescriptorAuthority, String> {
        let current = std::fs::File::open("/proc/thread-self/ns/time")
            .map_err(|error| format!("pin original time namespace: {error}"))?;
        require_same_clock_namespace(&current)?;
        crate::exec::InheritedDescriptorAuthority::from_owned_file(current, lease)
    }

    pub(super) fn require_same_clock_namespace(original: &std::fs::File) -> Result<(), String> {
        let expected = original.metadata().map_err(|error| error.to_string())?;
        for path in [
            "/proc/thread-self/ns/time",
            "/proc/thread-self/ns/time_for_children",
        ] {
            let current = std::fs::File::open(path)
                .map_err(|error| format!("pin actual clock namespace view: {error}"))?;
            let observed = current.metadata().map_err(|error| error.to_string())?;
            if expected.dev() != observed.dev() || expected.ino() != observed.ino() {
                return Err(
                    "original occupancy handoff cannot cross actual clock namespace".into(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod original_window_tests {
    use super::*;
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;

    fn duplicate(fd: i32) -> u32 {
        let raw = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        assert!(raw > 2);
        raw as u32 // transferred uniquely to the adopter
    }
    fn window() -> (OccupancyLimit, Duration, MonotonicDeadline) {
        (
            OccupancyLimit::new(occupancy_now().unwrap(), 10_000_000_000).unwrap(),
            Duration::from_secs(1),
            MonotonicDeadline::after(Duration::from_secs(5)),
        )
    }
    fn adopt_bytes(bytes: &[u8]) -> Result<AdoptedOriginalNamespaceOccupancy, String> {
        let sealed = crate::sealed_memfd(c"test-original-window", bytes).unwrap();
        let namespace = std::fs::File::open("/proc/thread-self/ns/time").unwrap();
        unsafe {
            AdoptedOriginalNamespaceOccupancy::take_inherited(
                duplicate(sealed.file().as_raw_fd()),
                duplicate(namespace.as_raw_fd()),
            )
        }
    }

    fn terminal_pipe() -> (
        crate::SupervisedLauncherAttachmentStatusPipe,
        crate::InheritedDescriptorAuthority,
    ) {
        let mut held = crate::supervised_launcher_attachment_status_pipe().unwrap();
        held.reader.require_mount_preparation_receipt().unwrap();
        held.reader
            .require_applied_launch_receipt("test.applied/v1")
            .unwrap();
        let writer = held
            .prepare_native_settlement_channel(MonotonicDeadline::after(Duration::from_secs(5)))
            .unwrap();
        (held, writer)
    }

    #[test]
    fn terminal_peer_scan_observes_original_roles_before_close_without_extending_window() {
        let (limit, cleanup, deadline) = window();
        let (sealed, namespace) =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline)
                .unwrap()
                .into_authorities();
        let companions = [
            0,
            1,
            2,
            sealed.inherited_descriptor().unwrap(),
            namespace.inherited_descriptor().unwrap(),
        ];
        let (_held, writer) = terminal_pipe();
        // Private shared owning body: actual sealed/namespace objects move once,
        // without an unregistered raw duplicate or synthetic proof fixture.
        let adopted = AdoptedOriginalNamespaceOccupancy::from_owned_original_roles(
            sealed,
            namespace,
            Some((&writer, &companions)),
        )
        .unwrap();
        assert!(adopted.deadline.deadline <= deadline.deadline);
        assert!(writer.file_identity().is_ok());
        let (actual_limit, actual_cleanup, actual_deadline) =
            adopted.into_original_parts().unwrap();
        assert_eq!(actual_limit, limit);
        assert_eq!(actual_cleanup, cleanup);
        assert!(actual_deadline.deadline <= deadline.deadline);
    }

    #[test]
    fn original_terminal_hook_rejects_live_writer_alias_and_missing_stdio() {
        let (_held, writer) = terminal_pipe();
        let alias = writer.duplicate_at_or_above(64).unwrap();
        let (limit, cleanup, deadline) = window();
        let (sealed, namespace) =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline)
                .unwrap()
                .into_authorities();
        let companions = [
            0,
            1,
            2,
            sealed.inherited_descriptor().unwrap(),
            namespace.inherited_descriptor().unwrap(),
            alias.inherited_descriptor().unwrap(),
        ];
        let error = AdoptedOriginalNamespaceOccupancy::from_owned_original_roles(
            sealed,
            namespace,
            Some((&writer, &companions)),
        )
        .err()
        .expect("writer alias admitted");
        assert!(error.contains("aliases"), "{error}");
        assert!(alias.file_identity().is_ok());
        let (limit, cleanup, deadline) = window();
        let (sealed, namespace) =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline)
                .unwrap()
                .into_authorities();
        let incomplete = [
            sealed.inherited_descriptor().unwrap(),
            namespace.inherited_descriptor().unwrap(),
        ];
        let error = AdoptedOriginalNamespaceOccupancy::from_owned_original_roles(
            sealed,
            namespace,
            Some((&writer, &incomplete)),
        )
        .err()
        .expect("missing stdio admitted");
        assert!(error.contains("omits stdio"), "{error}");
        assert!(writer.file_identity().is_ok());
    }

    #[test]
    fn sealed_original_window_roundtrip_cannot_extend_original_deadline() {
        let (limit, cleanup, deadline) = window();
        let handoff =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline).unwrap();
        let (sealed, namespace) = handoff.into_authorities();
        let adopted = unsafe {
            AdoptedOriginalNamespaceOccupancy::take_inherited(
                duplicate(sealed.file().as_raw_fd()),
                duplicate(namespace.file().as_raw_fd()),
            )
        }
        .unwrap();
        assert!(adopted.deadline.deadline <= deadline.deadline);
        let (actual_limit, actual_cleanup, actual_deadline) =
            adopted.into_original_parts().unwrap();
        assert_eq!(actual_limit, limit);
        assert_eq!(actual_cleanup, cleanup);
        assert!(actual_deadline.deadline <= deadline.deadline);
    }

    fn adopt_original_bound(
        limit: &OccupancyLimit,
        cleanup: Duration,
        deadline: MonotonicDeadline,
    ) -> AdoptedOriginalNamespaceOccupancy {
        let (sealed, namespace) =
            OriginalNamespaceOccupancyHandoff::from_original(limit, cleanup, deadline)
                .unwrap()
                .into_authorities();
        unsafe {
            AdoptedOriginalNamespaceOccupancy::take_inherited(
                duplicate(sealed.file().as_raw_fd()),
                duplicate(namespace.file().as_raw_fd()),
            )
        }
        .unwrap()
    }

    #[test]
    fn borrowed_original_service_preserves_deadline_and_clock_owner() {
        let (limit, cleanup, deadline) = window();
        let adopted = adopt_original_bound(&limit, cleanup, deadline);
        let clock_fd = adopted.clock_namespace.inherited_descriptor().unwrap();
        for _ in 0..3 {
            let (observed_limit, observed_cleanup, observed_deadline) =
                adopted.original_service_parts().unwrap();
            assert_eq!(observed_limit, limit);
            assert_eq!(observed_cleanup, cleanup);
            assert_eq!(observed_deadline.deadline, adopted.deadline.deadline);
            assert_eq!(
                adopted.clock_namespace.inherited_descriptor().unwrap(),
                clock_fd
            );
            assert_ne!(unsafe { libc::fcntl(clock_fd as i32, libc::F_GETFD) }, -1);
        }
    }

    #[test]
    fn borrowed_original_contact_cutoff_does_not_reopen_service() {
        let (limit, cleanup, _) = window();
        let adopted = adopt_original_bound(
            &limit,
            cleanup,
            MonotonicDeadline::after(Duration::from_millis(100)),
        );
        std::thread::sleep(Duration::from_millis(120));
        assert!(adopted.original_service_parts().is_err());
        assert!(matches!(
            adopted.original_cleanup_window().unwrap(),
            OccupancyWindowState::Service { .. }
        ));
        assert!(adopted.original_service_parts().is_err());
        assert!(adopted.clock_namespace.file().metadata().is_ok());
    }

    #[test]
    fn borrowed_original_cleanup_and_expiry_remain_inside_original_maximum() {
        let cleanup = Duration::from_secs(1);
        let limit = OccupancyLimit::new(occupancy_now().unwrap(), 1_200_000_000).unwrap();
        let adopted = adopt_original_bound(
            &limit,
            cleanup,
            MonotonicDeadline::after(Duration::from_secs(2)),
        );
        std::thread::sleep(Duration::from_millis(220));
        assert!(adopted.original_service_parts().is_err());
        let remaining = match adopted.original_cleanup_window().unwrap() {
            OccupancyWindowState::Cleanup { remaining } => remaining,
            other => panic!("expected original cleanup reserve, observed {other:?}"),
        };
        assert!(remaining > Duration::ZERO && remaining <= cleanup);
        std::thread::sleep(remaining + Duration::from_millis(10));
        assert_eq!(
            adopted.original_cleanup_window().unwrap(),
            OccupancyWindowState::Expired
        );
        assert!(adopted.original_service_parts().is_err());
        assert!(adopted.clock_namespace.file().metadata().is_ok());
    }

    #[test]
    fn borrowed_original_bounds_refuse_substituted_clock_owner() {
        let (limit, cleanup, deadline) = window();
        let mut adopted = adopt_original_bound(&limit, cleanup, deadline);
        let original_clock = adopted.clock_namespace.clone();
        adopted.clock_namespace = crate::sealed_memfd(c"foreign-clock", b"not a clock").unwrap();
        assert!(adopted.original_service_parts().is_err());
        assert!(adopted.original_cleanup_window().is_err());
        assert!(original_clock.file().metadata().is_ok());
        assert!(adopted.clock_namespace.file().metadata().is_ok());
    }

    #[test]
    fn retained_pair_duplicate_preserves_objects_deadline_and_original_owners() {
        use std::os::unix::fs::MetadataExt as _;
        let (limit, cleanup, deadline) = window();
        let original =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline).unwrap();
        let copy = original.duplicate_retained_pair().unwrap();
        assert_eq!(
            copy.original_deadline.deadline,
            original.original_deadline.deadline
        );
        let mut coordinates = Vec::new();
        for authority in [
            &original.window,
            &original.clock_namespace,
            &copy.window,
            &copy.clock_namespace,
        ] {
            let fd = authority.inherited_descriptor().unwrap();
            assert!(fd > 2);
            assert!(!coordinates.contains(&fd));
            coordinates.push(fd);
            let flags = unsafe { libc::fcntl(authority.file().as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        for (source, duplicate) in [
            (&original.window, &copy.window),
            (&original.clock_namespace, &copy.clock_namespace),
        ] {
            let source = source.file().metadata().unwrap();
            let duplicate = duplicate.file().metadata().unwrap();
            assert_eq!(
                (source.dev(), source.ino()),
                (duplicate.dev(), duplicate.ino())
            );
        }
        drop(copy);
        original.require_live_pair().unwrap();
        let copy = original.duplicate_retained_pair().unwrap();
        drop(original);
        copy.require_live_pair().unwrap();
    }

    #[test]
    fn retained_pair_duplicate_refuses_expiry_without_consuming_originals() {
        let (limit, cleanup, deadline) = window();
        let mut original =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline).unwrap();
        let fds = [
            original.window.inherited_descriptor().unwrap(),
            original.clock_namespace.inherited_descriptor().unwrap(),
        ];
        original.original_deadline = MonotonicDeadline {
            deadline: Instant::now(),
        };
        assert!(original.duplicate_retained_pair().is_err());
        assert_eq!(original.window.inherited_descriptor().unwrap(), fds[0]);
        assert_eq!(original.clock_namespace.inherited_descriptor().unwrap(), fds[1]);
        for fd in fds {
            assert_ne!(unsafe { libc::fcntl(fd as i32, libc::F_GETFD) }, -1);
        }
    }

    #[test]
    fn retained_pair_duplicate_refuses_aliased_roles_without_consuming_originals() {
        let (limit, cleanup, deadline) = window();
        let mut original =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline).unwrap();
        original.clock_namespace = original.window.clone();
        let fd = original.window.inherited_descriptor().unwrap();
        assert!(original.duplicate_retained_pair().is_err());
        assert_eq!(original.clock_namespace.inherited_descriptor().unwrap(), fd);
        assert_ne!(unsafe { libc::fcntl(fd as i32, libc::F_GETFD) }, -1);
    }

    #[test]
    fn original_window_rejects_unsealed_foreign_clock_and_descriptor_alias() {
        let (limit, cleanup, deadline) = window();
        let (sealed, namespace) =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline)
                .unwrap()
                .into_authorities();
        let bytes = crate::sandbox::read_sealed_inherited_descriptor(
            sealed.inherited_descriptor().unwrap(),
            4096,
        )
        .unwrap();
        let mut unsealed = tempfile::tempfile().unwrap();
        unsealed.write_all(&bytes).unwrap();
        assert!(
            unsafe {
                AdoptedOriginalNamespaceOccupancy::take_inherited(
                    duplicate(unsealed.as_raw_fd()),
                    duplicate(namespace.file().as_raw_fd()),
                )
            }
            .is_err()
        );
        let foreign = std::fs::File::open("/dev/null").unwrap();
        assert!(
            unsafe {
                AdoptedOriginalNamespaceOccupancy::take_inherited(
                    duplicate(sealed.file().as_raw_fd()),
                    duplicate(foreign.as_raw_fd()),
                )
            }
            .is_err()
        );
        // Rejected aliases are not consumed: they cannot be uniquely owned twice.
        let coordinate = sealed.inherited_descriptor().unwrap();
        assert!(
            unsafe { AdoptedOriginalNamespaceOccupancy::take_inherited(coordinate, coordinate) }
                .is_err()
        );
        assert_ne!(
            unsafe { libc::fcntl(sealed.file().as_raw_fd(), libc::F_GETFD) },
            -1
        );
    }

    #[test]
    fn original_window_strict_wire_refuses_expired_foreign_and_partial() {
        let (limit, cleanup, deadline) = window();
        let (sealed, _namespace) =
            OriginalNamespaceOccupancyHandoff::from_original(&limit, cleanup, deadline)
                .unwrap()
                .into_authorities();
        let bytes = crate::sandbox::read_sealed_inherited_descriptor(
            sealed.inherited_descriptor().unwrap(),
            4096,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for name in ["version", "limit", "cleanup_ns", "contact_expiry_tick_ns"] {
            let mut altered = value.clone();
            altered.as_object_mut().unwrap().remove(name);
            assert!(adopt_bytes(&serde_json::to_vec(&altered).unwrap()).is_err());
        }
        for field in ["contract_digest", "incarnation_digest"] {
            let mut altered = value.clone();
            altered["limit"]["start"][field] = serde_json::json!("f".repeat(64));
            assert!(adopt_bytes(&serde_json::to_vec(&altered).unwrap()).is_err());
        }
        for field in ["clock_namespace_dev", "clock_namespace_ino", "cleanup_ns"] {
            let mut altered = value.clone();
            altered[field] = serde_json::json!(0);
            assert!(adopt_bytes(&serde_json::to_vec(&altered).unwrap()).is_err());
        }
        let mut altered = value.clone();
        altered["contact_expiry_tick_ns"] = serde_json::json!(0);
        assert!(adopt_bytes(&serde_json::to_vec(&altered).unwrap()).is_err());
        let mut altered = value.clone();
        altered["extra"] = serde_json::json!(true);
        assert!(adopt_bytes(&serde_json::to_vec(&altered).unwrap()).is_err());
        let text = std::str::from_utf8(&bytes)
            .unwrap()
            .replacen('{', "{\"version\":1,", 1);
        assert!(adopt_bytes(text.as_bytes()).is_err());
        assert!(adopt_bytes(&[b' '; 4097]).is_err());
    }
}

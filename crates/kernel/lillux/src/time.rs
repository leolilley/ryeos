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

    /// Remaining duration in this process-local monotonic clock domain.
    /// Returning zero is the only expiry representation exposed to callers;
    /// the host `Instant` never escapes Lillux.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
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
#[cfg(target_os = "linux")]
pub struct OriginalNamespaceOccupancyHandoff {
    window: crate::InheritedDescriptorAuthority,
    clock_namespace: crate::InheritedDescriptorAuthority,
}

#[cfg(target_os = "linux")]
impl OriginalNamespaceOccupancyHandoff {
    pub fn from_original(
        limit: &OccupancyLimit,
        cleanup: Duration,
        original_deadline: MonotonicDeadline,
    ) -> Result<Self, String> {
        let lease = crate::exec::retain_fork_sensitive_descriptors_until(original_deadline)
            .map_err(|error| format!("retain original-window descriptors: {error}"))?;
        let clock_namespace = original_window::pin_current_clock_namespace(&lease)?;
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
            window,
            clock_namespace,
        })
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
        // The sealed input has served its one-use role; no FD reaches target.
        drop(window);
        Ok(Self {
            wire,
            deadline,
            clock_namespace,
        })
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

//! Live observation of the original clock; never reconstruction from signed data.
//! Keep original namespace/boot descriptors and fixed scratch, avoiding contract
//! serialization, hashes, allocations and boot-file reopen in each VM callback.
use super::{Duration, OccupancyCoordinate, OccupancyLimit, OccupancyWindowState};

mod terminal;
pub use terminal::{RetainedClockCloseFailure, RetiredOccupancyClock};

pub struct RetainedOccupancyClock {
    process_id: u32,
    start: OccupancyCoordinate,
    binding: [u8; 64],
    #[cfg(target_os = "linux")]
    namespace: crate::InheritedDescriptorAuthority,
    #[cfg(target_os = "linux")]
    boot: crate::InheritedDescriptorAuthority,
    #[cfg(target_os = "linux")]
    namespace_device: u64,
    #[cfg(target_os = "linux")]
    namespace_inode: u64,
    #[cfg(target_os = "linux")]
    boot_bytes: [u8; 64],
    #[cfg(target_os = "linux")]
    boot_length: usize,
}

// Same persisted canonical clock binding, written directly into the hash.
// Only these validated ASCII digests enter the string fields.
fn fixed_binding_digest(device: u64, inode: u64, start: &OccupancyCoordinate)
    -> Result<[u8; 64], &'static str> {
    use sha2::Digest;
    if !crate::valid_hash(&start.contract_digest) || !crate::valid_hash(&start.incarnation_digest) {
        return Err("original clock digest is not canonical");
    }
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        clock_namespace_device: u64,
        clock_namespace_inode: u64,
        contract_digest: &'a str,
        incarnation_digest: &'a str,
    }
    struct HashWriter(sha2::Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let wire = Wire {
        clock_namespace_device: device, clock_namespace_inode: inode,
        contract_digest: &start.contract_digest,
        incarnation_digest: &start.incarnation_digest,
    };
    let mut writer = HashWriter(sha2::Sha256::new());
    serde_json::to_writer(&mut writer, &wire).map_err(|_| "canonical clock binding encoding failed")?;
    let digest = writer.0.finalize();
    let mut bytes = [0u8; 64];
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (out, byte) in bytes.chunks_exact_mut(2).zip(digest) {
        out[0] = HEX[(byte >> 4) as usize]; out[1] = HEX[(byte & 15) as usize];
    }
    Ok(bytes)
}

impl RetainedOccupancyClock {
    /// Capture actual native descriptors BEFORE the original operation begins.
    /// The short local bound only limits fork-sensitive descriptor registration;
    /// it grants no execution/resource authority or operation deadline.
    #[cfg(target_os = "linux")]
    pub fn capture() -> Result<Self, String> {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let lease = crate::exec::retain_fork_sensitive_descriptors_until(
            super::MonotonicDeadline::after(Duration::from_secs(1)),
        )
        .map_err(|error| format!("retain original occupancy clock: {error}"))?;
        let namespace = super::original_window::pin_current_clock_namespace(&lease)?;
        let boot = std::fs::File::open("/proc/sys/kernel/random/boot_id")
            .map_err(|error| format!("pin occupancy boot identity: {error}"))?;
        let boot = crate::exec::InheritedDescriptorAuthority::from_owned_file(boot, &lease)?;
        let mut boot_bytes = [0; 64];
        let boot_length = boot
            .file()
            .read_at(&mut boot_bytes, 0)
            .map_err(|error| format!("observe original boot identity: {error}"))?;
        if boot_length == 0 || boot_length == boot_bytes.len() {
            return Err("original boot identity is empty or exceeds fixed capacity".into());
        }
        let boot_text = std::str::from_utf8(&boot_bytes[..boot_length])
            .map_err(|error| format!("original boot identity is not UTF-8: {error}"))?;
        let start = super::occupancy_now()?;
        if crate::sha256_hex(boot_text.trim().as_bytes()) != start.incarnation_digest {
            return Err("occupancy boot identity changed during capture".into());
        }
        super::original_window::require_same_clock_namespace(namespace.file())?;
        let identity = namespace
            .file()
            .metadata()
            .map_err(|error| error.to_string())?;
        let binding = fixed_binding_digest(identity.dev(), identity.ino(), &start)
            .map_err(str::to_owned)?;
        Ok(Self {
            process_id: std::process::id(),
            start,
            binding,
            namespace,
            boot,
            namespace_device: identity.dev(),
            namespace_inode: identity.ino(),
            boot_bytes,
            boot_length,
        })
    }
    #[cfg(not(target_os = "linux"))]
    pub fn capture() -> Result<Self, String> {
        Err("retained occupancy clock unavailable on this platform".into())
    }
    pub fn start(&self) -> &OccupancyCoordinate {
        &self.start
    }
    pub fn binding_digest(&self) -> &str {
        std::str::from_utf8(&self.binding).expect("fixed lowercase digest is ASCII")
    }

    /// Capture conservative local endpoints from this actual original window.
    /// Sample the local anchor BEFORE observing remaining BOOTTIME so capture
    /// work cannot extend either endpoint. Callers retain this pair, not durations.
    pub fn original_local_deadlines(
        &self,
        limit: &OccupancyLimit,
        cleanup: Duration,
    ) -> Result<(super::MonotonicDeadline, super::MonotonicDeadline), String> {
        let anchor = std::time::Instant::now();
        let OccupancyWindowState::Service { remaining } = self.window(limit, cleanup)? else {
            return Err("original service interval exhausted during endpoint capture".into());
        };
        let total = remaining.checked_add(cleanup)
            .ok_or("original cleanup interval overflow")?;
        let service = anchor.checked_add(remaining)
            .ok_or("original service endpoint overflow")?;
        let cleanup = anchor.checked_add(total)
            .ok_or("original cleanup endpoint overflow")?;
        Ok((super::MonotonicDeadline { deadline: service },
            super::MonotonicDeadline { deadline: cleanup }))
    }

    /// Exact same live namespace and boot, original start and fresh BOOTTIME.
    /// Ordinary callers receive owned diagnostics; admission keeps fixed errors.
    pub fn window(&self, limit: &OccupancyLimit, cleanup: Duration)
        -> Result<OccupancyWindowState, String> {
        self.window_fixed(limit, cleanup).map_err(str::to_owned)
    }
    #[cfg(not(target_os = "linux"))]
    pub fn window_fixed(&self, _: &OccupancyLimit, _: Duration)
        -> Result<OccupancyWindowState, &'static str> {
        Err("retained occupancy clock unavailable on this platform")
    }
    #[cfg(target_os = "linux")]
    pub fn window_fixed(&self, limit: &OccupancyLimit, cleanup: Duration)
        -> Result<OccupancyWindowState, &'static str> {
        use std::os::fd::AsRawFd as _;
        if limit.start != self.start { return Err("retained clock selects another original start"); }
        let mut expected: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.namespace.file().as_raw_fd(), &mut expected) } != 0 {
            return Err("original clock namespace descriptor observation refused");
        }
        if (expected.st_dev, expected.st_ino) != (self.namespace_device, self.namespace_inode) {
            return Err("original clock namespace descriptor changed");
        }
        for path in [c"/proc/thread-self/ns/time", c"/proc/thread-self/ns/time_for_children"] {
            let mut current: libc::stat = unsafe { std::mem::zeroed() };
            // These two procfs namespace magic links are the exact clock
            // observations used by the retained-clock contract; follow them.
            if unsafe { libc::stat(path.as_ptr(), &mut current) } != 0 {
                return Err("current clock namespace observation refused");
            }
            if (current.st_dev, current.st_ino) != (self.namespace_device, self.namespace_inode) {
                return Err("retained occupancy clock cannot cross actual namespace");
            }
        }
        let mut bytes = [0u8; 64];
        let length = unsafe { libc::pread(self.boot.file().as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0) };
        if length < 0 { return Err("retained boot identity observation refused"); }
        let length = length as usize;
        if length != self.boot_length || bytes[..length] != self.boot_bytes[..self.boot_length] {
            return Err("retained occupancy boot identity changed");
        }
        self.window_at_tick(limit, cleanup, super::occupancy_tick_ns_fixed()?)
    }
    fn window_at_tick(
        &self,
        limit: &OccupancyLimit,
        cleanup: Duration,
        tick: u64,
    ) -> Result<OccupancyWindowState, &'static str> {
        if limit.start != self.start || limit.maximum_occupancy_ns == 0 {
            return Err("retained clock limit has another start or empty duration".into());
        }
        limit
            .start
            .tick_ns
            .checked_add(limit.maximum_occupancy_ns)
            .ok_or("retained occupancy interval overflow")?;
        let elapsed = tick
            .checked_sub(self.start.tick_ns)
            .ok_or("retained occupancy clock moved backwards")?;
        let cleanup_ns = u64::try_from(cleanup.as_nanos())
            .map_err(|_| "cleanup allowance overflow")?;
        if cleanup_ns >= limit.maximum_occupancy_ns {
            return Err("cleanup consumes original limit".into());
        }
        let service = limit.maximum_occupancy_ns - cleanup_ns;
        if elapsed < service {
            return Ok(OccupancyWindowState::Service {
                remaining: Duration::from_nanos(service - elapsed),
            });
        }
        if elapsed < limit.maximum_occupancy_ns {
            return Ok(OccupancyWindowState::Cleanup {
                remaining: Duration::from_nanos(limit.maximum_occupancy_ns - elapsed),
            });
        }
        Ok(OccupancyWindowState::Expired)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    #[test]
    fn fixed_binding_matches_canonical_protocol() {
        for (device, inode) in [(0, 1), (u64::MAX, u64::MAX)] {
            for digest in ["a".repeat(64), "F".repeat(64)] {
                let start = super::OccupancyCoordinate {
                    contract_digest: digest.clone(), incarnation_digest: digest,
                    tick_ns: 123,
                };
                let canonical = crate::canonical_json(&serde_json::json!({
                    "clock_namespace_device": device, "clock_namespace_inode": inode,
                    "contract_digest": start.contract_digest,
                    "incarnation_digest": start.incarnation_digest,
                })).unwrap();
                let actual = super::fixed_binding_digest(device, inode, &start).unwrap();
                assert_eq!(std::str::from_utf8(&actual).unwrap(), crate::sha256_hex(canonical.as_bytes()));
            }
        }
        let invalid = super::OccupancyCoordinate {
            contract_digest: "é".repeat(32), incarnation_digest: "a".repeat(64), tick_ns: 1,
        };
        assert!(super::fixed_binding_digest(1, 1, &invalid).is_err());
    }
    use super::*;
    #[test]
    fn live_original_observer_preserves_start_and_exact_phase_boundaries() {
        let clock = RetainedOccupancyClock::capture().unwrap();
        let limit = OccupancyLimit::new(clock.start().clone(), 10_000).unwrap();
        let cleanup = Duration::from_nanos(2_000);
        let tick = clock.start.tick_ns;
        assert_eq!(
            clock.window_at_tick(&limit, cleanup, tick + 7_999).unwrap(),
            OccupancyWindowState::Service {
                remaining: Duration::from_nanos(1)
            }
        );
        assert_eq!(
            clock.window_at_tick(&limit, cleanup, tick + 8_000).unwrap(),
            OccupancyWindowState::Cleanup {
                remaining: Duration::from_nanos(2_000)
            }
        );
        assert_eq!(
            clock
                .window_at_tick(&limit, cleanup, tick + 10_000)
                .unwrap(),
            OccupancyWindowState::Expired
        );
        assert!(clock.window_at_tick(&limit, cleanup, tick - 1).is_err());
        let mut changed = limit.clone();
        changed.start.tick_ns += 1;
        assert!(clock.window(&changed, cleanup).is_err());
        assert!(
            clock
                .window_at_tick(&limit, Duration::from_nanos(10_000), tick)
                .is_err()
        );
        let live = OccupancyLimit::new(clock.start().clone(), 60_000_000_000).unwrap();
        assert!(matches!(
            clock.window(&live, Duration::from_secs(1)).unwrap(),
            OccupancyWindowState::Service { .. }
        ));
        assert_eq!(clock.start(), &live.start);
        assert!(crate::valid_hash(clock.binding_digest()));
    }
    #[test]
    fn retained_boot_and_namespace_substitution_refuse_observation() {
        let mut clock = RetainedOccupancyClock::capture().unwrap();
        let limit = OccupancyLimit::new(clock.start().clone(), 60_000_000_000).unwrap();
        clock.boot_bytes[0] ^= 1;
        assert!(clock.window(&limit, Duration::from_secs(1)).is_err());
        clock.boot_bytes[0] ^= 1;
        clock.namespace_inode ^= 1;
        assert!(clock.window(&limit, Duration::from_secs(1)).is_err());
    }
}

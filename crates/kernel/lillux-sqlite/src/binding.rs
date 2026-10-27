//! Private native context. Never borrows an externally supplied connection.
use crate::{NativeRetentionReason, SqliteCopyLease};
use anyhow::{Result, ensure};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr::NonNull,
};

// Matches ryeos_sqlite_binding_create's maximum main-component byte length.
// The trailing zero is owned in this fixed array, with no global allocation.
pub(super) const BOUND_COMPONENT_MAX_BYTES: usize = 200;
pub(super) struct BoundComponent {
    bytes: [u8; BOUND_COMPONENT_MAX_BYTES + 1],
    length: usize,
}
impl BoundComponent {
    pub(super) fn new(name: &std::ffi::OsStr) -> Result<Self, &'static str> {
        let source = name.as_encoded_bytes();
        if source.is_empty()
            || source.len() > BOUND_COMPONENT_MAX_BYTES
            || source.contains(&0)
            || source.contains(&b'/')
            || source == b"."
            || source == b".."
        {
            return Err("invalid or oversized bound SQLite component");
        }
        let mut bytes = [0; BOUND_COMPONENT_MAX_BYTES + 1];
        bytes[..source.len()].copy_from_slice(source);
        Ok(Self {
            bytes,
            length: source.len(),
        })
    }
    pub(super) fn as_os_str(&self) -> &std::ffi::OsStr {
        // SAFETY: this exact complete encoding came from OsStr on this
        // platform; no truncation or byte transformation occurs.
        unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&self.bytes[..self.length]) }
    }
    fn as_ptr(&self) -> *const c_char {
        self.bytes.as_ptr().cast()
    }
}

unsafe extern "C" {
    fn ryeos_projection_lease_open(
        pin: c_int,
        mode: c_int,
        out: *mut *mut c_void,
        actual_fd: *mut c_int,
    ) -> c_int;
    fn ryeos_projection_lease_close(
        owner: *mut c_void,
        pin_consumed: *mut c_int,
        fd_consumed: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_close_original_finalizer_file(
        fd: c_int,
        device: u64,
        inode: u64,
        kind: c_int,
        consumed: *mut c_int,
        observed_errno: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_close_original_metadata_pin(
        fd: c_int,
        device: u64,
        inode: u64,
        consumed: *mut c_int,
        observed_errno: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_binding_main_identity(
        context: *mut c_void,
        dev: *mut u64,
        ino: *mut u64,
    ) -> c_int;
    fn ryeos_sqlite_binding_main_descriptor(context: *mut c_void, fd: *mut c_int) -> c_int;
    fn ryeos_sqlite_binding_main_empty(context: *mut c_void, empty: *mut c_int) -> c_int;
    fn ryeos_sqlite_binding_sync_live(
        context: *mut c_void,
        db: *mut libsqlite3_sys::sqlite3,
        wal_synced: *mut c_int,
        main_synced: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_sync_closed_main(
        pin: c_int,
        wal: c_int,
        dev: u64,
        ino: u64,
        synced: *mut c_int,
        close_attempted: *mut c_int,
        close_error: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_close_original_output(
        fd: c_int,
        device: u64,
        inode: u64,
        maximum: c_int,
        consumed: *mut c_int,
        observed_errno: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_read_unbound_regular(
        pin: c_int,
        buffer: *mut u8,
        capacity: c_int,
        written: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_read_unbound_witness(
        pin: c_int,
        buffer: *mut u8,
        capacity: c_int,
        written: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_read_unbound_regular_chunk(
        pin: c_int,
        offset: u64,
        expected_length: u64,
        buffer: *mut u8,
        capacity: c_int,
        written: *mut c_int,
    ) -> c_int;
    fn ryeos_sqlite_stopped_create(
        root: c_int,
        names: *const *const c_char,
        count: c_int,
        selected: *const c_int,
        dev: *const u64,
        ino: *const u64,
        length: *const u64,
        out: *mut *mut c_void,
    ) -> c_int;
    fn ryeos_sqlite_stopped_acquire(context: *mut c_void) -> c_int;

    fn ryeos_sqlite_binding_create(
        root: c_int,
        name: *const c_char,
        mode: c_int,
        lease: *mut c_void,
        out: *mut *mut c_void,
    ) -> c_int;
    fn ryeos_sqlite_binding_created(context: *mut c_void) -> c_int;
    fn ryeos_sqlite_retirement_capture(context: *mut c_void, generation: *const c_char) -> c_int;
    fn ryeos_sqlite_retirement_barriers(context: *mut c_void) -> c_int;
    fn ryeos_sqlite_retirement_role_fd(context: *mut c_void, role: c_int) -> c_int;
    fn ryeos_sqlite_retirement_allow_release(context: *mut c_void) -> c_int;
    fn ryeos_sqlite_binding_drop(context: *mut c_void);
    fn ryeos_sqlite_binding_release(context: *mut c_void, lease: *mut *mut c_void) -> c_int;
    fn ryeos_sqlite_take_ready_copy_lease() -> *mut c_void;
    fn ryeos_sqlite_binding_abandon(context: *mut c_void, db: *mut libsqlite3_sys::sqlite3);
    fn ryeos_sqlite_binding_name(context: *mut c_void) -> *const c_char;
    fn ryeos_sqlite_binding_path(context: *mut c_void) -> *const c_char;
    fn ryeos_sqlite_binding_validate(
        context: *mut c_void,
        db: *mut libsqlite3_sys::sqlite3,
        all: c_int,
    ) -> c_int;
}

pub(super) struct LiveSyncOutcome {
    pub(super) code: c_int,
    pub(super) wal_synced: bool,
    pub(super) main_synced: bool,
}

/// Actual syscall-stage facts, never an error-only summary or authority.
pub(super) struct ClosedMainSyncOutcome {
    pub(super) code: c_int,
    pub(super) synced: bool,
    pub(super) close_attempted: bool,
    pub(super) close_error: c_int,
}
impl ClosedMainSyncOutcome {
    pub(super) fn require_success(&self) -> Result<()> {
        ensure!(
            self.code == libsqlite3_sys::SQLITE_OK
                && self.synced
                && self.close_attempted
                && self.close_error == 0,
            "original closed main fsync refused (SQLite {}, close errno {})",
            self.code,
            self.close_error
        );
        Ok(())
    }
}
#[cfg(target_os = "linux")]
pub(super) fn sync_closed_main(
    pin: &std::fs::File,
    wal: Option<&std::fs::File>,
    identity: (u64, u64),
) -> ClosedMainSyncOutcome {
    use std::os::fd::AsRawFd;
    let (mut synced, mut close_attempted, mut close_error) = (0, 0, 0);
    let code = unsafe {
        ryeos_sqlite_sync_closed_main(
            pin.as_raw_fd(),
            wal.map_or(-1, AsRawFd::as_raw_fd),
            identity.0,
            identity.1,
            &mut synced,
            &mut close_attempted,
            &mut close_error,
        )
    };
    ClosedMainSyncOutcome {
        code,
        synced: synced == 1,
        close_attempted: close_attempted == 1,
        close_error,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn read_unbound_regular(pin: &std::fs::File, maximum_bytes: usize) -> Result<Vec<u8>> {
    read_unbound(pin, maximum_bytes, ryeos_sqlite_read_unbound_regular)
}

/// The same guarded read of a retained original witness, whose projection
/// lease census is not charged to the arena budget. It refuses exactly as
/// [`read_unbound_regular`] does.
#[cfg(target_os = "linux")]
pub(super) fn read_unbound_witness(pin: &std::fs::File, maximum_bytes: usize) -> Result<Vec<u8>> {
    read_unbound(pin, maximum_bytes, ryeos_sqlite_read_unbound_witness)
}

#[cfg(target_os = "linux")]
fn read_unbound(
    pin: &std::fs::File,
    maximum_bytes: usize,
    read: unsafe extern "C" fn(c_int, *mut u8, c_int, *mut c_int) -> c_int,
) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    ensure!(
        maximum_bytes > 0 && maximum_bytes <= 262_144,
        "guarded SQLite evidence read exceeds its bound"
    );
    // Initialized storage is caller-owned before the native gate is acquired.
    // The extra byte distinguishes growth beyond the admitted bound.
    let mut bytes = vec![0u8; maximum_bytes + 1];
    let mut written = 0;
    let code = unsafe {
        read(
            pin.as_raw_fd(),
            bytes.as_mut_ptr(),
            bytes.len() as c_int,
            &mut written,
        )
    };
    if code == libsqlite3_sys::SQLITE_BUSY {
        // Another owner holds the file's locks: this read is refused, but
        // no admission fact failed, so service refusal does not latch.
        return Err(anyhow::Error::new(crate::NativeCheckContended)
            .context(format!("guarded SQLite evidence read refused ({code})")));
    }
    ensure!(
        code == libsqlite3_sys::SQLITE_OK,
        "guarded SQLite evidence read refused ({code})"
    );
    ensure!(
        written >= 0 && (written as usize) <= maximum_bytes,
        "guarded SQLite evidence grew beyond its bound"
    );
    bytes.truncate(written as usize);
    Ok(bytes)
}

#[cfg(target_os = "linux")]
pub(super) fn read_unbound_regular_chunk(
    pin: &std::fs::File,
    offset: u64,
    expected_length: u64,
    buffer: &mut [u8],
) -> Result<usize> {
    use std::os::fd::AsRawFd;
    ensure!(
        !buffer.is_empty()
            && buffer.len() <= 65_536
            && offset <= expected_length
            && expected_length <= i64::MAX as u64,
        "guarded SQLite copy chunk exceeds its bounds"
    );
    let mut written = 0;
    let code = unsafe {
        ryeos_sqlite_read_unbound_regular_chunk(
            pin.as_raw_fd(),
            offset,
            expected_length,
            buffer.as_mut_ptr(),
            buffer.len() as c_int,
            &mut written,
        )
    };
    ensure!(
        code == libsqlite3_sys::SQLITE_OK,
        "guarded SQLite copy chunk refused ({code})"
    );
    let expected = (expected_length - offset).min(buffer.len() as u64) as usize;
    ensure!(
        written >= 0 && written as usize == expected,
        "guarded SQLite copy chunk changed length"
    );
    Ok(written as usize)
}

pub(super) struct OriginalOutputClose {
    pub(super) code: c_int,
    pub(super) consumed: bool,
    pub(super) observed_errno: c_int,
}
pub(super) fn close_original_output(
    fd: c_int,
    device: u64,
    inode: u64,
    maximum: u32,
) -> OriginalOutputClose {
    if maximum == 0 || maximum > c_int::MAX as u32 {
        return OriginalOutputClose {
            code: libsqlite3_sys::SQLITE_MISUSE,
            consumed: false,
            observed_errno: 0,
        };
    }
    let mut consumed = 0;
    let mut observed_errno = 0;
    let code = unsafe {
        ryeos_sqlite_close_original_output(
            fd,
            device,
            inode,
            maximum as c_int,
            &mut consumed,
            &mut observed_errno,
        )
    };
    OriginalOutputClose {
        code,
        consumed: consumed != 0,
        observed_errno,
    }
}

pub(super) struct ProjectionLease(NonNull<c_void>);
unsafe impl Send for ProjectionLease {}
impl ProjectionLease {
    pub(super) fn open(pin: c_int, mode: c_int) -> (c_int, Option<Self>, Option<c_int>) {
        let mut out = std::ptr::null_mut();
        let mut actual_fd = -1;
        let code = unsafe { ryeos_projection_lease_open(pin, mode, &mut out, &mut actual_fd) };
        (
            code,
            NonNull::new(out).map(Self),
            (actual_fd >= 0).then_some(actual_fd),
        )
    }
    pub(super) fn close(&self) -> (c_int, bool, bool) {
        let (mut consumed, mut fd_consumed) = (0, 0);
        let code = unsafe {
            ryeos_projection_lease_close(self.0.as_ptr(), &mut consumed, &mut fd_consumed)
        };
        (code, consumed == 1, fd_consumed == 1)
    }
}
// No Drop: only explicit original cleanup disposes the native lease owner.

pub(super) struct Binding(Option<NonNull<c_void>>);
// SAFETY: the outer owner opens exactly one FULL_MUTEX connection. Native
// context admits one main handle, serializes callbacks under that connection,
// and retains descriptors until connection close. This permits moving the
// owner between threads; it does not make Connection or Binding Sync.
unsafe impl Send for Binding {}
impl Binding {
    #[cfg(target_os = "linux")]
    pub(super) fn create(
        root: &std::fs::File,
        name: &std::ffi::OsStr,
        mode: i32,
        lease: Option<SqliteCopyLease>,
    ) -> std::result::Result<Self, (anyhow::Error, Option<Self>)> {
        use std::os::fd::AsRawFd;
        let name =
            BoundComponent::new(name).map_err(|reason| (anyhow::Error::msg(reason), None))?;
        let mut pointer = std::ptr::null_mut();
        let lease = lease
            .map(|lease| Box::into_raw(Box::new(lease)).cast::<c_void>())
            .unwrap_or(std::ptr::null_mut());
        // Native returns the same partial context on any post-allocation error.
        let status = unsafe {
            ryeos_sqlite_binding_create(root.as_raw_fd(), name.as_ptr(), mode, lease, &mut pointer)
        };
        let binding = NonNull::new(pointer).map(|p| Self(Some(p)));
        if binding.is_none() {
            // Native did not allocate a context: transfer never occurred.
            unsafe { drop_copy_lease(lease) };
        }
        if status != libsqlite3_sys::SQLITE_OK {
            return Err((
                anyhow::anyhow!("bound SQLite acquisition refused ({status})"),
                binding,
            ));
        }
        binding.ok_or_else(|| (anyhow::anyhow!("native binding absent"), None))
    }
    #[cfg(target_os = "linux")]
    pub(super) fn create_stopped(
        root: &std::fs::File,
        names: &[String],
        selected: [i32; 3],
        expected: &[Option<crate::SqliteRoleIdentity>; 3],
    ) -> std::result::Result<Self, (anyhow::Error, Option<Self>)> {
        use std::os::fd::AsRawFd;
        let names = names
            .iter()
            .map(|name| CString::new(name.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| (error.into(), None))?;
        let pointers = names.iter().map(|name| name.as_ptr()).collect::<Vec<_>>();
        let dev = std::array::from_fn::<_, 3, _>(|i| {
            expected[i].as_ref().map_or(0, |identity| identity.device)
        });
        let ino = std::array::from_fn::<_, 3, _>(|i| {
            expected[i].as_ref().map_or(0, |identity| identity.inode)
        });
        let length = std::array::from_fn::<_, 3, _>(|i| {
            expected[i]
                .as_ref()
                .map_or(0, |identity| identity.byte_length)
        });
        let mut pointer = std::ptr::null_mut();
        let status = unsafe {
            ryeos_sqlite_stopped_create(
                root.as_raw_fd(),
                pointers.as_ptr(),
                pointers.len() as c_int,
                selected.as_ptr(),
                dev.as_ptr(),
                ino.as_ptr(),
                length.as_ptr(),
                &mut pointer,
            )
        };
        let binding = NonNull::new(pointer).map(|pointer| Self(Some(pointer)));
        if status != libsqlite3_sys::SQLITE_OK {
            return Err((
                anyhow::anyhow!("stopped SQLite intake allocation/claim refused ({status})"),
                binding,
            ));
        }
        binding.ok_or_else(|| (anyhow::anyhow!("stopped native owner absent"), None))
    }
    pub(super) fn acquire_stopped(&self) -> Result<()> {
        let status =
            unsafe { ryeos_sqlite_stopped_acquire(self.0.expect("same stopped owner").as_ptr()) };
        ensure!(
            status == libsqlite3_sys::SQLITE_OK,
            "stopped SQLite current-object acquisition refused ({status})"
        );
        Ok(())
    }
    pub(super) fn created(&self) -> bool {
        unsafe { ryeos_sqlite_binding_created(self.0.expect("owned context").as_ptr()) != 0 }
    }
    pub(super) fn capture_retirement(&self, generation: [u8; 16]) -> Result<()> {
        let generation = CString::new(
            generation
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        )?;
        let status = unsafe {
            ryeos_sqlite_retirement_capture(
                self.0.expect("owned context").as_ptr(),
                generation.as_ptr(),
            )
        };
        ensure!(
            status == libsqlite3_sys::SQLITE_OK,
            "native role capture refused ({status})"
        );
        Ok(())
    }
    pub(super) fn retirement_barriers(&self) -> Result<()> {
        let status =
            unsafe { ryeos_sqlite_retirement_barriers(self.0.expect("owned context").as_ptr()) };
        ensure!(
            status == libsqlite3_sys::SQLITE_OK,
            "native retirement peer barrier refused ({status})"
        );
        Ok(())
    }
    pub(super) fn allow_retirement_release(&self) -> Result<()> {
        let status = unsafe {
            ryeos_sqlite_retirement_allow_release(self.0.expect("owned context").as_ptr())
        };
        ensure!(
            status == libsqlite3_sys::SQLITE_OK,
            "native retirement release refused ({status})"
        );
        Ok(())
    }
    /// Borrow the SAME retained descriptor without dup/open/close. The private
    /// view cannot escape the host operation's lifetime and gives the
    /// descriptor back unclosed when it ends.
    pub(super) fn with_retirement_role<T>(
        &self,
        role: usize,
        inspect: impl FnOnce(Option<&std::fs::File>) -> Result<T>,
    ) -> Result<T> {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;
            let descriptor = unsafe {
                ryeos_sqlite_retirement_role_fd(
                    self.0.expect("owned context").as_ptr(),
                    role as c_int,
                )
            };
            if descriptor < 0 {
                return inspect(None);
            }
            // SAFETY: the descriptor is the live retained role descriptor,
            // valid for this call; the view hands it back unclosed on every
            // exit, so ownership never leaves the native binding.
            let view = BorrowedRoleView(Some(unsafe { std::fs::File::from_raw_fd(descriptor) }));
            inspect(view.0.as_ref())
        }
        #[cfg(not(unix))]
        {
            let _ = (role, inspect);
            anyhow::bail!("SQLite retirement descriptor borrow requires Unix")
        }
    }
    /// Drop fallback hands actual failed-close DB/context to reachable native
    /// custody. No retry/contact/settlement occurs without supervised caller.
    pub(super) fn abandon(&mut self, connection: &rusqlite::Connection) {
        if let Some(pointer) = self.0.take() {
            // SAFETY: exact owned live DB from failed close; native retains it
            // and the context under global mutex before Rust relinquishes it.
            unsafe { ryeos_sqlite_binding_abandon(pointer.as_ptr(), connection.handle()) }
        }
    }
    /// Called only after successful SQL close; native retains the caller-held
    /// context across Pending and cannot free it during background progress.
    pub(super) fn release(&mut self) -> Option<NativeRetentionReason> {
        let pointer = self.0.expect("owned native context");
        // SAFETY: same unique context, no live Rust Connection; native serializes
        // pending ownership and free under Unix global mutex.
        let mut lease = std::ptr::null_mut();
        let status = unsafe { ryeos_sqlite_binding_release(pointer.as_ptr(), &mut lease) };
        let reason = match status {
            0 => {
                self.0 = None;
                // Native context and every resource are gone; no SQLite mutex
                // is held when the opaque copy owner destructor runs.
                unsafe { drop_copy_lease(lease) };
                None
            }
            1 => Some(NativeRetentionReason::TrackedPeer),
            2 => Some(NativeRetentionReason::UnknownIdentity),
            3 => Some(NativeRetentionReason::LiveNativeFile),
            4 => Some(NativeRetentionReason::CloseError),
            _ => Some(NativeRetentionReason::UnknownIdentity),
        };
        // Other contexts may have become ready while this owner's release
        // remained Pending. Drain once on both success and failure paths.
        drain_ready_copy_leases();
        reason
    }
    pub(super) fn name(&self) -> Result<&str> {
        // SAFETY: owned registered context retains its NUL-terminated name.
        Ok(unsafe {
            CStr::from_ptr(ryeos_sqlite_binding_name(
                self.0.expect("owned native context").as_ptr(),
            ))
        }
        .to_str()?)
    }
    pub(super) fn path(&self) -> Result<&str> {
        // SAFETY: owned context retains its NUL-terminated main path.
        Ok(unsafe {
            CStr::from_ptr(ryeos_sqlite_binding_path(
                self.0.expect("owned native context").as_ptr(),
            ))
        }
        .to_str()?)
    }
    pub(super) fn main_identity(&self) -> Result<(u64, u64)> {
        let (mut dev, mut ino) = (0, 0);
        let code = unsafe {
            ryeos_sqlite_binding_main_identity(
                self.0.expect("owned native context").as_ptr(),
                &mut dev,
                &mut ino,
            )
        };
        ensure!(
            code == libsqlite3_sys::SQLITE_OK,
            "original native main identity refused ({code})"
        );
        Ok((dev, ino))
    }
    /// The durable identity Lillux computes from the live main keeper's own
    /// descriptor, after confirming the descriptor still names `raw`, the
    /// `(st_dev, st_ino)` the keeper reported for this same operation.
    pub(super) fn main_file_identity(
        &self,
        raw: (u64, u64),
    ) -> Result<lillux::PinnedRegularFileIdentity> {
        let mut fd: c_int = -1;
        let code = unsafe {
            ryeos_sqlite_binding_main_descriptor(
                self.0.expect("owned native context").as_ptr(),
                &mut fd,
            )
        };
        ensure!(
            code == libsqlite3_sys::SQLITE_OK && fd >= 0,
            "original native main descriptor refused ({code})"
        );
        // SAFETY: the keeper descriptor stays open and unchanged for the
        // duration of this call because the binding is borrowed and Lillux
        // duplicates it before use.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let identity = lillux::pinned_regular_file_identity_of_descriptor(borrowed)?;
        let duplicate = borrowed.try_clone_to_owned()?;
        let observed = lillux::observe_open_file_identity(&std::fs::File::from(duplicate))?;
        ensure!(
            (observed.device(), observed.inode()) == raw,
            "original native main keeper changed identity while it was observed"
        );
        Ok(identity)
    }
    /// The retained existing main keeper is zero bytes and no WAL role was
    /// found when it was acquired.
    pub(super) fn main_empty(&self) -> Result<bool> {
        let mut empty = 0;
        let code = unsafe {
            ryeos_sqlite_binding_main_empty(
                self.0.expect("owned native context").as_ptr(),
                &mut empty,
            )
        };
        ensure!(
            code == libsqlite3_sys::SQLITE_OK,
            "original native main emptiness refused ({code})"
        );
        Ok(empty != 0)
    }
    pub(super) fn sync_live(&self, connection: &rusqlite::Connection) -> LiveSyncOutcome {
        let (mut wal_synced, mut main_synced) = (0, 0);
        let code = unsafe {
            ryeos_sqlite_binding_sync_live(
                self.0.expect("owned native context").as_ptr(),
                connection.handle(),
                &mut wal_synced,
                &mut main_synced,
            )
        };
        LiveSyncOutcome {
            code,
            wal_synced: wal_synced == 1,
            main_synced: main_synced == 1,
        }
    }
    pub(super) fn validate(&self, connection: &rusqlite::Connection, all: bool) -> Result<()> {
        // SAFETY: this backend owns the connection and context; native locks
        // connection before official file lookup and Unix shared-node lookup.
        let status = unsafe {
            ryeos_sqlite_binding_validate(
                self.0.expect("owned native context").as_ptr(),
                connection.handle(),
                i32::from(all),
            )
        };
        ensure!(
            status == libsqlite3_sys::SQLITE_OK,
            "bound SQLite descriptor validation refused ({status})"
        );
        Ok(())
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        // SAFETY: outer owner closes connection before releasing context.
        if let Some(pointer) = self.0.take() {
            unsafe { ryeos_sqlite_binding_drop(pointer.as_ptr()) }
        }
        drain_ready_copy_leases();
    }
}
/// One ready node per call, taken/freed under native mutex; Rust destruction
/// follows after that mutex is released. Unresolved contexts never enter it.
fn drain_ready_copy_leases() {
    // Bound each entrypoint's work; leftover nodes remain reachable natively.
    for _ in 0..64 {
        let lease = unsafe { ryeos_sqlite_take_ready_copy_lease() };
        if lease.is_null() {
            break;
        }
        unsafe { drop_copy_lease(lease) };
    }
}
unsafe fn drop_copy_lease(lease: *mut c_void) {
    if !lease.is_null() {
        // Exactly the Box allocated above, detached once by native release.
        unsafe { drop(Box::from_raw(lease.cast::<SqliteCopyLease>())) };
    }
}

pub(super) fn close_original_metadata_pin(
    fd: c_int,
    device: u64,
    inode: u64,
) -> OriginalOutputClose {
    let mut consumed = 0;
    let mut observed_errno = 0;
    let code = unsafe {
        ryeos_sqlite_close_original_metadata_pin(
            fd,
            device,
            inode,
            &mut consumed,
            &mut observed_errno,
        )
    };
    OriginalOutputClose {
        code,
        consumed: consumed != 0,
        observed_errno,
    }
}

pub(super) fn close_original_finalizer_file(
    fd: c_int,
    device: u64,
    inode: u64,
    kind: c_int,
) -> OriginalOutputClose {
    let mut consumed = 0;
    let mut observed_errno = 0;
    let code = unsafe {
        ryeos_sqlite_close_original_finalizer_file(
            fd,
            device,
            inode,
            kind,
            &mut consumed,
            &mut observed_errno,
        )
    };
    OriginalOutputClose {
        code,
        consumed: consumed != 0,
        observed_errno,
    }
}

/// A `File` view of a descriptor the native binding keeps owning: its end
/// releases the descriptor number without closing it.
#[cfg(unix)]
struct BorrowedRoleView(Option<std::fs::File>);
#[cfg(unix)]
impl Drop for BorrowedRoleView {
    fn drop(&mut self) {
        use std::os::fd::IntoRawFd;
        if let Some(file) = self.0.take() {
            let _ = file.into_raw_fd();
        }
    }
}

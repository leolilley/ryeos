//! Sealed single-use secret payload descriptors.
//!
//! A parent that must hand secret bytes to one trusted child seals them into
//! an anonymous in-memory file and lets the child inherit that descriptor. The
//! bytes never enter an environment, argument vector, pathname, or the
//! filesystem. The child adopts the descriptor from a named environment slot
//! (which carries only the descriptor number), verifies that it is an
//! immutable sealed memory file, reads the whole payload into a zeroizing
//! buffer under a byte bound, and closes the descriptor.
//!
//! The payload's own format belongs to the caller; this module moves bytes.

use zeroize::Zeroizing;

use crate::InheritedDescriptorAuthority;

/// Diagnostic name of the anonymous file. It appears only as the target of
/// the descriptor's `/proc` link and carries no secret meaning.
const SEALED_SECRET_FILE_NAME: &std::ffi::CStr = c"ryeos-sealed-secret";

/// Seal `payload` into an immutable anonymous file.
///
/// The returned descriptor keeps `FD_CLOEXEC`, so it reaches a child only when
/// the caller explicitly retains it for that child's launch. The file is
/// owner-private and carries the shrink, grow, write and seal-seal seals, so
/// neither the parent nor the child can alter the bytes afterwards.
pub fn seal_secret_payload(payload: &[u8]) -> Result<InheritedDescriptorAuthority, String> {
    crate::sealed_memfd(SEALED_SECRET_FILE_NAME, payload)
}

/// Adopt the sealed secret descriptor whose number is held in the environment
/// variable `name`, read its payload, and close it.
///
/// The environment slot is consumed (removed) before anything else is checked,
/// so the descriptor number cannot reach a descendant even when the payload is
/// refused. The payload is refused if the descriptor is not a regular sealed
/// memory file carrying all four seals, or if it is longer than
/// `maximum_bytes`. Errors never carry payload bytes.
///
/// # Safety
/// The trusted parent must have installed the named descriptor exactly once
/// for this process and no Rust owner may already exist for it. Call only
/// during single-threaded startup, before any concurrent environment access.
pub unsafe fn take_sealed_secret_payload_from_env(
    name: &str,
    maximum_bytes: usize,
) -> Result<Zeroizing<Vec<u8>>, String> {
    // SAFETY: upheld by this function's caller.
    let authority = unsafe { crate::take_inherited_descriptor_authority_from_env(name)? };
    read_sealed_secret_payload(authority, maximum_bytes)
}

/// Read the whole payload of one sealed secret descriptor, then close it.
///
/// The read uses positional I/O, so it never moves the file offset shared with
/// any other holder of the same open file description.
pub fn read_sealed_secret_payload(
    descriptor: InheritedDescriptorAuthority,
    maximum_bytes: usize,
) -> Result<Zeroizing<Vec<u8>>, String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::FileExt as _;

        let file = descriptor.file();
        let raw = file.as_raw_fd();
        let mut observed: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(raw, &mut observed) } != 0 {
            return Err(format!(
                "inspect sealed secret descriptor: {}",
                std::io::Error::last_os_error()
            ));
        }
        if observed.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err("sealed secret descriptor is not a regular file".to_owned());
        }
        // F_GET_SEALS succeeds only on a sealing-capable anonymous file, so a
        // pipe, socket or ordinary file is refused here.
        let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
        if seals < 0 {
            return Err("sealed secret descriptor is not a sealable memory file".to_owned());
        }
        let required =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        if seals & required != required {
            return Err("sealed secret descriptor is missing required seals".to_owned());
        }
        let length = u64::try_from(observed.st_size)
            .map_err(|_| "sealed secret descriptor has a negative size".to_owned())?;
        if length > maximum_bytes as u64 {
            return Err("sealed secret payload exceeds its byte bound".to_owned());
        }
        let length = usize::try_from(length)
            .map_err(|_| "sealed secret payload exceeds the address space".to_owned())?;
        let mut payload = Zeroizing::new(vec![0_u8; length]);
        file.read_exact_at(&mut payload, 0)
            .map_err(|error| format!("read sealed secret payload: {error}"))?;
        // `descriptor` drops here, closing the coordinate.
        Ok(payload)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (descriptor, maximum_bytes);
        Err("sealed secret descriptors are supported only on Linux".to_owned())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd as _;

    #[test]
    fn payload_round_trips_and_descriptor_is_cloexec() {
        let sealed = seal_secret_payload(b"first\0second").unwrap();
        let raw = sealed.inherited_descriptor().unwrap() as i32;
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        let payload = read_sealed_secret_payload(sealed, 64).unwrap();
        assert_eq!(payload.as_slice(), b"first\0second");
    }

    #[test]
    fn sealed_file_cannot_be_written_or_resized() {
        let sealed = seal_secret_payload(b"value").unwrap();
        let path = sealed.path().to_path_buf();
        // F_SEAL_WRITE permits an open for writing but refuses every write.
        if let Ok(mut writable) = std::fs::OpenOptions::new().write(true).open(&path) {
            use std::io::Write;
            assert!(writable.write_all(b"changed").is_err());
        }
        let raw = sealed.inherited_descriptor().unwrap() as i32;
        let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
        let required =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & required, required);
        assert!(unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, 0) } < 0);
    }

    #[test]
    fn read_does_not_move_the_shared_offset() {
        let sealed = seal_secret_payload(b"abcdef").unwrap();
        let again = sealed.clone();
        let raw = sealed.inherited_descriptor().unwrap() as i32;
        let before = unsafe { libc::lseek(raw, 0, libc::SEEK_CUR) };
        assert_eq!(
            read_sealed_secret_payload(sealed, 16).unwrap().as_slice(),
            b"abcdef"
        );
        assert_eq!(unsafe { libc::lseek(raw, 0, libc::SEEK_CUR) }, before);
        drop(again);
    }

    #[test]
    fn oversize_payload_is_refused_without_a_value() {
        let sealed = seal_secret_payload(b"0123456789").unwrap();
        let error = read_sealed_secret_payload(sealed, 9).unwrap_err();
        assert!(error.contains("byte bound"), "{error}");
        assert!(!error.contains("0123456789"));
    }

    fn adopt(file: std::fs::File) -> InheritedDescriptorAuthority {
        let lease = crate::try_retain_fork_sensitive_descriptors().unwrap();
        InheritedDescriptorAuthority::from_owned_file(file, &lease).unwrap()
    }

    #[test]
    fn unsealed_memfd_is_refused() {
        let fd = unsafe {
            libc::memfd_create(
                c"unsealed".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(fd >= 0);
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let error = read_sealed_secret_payload(adopt(file), 16).unwrap_err();
        assert!(error.contains("missing required seals"), "{error}");
    }

    #[test]
    fn partially_sealed_memfd_is_refused() {
        let fd = unsafe {
            libc::memfd_create(
                c"partial".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(fd >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_WRITE) },
            0
        );
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let error = read_sealed_secret_payload(adopt(file), 16).unwrap_err();
        assert!(error.contains("missing required seals"), "{error}");
    }

    #[test]
    fn ordinary_file_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plain");
        std::fs::write(&path, b"VALUE-7f3c").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let error = read_sealed_secret_payload(adopt(file), 16).unwrap_err();
        assert!(error.contains("not a sealable memory file"), "{error}");
        assert!(!error.contains("VALUE-7f3c"), "{error}");
    }
}

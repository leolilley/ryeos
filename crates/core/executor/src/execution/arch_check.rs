//! Architecture mismatch detection for native binaries.
//!
//! Reads ELF headers to determine the target architecture and compares
//! against the host architecture. Prevents execution of binaries
//! compiled for the wrong platform.

use anyhow::Result;

/// Errors that can occur during architecture checks.
#[derive(Debug)]
pub enum ArchMismatchError {
    /// The binary's magic bytes don't match any supported format.
    UnsupportedFormat {
        magic: u64,
    },
    /// The binary targets a different architecture than the host.
    Mismatch {
        binary_arch: String,
        host_arch: String,
    },
}

impl std::fmt::Display for ArchMismatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat { magic } => {
                write!(f, "unsupported binary format (magic: 0x{magic:08x})")
            }
            Self::Mismatch { binary_arch, host_arch } => {
                write!(f, "architecture mismatch: binary is {binary_arch}, host is {host_arch}")
            }
        }
    }
}

impl std::error::Error for ArchMismatchError {}

/// Map ELF e_machine values to architecture names.
fn elf_arch_name(e_machine: u16) -> Option<&'static str> {
    match e_machine {
        0x03 => Some("x86"),       // EM_386
        0x3E => Some("x86_64"),    // EM_X86_64
        0x28 => Some("arm"),       // EM_ARM
        0xB7 => Some("aarch64"),   // EM_AARCH64
        0xF3 => Some("riscv64"),   // EM_RISCV
        _ => None,
    }
}

/// Check that a binary blob targets the host architecture.
///
/// Reads the first 16 bytes of `blob_bytes` to identify the format:
/// - ELF: reads e_machine and compares against `host_arch`
/// - Anything else: returns `UnsupportedFormat` (Linux-only ELF)
///
/// Returns `Ok(())` if the binary is a valid ELF for the host arch.
/// Too-short blobs (< 16 bytes) pass through (don't block).
pub fn check_arch(blob_bytes: &[u8], host_arch: &str) -> Result<(), ArchMismatchError> {
    if blob_bytes.len() < 16 {
        return Ok(()); // Too short to determine — don't block
    }

    // Check ELF magic: 0x7f 'E' 'L' 'F'
    if blob_bytes[0] == 0x7f && &blob_bytes[1..4] == b"ELF" {
        // ELF binary — read e_machine at offset 18-19 (little-endian u16)
        let e_machine = u16::from_le_bytes([blob_bytes[18], blob_bytes[19]]);
        let binary_arch = elf_arch_name(e_machine)
            .unwrap_or("unknown")
            .to_string();

        if binary_arch != host_arch {
            return Err(ArchMismatchError::Mismatch {
                binary_arch,
                host_arch: host_arch.to_string(),
            });
        }
        return Ok(());
    }

    // On Linux, only ELF binaries are supported. Any other format is
    // rejected as unsupported to prevent executing unknown binary types.
    // The caller can read the first byte to identify the format for
    // error reporting.
    let magic = u64::from_be_bytes([
        blob_bytes[0], blob_bytes[1], blob_bytes[2], blob_bytes[3],
        0, 0, 0, 0,
    ]);
    Err(ArchMismatchError::UnsupportedFormat { magic })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elf_x86_64_on_x86_64_passes() {
        // Minimal ELF header for x86_64: magic + class + padding + e_machine
        let mut bytes = vec![0u8; 20];
        // ELF magic
        bytes[0] = 0x7f;
        bytes[1] = b'E';
        bytes[2] = b'L';
        bytes[3] = b'F';
        bytes[4] = 2; // ELFCLASS64
        // e_machine = EM_X86_64 = 0x3E at offset 18 (little-endian)
        bytes[18] = 0x3E;
        bytes[19] = 0x00;

        if std::env::consts::ARCH == "x86_64" {
            assert!(check_arch(&bytes, std::env::consts::ARCH).is_ok());
        }
    }

    #[test]
    fn elf_aarch64_on_x86_64_refused() {
        let mut bytes = vec![0u8; 20];
        bytes[0] = 0x7f;
        bytes[1] = b'E';
        bytes[2] = b'L';
        bytes[3] = b'F';
        bytes[4] = 2;
        // e_machine = EM_AARCH64 = 0xB7
        bytes[18] = 0xB7;
        bytes[19] = 0x00;

        if std::env::consts::ARCH == "x86_64" {
            let err = check_arch(&bytes, "x86_64").unwrap_err();
            assert!(err.to_string().contains("aarch64"));
            assert!(err.to_string().contains("x86_64"));
        }
    }

    #[test]
    fn non_elf_magic_refused() {
        let bytes = [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x00,
                     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let err = check_arch(&bytes, "x86_64").unwrap_err();
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn short_blob_passes() {
        let bytes = [0x7f; 8]; // Too short
        assert!(check_arch(&bytes, "x86_64").is_ok());
    }

    #[test]
    fn host_arch_is_sensible() {
        let arch = std::env::consts::ARCH;
        assert!(!arch.is_empty());
        assert!(arch.len() < 20);
    }
}

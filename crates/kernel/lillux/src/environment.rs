//! Single-threaded startup capture of named process-environment values.
//!
//! Removing a variable through the C library only unlinks its `environ`
//! entry. The original `NAME=value` bytes stay in process memory, and the
//! initial ones remain readable through `/proc/<pid>/environ` by any process
//! of the same account. Capture therefore copies each named value into
//! zeroizing ownership, overwrites the value bytes in place, and only then
//! removes the variable.

use std::collections::BTreeSet;

use zeroize::Zeroizing;

/// Longest environment name accepted for capture.
pub const MAX_ENVIRONMENT_NAME_BYTES: usize = 256;

/// Take the values of `names` out of the process environment.
///
/// Returns the present names in request order, each with its value in
/// zeroizing ownership. Absent names are omitted. Every matched value is
/// overwritten with zeros in place and its variable removed, including when
/// the call refuses. Refusals name the variable and never carry its value.
///
/// # Safety
/// Call only during single-threaded process startup, before any runtime,
/// application or foreign library thread exists. No other code may hold a
/// pointer into the process environment across this call.
pub unsafe fn take_environment_values(
    names: &[String],
) -> Result<Vec<(String, Zeroizing<String>)>, String> {
    let mut unique = BTreeSet::new();
    for name in names {
        if name.is_empty() || name.len() > MAX_ENVIRONMENT_NAME_BYTES || name.contains(['=', '\0'])
        {
            return Err("environment capture name is invalid".to_owned());
        }
        if !unique.insert(name.as_str()) {
            return Err(format!("environment capture name `{name}` is repeated"));
        }
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    // SAFETY: upheld by this function's caller.
    unsafe { imp::take(names) }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::c_char;
    use std::sync::atomic::{Ordering, compiler_fence};

    use zeroize::Zeroizing;

    unsafe extern "C" {
        static mut environ: *mut *mut c_char;
    }

    /// One `NAME=value` entry: its start and its length without the NUL.
    struct Entry {
        start: *mut c_char,
        length: usize,
    }

    pub(super) unsafe fn take(
        names: &[String],
    ) -> Result<Vec<(String, Zeroizing<String>)>, String> {
        let mut taken = Vec::with_capacity(names.len());
        let mut refusal = None;
        for name in names {
            // SAFETY: upheld by the caller; names were validated above.
            match unsafe { take_one(name) } {
                Ok(Some(value)) => taken.push((name.clone(), value)),
                Ok(None) => {}
                Err(error) => {
                    if refusal.is_none() {
                        refusal = Some(error);
                    }
                }
            }
        }
        match refusal {
            Some(error) => Err(error),
            None => Ok(taken),
        }
    }

    /// Copy, scrub and remove one name. Every occurrence is scrubbed even
    /// when the value is refused.
    unsafe fn take_one(name: &str) -> Result<Option<Zeroizing<String>>, String> {
        // SAFETY: single-threaded startup; no concurrent environment writer.
        let entries = unsafe { matching_entries(name) };
        if entries.is_empty() {
            return Ok(None);
        }
        let prefix = name.len() + 1;
        let outcome = match entries.as_slice() {
            [entry] => {
                // SAFETY: the entry is a live NUL-terminated `environ` string
                // of `length` bytes that starts with `NAME=`. The slice is
                // dropped before any write below.
                let value = unsafe {
                    std::slice::from_raw_parts(
                        entry.start.cast::<u8>().add(prefix),
                        entry.length - prefix,
                    )
                };
                match std::str::from_utf8(value) {
                    Ok(text) => {
                        let mut owned = Zeroizing::new(String::with_capacity(text.len()));
                        owned.push_str(text);
                        Ok(Some(owned))
                    }
                    Err(_) => Err(format!("environment value for `{name}` is not UTF-8")),
                }
            }
            _ => Err(format!("environment name `{name}` occurs more than once")),
        };
        for entry in &entries {
            for offset in prefix..entry.length {
                // SAFETY: `offset` stays inside this entry's value bytes, and
                // environment strings are writable process memory.
                unsafe { std::ptr::write_volatile(entry.start.cast::<u8>().add(offset), 0) };
            }
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: single-threaded startup; the name was validated as non-empty
        // and free of `=` and NUL.
        unsafe { std::env::remove_var(name) };
        outcome
    }

    unsafe fn matching_entries(name: &str) -> Vec<Entry> {
        let mut entries = Vec::new();
        // SAFETY: reading the pointer value copies it; no reference to the
        // mutable static is formed.
        let table = unsafe { environ };
        if table.is_null() {
            return entries;
        }
        let mut index = 0;
        loop {
            // SAFETY: `environ` is a NULL-terminated array of entries.
            let start = unsafe { *table.add(index) };
            if start.is_null() {
                break;
            }
            index += 1;
            // SAFETY: each entry is a NUL-terminated string.
            let length = unsafe { libc::strlen(start) };
            // SAFETY: `length` bytes are readable; the slice ends this turn.
            let bytes = unsafe { std::slice::from_raw_parts(start.cast::<u8>(), length) };
            if bytes.len() > name.len()
                && bytes.starts_with(name.as_bytes())
                && bytes[name.len()] == b'='
            {
                entries.push(Entry { start, length });
            }
        }
        entries
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use zeroize::Zeroizing;

    pub(super) unsafe fn take(
        _names: &[String],
    ) -> Result<Vec<(String, Zeroizing<String>)>, String> {
        Err("environment capture is unavailable on this platform".to_owned())
    }
}

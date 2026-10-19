//! Owner-tagged hidden temporary names for secure publication.
//!
//! Every hidden temporary created by a secure publication writer is named
//! `.secure.tmp.<purpose>.<pid>.<seq>`. The purpose token is caller data: it
//! names the subsystem that owns the publication so a scan of a shared
//! directory acts only on its own interrupted debris. Lillux never interprets
//! a token beyond validating its spelling.

use std::ffi::OsStr;

const SECURE_TEMP_PREFIX: &[u8] = b".secure.tmp.";
const MAX_PURPOSE_BYTES: usize = 16;

/// Owner token embedded in secure temporary names. Construct it in a `const`
/// item so an invalid token fails the build rather than a running process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TempPurpose(&'static str);

impl TempPurpose {
    /// The token must match `[a-z][a-z0-9-]{0,15}`.
    pub const fn new(token: &'static str) -> Self {
        assert!(
            purpose_is_valid(token.as_bytes()),
            "secure temp purpose must match [a-z][a-z0-9-]{{0,15}}"
        );
        Self(token)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }

    pub(crate) fn require_valid(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            purpose_is_valid(self.0.as_bytes()),
            "invalid secure temp purpose token"
        );
        Ok(())
    }
}

const fn purpose_is_valid(token: &[u8]) -> bool {
    if token.is_empty() || token.len() > MAX_PURPOSE_BYTES || !token[0].is_ascii_lowercase() {
        return false;
    }
    let mut index = 1;
    while index < token.len() {
        let byte = token[index];
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
            return false;
        }
        index += 1;
    }
    true
}

/// Classification of one directory entry name against the secure temporary
/// naming scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureTempClass<'a> {
    /// The name does not carry the `.secure.tmp.` prefix.
    NotTemp,
    /// An exact `.secure.tmp.<purpose>.<pid>.<seq>` name.
    Temp {
        purpose: &'a str,
        pid: u32,
        seq: u64,
    },
    /// Any other name carrying the prefix, including the retired
    /// `.secure.tmp.<pid>.<seq>` spelling.
    Malformed,
}

/// Ownership of one entry relative to a scanning owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecureTempOwnership {
    NotTemp,
    /// Exact temporary debris carrying the scanner's own token.
    Own,
    /// Exact temporary debris carrying another owner's token; scans leave it
    /// in place and do not refuse because of it.
    Foreign,
    /// A prefixed name that is not an exact temporary; scans refuse.
    Malformed,
}

impl SecureTempClass<'_> {
    pub fn ownership(&self, owner: TempPurpose) -> SecureTempOwnership {
        match self {
            Self::NotTemp => SecureTempOwnership::NotTemp,
            Self::Temp { purpose, .. } if *purpose == owner.as_str() => SecureTempOwnership::Own,
            Self::Temp { .. } => SecureTempOwnership::Foreign,
            Self::Malformed => SecureTempOwnership::Malformed,
        }
    }
}

/// The only parser of secure temporary names.
pub fn parse_secure_temp_name(name: &OsStr) -> SecureTempClass<'_> {
    let bytes = name.as_encoded_bytes();
    let Some(rest) = bytes.strip_prefix(SECURE_TEMP_PREFIX) else {
        return SecureTempClass::NotTemp;
    };
    let Ok(rest) = std::str::from_utf8(rest) else {
        return SecureTempClass::Malformed;
    };
    let mut fields = rest.split('.');
    let (Some(purpose), Some(pid), Some(seq), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return SecureTempClass::Malformed;
    };
    if !purpose_is_valid(purpose.as_bytes()) {
        return SecureTempClass::Malformed;
    }
    match (canonical_decimal::<u32>(pid), canonical_decimal::<u64>(seq)) {
        (Some(pid), Some(seq)) => SecureTempClass::Temp { purpose, pid, seq },
        _ => SecureTempClass::Malformed,
    }
}

/// Classify `name` for a scan owned by `owner`.
pub fn classify_secure_temp_name(name: &OsStr, owner: TempPurpose) -> SecureTempOwnership {
    parse_secure_temp_name(name).ownership(owner)
}

fn canonical_decimal<T: std::str::FromStr>(field: &str) -> Option<T> {
    let bytes = field.as_bytes();
    if bytes.is_empty()
        || !bytes.iter().all(u8::is_ascii_digit)
        || (bytes.len() > 1 && bytes[0] == b'0')
    {
        return None;
    }
    field.parse().ok()
}

/// Format the next temporary name for `purpose` in this process.
#[cfg(unix)]
pub(crate) fn next_secure_temp_name(purpose: TempPurpose) -> anyhow::Result<std::ffi::CString> {
    purpose.require_valid()?;
    let sequence = crate::atomic_fs::next_temp_sequence();
    Ok(std::ffi::CString::new(format!(
        ".secure.tmp.{}.{}.{}",
        purpose.as_str(),
        std::process::id(),
        sequence
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: TempPurpose = TempPurpose::new("state");

    fn class(name: &str) -> SecureTempClass<'_> {
        parse_secure_temp_name(OsStr::new(name))
    }

    #[test]
    #[cfg(unix)]
    fn exact_names_round_trip() {
        let name = next_secure_temp_name(OWNER).unwrap();
        let text = name.to_str().unwrap();
        match class(text) {
            SecureTempClass::Temp { purpose, pid, .. } => {
                assert_eq!(purpose, "state");
                assert_eq!(pid, std::process::id());
            }
            other => panic!("unexpected class {other:?}"),
        }
        assert_eq!(
            class(".secure.tmp.content.0.0"),
            SecureTempClass::Temp {
                purpose: "content",
                pid: 0,
                seq: 0
            }
        );
        assert_eq!(
            class(".secure.tmp.a-1.4294967295.18446744073709551615"),
            SecureTempClass::Temp {
                purpose: "a-1",
                pid: u32::MAX,
                seq: u64::MAX
            }
        );
        assert_eq!(
            class(".secure.tmp.abcdefghijklmnop.1.2"),
            SecureTempClass::Temp {
                purpose: "abcdefghijklmnop",
                pid: 1,
                seq: 2
            }
        );
    }

    #[test]
    fn non_temp_names_are_not_temp() {
        for name in [
            "",
            "state.json",
            ".secure.tmp",
            ".secure.authoring.1.2",
            "secure.tmp.state.1.2",
        ] {
            assert_eq!(class(name), SecureTempClass::NotTemp, "{name}");
        }
    }

    #[test]
    fn old_format_and_inexact_names_are_malformed() {
        for name in [
            ".secure.tmp.",
            ".secure.tmp.4242.7",
            ".secure.tmp.interrupted",
            ".secure.tmp.state.1",
            ".secure.tmp.state.1.2.3",
            ".secure.tmp.state..2",
            ".secure.tmp.state.1.",
            ".secure.tmp.State.1.2",
            ".secure.tmp.1state.1.2",
            ".secure.tmp.-state.1.2",
            ".secure.tmp.st_ate.1.2",
            ".secure.tmp.abcdefghijklmnopq.1.2",
            ".secure.tmp.state.01.2",
            ".secure.tmp.state.1.02",
            ".secure.tmp.state.+1.2",
            ".secure.tmp.state.1.-2",
            ".secure.tmp.state.4294967296.2",
            ".secure.tmp.state.1.18446744073709551616",
            ".secure.tmp.state. 1.2",
        ] {
            assert_eq!(class(name), SecureTempClass::Malformed, "{name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            assert_eq!(
                parse_secure_temp_name(OsStr::from_bytes(b".secure.tmp.st\xffate.1.2")),
                SecureTempClass::Malformed
            );
        }
    }

    #[test]
    fn ownership_separates_own_foreign_and_malformed() {
        let owner = TempPurpose::new("state");
        assert_eq!(
            classify_secure_temp_name(OsStr::new(".secure.tmp.state.1.2"), owner),
            SecureTempOwnership::Own
        );
        assert_eq!(
            classify_secure_temp_name(OsStr::new(".secure.tmp.content.1.2"), owner),
            SecureTempOwnership::Foreign
        );
        assert_eq!(
            classify_secure_temp_name(OsStr::new(".secure.tmp.1.2"), owner),
            SecureTempOwnership::Malformed
        );
        assert_eq!(
            classify_secure_temp_name(OsStr::new("record"), owner),
            SecureTempOwnership::NotTemp
        );
    }

    #[test]
    fn invalid_tokens_are_refused_at_construction() {
        for token in ["", "State", "1x", "-x", "x_y", "x.y", "abcdefghijklmnopq"] {
            assert!(!purpose_is_valid(token.as_bytes()), "{token}");
            assert!(
                std::panic::catch_unwind(|| TempPurpose::new(token)).is_err(),
                "{token}"
            );
        }
        assert!(TempPurpose("Bad").require_valid().is_err());
        #[cfg(unix)]
        assert!(next_secure_temp_name(TempPurpose("Bad")).is_err());
    }

    #[cfg(target_os = "linux")]
    mod writers {
        use super::*;
        use crate::secure_fs::PinnedDirectory;
        use crate::test_hooks::{
            AtomicCreateHookAction, AtomicCreateHookSelector, set_atomic_create_hook,
        };
        use std::os::unix::process::ExitStatusExt as _;

        const CHILD_REQUEST: &str = "LILLUX_SECURE_TEMP_ABORT_CHILD";
        const ABORT_PURPOSE: TempPurpose = TempPurpose::new("hook-abort");
        const FAIL_PURPOSE: TempPurpose = TempPurpose::new("hook-fail");
        const WRITERS: [&str; 4] = ["create", "reader", "write", "prepare"];

        fn entry_names(directory: &std::path::Path) -> Vec<String> {
            let mut names = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        }

        fn write_with(
            writer: &str,
            directory: &PinnedDirectory,
            target: &str,
            bytes: &[u8],
            purpose: TempPurpose,
        ) -> anyhow::Result<()> {
            let name = OsStr::new(target);
            match writer {
                "create" => directory
                    .atomic_create_regular(name, bytes, 0o600, purpose)
                    .map(drop),
                "reader" => directory
                    .atomic_create_regular_from_reader(
                        name,
                        &mut std::io::Cursor::new(bytes),
                        4096,
                        0o600,
                        purpose,
                    )
                    .map(drop),
                "write" => directory.atomic_write_if_same(name, None, bytes, 0o600, purpose),
                "prepare" => {
                    let prepared = directory
                        .prepare_atomic_create(name, bytes, 0o600, purpose)?
                        .expect("prepared target is absent");
                    prepared.publish().map(drop)
                }
                other => panic!("unknown writer {other}"),
            }
        }

        /// Child half of `armed_abort_leaves_owner_tagged_debris_for_every_writer`.
        /// It does nothing unless that test spawned this process.
        #[test]
        fn abort_hook_child() {
            let Ok(request) = std::env::var(CHILD_REQUEST) else {
                return;
            };
            let (writer, path) = request.split_once(':').unwrap();
            crate::disable_process_core_dumps().unwrap();
            let directory = PinnedDirectory::open(std::path::Path::new(path))
                .unwrap()
                .unwrap();
            set_atomic_create_hook(
                AtomicCreateHookSelector {
                    purpose: ABORT_PURPOSE,
                    target: "target".into(),
                    occurrence: 1,
                },
                AtomicCreateHookAction::Abort,
            );
            let result = write_with(writer, &directory, "target", b"interrupted", ABORT_PURPOSE);
            panic!("armed abort hook did not fire for {writer}: {result:?}");
        }

        #[test]
        fn armed_abort_leaves_owner_tagged_debris_for_every_writer() {
            for writer in WRITERS {
                let root = tempfile::tempdir().unwrap();
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "secure_fs::temp_name::tests::writers::abort_hook_child",
                        "--test-threads=1",
                        "--nocapture",
                    ])
                    .env(CHILD_REQUEST, format!("{writer}:{}", root.path().display()))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let pid = child.id();
                let status = child.wait_with_output().unwrap().status;
                assert_eq!(status.signal(), Some(libc::SIGABRT), "{writer}: {status:?}");
                let names = entry_names(root.path());
                assert_eq!(names.len(), 1, "{writer}: {names:?}");
                match parse_secure_temp_name(OsStr::new(&names[0])) {
                    SecureTempClass::Temp {
                        purpose,
                        pid: owner,
                        ..
                    } => {
                        assert_eq!(purpose, "hook-abort", "{writer}");
                        assert_eq!(owner, pid, "{writer}");
                    }
                    other => panic!("{writer}: unexpected debris {other:?}"),
                }
                assert_eq!(
                    std::fs::read(root.path().join(&names[0])).unwrap(),
                    b"interrupted",
                    "{writer}"
                );
            }
        }

        #[test]
        fn armed_fail_selects_purpose_target_and_occurrence() {
            for writer in WRITERS {
                let root = tempfile::tempdir().unwrap();
                let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
                set_atomic_create_hook(
                    AtomicCreateHookSelector {
                        purpose: FAIL_PURPOSE,
                        target: format!("target-{writer}").into(),
                        occurrence: 2,
                    },
                    AtomicCreateHookAction::Fail,
                );
                let target = format!("target-{writer}");
                // Neither another target nor another owner advances the count.
                write_with(writer, &directory, "other", b"x", FAIL_PURPOSE).unwrap();
                write_with(
                    writer,
                    &directory,
                    &format!("{target}.a"),
                    b"x",
                    ABORT_PURPOSE,
                )
                .unwrap();
                write_with(writer, &directory, &target, b"first", FAIL_PURPOSE).unwrap();
                std::fs::remove_file(root.path().join(&target)).unwrap();
                let error =
                    write_with(writer, &directory, &target, b"second", FAIL_PURPOSE).unwrap_err();
                assert!(
                    format!("{error:#}").contains("armed test hook"),
                    "{writer}: {error:#}"
                );
                if let Some(failure) =
                    error.downcast_ref::<crate::secure_fs::AtomicRegularCreationFailure>()
                {
                    // Create-only publication hands its unpublished inode to
                    // the caller instead of unlinking it.
                    assert_eq!(writer, "create");
                    assert_eq!(
                        failure.phase(),
                        crate::secure_fs::AtomicRegularCreationPhase::TemporaryCreated
                    );
                    assert!(matches!(
                        parse_secure_temp_name(failure.temporary_name()),
                        SecureTempClass::Temp {
                            purpose: "hook-fail",
                            ..
                        }
                    ));
                    directory
                        .remove_if_same(failure.temporary_name(), failure.original_file())
                        .unwrap();
                }
                assert_eq!(
                    entry_names(root.path()),
                    vec!["other".to_owned(), format!("{target}.a")],
                    "{writer}: the failed publication must clean its temporary"
                );
                write_with(writer, &directory, &target, b"third", FAIL_PURPOSE).unwrap();
                assert_eq!(std::fs::read(root.path().join(&target)).unwrap(), b"third");
            }
        }
    }
}

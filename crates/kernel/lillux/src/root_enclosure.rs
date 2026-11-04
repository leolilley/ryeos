//! Private enclosure in which an unprivileged caller is the administrator of
//! a constructed root filesystem that also maps one non-root account.
//!
//! The invoking user becomes UID/GID 0 of a new user namespace and the first
//! id of the user's delegated subordinate range becomes the given account, so
//! ownership grants and an irreversible drop to that account behave as they
//! do for a real administrator. A new PID namespace makes the body its init:
//! every process the body starts dies when it returns. A new mount namespace
//! pivots into a bounded tmpfs that carries only the named host entries plus
//! an empty `/tmp`. Nothing done inside is visible from, or survives in, the
//! host view.

use std::path::Path;

use crate::sandbox::TmpfsLimits;

/// Largest number of host entries one enclosure may carry.
pub const MAX_ROOT_ENCLOSURE_HOST_ENTRIES: usize = 32;

/// What the enclosure maps and carries.
#[derive(Debug, Clone, Copy)]
pub struct RootEnclosureSpec<'a> {
    /// Non-root account mapped beside the enclosure administrator.
    pub account_uid: u32,
    pub account_gid: u32,
    /// Bounds of the tmpfs that becomes `/`.
    pub root_limits: TmpfsLimits,
    /// Top-level host names carried into the new root. A directory is bound
    /// recursively, a symbolic link is recreated with the same target, and
    /// `proc` is a fresh procfs of the enclosure's PID namespace. Absent
    /// names are skipped.
    pub host_entries: &'a [&'a str],
}

impl RootEnclosureSpec<'_> {
    fn validate(&self) -> Result<(), String> {
        crate::ControllerAccount::unix(self.account_uid, self.account_gid).validate()?;
        self.root_limits.validate()?;
        if self.host_entries.len() > MAX_ROOT_ENCLOSURE_HOST_ENTRIES {
            return Err("root enclosure carries too many host entries".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for name in self.host_entries {
            if name.is_empty()
                || *name == "."
                || *name == ".."
                || *name == "tmp"
                || name.contains('/')
                || name.contains('\0')
                || !seen.insert(*name)
            {
                return Err(format!(
                    "root enclosure host entry {name:?} is not one plain name"
                ));
            }
        }
        Ok(())
    }
}

/// Why this host cannot create a root enclosure for the account, or `None`
/// when the delegated id mapping it needs is present. User, PID and mount
/// namespace creation must be checked separately with host facts.
pub fn root_enclosure_unavailable_reason(spec: &RootEnclosureSpec<'_>) -> Option<String> {
    if let Err(error) = spec.validate() {
        return Some(error);
    }
    imp::unavailable_reason()
}

/// Run `body` as the enclosure administrator inside the constructed root and
/// return its exit code. The caller must be single-threaded; the calling
/// process itself changes user and mount namespaces, so it should be a
/// dedicated process that exits after this returns. Setup refusals return an
/// error naming the failed step.
pub fn run_in_root_enclosure(
    spec: &RootEnclosureSpec<'_>,
    body: impl FnOnce() -> i32,
) -> Result<i32, String> {
    spec.validate()?;
    imp::run(spec, body)
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::RootEnclosureSpec;

    pub(super) fn unavailable_reason() -> Option<String> {
        Some("root enclosures require Linux namespaces".into())
    }

    pub(super) fn run(
        _spec: &RootEnclosureSpec<'_>,
        _body: impl FnOnce() -> i32,
    ) -> Result<i32, String> {
        Err("root enclosures require Linux namespaces".into())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::io::{Read as _, Write as _};
    use std::os::fd::{FromRawFd as _, OwnedFd};

    use super::{Path, RootEnclosureSpec};

    const ID_MAP_TOOLS: [(&str, &str); 2] = [
        ("/usr/bin/newuidmap", "/etc/subuid"),
        ("/usr/bin/newgidmap", "/etc/subgid"),
    ];
    const NEW_ROOT: &str = "/tmp";
    const OLD_ROOT: &str = ".enclosure-former-root";
    const SETUP_REFUSED: i32 = 125;

    fn last_error(label: &str) -> String {
        format!("{label}: {}", std::io::Error::last_os_error())
    }

    fn zero(result: libc::c_int, label: &str) -> Result<(), String> {
        if result == 0 {
            Ok(())
        } else {
            Err(last_error(label))
        }
    }

    fn c_path(path: &str) -> Result<CString, String> {
        CString::new(path).map_err(|_| format!("enclosure path {path:?} contains NUL"))
    }

    fn user_name(uid: libc::uid_t) -> Result<String, String> {
        let mut buffer = vec![0_u8; 16 * 1024];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: the buffer outlives the call and the result only borrows it
        // until the name is copied below.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if code != 0 || found.is_null() {
            return Err(format!("the invoking user {uid} has no account name"));
        }
        // SAFETY: getpwuid_r returned a NUL-terminated name inside `buffer`.
        Ok(unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned())
    }

    /// First id of the invoking user's delegated range in `table`.
    fn subordinate_start(table: &str, uid: libc::uid_t, name: &str) -> Result<u32, String> {
        let text = std::fs::read_to_string(table)
            .map_err(|error| format!("read subordinate id table {table}: {error}"))?;
        let uid = uid.to_string();
        for line in text.lines() {
            let mut fields = line.trim().split(':');
            let (Some(owner), Some(start), Some(count)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if owner != name && owner != uid {
                continue;
            }
            let (Ok(start), Ok(count)) = (start.parse::<u32>(), count.parse::<u32>()) else {
                continue;
            };
            if count > 0 && start > 0 {
                return Ok(start);
            }
        }
        Err(format!("{table} delegates no subordinate ids to {name}"))
    }

    fn id_map_plan() -> Result<[(CString, u32, u32); 2], String> {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        if uid == 0 {
            return Err("a root enclosure is for unprivileged callers".into());
        }
        let name = user_name(uid)?;
        let mut plan = Vec::with_capacity(2);
        for ((tool, table), own) in ID_MAP_TOOLS.into_iter().zip([uid, gid]) {
            if !Path::new(tool).is_file() {
                return Err(format!("{tool} is not installed"));
            }
            plan.push((c_path(tool)?, own, subordinate_start(table, uid, &name)?));
        }
        plan.try_into()
            .map_err(|_| "root enclosure id map plan is incomplete".to_string())
    }

    pub(super) fn unavailable_reason() -> Option<String> {
        id_map_plan().err()
    }

    fn single_threaded() -> Result<(), String> {
        let tasks = std::fs::read_dir("/proc/self/task")
            .map_err(|error| format!("inspect enclosure caller threads: {error}"))?
            .count();
        if tasks == 1 {
            Ok(())
        } else {
            Err("a root enclosure requires a single-threaded caller".into())
        }
    }

    fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
        let mut fds = [-1; 2];
        zero(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) },
            "create enclosure pipe",
        )?;
        // SAFETY: pipe2 returned two new descriptors owned here.
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    fn wait_exit(pid: libc::pid_t) -> Result<i32, String> {
        let mut status = 0;
        loop {
            if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                break;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return Err(last_error("reap enclosure child"));
            }
        }
        if libc::WIFEXITED(status) {
            Ok(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            Ok(128 + libc::WTERMSIG(status))
        } else {
            Err("enclosure child ended without an exit status".into())
        }
    }

    /// Forked helper, still in the caller's user namespace: once the caller
    /// has unshared, install both id maps for it with the setuid tools.
    fn map_ids_for(
        target: libc::pid_t,
        go: OwnedFd,
        plan: &[(CString, u32, u32); 2],
        account: [u32; 2],
    ) -> ! {
        let mut byte = [0_u8; 1];
        let mut go = std::fs::File::from(go);
        if go.read_exact(&mut byte).is_err() {
            unsafe { libc::_exit(2) };
        }
        for ((tool, own, subordinate), inner) in plan.iter().zip(account) {
            let args = [
                tool.clone(),
                c_path(&target.to_string()).unwrap_or_default(),
                c_path("0").unwrap_or_default(),
                c_path(&own.to_string()).unwrap_or_default(),
                c_path("1").unwrap_or_default(),
                c_path(&inner.to_string()).unwrap_or_default(),
                c_path(&subordinate.to_string()).unwrap_or_default(),
                c_path("1").unwrap_or_default(),
            ];
            let mut argv: Vec<*const libc::c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
            argv.push(std::ptr::null());
            let child = unsafe { libc::fork() };
            if child == 0 {
                unsafe {
                    libc::execv(tool.as_ptr(), argv.as_ptr());
                    libc::_exit(127);
                }
            }
            if child < 0 || wait_exit(child) != Ok(0) {
                unsafe { libc::_exit(3) };
            }
        }
        unsafe { libc::_exit(0) }
    }

    fn enter_mapped_user_namespace(spec: &RootEnclosureSpec<'_>) -> Result<(), String> {
        let plan = id_map_plan()?;
        let (go_reader, go_writer) = pipe()?;
        let target = unsafe { libc::getpid() };
        let helper = unsafe { libc::fork() };
        if helper < 0 {
            return Err(last_error("fork enclosure id-map helper"));
        }
        if helper == 0 {
            drop(go_writer);
            map_ids_for(
                target,
                go_reader,
                &plan,
                [spec.account_uid, spec.account_gid],
            );
        }
        drop(go_reader);
        let unshared = zero(
            unsafe { libc::unshare(libc::CLONE_NEWUSER) },
            "create enclosure user namespace",
        );
        let signalled = match unshared {
            Ok(()) => std::fs::File::from(go_writer)
                .write_all(&[1])
                .map_err(|error| format!("release enclosure id-map helper: {error}")),
            Err(error) => {
                drop(go_writer);
                Err(error)
            }
        };
        let helper_exit = wait_exit(helper)?;
        signalled?;
        if helper_exit != 0 {
            return Err(format!(
                "the subordinate id tools refused the enclosure map (helper exit {helper_exit})"
            ));
        }
        zero(
            unsafe { libc::setresgid(0, 0, 0) },
            "become enclosure administrator group",
        )?;
        zero(
            unsafe { libc::setresuid(0, 0, 0) },
            "become enclosure administrator",
        )?;
        zero(
            unsafe { libc::setgroups(0, std::ptr::null()) },
            "clear enclosure supplementary groups",
        )
    }

    fn mount(
        source: &str,
        target: &str,
        filesystem: Option<&str>,
        flags: libc::c_ulong,
        data: Option<&str>,
    ) -> Result<(), String> {
        let source_c = c_path(source)?;
        let target_c = c_path(target)?;
        let filesystem_c = filesystem.map(c_path).transpose()?;
        let data_c = data.map(c_path).transpose()?;
        zero(
            unsafe {
                libc::mount(
                    source_c.as_ptr(),
                    target_c.as_ptr(),
                    filesystem_c
                        .as_ref()
                        .map_or(std::ptr::null(), |value| value.as_ptr()),
                    flags,
                    data_c
                        .as_ref()
                        .map_or(std::ptr::null(), |value| value.as_ptr().cast()),
                )
            },
            &format!("mount {source} at {target}"),
        )
    }

    fn make_directory(path: &str, mode: libc::mode_t) -> Result<(), String> {
        let path_c = c_path(path)?;
        zero(
            unsafe { libc::mkdir(path_c.as_ptr(), mode) },
            &format!("create enclosure directory {path}"),
        )?;
        zero(
            unsafe { libc::chmod(path_c.as_ptr(), mode) },
            &format!("set enclosure directory mode {path}"),
        )
    }

    fn construct_root(spec: &RootEnclosureSpec<'_>) -> Result<(), String> {
        let options = format!(
            "mode=0755,size={},nr_inodes={}",
            spec.root_limits.size_bytes, spec.root_limits.inodes
        );
        mount(
            "tmpfs",
            NEW_ROOT,
            Some("tmpfs"),
            libc::MS_NOSUID,
            Some(&options),
        )?;
        for name in spec.host_entries {
            let host = format!("/{name}");
            let target = format!("{NEW_ROOT}/{name}");
            let Ok(metadata) = std::fs::symlink_metadata(&host) else {
                continue;
            };
            if *name == "proc" {
                make_directory(&target, 0o555)?;
                mount(
                    "proc",
                    &target,
                    Some("proc"),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    None,
                )?;
            } else if metadata.file_type().is_symlink() {
                let link = std::fs::read_link(&host)
                    .map_err(|error| format!("read host link {host}: {error}"))?;
                std::os::unix::fs::symlink(&link, &target)
                    .map_err(|error| format!("recreate host link {host}: {error}"))?;
            } else if metadata.is_dir() {
                make_directory(&target, 0o755)?;
                mount(&host, &target, None, libc::MS_BIND | libc::MS_REC, None)?;
            } else {
                return Err(format!(
                    "host entry {host} is neither a directory nor a link"
                ));
            }
        }
        make_directory(&format!("{NEW_ROOT}/tmp"), 0o1777)?;
        make_directory(&format!("{NEW_ROOT}/{OLD_ROOT}"), 0o700)?;
        let new_root = c_path(NEW_ROOT)?;
        zero(
            unsafe { libc::chdir(new_root.as_ptr()) },
            "enter enclosure root",
        )?;
        let old = c_path(OLD_ROOT)?;
        if unsafe { libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), old.as_ptr()) } != 0 {
            return Err(last_error("pivot into enclosure root"));
        }
        zero(unsafe { libc::chdir(c"/".as_ptr()) }, "enter enclosure /")?;
        let old_absolute = c_path(&format!("/{OLD_ROOT}"))?;
        zero(
            unsafe { libc::umount2(old_absolute.as_ptr(), libc::MNT_DETACH) },
            "detach former host root",
        )?;
        zero(
            unsafe { libc::rmdir(old_absolute.as_ptr()) },
            "remove former host root mountpoint",
        )
    }

    pub(super) fn run(
        spec: &RootEnclosureSpec<'_>,
        body: impl FnOnce() -> i32,
    ) -> Result<i32, String> {
        single_threaded()?;
        enter_mapped_user_namespace(spec)?;
        zero(
            unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) },
            "create enclosure mount and PID namespaces",
        )?;
        mount("none", "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
        let (status_reader, status_writer) = pipe()?;
        let init = unsafe { libc::fork() };
        if init < 0 {
            return Err(last_error("fork enclosure init"));
        }
        if init == 0 {
            drop(status_reader);
            // A lost caller must not leave the enclosure running.
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
            if let Err(error) = construct_root(spec) {
                let mut writer = std::fs::File::from(status_writer);
                let _ = writer.write_all(error.as_bytes());
                unsafe { libc::_exit(SETUP_REFUSED) };
            }
            drop(status_writer);
            let code = body();
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            unsafe { libc::_exit(code) };
        }
        drop(status_writer);
        let mut refusal = Vec::new();
        let read = std::fs::File::from(status_reader).read_to_end(&mut refusal);
        let code = wait_exit(init)?;
        read.map_err(|error| format!("read enclosure setup status: {error}"))?;
        if !refusal.is_empty() {
            return Err(String::from_utf8_lossy(&refusal).into_owned());
        }
        Ok(code)
    }

    #[cfg(test)]
    mod tests {
        use super::subordinate_start;

        #[test]
        fn subordinate_range_matches_name_or_numeric_owner() {
            let table = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                table.path(),
                "other:200000:65536\nleo:100000:65536\n1001:300000:10\nbad:x:1\n",
            )
            .unwrap();
            let path = table.path().to_str().unwrap();
            assert_eq!(subordinate_start(path, 1000, "leo").unwrap(), 100000);
            assert_eq!(
                subordinate_start(path, 1001, "nobody-else").unwrap(),
                300000
            );
            assert!(subordinate_start(path, 1002, "absent").is_err());
        }
    }
}

//! Detached workspace mechanics. The template is deliberately never attached:
//! attaching it would give it a namespace origin and defeat independent
//! borrowers' ability to clone the same filesystem in fresh user namespaces.
//! Ordinary source reanchoring belongs before filesystem creation; applying
//! it to this template would replace its authority with unrelated host bytes.

use super::*;

const FSOPEN_CLOEXEC: libc::c_uint = 1;
const FSMOUNT_CLOEXEC: libc::c_uint = 1;
const FSCONFIG_SET_FLAG: libc::c_uint = 0;
const FSCONFIG_SET_FD: libc::c_uint = 5;
const FSCONFIG_CMD_CREATE: libc::c_uint = 6;
const OVERLAYFS_SUPER_MAGIC: libc::c_long = 0x794c_7630;

pub(crate) fn take_overlay_template(fd: u32) -> Result<LinuxOverlayTemplate, String> {
    if fd <= 2 {
        return Err("overlay template descriptor overlaps stdio".to_string());
    }
    let descriptor = raw_fd(fd)?;
    let lease = crate::retain_fork_sensitive_descriptors();
    // SAFETY: the outer public adoption contract transfers unique inherited
    // ownership. Registration/type refusal retains or closes it on every path.
    let file = unsafe { File::from_raw_fd(descriptor) };
    LinuxOverlayTemplate::from_transferred_authority(
        crate::InheritedDescriptorAuthority::from_owned_file(file, &lease)?,
    )
}

pub(crate) fn create_overlay_template(
    project_fd: u32,
    state_fd: u32,
) -> Result<LinuxOverlayTemplate, String> {
    if project_fd == state_fd {
        return Err("overlay lower and state authorities alias".to_string());
    }
    // Cover every temporary pin/duplicate until the returned template has
    // joined the existing registered inheritance owner. Registering only the
    // final mount cannot retroactively protect earlier descriptor creation.
    let _creation_lease = crate::retain_fork_sensitive_descriptors();
    // Hold the original objects throughout the namespace transition and
    // identity comparison. A path is only a locator for those exact objects.
    let original_lower = inherited_directory(project_fd, "overlay template lower")?;
    let original_state = inherited_directory(state_fd, "overlay template state")?;
    let original_upper = original_state
        .open_child_directory(OsStr::new("upper"))
        .map_err(|error| format!("pin template upper: {error}"))?
        .ok_or_else(|| "overlay template upper is missing".to_string())?;
    let original_work = original_state
        .open_child_directory(OsStr::new("work"))
        .map_err(|error| format!("pin template work: {error}"))?
        .ok_or_else(|| "overlay template work is missing".to_string())?;
    let lower = original_lower
        .try_clone_descriptor()
        .map_err(|error| format!("retain template lower: {error}"))?;
    let upper = original_upper
        .try_clone_descriptor()
        .map_err(|error| format!("retain template upper: {error}"))?;
    let work = original_work
        .try_clone_descriptor()
        .map_err(|error| format!("retain template work: {error}"))?;

    enter_mapped_user_namespace()?;
    syscall_zero(
        unsafe { libc::unshare(libc::CLONE_NEWNS) },
        "create overlay template mount namespace",
    )?;
    mount_raw(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)
        .map_err(|error| format!("make template mount propagation private: {error}"))?;
    let lower = reanchor_mount_source(lower.as_raw_fd())?;
    let upper = reanchor_mount_source(upper.as_raw_fd())?;
    let work = reanchor_mount_source(work.as_raw_fd())?;
    create_from_layers(lower.file(), upper.file(), work.file())
}

fn create_from_layers(
    lower: &File,
    upper: &File,
    work: &File,
) -> Result<LinuxOverlayTemplate, String> {
    // Registration must cover creation, not just eventual transport. The
    // existing inheritance owner prevents unrelated held children retaining
    // this mount after its legitimate workspace owner drops the last handle.
    let lease = crate::retain_fork_sensitive_descriptors();
    let context_fd =
        unsafe { libc::syscall(libc::SYS_fsopen, c"overlay".as_ptr(), FSOPEN_CLOEXEC) } as RawFd;
    if context_fd < 0 {
        return Err(format!(
            "open detached overlay filesystem context: {}",
            std::io::Error::last_os_error()
        ));
    }
    let context = unsafe { File::from_raw_fd(context_fd) };
    for (name, source) in [
        (c"lowerdir+", lower),
        (c"upperdir", upper),
        (c"workdir", work),
    ] {
        syscall_zero(
            unsafe {
                libc::syscall(
                    libc::SYS_fsconfig,
                    context.as_raw_fd(),
                    FSCONFIG_SET_FD,
                    name.as_ptr(),
                    std::ptr::null::<u8>(),
                    source.as_raw_fd(),
                ) as i32
            },
            "configure exact detached overlay layer",
        )?;
    }
    syscall_zero(
        unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                context.as_raw_fd(),
                FSCONFIG_SET_FLAG,
                c"userxattr".as_ptr(),
                std::ptr::null::<u8>(),
                0,
            ) as i32
        },
        "configure rootless detached overlay xattrs",
    )?;
    syscall_zero(
        unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                context.as_raw_fd(),
                FSCONFIG_CMD_CREATE,
                std::ptr::null::<u8>(),
                std::ptr::null::<u8>(),
                0,
            ) as i32
        },
        "create one detached overlay filesystem",
    )?;
    let mount_fd = unsafe {
        libc::syscall(
            libc::SYS_fsmount,
            context.as_raw_fd(),
            FSMOUNT_CLOEXEC,
            MOUNT_ATTR_NOSUID as libc::c_uint | MOUNT_ATTR_NODEV as libc::c_uint,
        )
    } as RawFd;
    if mount_fd < 0 {
        return Err(format!(
            "retain never-attached overlay mount: {}",
            std::io::Error::last_os_error()
        ));
    }
    let authority = crate::InheritedDescriptorAuthority::from_owned_file(
        unsafe { File::from_raw_fd(mount_fd) },
        &lease,
    )?;
    LinuxOverlayTemplate::from_transferred_authority(authority)
}

pub(crate) fn validate_overlay_template(
    authority: &crate::InheritedDescriptorAuthority,
) -> Result<(), String> {
    let descriptor = authority.file().as_raw_fd();
    let stat = mount_source_stat(descriptor)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err("overlay template is not a directory".to_string());
    }
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    syscall_zero(
        unsafe { libc::fstatfs(descriptor, filesystem.as_mut_ptr()) },
        "inspect overlay template filesystem",
    )?;
    if unsafe { filesystem.assume_init() }.f_type != OVERLAYFS_SUPER_MAGIC {
        return Err("overlay template is not an overlay filesystem".to_string());
    }
    // Filesystem class alone is NOT origin/creator proof. Authenticated private
    // transport supplies that role; clone in a fresh namespace enforces the
    // kernel's anonymous-mount rule. Never accept a pathname in its place.
    Ok(())
}

/// Exercise the actual Create/transfer/borrow topology, not a second overlay
/// mount over the same upper/work. All scratch bytes live in this probe's
/// private tmpfs; no host-side temporary paths or namespace keepers survive.
pub(super) fn probe() -> Result<(), String> {
    bounded_probe_child(|| {
        enter_mapped_user_namespace()?;
        syscall_zero(
            unsafe { libc::unshare(libc::CLONE_NEWNS) },
            "create template probe namespace",
        )?;
        mount_raw(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)
            .map_err(|error| format!("make template probe mounts private: {error}"))?;
        mount_private_root()?;
        let root = crate::PinnedDirectory::open(std::path::Path::new(ROOT))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "template probe root is missing".to_string())?;
        let lower = root
            .create_child(OsStr::new("lower"), 0o700)
            .map_err(|error| error.to_string())?;
        let state = root
            .create_child(OsStr::new("state"), 0o700)
            .map_err(|error| error.to_string())?;
        state
            .create_child(OsStr::new("upper"), 0o700)
            .map_err(|error| error.to_string())?;
        state
            .create_child(OsStr::new("work"), 0o700)
            .map_err(|error| error.to_string())?;
        lower
            .atomic_write_if_same(OsStr::new("seed"), None, b"lower", 0o600)
            .map_err(|error| error.to_string())?;
        let lower = lower
            .inherited_descriptor_authority()
            .map_err(|error| error.to_string())?;
        let state = state
            .inherited_descriptor_authority()
            .map_err(|error| error.to_string())?;
        let (receiver, sender) =
            crate::inherited_descriptor_transfer_pair().map_err(|error| error.to_string())?;
        let parent = unsafe { libc::getpid() };
        let creator = unsafe { libc::fork() };
        if creator < 0 {
            return Err(format!(
                "fork exact template creator: {}",
                std::io::Error::last_os_error()
            ));
        }
        if creator == 0 {
            arm_probe_child(parent);
            drop(receiver);
            let result = (|| {
                let template = create_overlay_template(
                    lower.inherited_descriptor()?,
                    state.inherited_descriptor()?,
                )?;
                sender
                    .into_sender()
                    .send(
                        b"view",
                        std::slice::from_ref(template.inherited_authority()),
                        crate::DescriptorTransferBounds::new(4, 1)
                            .map_err(|error| error.to_string())?,
                        crate::time::MonotonicDeadline::after(std::time::Duration::from_secs(10)),
                    )
                    .map_err(|error| error.to_string())
            })();
            finish_probe_child(result);
        }
        drop(sender);
        let received = receiver.receive(
            crate::DescriptorTransferBounds::new(4, 1).map_err(|error| error.to_string())?,
            crate::time::MonotonicDeadline::after(std::time::Duration::from_secs(10)),
        );
        // Always reap the exact creator, including malformed/failed receipt.
        // Its bounded alarm also prevents an inspection wait from hanging.
        require_probe_exit((LinuxSandboxProcess { pid: creator }).wait()?)?;
        let (payload, mut descriptors) = received.map_err(|error| error.to_string())?.into_parts();
        if payload != b"view" || descriptors.len() != 1 {
            return Err("template probe received an unexpected capability packet".to_string());
        }
        let template = LinuxOverlayTemplate::from_transferred_authority(
            descriptors.pop().unwrap().for_child()?,
        )?;
        bounded_probe_child(|| {
            enter_namespaces(LinuxSandboxNetwork::Isolated)?;
            mount_private_root()?;
            create_directory_target(&rooted(&PathBuf::from("/project"))?)?;
            mount_overlay(&LinuxSandboxOverlay {
                template: template.clone(),
                destination: PathBuf::from("/project"),
                writable_descendant_mounts: Vec::new(),
            })?;
            require_probe_bytes("/tmp/project/seed", b"lower")?;
            std::fs::write("/tmp/project/seed", b"borrower one")
                .map_err(|error| error.to_string())?;
            std::fs::create_dir("/tmp/project/private").map_err(|error| error.to_string())?;
            mount_raw(
                Some("tmpfs"),
                "/tmp/project/private",
                Some("tmpfs"),
                libc::MS_NOSUID | libc::MS_NODEV,
                Some("mode=0700"),
            )
            .map_err(|error| error.to_string())?;
            std::fs::write("/tmp/project/private/secret", b"private")
                .map_err(|error| error.to_string())
        })?;
        bounded_probe_child(|| {
            enter_namespaces(LinuxSandboxNetwork::Isolated)?;
            mount_private_root()?;
            create_directory_target(&rooted(&PathBuf::from("/project"))?)?;
            mount_overlay(&LinuxSandboxOverlay {
                template: template.clone(),
                destination: PathBuf::from("/project"),
                writable_descendant_mounts: Vec::new(),
            })?;
            require_probe_bytes("/tmp/project/seed", b"borrower one")?;
            if std::path::Path::new("/tmp/project/private/secret").exists() {
                return Err("template clone inherited another borrower's private mount".to_string());
            }
            std::fs::write("/tmp/project/seed", b"borrower two").map_err(|error| error.to_string())
        })?;
        drop(template);
        require_probe_bytes("/tmp/lower/seed", b"lower")?;
        require_probe_bytes("/tmp/state/upper/seed", b"borrower two")
    })
}

// Only the dedicated single-threaded inspection entry and explicitly isolated
// kernel tests use this finite fork helper. This is not a workload launcher.
fn bounded_probe_child(operation: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    let parent = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!(
            "fork template probe: {}",
            std::io::Error::last_os_error()
        ));
    }
    if pid == 0 {
        arm_probe_child(parent);
        finish_probe_child(operation());
    }
    require_probe_exit((LinuxSandboxProcess { pid }).wait()?)
}

fn arm_probe_child(parent: libc::pid_t) {
    unsafe {
        libc::alarm(20);
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent {
            libc::_exit(125);
        }
    }
}

fn finish_probe_child(result: Result<(), String>) -> ! {
    if let Err(error) = &result {
        eprintln!("detached overlay probe: {error}");
    }
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) }
}

fn require_probe_exit(outcome: LinuxSandboxExit) -> Result<(), String> {
    match outcome {
        LinuxSandboxExit::Code(0) => Ok(()),
        outcome => Err(format!("detached overlay probe failed: {outcome:?}")),
    }
}

fn require_probe_bytes(path: &str, expected: &[u8]) -> Result<(), String> {
    if std::fs::read(path).map_err(|error| error.to_string())? != expected {
        return Err(format!("shared-view probe bytes differ at {path}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATOR_PROBE_ENV: &str = "LILLUX_TEMPLATE_CREATOR_PROBE";
    const CREATOR_PROBE_PAYLOAD: &[u8] = b"exact-overlay-template";

    // This harness entry is reached only by the descriptor-configured exec
    // below. The inner fork makes namespace entry single-threaded, as required
    // by the production creator contract, without changing the test driver.
    #[test]
    fn template_transfer_creator_entry() {
        let Ok(encoded) = std::env::var(CREATOR_PROBE_ENV) else {
            return;
        };
        let [lower, state, sender]: [u32; 3] = serde_json::from_str(&encoded).unwrap();
        isolated_probe(|| {
            // SAFETY: the exact harness exec inherited this connected endpoint
            // from the typed parent request. No Rust owner survived that exec.
            let sender = unsafe { crate::take_inherited_descriptor_transfer_sender(sender) }
                .map_err(|error| error.to_string())?;
            let template = create_overlay_template(lower, state)?;
            sender
                .send(
                    CREATOR_PROBE_PAYLOAD,
                    std::slice::from_ref(template.inherited_authority()),
                    crate::DescriptorTransferBounds::new(CREATOR_PROBE_PAYLOAD.len(), 1)
                        .map_err(|error| error.to_string())?,
                    crate::time::MonotonicDeadline::after(std::time::Duration::from_secs(10)),
                )
                .map_err(|error| error.to_string())
        })
        .unwrap();
    }

    #[test]
    #[ignore = "requires exact descriptor transfer and detached Overlayfs cloning across sibling user namespaces; run alone"]
    fn transferred_template_survives_creator_exit_in_independent_borrowers() {
        let temporary = tempfile::tempdir().unwrap();
        let root = crate::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let lower = root.create_child(OsStr::new("lower"), 0o700).unwrap();
        let state = root.create_child(OsStr::new("state"), 0o700).unwrap();
        state.create_child(OsStr::new("upper"), 0o700).unwrap();
        state.create_child(OsStr::new("work"), 0o700).unwrap();
        lower
            .atomic_write_if_same(OsStr::new("seed"), None, b"lower", 0o600)
            .unwrap();
        let lower = lower.inherited_descriptor_authority().unwrap();
        let state = state.inherited_descriptor_authority().unwrap();
        let executable =
            crate::secure_fs::pin_canonical_mount_source(&std::env::current_exe().unwrap())
                .unwrap();
        let (receiver, sender) = crate::inherited_descriptor_transfer_pair().unwrap();
        let mut request = crate::SubprocessRequest {
            cmd: executable.path().to_string_lossy().into_owned(),
            argv0: None,
            args: vec![
                "--exact".into(),
                "sandbox::imp::detached_overlay::tests::template_transfer_creator_entry".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            cwd: None,
            envs: vec![(
                CREATOR_PROBE_ENV.into(),
                serde_json::to_string(&[
                    lower.inherited_descriptor().unwrap(),
                    state.inherited_descriptor().unwrap(),
                    sender.inherited_descriptor().unwrap(),
                ])
                .unwrap(),
            )],
            stdin_data: None,
            timeout: 20.0,
            limits: Some(crate::SubprocessLimits {
                max_stdout_bytes: Some(64 * 1024),
                max_stderr_bytes: Some(64 * 1024),
                ..crate::SubprocessLimits::default()
            }),
            inherited_fds: vec![executable, lower, state],
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        sender.retain_for_child(&mut request.inherited_fds);
        let creator = crate::spawn(request).unwrap();
        drop(sender);
        let packet = receiver.receive(
            crate::DescriptorTransferBounds::new(CREATOR_PROBE_PAYLOAD.len(), 1).unwrap(),
            crate::time::MonotonicDeadline::after(std::time::Duration::from_secs(10)),
        );
        // Reap the exact creator before borrowing. The template is its only
        // exported authority; no user/mount namespace descriptor or keeper
        // process is carried to this host-side owner.
        let result = creator.wait();
        assert!(result.success, "creator failed: {}", result.stderr);
        let (payload, mut descriptors) = packet.unwrap().into_parts();
        assert_eq!(payload, CREATOR_PROBE_PAYLOAD);
        assert_eq!(descriptors.len(), 1);
        let template = LinuxOverlayTemplate::from_transferred_authority(
            descriptors.pop().unwrap().for_child().unwrap(),
        )
        .unwrap();
        let runtime_view = template
            .inherited_authority()
            .open_or_create_private_directory_descendant(std::path::Path::new(
                ".ai/cache/ryeos-runtime/native-consumer/tmp",
            ))
            .unwrap();
        let runtime_view_mount = LinuxSandboxOverlayDescendantMount {
            source_fd: runtime_view.inherited_descriptor().unwrap(),
            relative_path: PathBuf::from(".ai/cache/ryeos-runtime/native-consumer/tmp"),
            destination: PathBuf::from("/runtime-view"),
        };

        // Both borrowers begin at the original host-side owner, not by
        // joining or nesting below the now-dead creator's user namespace.
        isolated_probe(|| {
            enter_namespaces(LinuxSandboxNetwork::Isolated)?;
            mount_private_root()?;
            attach_probe_clone(&template)?;
            let source = reanchor_overlay_descendant_source(
                &PathBuf::from("/project"),
                &runtime_view_mount,
            )?;
            let mount = LinuxSandboxMount {
                source_fd: source.inherited_descriptor()?,
                destination: runtime_view_mount.destination.clone(),
                access: LinuxSandboxMountAccess::Writable,
                layer: 10,
            };
            create_target(&rooted(&mount.destination)?, DescriptorKind::Directory)?;
            bind_descriptor_mount(&mount)?;
            std::fs::write("/tmp/runtime-view/marker", b"exact descendant")
                .map_err(|error| error.to_string())?;
            assert_bytes("/tmp/project/seed", b"lower")?;
            assert_bytes(
                "/tmp/project/.ai/cache/ryeos-runtime/native-consumer/tmp/marker",
                b"exact descendant",
            )?;
            std::fs::write("/tmp/project/seed", b"first borrower")
                .map_err(|error| error.to_string())
        })
        .unwrap();
        isolated_probe(|| {
            enter_namespaces(LinuxSandboxNetwork::Isolated)?;
            mount_private_root()?;
            attach_probe_clone(&template)?;
            assert_bytes("/tmp/project/seed", b"first borrower")?;
            assert_bytes(
                "/tmp/project/.ai/cache/ryeos-runtime/native-consumer/tmp/marker",
                b"exact descendant",
            )?;
            std::fs::write("/tmp/project/seed", b"second borrower")
                .map_err(|error| error.to_string())
        })
        .unwrap();
        drop(template);
        assert_eq!(
            std::fs::read(temporary.path().join("lower/seed")).unwrap(),
            b"lower"
        );
        assert_eq!(
            std::fs::read(temporary.path().join("state/upper/seed")).unwrap(),
            b"second borrower"
        );
    }

    // Kernel probes run alone in a forked, bounded, single-threaded process.
    // Neither namespace changes nor raw descriptor inspection leave Lillux.
    fn isolated_probe(operation: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        bounded_probe_child(operation)
    }

    fn attach_probe_clone(template: &LinuxOverlayTemplate) -> Result<(), String> {
        let destination = PathBuf::from("/project");
        create_directory_target(&rooted(&destination)?)?;
        mount_overlay(&LinuxSandboxOverlay {
            template: template.clone(),
            destination,
            writable_descendant_mounts: Vec::new(),
        })
    }

    fn assert_bytes(path: &str, expected: &[u8]) -> Result<(), String> {
        require_probe_bytes(path, expected)
    }

    #[test]
    #[ignore = "requires native sandbox kernel facilities; run alone to exercise the actual production inspector"]
    fn actual_native_inspection_requires_transferred_shared_view() {
        let inspection = inspect().unwrap();
        assert!(inspection.overlay_workspace);
    }

    #[test]
    #[ignore = "requires detached Overlayfs clone support and fresh Linux user/mount namespaces; run alone"]
    fn one_template_shares_edits_but_not_borrower_private_mounts() {
        // This tests the production creator and clone mechanics, but these
        // borrowers are nested below the creator's user namespace. The final
        // production topology returns the template to its host-side owner and
        // creates independent sibling namespaces. That requires the private
        // descriptor-transfer integration test; this probe cannot replace it.
        let temporary = tempfile::tempdir().unwrap();
        let root = crate::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let lower = root.create_child(OsStr::new("lower"), 0o700).unwrap();
        let state = root.create_child(OsStr::new("state"), 0o700).unwrap();
        state.create_child(OsStr::new("upper"), 0o700).unwrap();
        state.create_child(OsStr::new("work"), 0o700).unwrap();
        lower
            .atomic_write_if_same(OsStr::new("seed"), None, b"lower", 0o600)
            .unwrap();
        let lower_fd = lower.try_clone_descriptor().unwrap();
        let state_fd = state.try_clone_descriptor().unwrap();

        isolated_probe(|| {
            let template =
                create_overlay_template(lower_fd.as_raw_fd() as u32, state_fd.as_raw_fd() as u32)?;
            mount_private_root()?;
            attach_probe_clone(&template)?;
            std::fs::write("/tmp/project/seed", b"parent").map_err(|error| error.to_string())?;
            std::fs::write("/tmp/project/old", b"rename").map_err(|error| error.to_string())?;
            std::fs::write("/tmp/project/remove", b"delete").map_err(|error| error.to_string())?;
            std::fs::create_dir("/tmp/project/private").map_err(|error| error.to_string())?;
            let parent_user =
                mount_source_stat(File::open("/proc/self/ns/user").unwrap().as_raw_fd())?.st_ino;
            let parent_mount =
                mount_source_stat(File::open("/proc/self/ns/mnt").unwrap().as_raw_fd())?.st_ino;

            isolated_probe(|| {
                enter_namespaces(LinuxSandboxNetwork::Isolated)?;
                let user =
                    mount_source_stat(File::open("/proc/self/ns/user").unwrap().as_raw_fd())?
                        .st_ino;
                let mount =
                    mount_source_stat(File::open("/proc/self/ns/mnt").unwrap().as_raw_fd())?.st_ino;
                if user == parent_user || mount == parent_mount {
                    return Err("borrower reused a parent's confinement namespace".to_string());
                }
                mount_private_root()?;
                attach_probe_clone(&template)?;
                assert_bytes("/tmp/project/seed", b"parent")?;
                std::fs::write("/tmp/project/seed", b"child").map_err(|error| error.to_string())?;
                std::fs::rename("/tmp/project/old", "/tmp/project/renamed")
                    .map_err(|error| error.to_string())?;
                std::fs::remove_file("/tmp/project/remove").map_err(|error| error.to_string())?;
                mount_raw(
                    Some("tmpfs"),
                    "/tmp/project/private",
                    Some("tmpfs"),
                    libc::MS_NOSUID | libc::MS_NODEV,
                    Some("mode=0700"),
                )
                .map_err(|error| format!("mount borrower-private probe: {error}"))?;
                std::fs::write("/tmp/project/private/secret", b"child only")
                    .map_err(|error| error.to_string())?;
                Ok(())
            })?;
            assert_bytes("/tmp/project/seed", b"child")?;
            assert_bytes("/tmp/project/renamed", b"rename")?;
            if std::path::Path::new("/tmp/project/remove").exists()
                || std::path::Path::new("/tmp/project/private/secret").exists()
            {
                return Err("child deletion or private-mount isolation failed".to_string());
            }
            isolated_probe(|| {
                enter_namespaces(LinuxSandboxNetwork::Isolated)?;
                mount_private_root()?;
                attach_probe_clone(&template)?;
                assert_bytes("/tmp/project/seed", b"child")?;
                if std::path::Path::new("/tmp/project/private/secret").exists() {
                    return Err("later borrower inherited an earlier private mount".to_string());
                }
                std::fs::write("/tmp/project/seed", b"later child")
                    .map_err(|error| error.to_string())?;
                Ok(())
            })?;
            assert_bytes("/tmp/project/seed", b"later child")
        })
        .unwrap();
        assert_eq!(
            std::fs::read(temporary.path().join("lower/seed")).unwrap(),
            b"lower"
        );
    }

    #[test]
    fn template_refuses_regular_descriptor_before_any_namespace_change() {
        let regular = crate::sealed_memfd(c"not-an-overlay", b"bytes").unwrap();
        let error = LinuxOverlayTemplate::from_transferred_authority(regular).unwrap_err();
        assert!(error.contains("not a directory"), "{error}");
    }

    #[test]
    fn template_refuses_non_overlay_directory() {
        // proc is unambiguously not Overlayfs even when this test's checkout
        // or temporary directory itself happens to run on Overlayfs.
        let directory =
            crate::secure_fs::pin_canonical_mount_source(std::path::Path::new("/proc")).unwrap();
        let error = LinuxOverlayTemplate::from_transferred_authority(directory).unwrap_err();
        assert!(error.contains("not an overlay filesystem"), "{error}");
    }

    #[test]
    fn template_refuses_aliased_inputs_before_any_namespace_change() {
        let error = create_overlay_template(999_999, 999_999).unwrap_err();
        assert!(error.contains("authorities alias"), "{error}");
    }

    #[test]
    fn template_requires_prepared_upper_and_work_before_namespace_change() {
        let temporary = tempfile::tempdir().unwrap();
        let root = crate::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let lower = root.create_child(OsStr::new("lower"), 0o700).unwrap();
        let state = root.create_child(OsStr::new("state"), 0o700).unwrap();
        let lower_fd = lower.try_clone_descriptor().unwrap();
        let state_fd = state.try_clone_descriptor().unwrap();
        let error =
            create_overlay_template(lower_fd.as_raw_fd() as u32, state_fd.as_raw_fd() as u32)
                .unwrap_err();
        assert!(error.contains("upper is missing"), "{error}");
        state.create_child(OsStr::new("upper"), 0o700).unwrap();
        let error =
            create_overlay_template(lower_fd.as_raw_fd() as u32, state_fd.as_raw_fd() as u32)
                .unwrap_err();
        assert!(error.contains("work is missing"), "{error}");
    }
}

//! Descriptor-rooted fixed-parent views. Callers supply every denied path and
//! bound; there is no product, policy-file, or workload vocabulary here.
use super::*;

#[derive(Default)]
struct Branch {
    denied: bool,
    children: BTreeMap<OsString, Branch>,
}

fn tree(view: &LinuxSandboxFixedParentView) -> Result<Branch, String> {
    if view.max_entries == 0 || view.max_depth == 0 || view.denied_paths.is_empty() {
        return Err("fixed-parent view requires explicit positive bounds and denied paths".into());
    }
    let mut root = Branch::default();
    let mut previous: Option<&PathBuf> = None;
    let mut entries = 0usize;
    for path in &view.denied_paths {
        if path.as_os_str().as_bytes().contains(&0) || previous.is_some_and(|prev| prev >= path) {
            return Err("fixed-parent denied paths must be NUL-free, sorted and unique".into());
        }
        let parts = path.components().collect::<Vec<_>>();
        if parts.len() < 2 || parts.len() > view.max_depth {
            return Err(
                "fixed-parent denial requires an existing ancestor and bounded depth".into(),
            );
        }
        let mut current = &mut root;
        for part in parts {
            let std::path::Component::Normal(name) = part else {
                return Err("fixed-parent denied paths must be normalized relative paths".into());
            };
            if current.denied {
                return Err("fixed-parent denied paths overlap".into());
            }
            if !current.children.contains_key(name) {
                entries = entries
                    .checked_add(1)
                    .ok_or("fixed-parent entry count overflow")?;
                if entries > view.max_entries {
                    return Err("fixed-parent path tree exceeds entry bound".into());
                }
            }
            current = current.children.entry(name.to_owned()).or_default();
        }
        if !current.children.is_empty() {
            return Err("fixed-parent denied paths overlap".into());
        }
        current.denied = true;
        previous = Some(path);
    }
    Ok(root)
}

pub(super) fn validate(request: &LinuxSandboxRequest) -> Result<(), String> {
    for mount in &request.mounts {
        for ancestor in &request.mounts {
            if ancestor.destination != mount.destination
                && mount.destination.starts_with(&ancestor.destination)
                && (ancestor.layer > mount.layer
                    || descriptor_kind(ancestor.source_fd)? != DescriptorKind::Directory)
            {
                return Err(
                    "mount ancestor must be a directory installed before its children".into(),
                );
            }
        }
        if request
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.destination.starts_with(&mount.destination))
        {
            return Err("ordinary mount would hide the private workspace overlay".into());
        }
    }
    let mut destinations = BTreeSet::new();
    for view in &request.fixed_parent_views {
        validate_absolute_path(&view.destination, "fixed-parent destination")?;
        if !destinations.insert(&view.destination) {
            return Err("duplicate fixed-parent view".into());
        }
        tree(view)?;
        let source = request
            .mounts
            .iter()
            .find(|mount| mount.destination == view.destination)
            .ok_or("fixed-parent view has no exact directory mount")?;
        if descriptor_kind(source.source_fd)? != DescriptorKind::Directory {
            return Err("fixed-parent source is not a directory".into());
        }
        // Kernel-reported locators can reject known aliases; they never grant
        // authority. The original descriptors and reanchoring identity check
        // still decide which objects can be mounted. This is not a claim to
        // find arbitrary pre-existing hardlink or host bind-mount aliases.
        let source_path = std::fs::read_link(format!("/proc/self/fd/{}", source.source_fd))
            .map_err(|error| format!("locate fixed-parent source: {error}"))?;
        let source_stat = mount_source_stat(raw_fd(source.source_fd)?)?;
        let mut other_sources = request
            .mounts
            .iter()
            .filter(|mount| mount.destination != view.destination)
            .map(|mount| mount.source_fd)
            .collect::<BTreeSet<_>>();
        if let Some(overlay) = &request.overlay {
            other_sources.insert(
                overlay
                    .template
                    .inherited_authority()
                    .inherited_descriptor()?,
            );
            other_sources.extend(
                overlay
                    .writable_descendant_mounts
                    .iter()
                    .map(|mount| mount.source_fd),
            );
        }
        for fd in other_sources {
            let stat = mount_source_stat(raw_fd(fd)?)?;
            if (stat.st_dev, stat.st_ino) == (source_stat.st_dev, source_stat.st_ino) {
                return Err("unfiltered mount aliases a fixed-parent source".into());
            }
            if mount_source_is_sealed(raw_fd(fd)?)? {
                continue;
            }
            let path = std::fs::read_link(format!("/proc/self/fd/{fd}"))
                .map_err(|error| format!("locate potential filtered-source alias: {error}"))?;
            if view.denied_paths.iter().any(|denied| {
                let denied = source_path.join(denied);
                path.starts_with(&denied) || denied.starts_with(&path)
            }) {
                return Err("unfiltered mount reintroduces a protected source path".into());
            }
        }
        for mount in &request.mounts {
            if mount.destination == view.destination {
                continue;
            }
            for denied in &view.denied_paths {
                let denied = view.destination.join(denied);
                if mount.destination.starts_with(&denied)
                    || (denied.starts_with(&mount.destination)
                        && mount.destination.starts_with(&view.destination))
                {
                    return Err("positive mount conflicts with a fixed-parent restriction".into());
                }
            }
        }
        if let Some(overlay) = &request.overlay {
            for mount in &overlay.writable_descendant_mounts {
                for denied in &view.denied_paths {
                    let denied = view.destination.join(denied);
                    if mount.destination.starts_with(&denied)
                        || (denied.starts_with(&mount.destination)
                            && mount.destination.starts_with(&view.destination))
                    {
                        return Err(
                            "overlay-descendant mount conflicts with a fixed-parent restriction"
                                .into(),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn install(
    mount: &LinuxSandboxMount,
    view: &LinuxSandboxFixedParentView,
    index: usize,
) -> Result<(), String> {
    let source = inherited_directory(mount.source_fd, "filtered source")?;
    let branches = tree(view)?;
    let mut remaining = view.max_entries;
    for (ordinal, (name, branch)) in branches.children.iter().enumerate() {
        let source = source
            .open_child_directory(name)
            .map_err(|error| error.to_string())?
            .ok_or("fixed-parent top ancestor must already exist as a real source directory")?;
        let destination = view.destination.join(name);
        let target = open_mount_target_no_symlinks(&destination)?;
        let expected = source.device_inode().map_err(|error| error.to_string())?;
        let observed = mount_source_stat(target.as_raw_fd())?;
        if expected != (observed.st_dev, observed.st_ino) {
            return Err("fixed-parent source ancestor changed before view attachment".into());
        }
        // A fresh private mount is the only place where synthetic entries are
        // created. Moving it onto an exact source target never writes source
        // entries. The staging alias is removed before target release.
        let staging = PathBuf::from(format!("{ROOT}/.lillux-fixed-parent-{index}-{ordinal}"));
        mkdir_one(path_string(&staging)?, 0o700).map_err(|error| error.to_string())?;
        populate(
            Some(&source),
            branch,
            &staging,
            mount.access,
            &mut remaining,
        )?;
        move_path_mount_to_target(&staging, target.as_raw_fd())?;
        let name = c_string(staging.as_os_str(), "filtered staging alias")?;
        syscall_zero(
            unsafe { libc::rmdir(name.as_ptr()) },
            "remove filtered staging alias",
        )?;
    }
    Ok(())
}

fn populate(
    source: Option<&crate::PinnedDirectory>,
    branch: &Branch,
    destination: &PathBuf,
    access: LinuxSandboxMountAccess,
    remaining: &mut usize,
) -> Result<(), String> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or("fixed-parent construction exceeds entry bound")?;
    mount_raw(
        Some("tmpfs"),
        path_string(destination)?,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755"),
    )
    .map_err(|error| format!("mount private connector: {error}"))?;
    let entries = match source {
        Some(source) => source
            .entries_no_follow_bounded(*remaining)
            .map_err(|error| error.to_string())?,
        None => Vec::new(),
    };
    *remaining = remaining
        .checked_sub(entries.len())
        .ok_or("fixed-parent entries exceed bound")?;
    let private = crate::PinnedDirectory::open(destination)
        .map_err(|error| error.to_string())?
        .ok_or("private connector disappeared")?;
    for entry in &entries {
        if branch.children.contains_key(&entry.name) {
            continue;
        }
        let source = source.expect("entries came from this descriptor");
        if entry.entry_type == crate::PinnedEntryType::Symlink {
            // PATH_MAX is the kernel's pathname bound, not a RyeOS path rule.
            let target = source
                .read_symlink_target(&entry.name, libc::PATH_MAX as usize)
                .map_err(|error| error.to_string())?
                .ok_or("source symlink disappeared")?;
            private
                .create_symlink(&entry.name, &target)
                .map_err(|error| error.to_string())?;
            continue;
        }
        let pinned = source
            .open_mount_entry(&entry.name)
            .map_err(|error| error.to_string())?
            .ok_or("source entry disappeared during filtered construction")?;
        let stat = mount_source_stat(pinned.as_raw_fd())?;
        if (stat.st_dev, stat.st_ino, stat.st_mode)
            != (entry.containing_device, entry.inode, entry.mode)
        {
            return Err("source entry changed during filtered construction".into());
        }
        let target = destination.join(&entry.name);
        create_target(&target, descriptor_kind(pinned.as_raw_fd() as u32)?)?;
        let relative = PathBuf::from("/").join(
            target
                .strip_prefix(ROOT)
                .map_err(|error| error.to_string())?,
        );
        let target = open_mount_target_no_symlinks(&relative)?;
        bind_fd_to_mount_target(
            pinned.as_raw_fd(),
            target.as_raw_fd(),
            access == LinuxSandboxMountAccess::ReadOnly,
            entry.entry_type == crate::PinnedEntryType::Directory,
        )?;
    }
    for (name, child) in &branch.children {
        if child.denied {
            continue;
        }
        let original = source
            .map(|source| source.open_child_directory(name))
            .transpose()
            .map_err(|error| error.to_string())?
            .flatten();
        let target = destination.join(name);
        mkdir_one(path_string(&target)?, 0o755).map_err(|error| error.to_string())?;
        populate(original.as_ref(), child, &target, access, remaining)?;
    }
    if let Some(source) = source {
        if source
            .entries_no_follow_bounded(entries.len())
            .map_err(|error| error.to_string())?
            != entries
        {
            return Err("source topology changed during filtered construction".into());
        }
    }
    // Nonrecursive: permitted child binds retain their separately admitted
    // access. Recursive read-only sealing would silently disable live writes.
    set_mount_attributes(destination, true, true, false)
}

/// Run inside the probe's private namespace/root. Capability testimony must
/// prove the nonrecursive sealing behavior, not merely syscall availability.
pub(super) fn probe() -> Result<(), String> {
    let source_path = PathBuf::from(format!("{ROOT}/.fixed-parent-probe-source"));
    let target_path = PathBuf::from(format!("{ROOT}/.fixed-parent-probe-target"));
    std::fs::create_dir_all(source_path.join("control/open")).map_err(|error| error.to_string())?;
    std::fs::write(source_path.join("control/secret"), b"private")
        .map_err(|error| error.to_string())?;
    let source = crate::PinnedDirectory::open(&source_path)
        .map_err(|error| error.to_string())?
        .ok_or("probe source missing")?;
    let source = source
        .try_clone_descriptor()
        .map_err(|error| error.to_string())?;
    create_target(&target_path, DescriptorKind::Directory)?;
    let mount = LinuxSandboxMount {
        source_fd: source.as_raw_fd() as u32,
        destination: PathBuf::from("/.fixed-parent-probe-target"),
        access: LinuxSandboxMountAccess::Writable,
        layer: 0,
    };
    bind_descriptor_mount(&mount)?;
    install(
        &mount,
        &LinuxSandboxFixedParentView {
            destination: mount.destination.clone(),
            denied_paths: vec![PathBuf::from("control/secret")],
            // Probe fixture bounds only; execution bounds always come from caller.
            max_entries: 16,
            max_depth: 2,
        },
        0,
    )?;
    if target_path
        .join("control/secret")
        .try_exists()
        .map_err(|error| error.to_string())?
    {
        return Err("fixed-parent probe exposed denied entry".into());
    }
    let refusal = std::fs::write(target_path.join("control/secret"), b"replacement")
        .err()
        .ok_or("fixed-parent connector admitted a denied write")?;
    if refusal.raw_os_error() != Some(libc::EROFS) {
        return Err(format!(
            "fixed-parent probe did not prove read-only connector: {refusal}"
        ));
    }
    std::fs::write(target_path.join("control/open/live"), b"live")
        .map_err(|error| format!("fixed-parent live child write: {error}"))?;
    std::fs::write(target_path.join("root-live"), b"root")
        .map_err(|error| format!("fixed-parent live root write: {error}"))?;
    if std::fs::read(source_path.join("control/open/live")).map_err(|error| error.to_string())?
        != b"live"
        || std::fs::read(source_path.join("root-live")).map_err(|error| error.to_string())?
            != b"root"
        || std::fs::read(source_path.join("control/secret")).map_err(|error| error.to_string())?
            != b"private"
    {
        return Err("fixed-parent probe lost live writes or changed protected source".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> LinuxSandboxFixedParentView {
        LinuxSandboxFixedParentView {
            destination: PathBuf::from("/project"),
            denied_paths: vec![
                "control/absent".into(),
                "control/file".into(),
                "control/nested/missing".into(),
                "control/private".into(),
            ],
            max_entries: 64,
            max_depth: 4,
        }
    }

    #[test]
    fn fixed_parent_contract_rejects_unbounded_or_ambiguous_paths() {
        assert!(tree(&view()).is_ok());
        for paths in [
            vec!["control"],
            vec!["/control/private"],
            vec!["control/../private"],
            vec!["control/private", "control/private/child"],
            vec!["control/private", "control/file"],
        ] {
            let mut invalid = view();
            invalid.denied_paths = paths.into_iter().map(PathBuf::from).collect();
            assert!(tree(&invalid).is_err(), "{:?}", invalid.denied_paths);
        }
        let mut invalid = view();
        invalid.max_entries = 1;
        assert!(tree(&invalid).is_err());
        invalid = view();
        invalid.max_depth = 1;
        assert!(tree(&invalid).is_err());
    }

    #[test]
    fn fixed_parent_validation_rejects_raw_alias_and_positive_override() {
        let temporary = tempfile::tempdir().unwrap();
        let source = crate::secure_fs::pin_canonical_mount_source(temporary.path()).unwrap();
        let root_mount = LinuxSandboxMount {
            source_fd: source.inherited_descriptor().unwrap(),
            destination: PathBuf::from("/project"),
            access: LinuxSandboxMountAccess::Writable,
            layer: 0,
        };
        let mut request = super::super::super::tests::minimal_request();
        request.mounts = vec![root_mount.clone()];
        request.fixed_parent_views = vec![view()];
        validate(&request).unwrap();
        request.mounts.push(LinuxSandboxMount {
            destination: "/alias".into(),
            ..root_mount
        });
        assert!(validate(&request).unwrap_err().contains("aliases"));
        let other = tempfile::tempdir().unwrap();
        let other = crate::secure_fs::pin_canonical_mount_source(other.path()).unwrap();
        request.mounts[1].source_fd = other.inherited_descriptor().unwrap();
        request.mounts[1].destination = "/project/control/file".into();
        assert!(validate(&request).unwrap_err().contains("conflicts"));
        request.mounts[1].destination = "/project/control".into();
        assert!(validate(&request).unwrap_err().contains("conflicts"));
    }

    #[test]
    #[ignore = "requires the supported Linux user/mount/network namespace floor"]
    fn fixed_parent_real_view_preserves_live_writes_and_denies_all_leaf_shapes() {
        for access in [
            LinuxSandboxMountAccess::Writable,
            LinuxSandboxMountAccess::ReadOnly,
        ] {
            let source_dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(source_dir.path().join("control/open")).unwrap();
            std::fs::create_dir_all(source_dir.path().join("control/private")).unwrap();
            std::fs::write(source_dir.path().join("control/file"), b"secret").unwrap();
            std::fs::write(source_dir.path().join("control/plain"), b"plain").unwrap();
            std::os::unix::fs::symlink("private", source_dir.path().join("control/link")).unwrap();
            let source = crate::secure_fs::pin_canonical_mount_source(source_dir.path()).unwrap();
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let result = (|| {
                    enter_namespaces(LinuxSandboxNetwork::Isolated)?;
                    let source = reanchor_mount_source(source.file().as_raw_fd())?;
                    mount_private_root()?;
                    let target = rooted(&PathBuf::from("/project"))?;
                    create_target(&target, DescriptorKind::Directory)?;
                    let mount = LinuxSandboxMount {
                        source_fd: source.inherited_descriptor()?,
                        destination: "/project".into(),
                        access,
                        layer: 0,
                    };
                    bind_descriptor_mount(&mount)?;
                    install(&mount, &view(), 0)?;
                    for denied in [
                        "control/absent",
                        "control/file",
                        "control/private",
                        "control/nested/missing",
                        "control/link",
                    ] {
                        if target
                            .join(denied)
                            .try_exists()
                            .map_err(|e| e.to_string())?
                        {
                            return Err(format!("exposed {denied}"));
                        }
                    }
                    for path in ["control/absent", "control/nested/missing", "control/new"] {
                        let error = std::fs::write(target.join(path), b"forbidden")
                            .err()
                            .ok_or("connector accepted creation")?;
                        if error.raw_os_error() != Some(libc::EROFS) {
                            return Err(error.to_string());
                        }
                    }
                    if std::fs::rename(
                        target.join("control/plain"),
                        target.join("control/replaced"),
                    )
                    .is_ok()
                        || std::fs::remove_file(target.join("control/plain")).is_ok()
                        || std::fs::rename(target.join("control"), target.join("renamed")).is_ok()
                    {
                        return Err("connector membership was mutable".into());
                    }
                    for path in ["live-root", "control/open/live", "control/plain"] {
                        let result = std::fs::write(target.join(path), b"changed");
                        if access == LinuxSandboxMountAccess::Writable {
                            result.map_err(|e| e.to_string())?;
                        } else if result.err().and_then(|e| e.raw_os_error()) != Some(libc::EROFS) {
                            return Err("read-only source admitted writes".into());
                        }
                    }
                    Ok::<(), String>(())
                })();
                if let Err(error) = &result {
                    eprintln!("fixed-parent child: {error}");
                }
                unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            assert_eq!(
                std::fs::read(source_dir.path().join("control/file")).unwrap(),
                b"secret"
            );
            assert!(!source_dir.path().join("control/nested").exists());
            assert!(!source_dir.path().join("control/absent").exists());
            assert_eq!(
                source_dir.path().join("live-root").exists(),
                access == LinuxSandboxMountAccess::Writable
            );
            if access == LinuxSandboxMountAccess::Writable {
                assert_eq!(
                    std::fs::read(source_dir.path().join("control/open/live")).unwrap(),
                    b"changed"
                );
                assert_eq!(
                    std::fs::read(source_dir.path().join("control/plain")).unwrap(),
                    b"changed"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires the supported Linux user/mount/network namespace floor"]
    fn fixed_parent_partial_setup_failure_never_creates_source_targets() {
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("control")).unwrap();
        std::fs::write(source_dir.path().join("control/file"), b"source").unwrap();
        let source = crate::secure_fs::pin_canonical_mount_source(source_dir.path()).unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let result = (|| {
                enter_namespaces(LinuxSandboxNetwork::Isolated)?;
                let source = reanchor_mount_source(source.file().as_raw_fd())?;
                mount_private_root()?;
                create_target(
                    &rooted(&PathBuf::from("/project"))?,
                    DescriptorKind::Directory,
                )?;
                let mount = LinuxSandboxMount {
                    source_fd: source.inherited_descriptor()?,
                    destination: "/project".into(),
                    access: LinuxSandboxMountAccess::Writable,
                    layer: 0,
                };
                bind_descriptor_mount(&mount)?;
                let mut denied = view();
                denied.denied_paths = vec!["control/file".into(), "missing/secret".into()];
                let error = install(&mount, &denied, 0)
                    .err()
                    .ok_or("missing top ancestor was accepted")?;
                if !error.contains("top ancestor") {
                    return Err(error);
                }
                Ok::<(), String>(())
            })();
            if let Err(error) = &result {
                eprintln!("fixed-parent failure probe: {error}");
            }
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 125 }) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert_eq!(
            std::fs::read(source_dir.path().join("control/file")).unwrap(),
            b"source"
        );
        assert!(!source_dir.path().join("missing").exists());
        assert_eq!(std::fs::read_dir(source_dir.path()).unwrap().count(), 1);
    }
}

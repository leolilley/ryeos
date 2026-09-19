#!/usr/bin/env python3
"""Install an explicitly pinned OCI adapter and register an opt-in Docker runtime.

Does not reload/restart Docker, initialize nodes, or change the default runtime.
The operator activates the validated configuration in their service manager.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile


RUNTIME = "ryeos-contained"


def merge_config(existing, executable, runc):
    if not isinstance(existing, dict):
        raise ValueError("Docker configuration must be an object")
    result = dict(existing)
    runtimes = result.get("runtimes", {})
    if not isinstance(runtimes, dict):
        raise ValueError("Docker runtimes must be an object")
    runtime = {"path": str(executable), "runtimeArgs": ["docker-runtime", "--runc", str(runc), "--"]}
    if RUNTIME in runtimes and runtimes[RUNTIME] != runtime:
        raise ValueError("ryeos-contained is already registered differently; explicit runtime migration is required")
    result["runtimes"] = dict(runtimes, **{RUNTIME: runtime})
    return result


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate Docker configuration key: {key}")
        result[key] = value
    return result


def protected_directory(path, create=False):
    """Walk from / through root-owned, non-writable directory descriptors."""
    path = Path(path)
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("administrator path must be absolute without parent traversal")
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.parts[1:]:
            if create:
                try:
                    os.mkdir(part, 0o755, dir_fd=descriptor)
                except FileExistsError:
                    pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
            metadata = os.fstat(descriptor)
            if metadata.st_uid != 0 or metadata.st_mode & 0o022:
                raise ValueError(f"unsafe administrator directory: {path}")
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def read_regular(path, parent=None, protected=False, maximum=536870912, *, allow_hardlinks=False):
    descriptor = os.open(str(path), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
    with os.fdopen(descriptor, "rb") as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > maximum:
            raise ValueError(f"unsafe or oversized file: {path}")
        if metadata.st_nlink != 1 and (protected or not allow_hardlinks):
            raise ValueError(f"file must have exactly one hard link: {path}")
        if protected and (metadata.st_uid != 0 or metadata.st_mode & 0o022):
            raise ValueError(f"file is not administrator protected: {path}")
        value = stream.read(maximum + 1)
        if len(value) > maximum:
            raise ValueError(f"file exceeds size bound: {path}")
        return value


def require_executable(path):
    parent = protected_directory(path.parent)
    try:
        descriptor = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=parent)
        try:
            metadata = os.fstat(descriptor)
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0
                    or metadata.st_nlink != 1 or metadata.st_mode & 0o022
                    or not metadata.st_mode & 0o111):
                raise ValueError(f"unsafe installed executable: {path}")
        finally:
            os.close(descriptor)
    finally:
        os.close(parent)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--runc", type=Path, default=Path("/usr/bin/runc"))
    parser.add_argument("--dockerd", type=Path, default=Path("/usr/bin/dockerd"))
    parser.add_argument("--config", type=Path, default=Path("/etc/docker/daemon.json"))
    parser.add_argument("--confirm", action="store_true", required=True)
    args = parser.parse_args()
    if os.geteuid() != 0:
        raise ValueError("installation requires administrator authority")
    if not re.fullmatch(r"[0-9a-f]{64}", args.sha256):
        raise ValueError("--sha256 must pin the exact adapter bytes")
    require_executable(args.runc)
    require_executable(args.dockerd)
    # Cargo hard-links target/debug/<binary> to its deps artifact. This is an
    # untrusted input snapshot: authenticate the bytes below, then copy those
    # same bytes into a fresh protected inode. Installed files stay single-link.
    binary = read_regular(args.binary, allow_hardlinks=True)
    if hashlib.sha256(binary).hexdigest() != args.sha256:
        raise ValueError("adapter digest does not match --sha256")

    config_parent = protected_directory(args.config.parent, create=True)
    try:
        # Serialize installers in this directory without placing a writable lock
        # file into a pre-existing namespace. Uncoordinated edits are rechecked.
        fcntl.flock(config_parent, fcntl.LOCK_EX)
        try:
            original = read_regular(args.config.name, config_parent, True, 1048576)
        except FileNotFoundError:
            original = None
        existing = json.loads(original, object_pairs_hook=unique_object) if original is not None else {}
        installation = Path("/usr/lib/ryeos/contained-oci") / args.sha256
        executable = installation / "ryeos-lillux-oci-hook"
        merged = merge_config(existing, executable, args.runc)

        installed_parent = protected_directory(installation, create=True)
        try:
            try:
                installed = read_regular(executable.name, installed_parent, True)
            except FileNotFoundError:
                installed = None
            if installed is not None and installed != binary:
                raise ValueError("immutable adapter installation contains different bytes")
            if installed is None:
                fd, temporary = tempfile.mkstemp(prefix=".hook-", dir=installation)
                try:
                    with os.fdopen(fd, "wb") as stream:
                        stream.write(binary)
                        stream.flush()
                        os.fchmod(stream.fileno(), 0o555)
                        os.fsync(stream.fileno())
                    # No overwrite of an installed immutable coordinate.
                    os.link(temporary, executable, follow_symlinks=False)
                    os.unlink(temporary)
                    temporary = None
                    os.fsync(installed_parent)
                finally:
                    if temporary is not None:
                        os.unlink(temporary)
            require_executable(executable)
        finally:
            os.close(installed_parent)

        fd, temporary = tempfile.mkstemp(prefix=".ryeos-runtime-", dir=args.config.parent)
        try:
            with os.fdopen(fd, "w") as stream:
                json.dump(merged, stream, indent=2)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            subprocess.run([str(args.dockerd), "--validate", "--config-file", temporary], check=True)
            subprocess.run([str(executable), "install-host-state"], check=True)
            try:
                current = read_regular(args.config.name, config_parent, True, 1048576)
            except FileNotFoundError:
                current = None
            if current != original:
                raise ValueError("Docker configuration changed during installation; refusing replacement")
            if original is not None:
                backup = args.config.name + ".before-ryeos-" + hashlib.sha256(original).hexdigest()
                try:
                    backup_fd = os.open(backup, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=config_parent)
                except FileExistsError:
                    if read_regular(backup, config_parent, True, 1048576) != original:
                        raise ValueError("configuration backup has unexpected contents")
                else:
                    with os.fdopen(backup_fd, "wb") as stream:
                        stream.write(original)
                        stream.flush()
                        os.fsync(stream.fileno())
            os.replace(temporary, args.config)
            temporary = None
            os.fsync(config_parent)
        finally:
            if temporary is not None:
                os.unlink(temporary)
    finally:
        os.close(config_parent)
    print(f"Installed {executable}")
    print(f"Registered opt-in {RUNTIME} runtime in {args.config}; default runtime preserved.")
    print("Docker was not reloaded or restarted. Activate this configuration through your Docker service manager.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"contained Docker runtime installation failed: {error}")

"""Enabled-generation guardian: never release roots from a service stop hook."""
import fcntl
import os
import re
from pathlib import Path
import stat
from contextlib import contextmanager
import subprocess
import sys
import time

# Assigned before this source by the evaluated NixOS module.
ROOTS: str
SYSTEMCTL: str
SENDER: str

def enabled():
    current = Path("/run/current-system")
    if not (current / "etc/systemd/system").is_dir():
        raise RuntimeError("no activated NixOS generation; retaining roots")
    return (current / "etc/repro2-sender-lifecycle").is_file()


def worker_inactive():
    result = subprocess.run([SYSTEMCTL, "show", "repro2-sender.service", "--property=ActiveState", "--value"],
                            text=True, capture_output=True, check=True)
    return result.stdout.strip() in ("inactive", "failed")


def system_running():
    result = subprocess.run([SYSTEMCTL, "show", "--property=SystemState", "--value"],
                            text=True, capture_output=True, check=True)
    return result.stdout.strip() in ("running", "degraded")


def release_roots():
    # The expected two-level tree is job-id / numbered store symlinks.
    # Validate the entire tree before unlinking anything, never recurse, and
    # anchor every operation at opened O_NOFOLLOW directory descriptors.
    if not isinstance(ROOTS, str) or not ROOTS.startswith("/nix/var/nix/gcroots/"):
        raise RuntimeError("GC roots must be strictly below /nix/var/nix/gcroots")
    parts = ROOTS.split("/")[1:]
    if any(not re.fullmatch(r"[A-Za-z0-9._-]+", part) or part in (".", "..") for part in parts):
        raise RuntimeError("non-normalized GC root path")
    if parts[4] in ("auto", "per-user", "profiles"):
        raise RuntimeError("reserved Nix GC roots subtree")
    descriptors = []
    jobs = []
    try:
        try:
            parent = secure_directory("/" + "/".join(parts[:-1]))
            descriptors.append(parent)
            roots = os.open(parts[-1], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
        except FileNotFoundError:
            return
        descriptors.append(roots)
        check_directory(roots, private=True)
        root_metadata = os.fstat(roots)
        for name in os.listdir(roots):
            if not re.fullmatch(r"[0-9]+-[0-9]+", name):
                raise RuntimeError("unexpected job directory")
            job = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=roots)
            descriptors.append(job)
            check_directory(job, private=True)
            if os.fstat(job).st_dev != root_metadata.st_dev:
                raise RuntimeError("unexpected mounted job subtree")
            names = os.listdir(job)
            for number in names:
                metadata = os.stat(number, dir_fd=job, follow_symlinks=False)
                if not number.isascii() or not number.isdigit() or not stat.S_ISLNK(metadata.st_mode) or metadata.st_uid != 0:
                    raise RuntimeError("unexpected GC root entry")
                target = os.readlink(number, dir_fd=job)
                if not re.fullmatch(r"/nix/store/[0123456789abcdfghijklmnpqrsvwxyz]{32}-[A-Za-z0-9+._?=-]+", target):
                    raise RuntimeError("GC root target is not a top-level Nix store path")
            jobs.append((name, job, names))
        for name, job, names in jobs:
            # A root-only directory cannot be replaced by an untrusted writer.
            # Still detect replacement by another privileged process and fail.
            if os.stat(name, dir_fd=roots, follow_symlinks=False).st_ino != os.fstat(job).st_ino:
                raise RuntimeError("job directory changed during teardown")
            for number in names:
                os.unlink(number, dir_fd=job)
            os.fsync(job)
            os.rmdir(name, dir_fd=roots)
        os.fsync(roots)
        if os.stat(parts[-1], dir_fd=parent, follow_symlinks=False).st_ino != root_metadata.st_ino:
            raise RuntimeError("root directory changed during teardown")
        os.rmdir(parts[-1], dir_fd=parent)
        os.fsync(parent)
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)


def check_directory(fd, private=False):
    metadata = os.fstat(fd)
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & (0o077 if private else 0o022):
        raise RuntimeError("unsafe root-owned directory")


def secure_directory(path):
    descriptor = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        check_directory(descriptor)
        for component in path.split("/")[1:]:
            if not component or component in (".", ".."):
                raise RuntimeError("non-normalized directory")
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
            check_directory(descriptor)
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


@contextmanager
def owned_lock(directory, name, private):
    parent = secure_directory(directory)
    fd = None
    try:
        if private:
            check_directory(parent, private=True)
        fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                     0o600 if private else 0o644, dir_fd=parent)
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_mode & (0o077 if private else 0o022):
            raise RuntimeError("unsafe lifecycle lock")
        yield fd
    finally:
        if fd is not None:
            os.close(fd)
        os.close(parent)


@contextmanager
def gate_lock(exclusive):
    with owned_lock("/run/repro2-sender-lifecycle", "gate.lock", private=True) as fd:
        fcntl.flock(fd, (fcntl.LOCK_EX | fcntl.LOCK_NB) if exclusive else fcntl.LOCK_SH)
        yield fd


def gate(arguments):
    with gate_lock(False) as fd:
        if not enabled():
            return 0
        # The sender must retain the shared lock even if Nix kills its wrapper.
        return subprocess.run([SENDER, *arguments], check=False, pass_fds=(fd,)).returncode


def step():
    # Avoid competing with rebuilds while enabled. This is only a fast-path;
    # the authoritative marker check remains under both teardown locks below.
    if enabled():
        return False
    # NixOS holds this lock across stop -> activation -> daemon reload/start.
    # Never release before that transaction has completed.
    with owned_lock("/run/nixos", "switch-to-configuration.lock", private=False) as switch:
        try:
            fcntl.flock(switch, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        try:
            with gate_lock(True):
                if enabled() or not system_running() or not worker_inactive():
                    return False
                release_roots()
        except BlockingIOError:
            return False
        print("WARNING repro2 GC roots released; queue preserved; GC may make unsent jobs unrecoverable", file=sys.stderr)
        return True


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "gate":
        try:
            sys.exit(gate(sys.argv[2:]))
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            print(f"CRITICAL repro2 hook gate refused job: {error}", file=sys.stderr)
            sys.exit(1)
    while True:
        try:
            if step():
                break
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            print(f"repro2 lifecycle: retaining roots: {error}", file=sys.stderr)
        time.sleep(2)

"""Execute the evaluated lifecycle program in per-test scratch chroots.

No host GC roots or queue are touched. systemd manager/worker state responses
are mocked; generation paths, locks, gate and root unlink are real.
This is not a booted NixOS switch test.
"""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

script = sys.argv.pop(1)
loader = importlib.machinery.SourceFileLoader("lifecycle", script)
spec = importlib.util.spec_from_loader(loader.name, loader)
assert spec is not None
lifecycle = importlib.util.module_from_spec(spec)
SOURCE = Path(script).read_text()
loader.exec_module(lifecycle)

class LifecycleTest(unittest.TestCase):
    def setUp(self):
        self.host = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
        self.tmp = tempfile.mkdtemp(prefix="lifecycle-")
        os.chroot(self.tmp)
        os.chdir("/")
        for path in ["/run/nixos", "/run/repro2-sender-lifecycle", "/nix/store",
                     "/nix/var/nix/gcroots/repro2", "/var/lib/repro2-sender/123-456",
                     "/generations/enabled/etc/systemd/system", "/generations/disabled/etc/systemd/system",
                     "/generations/removed/etc/systemd/system"]:
            Path(path).mkdir(parents=True, mode=0o700, exist_ok=True)
        Path("/generations/enabled/etc/repro2-sender-lifecycle").write_text("enabled\n")
        Path("/var/lib/repro2-sender/123-456/job.json").write_text('{"retained":true}\n')
        self.roots = Path(lifecycle.ROOTS)
        self.roots.mkdir(parents=True, mode=0o700, exist_ok=True)
        self.job = self.roots / "123-456"
        self.job.mkdir(mode=0o700)
        (self.job / "0").symlink_to("/nix/store/" + "a" * 32 + "-fixture.drv")
        Path("/nix/store/sentinel").write_text("store retained")
        self.activate("enabled")
        self.systemd = patch.object(lifecycle, "worker_inactive", return_value=True)
        self.systemd.start()
        self.running = patch.object(lifecycle, "system_running", return_value=True)
        self.running.start()

    def tearDown(self):
        self.systemd.stop()
        self.running.stop()
        os.fchdir(self.host)
        os.chroot(".")
        os.chdir("/")
        os.close(self.host)
        shutil.rmtree(self.tmp)

    def activate(self, generation):
        current = Path("/run/current-system")
        current.unlink(missing_ok=True)
        current.symlink_to("/generations/" + generation)

    def assert_retained(self):
        self.assertTrue((self.job / "0").is_symlink())
        self.assertEqual(Path("/var/lib/repro2-sender/123-456/job.json").read_text(), '{"retained":true}\n')
        self.assertEqual(Path("/nix/store/sentinel").read_text(), "store retained")

    def test_enabled_guardian_does_not_acquire_switch_lock(self):
        with patch.object(lifecycle, "owned_lock", side_effect=AssertionError("enabled guardian acquired switch lock")):
            self.assertFalse(lifecycle.step())
        self.assert_retained()

    def test_enabled_to_disabled_releases_only_roots(self):
        self.assertFalse(lifecycle.step())
        self.assert_retained()
        self.activate("disabled")
        self.assertTrue(lifecycle.step())
        self.assertFalse(self.roots.exists())
        self.assertTrue(Path("/var/lib/repro2-sender/123-456/job.json").exists())
        self.assertTrue(Path("/nix/store/sentinel").exists())

    def test_old_hook_cannot_enqueue_after_removal(self):
        with patch.object(lifecycle.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as sender:
            self.assertEqual(lifecycle.gate(["hook"]), 0)
            self.assertEqual(sender.call_count, 1)
            self.activate("removed")
            self.assertEqual(lifecycle.gate(["hook"]), 0)
            self.assertEqual(sender.call_count, 1)

    def test_inflight_hook_keeps_roots_until_gate_lock_released(self):
        self.activate("removed")
        with open("/run/repro2-sender-lifecycle/gate.lock", "a") as hook:
            os.fchmod(hook.fileno(), 0o600)
            import fcntl
            fcntl.flock(hook, fcntl.LOCK_SH | fcntl.LOCK_NB)
            self.assertFalse(lifecycle.step())
            self.assert_retained()
        self.assertTrue(lifecycle.step())

    def refuses(self):
        self.activate("removed")
        with self.assertRaises((OSError, RuntimeError)):
            lifecycle.step()
        self.assertTrue(Path("/var/lib/repro2-sender/123-456/job.json").exists())
        self.assertTrue(Path("/nix/store/sentinel").exists())

    def test_unexpected_regular_file_retains_entire_tree(self):
        (self.job / "1").write_text("unexpected")
        self.refuses()
        self.assert_retained()

    def test_unexpected_nested_directory_retains_entire_tree(self):
        (self.job / "nested").mkdir()
        self.refuses()
        self.assert_retained()

    def test_symlink_root_directory_refused(self):
        self.roots.rename(str(self.roots) + "-saved")
        self.roots.symlink_to(str(self.roots) + "-saved")
        self.refuses()
        self.assert_retained()

    def test_symlink_ancestor_refused(self):
        ancestor = Path("/nix/var/nix/gcroots")
        ancestor.rename("/nix/var/nix/saved")
        ancestor.symlink_to("/nix/var/nix/saved")
        self.refuses()
        self.assert_retained()

    def test_symlink_job_directory_refused(self):
        self.job.rename(str(self.job) + "-saved")
        self.job.symlink_to(str(self.job) + "-saved")
        self.refuses()
        self.assert_retained()

    def test_foreign_root_target_refused(self):
        (self.job / "1").symlink_to("/var/lib/repro2-sender/123-456/job.json")
        self.refuses()
        self.assert_retained()

    def test_traversal_path_refused(self):
        with patch.object(lifecycle, "ROOTS", "/nix/var/nix/gcroots/../gcroots/repro2"):
            self.refuses()
        self.assert_retained()

    def test_other_gc_roots_are_untouched(self):
        other = Path("/nix/var/nix/gcroots/other")
        other.mkdir()
        (other / "keep").symlink_to("/nix/store/sentinel")
        self.activate("removed")
        self.assertTrue(lifecycle.step())
        self.assertTrue((other / "keep").is_symlink())

    def test_symlink_gate_parent_refused(self):
        parent = Path("/run/repro2-sender-lifecycle")
        parent.rename("/run/saved-lifecycle")
        parent.symlink_to("/run/saved-lifecycle")
        self.refuses()
        self.assert_retained()

    def test_symlink_switch_lock_refused(self):
        Path("/run/nixos/switch-to-configuration.lock").symlink_to("/var/lib/repro2-sender/123-456/job.json")
        self.refuses()
        self.assert_retained()

    def test_writable_ancestor_refused(self):
        Path("/nix/var").chmod(0o777)
        self.refuses()
        self.assert_retained()

    def test_reserved_auto_subtree_refused(self):
        with patch.object(lifecycle, "ROOTS", "/nix/var/nix/gcroots/auto/repro2"):
            self.refuses()
        self.assert_retained()

    def test_shutdown_after_disable_retains_roots(self):
        self.activate("removed")
        with patch.object(lifecycle, "system_running", return_value=False, create=True):
            self.assertFalse(lifecycle.step())
        self.assert_retained()

    def test_worker_still_active_retains_roots(self):
        self.activate("removed")
        with patch.object(lifecycle, "worker_inactive", return_value=False):
            self.assertFalse(lifecycle.step())
        self.assert_retained()

    def test_switch_transaction_retains_roots(self):
        self.activate("removed")
        with open("/run/nixos/switch-to-configuration.lock", "a") as switch:
            import fcntl
            fcntl.flock(switch, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertFalse(lifecycle.step())
            self.assert_retained()
        self.assertTrue(lifecycle.step())

    def test_enabled_upgrade_retains_roots(self):
        Path("/generations/upgrade/etc/systemd/system").mkdir(parents=True)
        Path("/generations/upgrade/etc/repro2-sender-lifecycle").write_text("enabled\n")
        self.activate("upgrade")
        self.assertFalse(lifecycle.step())
        self.assert_retained()

    def test_import_removal_releases_roots_idempotently(self):
        self.activate("removed")
        self.assertTrue(lifecycle.step())
        self.assertTrue(lifecycle.step())
        self.assertFalse(self.roots.exists())
        self.assertTrue(Path("/var/lib/repro2-sender/123-456/job.json").exists())

    def test_orphaned_sender_holds_gate_lock_until_exit(self):
        # Real subprocess exec, not an unconditional cleanup mock. The child
        # only restores the enclosing scratch root to load its interpreter;
        # it never writes there. Its inherited lock remains in this fixture.
        import select
        import signal
        import time
        ready_read, ready_write = os.pipe()
        original_run = subprocess.run
        host_root = self.host
        def restore_scratch_root():
            os.fchdir(host_root)
            os.chroot(".")
            os.chdir("/")
        def run_sender(command, **kwargs):
            inherited = kwargs.pop("pass_fds", ())
            kwargs["pass_fds"] = (*inherited, host_root, ready_write)
            kwargs["preexec_fn"] = restore_scratch_root
            return original_run(command, **kwargs)
        wrapper = os.fork()
        if wrapper == 0:
            try:
                with patch.object(lifecycle, "SENDER", sys.executable), patch.object(lifecycle.subprocess, "run", side_effect=run_sender):
                    lifecycle.gate(["-c", f"import os,time; os.write({ready_write},str(os.getpid()).encode()); time.sleep(20)"])
                os._exit(0)
            except BaseException:
                os._exit(1)
        sender_pid = None
        try:
            self.assertTrue(select.select([ready_read], [], [], 5)[0], "sender did not exec")
            sender_pid = int(os.read(ready_read, 64))
            os.kill(wrapper, signal.SIGKILL)
            os.waitpid(wrapper, 0)
            wrapper = None
            self.activate("removed")
            self.assertFalse(lifecycle.step(), "orphan sender lost its shared lock")
            self.assert_retained()
        finally:
            if wrapper is not None:
                os.kill(wrapper, signal.SIGKILL)
                os.waitpid(wrapper, 0)
            if sender_pid is not None:
                os.kill(sender_pid, signal.SIGKILL)
            os.close(ready_read)
            os.close(ready_write)
        deadline = time.monotonic() + 5
        while not lifecycle.step():
            self.assertLess(time.monotonic(), deadline, "orphan lock was not released")
            time.sleep(0.02)

    def test_guardian_sigterm_preserves_roots(self):
        import signal
        import select
        import time
        ready_read, ready_write = os.pipe()
        child = os.fork()
        if child == 0:
            os.close(ready_read)
            original_sleep = time.sleep
            def ready_sleep(seconds):
                os.write(ready_write, b"ready")
                original_sleep(seconds)
            # Observe a completed guardian iteration, not a lock-file side effect.
            time.sleep = ready_sleep
            sys.argv = ["repro2-lifecycle.py"]
            exec(compile(SOURCE, "evaluated-lifecycle.py", "exec"), {"__name__": "__main__"})
            os._exit(0)
        os.close(ready_write)
        try:
            self.assertTrue(select.select([ready_read], [], [], 5)[0], "guardian did not reach polling loop")
            self.assertEqual(os.read(ready_read, 5), b"ready")
            os.kill(child, signal.SIGTERM)
            _, status = os.waitpid(child, 0)
            child = None
            self.assertEqual(os.waitstatus_to_exitcode(status), -signal.SIGTERM)
            self.assert_retained()
        finally:
            os.close(ready_read)
            if child is not None:
                os.kill(child, signal.SIGKILL)
                os.waitpid(child, 0)

    def test_sender_stop_or_restart_while_enabled_preserves_roots(self):
        for inactive in (True, False, True):
            with patch.object(lifecycle, "worker_inactive", return_value=inactive):
                self.assertFalse(lifecycle.step())
                self.assert_retained()

if __name__ == "__main__":
    unittest.main(verbosity=2)

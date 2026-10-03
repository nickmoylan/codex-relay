"""Black-box installer checks using disposable files, never global configuration."""
import hashlib
import importlib.util
import os
from unittest import mock
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[1] / "scripts/install.py"


class Installation(unittest.TestCase):
    def test_upgrade_rollback_preservation_and_modified_install_refusal(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            prefix = root / "install with spaces"
            source = root / "source"
            source.write_bytes(b"first synthetic executable\n")
            source.chmod(0o755)

            def call(*args, success=True):
                result = subprocess.run(
                    [shutil.which("rtk"), "proxy", "python3", str(INSTALLER),
                     "--prefix", str(prefix), "--binary", str(source), *args],
                    capture_output=True, text=True, check=False,
                )
                self.assertEqual(result.returncode, 0 if success else 1, result.stderr)
                return json.loads(result.stdout) if success else result.stderr

            call()
            binary = prefix / "bin/codex-relay"
            unrelated = prefix / "keep-me"
            unrelated.write_text("owner file")
            first = binary.read_bytes()
            source.write_bytes(b"second synthetic executable\n")
            call("--replace")
            second = binary.read_bytes()
            self.assertNotEqual(first, second)
            call("--rollback")
            self.assertEqual(binary.read_bytes(), first)
            receipt = json.loads((prefix / "share/codex-relay/install.json").read_text())
            self.assertEqual(receipt["sha256"], hashlib.sha256(first).hexdigest())
            binary.write_bytes(b"changed by owner")
            self.assertIn("differs from its receipt", call("--uninstall", success=False))
            self.assertEqual(binary.read_bytes(), b"changed by owner")
            binary.write_bytes(first)
            call("--uninstall")
            self.assertFalse(binary.exists())
            self.assertEqual(unrelated.read_text(), "owner file")

    def test_unowned_and_symlinked_paths_are_preserved(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            source = root / "source"
            source.write_text("synthetic")
            source.chmod(0o755)
            prefix = root / "prefix"
            (prefix / "bin").mkdir(parents=True)
            destination = prefix / "bin/codex-relay"
            destination.write_text("unowned")
            args = [shutil.which("rtk"), "proxy", "python3", str(INSTALLER),
                    "--prefix", str(prefix), "--binary", str(source)]
            self.assertEqual(subprocess.run(args, capture_output=True).returncode, 1)
            self.assertEqual(destination.read_text(), "unowned")
            destination.unlink()
            destination.symlink_to(source)
            self.assertEqual(subprocess.run(args, capture_output=True).returncode, 1)
            self.assertTrue(destination.is_symlink())
            self.assertEqual(source.read_text(), "synthetic")


    def test_interrupted_operations_recover_after_every_mutation(self):
        for operation in ["install", "replace", "rollback", "uninstall"]:
            for point in ["journal", "previous", "binary", "receipt", "commit"]:
                with self.subTest(operation=operation, point=point), tempfile.TemporaryDirectory() as temp:
                    root = Path(temp).resolve()
                    prefix, source = root / "prefix", root / "source"
                    source.write_bytes(b"first")
                    source.chmod(0o755)

                    def call(*args, fault=None, success=True):
                        env = dict(os.environ)
                        if fault:
                            env["CODEX_RELAY_INSTALL_TEST_FAIL_AFTER"] = fault
                        result = subprocess.run(
                            [shutil.which("rtk"), "proxy", "python3", str(INSTALLER),
                             "--prefix", str(prefix), "--binary", str(source), *args],
                            capture_output=True, text=True, env=env, check=False)
                        self.assertEqual(result.returncode, 0 if success else 1, result.stderr)
                        return result

                    if operation != "install":
                        call()
                    source.write_bytes(b"second")
                    if operation in ["rollback", "uninstall"]:
                        call("--replace")
                    unrelated = prefix / "keep-me"
                    if not prefix.exists():
                        prefix.mkdir()
                    unrelated.write_bytes(b"owner data")
                    args = [] if operation == "install" else ["--" + operation]
                    call(*args, fault=point, success=False)
                    call("--recover")
                    binary = prefix / "bin/codex-relay"
                    receipt = prefix / "share/codex-relay/install.json"
                    backup = prefix / "bin/codex-relay.previous"
                    if operation == "uninstall":
                        self.assertFalse(binary.exists() or receipt.exists() or backup.exists())
                    else:
                        expected = b"first" if operation == "rollback" else b"second"
                        self.assertEqual(binary.read_bytes(), expected)
                        record = json.loads(receipt.read_text())
                        self.assertEqual(record["sha256"], hashlib.sha256(expected).hexdigest())
                        self.assertEqual(record["previous_sha256"],
                            hashlib.sha256(backup.read_bytes()).hexdigest() if backup.exists() else None)
                        call("--uninstall")
                    self.assertEqual(unrelated.read_bytes(), b"owner data")
                    self.assertFalse((prefix / "share/codex-relay/transaction.json").exists())

    def test_interrupted_recovery_is_idempotent(self):
        for point in ["previous", "binary", "receipt", "commit"]:
            with self.subTest(point=point), tempfile.TemporaryDirectory() as temp:
                root = Path(temp).resolve()
                prefix, source = root / "prefix", root / "source"
                source.write_bytes(b"synthetic")
                source.chmod(0o755)
                args = [shutil.which("rtk"), "proxy", "python3", str(INSTALLER),
                        "--prefix", str(prefix), "--binary", str(source)]
                for mode, fault in [([], "journal"), (["--recover"], point)]:
                    env = dict(os.environ, CODEX_RELAY_INSTALL_TEST_FAIL_AFTER=fault)
                    self.assertEqual(subprocess.run(args + mode, env=env, capture_output=True).returncode, 1)
                self.assertEqual(subprocess.run(args + ["--recover"], capture_output=True).returncode, 0)
                self.assertEqual((prefix / "bin/codex-relay").read_bytes(), b"synthetic")
                self.assertEqual(subprocess.run(args + ["--uninstall"], capture_output=True).returncode, 0)

    def test_recovery_preserves_intervening_owner_edits(self):
        for name in ["bin/codex-relay", "bin/codex-relay.previous", "share/codex-relay/install.json"]:
            with self.subTest(path=name), tempfile.TemporaryDirectory() as temp:
                root = Path(temp).resolve()
                prefix, source = root / "prefix", root / "source"
                source.write_bytes(b"first")
                source.chmod(0o755)
                args = [shutil.which("rtk"), "proxy", "python3", str(INSTALLER),
                        "--prefix", str(prefix), "--binary", str(source)]
                self.assertEqual(subprocess.run(args, capture_output=True).returncode, 0)
                source.write_bytes(b"second")
                env = dict(os.environ, CODEX_RELAY_INSTALL_TEST_FAIL_AFTER="binary")
                self.assertEqual(subprocess.run(args + ["--replace"], env=env, capture_output=True).returncode, 1)
                changed = prefix / name
                changed.write_bytes(b"owner edit")
                before = {p: p.read_bytes() for p in [prefix / "bin/codex-relay",
                         prefix / "bin/codex-relay.previous", prefix / "share/codex-relay/install.json"]}
                result = subprocess.run(args + ["--recover"], capture_output=True, text=True)
                self.assertEqual(result.returncode, 1)
                self.assertIn("owner-modified", result.stderr)
                self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_receipt_write_failure_can_recover_then_rollback(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            prefix, source = root / "prefix", root / "source"
            source.write_bytes(b"first")
            source.chmod(0o755)
            args = [shutil.which("rtk"), "proxy", "python3", str(INSTALLER),
                    "--prefix", str(prefix), "--binary", str(source)]
            self.assertEqual(subprocess.run(args, capture_output=True).returncode, 0)
            source.write_bytes(b"second")
            spec = importlib.util.spec_from_file_location("relay_installer", INSTALLER)
            installer = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(installer)
            original = installer.atomic

            def fail_receipt(path, data, mode):
                if path.name == "install.json":
                    raise OSError("synthetic receipt write failure")
                return original(path, data, mode)

            with mock.patch.object(installer, "atomic", side_effect=fail_receipt), mock.patch(
                    "sys.argv", [str(INSTALLER), "--prefix", str(prefix), "--binary", str(source), "--replace"]):
                with self.assertRaisesRegex(OSError, "receipt write failure"):
                    installer.main()
            self.assertEqual(subprocess.run(args + ["--rollback"], capture_output=True).returncode, 0)
            self.assertEqual((prefix / "bin/codex-relay").read_bytes(), b"first")
            self.assertEqual(subprocess.run(args + ["--uninstall"], capture_output=True).returncode, 0)


if __name__ == "__main__":
    unittest.main()

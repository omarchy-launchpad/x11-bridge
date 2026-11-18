"""Tests use synthetic fixture settings, never real connection details."""
import importlib.util
import io
from pathlib import Path
import shlex
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import subprocess

spec = importlib.util.spec_from_file_location("runner", Path(__file__).resolve().parents[1] / "scripts" / "test-remote.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class RunnerTests(unittest.TestCase):
    def test_configuration_requires_explicit_private_settings(self):
        with self.assertRaises(ValueError):
            runner.configuration({})
        with self.assertRaises(ValueError):
            runner.configuration({"X11_BRIDGE_SSH_TARGET": "-oProxyCommand=bad", "X11_BRIDGE_DISPLAY": ":fixture"})

    def test_remote_shell_quotes_values_and_requires_known_host(self):
        value = "fixture ' $(touch /should-not-exist); display"
        command = runner.configuration({"X11_BRIDGE_SSH_TARGET": "fixture-host", "X11_BRIDGE_DISPLAY": value})
        self.assertIn("StrictHostKeyChecking=yes", command)
        script = shlex.split(command[-1])[2]
        export = next(line for line in script.splitlines() if line.startswith("export X11_BRIDGE_DISPLAY="))
        self.assertEqual(shlex.split(export)[1], "X11_BRIDGE_DISPLAY=" + value)
        self.assertIn("--skip targeted_input_and_held_key_cleanup", script)

    def test_archive_excludes_private_files_and_metadata(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            for name in ("Cargo.toml", "Cargo.lock", "crates/agent/Cargo.toml", "crates/protocol/Cargo.toml", "crates/agent/src/main.rs", "crates/agent/examples/fixture.rs", ".env", "private/secret", "crates/agent/src/config.local"):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fixture")
            with tarfile.open(fileobj=io.BytesIO(runner.source_archive(root))) as archive:
                self.assertEqual(set(archive.getnames()), {"Cargo.toml", "Cargo.lock", "crates/agent/Cargo.toml", "crates/protocol/Cargo.toml", "crates/agent/src/main.rs", "crates/agent/examples/fixture.rs"})
                for entry in archive:
                    self.assertEqual((entry.uid, entry.gid, entry.uname, entry.gname), (0, 0, "", ""))
            (root / "crates/agent/src/main.rs").unlink()
            (root / "crates/agent/src/main.rs").symlink_to(root / ".env")
            with self.assertRaises(ValueError):
                runner.source_archive(root)

    def test_stream_failure_still_removes_remote_workspace(self):
        environ = {"X11_BRIDGE_SSH_TARGET": "fixture-host", "X11_BRIDGE_DISPLAY": ":fixture", "X11_BRIDGE_REMOTE_TMP": "/tmp/fixture space"}
        ok = subprocess.CompletedProcess([], 0, stdout=b"", stderr=b"")
        metadata = subprocess.CompletedProcess([], 0, stdout=b'{"target_directory":"/tmp/local-fixture-target"}', stderr=b"")
        failure = subprocess.CalledProcessError(1, ["fixture-consumer"])
        with patch.object(runner.secrets, "token_hex", return_value="fixture-token"), patch.object(runner.subprocess, "run", side_effect=[ok, ok, metadata, failure, ok]) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                runner.ssh_stream(environ, b"fixture-archive", Path("."))
            command = run.call_args_list[-1].args[0]
            self.assertEqual(shlex.split(command[-1]), ["rm", "-rf", "--", "/tmp/fixture space/x11-bridge-stream.fixture-token"])
            setup = shlex.split(run.call_args_list[1].args[0][-1])[2]
            self.assertIn('export CARGO_TARGET_DIR="$work/target"', setup)


if __name__ == "__main__":
    unittest.main()

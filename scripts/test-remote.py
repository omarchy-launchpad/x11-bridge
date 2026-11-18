#!/usr/bin/env python3
"""Copy source-only fixtures over SSH and test an authenticated X11 session.

Connection details come exclusively from environment variables. No remote
output is printed: SSH, Cargo and X11 errors can contain private machine data.
"""
import io
import os
from pathlib import Path
import re
import secrets
import shlex
import subprocess
import sys
import tarfile


def settings(environ):
    required = ("X11_BRIDGE_SSH_TARGET", "X11_BRIDGE_DISPLAY")
    for name in required:
        if not environ.get(name):
            raise ValueError(f"Set {name} outside the repository")
    target = environ[required[0]]
    if target.startswith("-") or any(c.isspace() for c in target):
        raise ValueError("Invalid X11_BRIDGE_SSH_TARGET")
    ssh = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=10"]
    if environ.get("X11_BRIDGE_SSH_PORT"):
        port = int(environ["X11_BRIDGE_SSH_PORT"])
        if not 1 <= port <= 65535:
            raise ValueError("Invalid X11_BRIDGE_SSH_PORT")
        ssh += ["-p", str(port)]
    if environ.get("X11_BRIDGE_SSH_IDENTITY"):
        ssh += ["-i", environ["X11_BRIDGE_SSH_IDENTITY"]]
    remote_env = {"X11_BRIDGE_DISPLAY": environ["X11_BRIDGE_DISPLAY"]}
    if environ.get("X11_BRIDGE_XAUTHORITY"):
        remote_env["XAUTHORITY"] = environ["X11_BRIDGE_XAUTHORITY"]
    if environ.get("X11_BRIDGE_REMOTE_TMP"):
        remote_env["TMPDIR"] = environ["X11_BRIDGE_REMOTE_TMP"]
    test_input = environ.get("X11_BRIDGE_TEST_INPUT") == "1"
    if test_input:
        remote_env["X11_BRIDGE_TEST_INPUT"] = "1"
    exports = "\n".join(f"export {key}={shlex.quote(value)}" for key, value in remote_env.items())
    return ssh + [target], exports, test_input


def configuration(environ):
    ssh, exports, test_input = settings(environ)
    test_options = "" if test_input else " --skip targeted_input_and_held_key_cleanup"
    script = "\n".join([
        "set -eu", "umask 077", exports,
        'export PATH="$HOME/.cargo/bin:$PATH"',
        'work=$(mktemp -d "${TMPDIR:-/tmp}/x11-bridge-test.XXXXXXXX")',
        "trap 'rm -rf -- \"$work\"' EXIT HUP INT TERM",
        'cd "$work"', 'export CARGO_TARGET_DIR="$work/target"', "tar -xf -",
        "timeout 300 cargo test --workspace --locked",
        "timeout 120 cargo test -p x11-bridge-agent --test x11_live --locked -- --ignored --test-threads=1 --nocapture" + test_options,
    ])
    return ssh + ["bash -c " + shlex.quote(script)]


def source_archive(root):
    # Explicit allowlist excludes .git, env files, credentials, build output,
    # local settings and every file outside this repository.
    paths = [root / "Cargo.toml", root / "Cargo.lock"]
    for crate in ("agent", "protocol"):
        base = root / "crates" / crate
        paths.append(base / "Cargo.toml")
        for folder in ("src", "tests", "examples"):
            paths.extend(sorted((base / folder).rglob("*.rs")))
    archive = io.BytesIO()
    with tarfile.open(fileobj=archive, mode="w") as tar:
        for path in paths:
            if path.is_symlink() or not path.resolve().is_relative_to(root.resolve()):
                raise ValueError("Source archive cannot contain external files or symlinks")
            data = path.read_bytes()
            info = tarfile.TarInfo(path.relative_to(root).as_posix())
            info.size = len(data)
            info.mode = 0o600
            tar.addfile(info, io.BytesIO(data))
    return archive.getvalue()


def ssh_stream(environ, archive, root):
    """Build a disposable remote fixture; read its frames with the local Rust API."""
    ssh, exports, _ = settings(environ)
    token = secrets.token_hex(16)
    base = environ.get("X11_BRIDGE_REMOTE_TMP") or "/tmp"
    work = base.rstrip("/") + "/x11-bridge-stream." + token
    # Resolve once in the remote shell; reject relative paths because SSH runs
    # independent commands that must agree on the workspace location.
    if not base.startswith("/"):
        raise ValueError("Invalid X11_BRIDGE_REMOTE_TMP: absolute path required")
    assignment = "work=" + shlex.quote(work)
    setup = "\n".join([
        "set -eu", "umask 077", assignment,
        'mkdir -- "$work"',
        "trap 'rm -rf -- \"$work\"' EXIT HUP INT TERM",
        'cd "$work"', 'export CARGO_TARGET_DIR="$work/target"', "tar -xf -",
        'export PATH="$HOME/.cargo/bin:$PATH"',
        "timeout 300 cargo build --workspace --bins --examples --locked",
        "trap - EXIT HUP INT TERM",
    ])
    subprocess.run(["cargo", "build", "-p", "x11-bridge-protocol", "--example", "stream_smoke", "--locked"],
                   cwd=root, capture_output=True, timeout=120, check=True)
    created = False
    try:
        subprocess.run(ssh + ["bash -c " + shlex.quote(setup)], input=archive,
                       capture_output=True, timeout=360, check=True)
        created = True
        remote = "\n".join(["set -eu", exports, "cd " + shlex.quote(work),
                              "exec timeout 40 target/debug/examples/streaming_fixture"])
        # Honor the local CARGO_TARGET_DIR setting. Metadata supplies the actual
        # executable path without recording that private path in diagnostics.
        metadata = subprocess.run(["cargo", "metadata", "--format-version=1", "--no-deps"],
                                  cwd=root, capture_output=True, timeout=30, check=True)
        import json
        binary = Path(json.loads(metadata.stdout)["target_directory"]) / "debug/examples/stream_smoke"
        result = subprocess.run([str(binary)] + ssh + ["bash -c " + shlex.quote(remote)],
                                env=environ, capture_output=True, timeout=60, check=True)
        for line in result.stdout.decode("utf-8", errors="replace").splitlines():
            if re.fullmatch(r"bridge-ssh frames=2 pixel_bytes=49152 first_frame_ms=[0-9]+ input=[01]", line):
                print(line)
                return
        raise ValueError("Streaming fixture returned no verified result")
    finally:
        if created:
            cleanup = "rm -rf -- " + shlex.quote(work)
            subprocess.run(ssh + [cleanup], capture_output=True, timeout=20, check=True)


def main():
    try:
        command = configuration(os.environ)
        archive = source_archive(Path(__file__).resolve().parent.parent)
        result = subprocess.run(command, input=archive, capture_output=True, timeout=480, check=False)
    except ValueError as error:
        # Only fixed validation messages, never configuration values.
        print(str(error) if str(error).startswith(("Set X11_", "Invalid X11_", "Source archive")) else "Invalid environment configuration", file=sys.stderr)
        return 2
    except (OSError, subprocess.TimeoutExpired):
        print("Remote test could not complete; check SSH access and remote prerequisites privately.", file=sys.stderr)
        return 1
    if result.returncode:
        print("Remote tests failed. Check authenticated display access, Rust, timeout and X11 extensions privately. Remote diagnostics suppressed to protect machine details.", file=sys.stderr)
        return 1
    for line in result.stdout.decode("utf-8", errors="replace").splitlines():
        match = re.search(r"bridge-metric first_frame_bytes=([0-9]+) first_frame_ms=([0-9]+)$", line)
        if match:
            print(f"Fixture first frame: {match[1]} bytes, {match[2]} ms")
    print("Remote X11 fixture tests passed.", flush=True)
    try:
        ssh_stream(os.environ, archive, Path(__file__).resolve().parent.parent)
    except (OSError, ValueError, subprocess.SubprocessError):
        print("SSH streaming test or cleanup failed; private diagnostics suppressed.", file=sys.stderr)
        return 1
    print("Local Rust consumer received verified fixture frames through SSH.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

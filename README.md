# x11-bridge

Rust protocol library and Linux agent for streaming a selected X11 window over
stdin/stdout. No xpra dependency or wire compatibility. Native rendering and SSH
profile management belong to the consumer.

`x11_bridge_protocol::client` provides catalogue reading, capability negotiation,
validated frame/lifecycle events and separate read/control handles. See
[Seams integration](docs/seams-integration.md) for ownership, threading and input
requirements. The `stream_smoke` example exercises this API end to end.

The agent captures Composite pixmaps, converts TrueColor pixels to opaque RGBA8,
tracks Damage and window lifecycle events, and optionally accepts XTEST input.
It sends complete frames at roughly 30 Hz and keeps one pending update, replacing
stale frames when the consumer is slow. This is a raw-pixel correctness milestone,
not a compressed remote-desktop implementation.

## Build and run

Rust 1.88 or newer is required. Linux needs Composite 0.2+, Damage 1.0+, and XTEST
when input is enabled. The protocol library and agent also compile on macOS.

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

GitHub Actions runs these checks on Linux and macOS, the Python runner tests,
and the Docker X11/SSH end-to-end suite on pushes to `main` and pull requests.
To publish a Linux x86-64 agent release, set the workspace version in
`Cargo.toml`, update `Cargo.lock`, and push a matching tag such as `v0.1.0`.
After all checks pass, Actions creates a GitHub release with the binary archive
and its SHA-256 checksum. The workflow requires GitHub Actions to have permission
to write repository contents.

Supply the authenticated session display explicitly. `x11rb` reads Xauthority
credentials using the standard `XAUTHORITY` environment variable. Never disable
X authentication. The following variables must be set in your private shell:

```sh
# Catalogue output is binary. Window packet identities are XIDs in this mode.
cargo run -p x11-bridge-agent -- --display "$X11_BRIDGE_DISPLAY" > catalogue.bin

# A consumer must exchange protocol messages on stdin/stdout in streaming mode.
# X11_BRIDGE_WINDOW is a decimal or 0x-prefixed XID selected by the consumer.
target/debug/x11-bridge-agent --display "$X11_BRIDGE_DISPLAY" \
  --window "$X11_BRIDGE_WINDOW"
```

The catalogue requires EWMH `_NET_CLIENT_LIST`. Direct `--window` selection does
not require a window manager. Add `--allow-input` to advertise input support;
capture is read-only by default. Diagnostics go to stderr, protocol data to stdout.

On a Wayland desktop, this agent can access X11/Xwayland windows only. Native
Wayland windows are outside its scope. Real Omarchy acceptance remains separate
from Xvfb fixture results.

## Transport contract

See [protocol.md](docs/protocol.md) for framing and session rules. In streaming
mode, the agent sends Hello with a fresh random generation and window identity 0.
The consumer replies with Hello using that generation and a supported capability
subset that includes capture. The selected window then has identity 1 for that
transport, regardless of its XID. Generations and subscriptions cannot be reused.

Each update contains Window metadata followed by a full Frame. Resize and remap
obtain fresh Composite pixmap references. Unmap releases held input and emits
Pause; a later Window/Frame pair resumes the visible image. Destroy terminates
the subscription. EOF ends the transport. Applications survive disconnect.

Consumer Pause stops capture and releases input. Resume requests a fresh frame.
Malformed messages, stale identities, unsupported directions and rejected input
terminate the transport and release held input. Text is limited to 4096 bytes;
packets are limited to 32 MiB, consumer payloads to 64 bytes.

XTEST is global input. The agent never steals focus. It requires the selected
window itself to hold keyboard focus, serializes focus checks and injection with
an X server grab, and checks that buttons land on the selected window. It accepts
level-zero keysyms with explicit modifiers and core buttons 1 through 5. Existing
physical key/button presses are rejected rather than adopted. Held input is
released on pause, unmap, focus loss, protocol failure and transport EOF.
Physical input, grabs by other applications, keyboard layouts and compositor
policies still affect delivery. Descendant keyboard focus is currently rejected.
Abrupt process termination or X server failure cannot guarantee input cleanup.

## Integration tests

Normal `cargo test` leaves live tests ignored. Fixtures create their own windows;
they do not enumerate or capture personal applications. Tests cover pixels,
Damage updates, resize, pause/resume, remap, destruction, stale identities,
disconnect, pointer/button delivery, held-input cleanup and stalled output. Run serially because input focus is shared.

For an authenticated local Linux session:

```sh
cargo test -p x11-bridge-agent --test x11_live --locked -- \
  --ignored --test-threads=1 --skip targeted_input_and_held_key_cleanup
```

Set `X11_BRIDGE_DISPLAY` first and `XAUTHORITY` if the session needs it. To include
input tests, set `X11_BRIDGE_TEST_INPUT=1` and omit `--skip`. Input tests temporarily
focus a fixture window and restore the prior focus afterward.

To see one captured window without running the test suite:

```sh
sh scripts/test-docker-e2e.sh --preview
```

This draws a 640x400 demo window in Xvfb, captures it through the agent and Rust
client, writes `target/preview.png`, and opens it in your image viewer when one is
available. It is a static capture, not an interactive desktop. An optional second
argument chooses the output PNG path. No host desktop or remote machine is used.
The PNG encoder is an example-only dependency; the protocol library still has no
runtime dependencies.

For a complete Docker end-to-end run, including real SSH transport:

```sh
sh scripts/test-docker-e2e.sh
```

The container runs authenticated Xvfb, an SSH server and the Rust consumer. It
verifies capture, resize/lifecycle handling, pause/resume, key delivery, changed
pixels and graceful disconnect. SSH runs over container loopback with generated
client/host keys and pinned host trust. Negative checks reject an unknown SSH key
and an X11 client without its authentication cookie. The input fixture runs as a
separate, unprivileged container user.

Runtime uses `--network none`; no ports, host directories, SSH agent or host
desktop are exposed. Build downloads dependencies first, then runtime Cargo runs
offline. All credentials and X11 resources are disposable. This tests the bridge
and its Rust consumer, not Seams' AppKit rendering or compositor-specific behavior.
Docker Engine/Desktop is the only host prerequisite. No Omarchy settings required.

`test-docker-e2e.sh` is the Docker entry point. `run-container-tests.sh` is its
internal container runner and always executes the full suite. Use
`test-remote.py` for a real authenticated desktop.

Local verification passed 17 Rust unit/client tests, 4 runner privacy/cleanup tests, formatting,
Clippy with warnings denied, and all 4 authenticated Xvfb integration suites.
The lifecycle fix also passed 10 consecutive additional live runs. Xvfb reported
Composite 0.4 and Damage 1.1. The final 96x64 fixture transferred 24,576 pixel
bytes and reached its first frame in 19 ms locally. These fixture measurements
do not establish real-desktop or SSH performance.

Authenticated Omarchy verification passed all 17 Rust unit/client tests and all
4 capture/input/lifecycle integration suites. The remote fixture produced its
first 24,576-byte frame in 8 ms. A local Rust consumer then received two verified
frames through SSH, totaling 49,152 pixel bytes, with the first frame at 233 ms
including SSH startup. The consumer sent a key and release; the fixture received
both, changed color, and the consumer verified those changed pixels. This proves
the fixture path, not arbitrary application compatibility or steady-state latency.
Input tests require the fixture to retain focus; desktop activity can interrupt
them. Connection settings and remote diagnostics are not recorded here.

## Private Omarchy configuration

Keep connection settings in your shell or a file outside the repository. The
runner does not discover hosts or read personal project settings. `.env`,
`.env.*`, `*.local` and `private/` are ignored as an additional guard; the runner
does not load them automatically. `.env.example` contains variable names only.

| Variable | Purpose |
| --- | --- |
| `X11_BRIDGE_SSH_TARGET` | Required. Private SSH alias or `user@host`. |
| `X11_BRIDGE_DISPLAY` | Required. Display belonging to the remote session. |
| `X11_BRIDGE_XAUTHORITY` | Optional. Remote Xauthority path; remote default otherwise. |
| `X11_BRIDGE_SSH_PORT` | Optional. SSH port; SSH configuration default otherwise. |
| `X11_BRIDGE_SSH_IDENTITY` | Optional. Local SSH identity path; SSH configuration default otherwise. |
| `X11_BRIDGE_REMOTE_TMP` | Optional. Existing remote temporary directory; `/tmp` otherwise. |
| `X11_BRIDGE_TEST_INPUT` | Set to `1` to include the focus/input fixture test. |

```sh
python3 scripts/test-remote.py
```

Remote prerequisites are Rust, Cargo, tar, bash, GNU timeout and access to the
authenticated desktop X server. SSH uses batch mode and requires an existing
known-host entry. It uses normal SSH configuration and never copies private keys
or Xauthority files. Only Cargo manifests, the lockfile and Rust source/test/example files
are uploaded to a temporary directory with mode 0700. Build output and fixtures
are removed on completion. The first build may download locked dependencies.

After remote tests, the runner builds a disposable fixture and reads its frames
with the Rust client on the local machine over SSH. Cargo must be installed
locally too. It checks pixels, metadata, transport negotiation and shutdown. With
input enabled, the fixture must receive the key and release before turning green.
The local fixture's first-frame metric includes SSH startup; it is not steady-state
frame latency. Temporary remote workspaces are removed after testing.

The runner prints pass/fail and synthetic fixture byte/latency measurements.
It suppresses remote stdout/stderr because compiler and SSH diagnostics can expose
paths or hostnames. Connection values still exist in process arguments and the
remote process environment; keep local process access and shell history private.
No connection details or remote logs are written into the repository.

Test runner privacy checks independently:

```sh
python3 -m unittest discover -s tests -p 'test_*.py'
```

Clipboard, audio, codecs, persistent reconnect, application launching and native
macOS rendering are not implemented.

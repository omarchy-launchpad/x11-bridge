# x11-bridge

Rust library and agent for a custom remote-window protocol. No xpra dependency
or xpra wire compatibility. Early implementation, not yet a window-streaming engine.

- `x11-bridge-protocol`: dependency-free framing, typed messages, RGBA validation,
  subscription identity checks and aspect-fit pointer mapping.
- `x11-bridge-agent`: connects to an explicitly selected, authenticated X11 display
  and emits a handshake and EWMH client-window metadata over stdout, then exits.

## Run

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p x11-bridge-agent -- --display :0 > catalogue.bin
```

Use the authenticated display belonging to your session. Xauthority credentials
are handled by x11rb. Never disable X authentication. Output is binary, not a
terminal listing; diagnostics go to stderr. A window manager providing
`_NET_CLIENT_LIST` is currently required. The agent does not launch applications,
capture pixels, inject input or monitor changes yet. Consumers must treat EOF
as the end of the catalogue and discard its subscriptions.

`x11rb` supplies maintained X11 connection and protocol handling without a
system libX11/libxcb dependency. Extension features remain disabled until needed.
The shared protocol uses only the standard library. Workspace code forbids unsafe
Rust. Cargo.lock records transitive dependencies.

Six protocol tests pass locally on macOS, along with formatting and Clippy.
Actual Linux/X11 enumeration remains unverified. Next milestones: Xvfb integration
coverage, Composite capture, input targeting/release and SSH consumer integration.
Seams will own macOS rendering outside this repository.

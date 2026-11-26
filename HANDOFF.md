# x11-bridge handoff

## Decision

Create a separate repository named `x11-bridge`. Use Rust and Jujutsu version
control. This repository will own the reusable remote-window protocol and Linux
agent. Omarchy Launchpad will consume the protocol library and own native
macOS rendering.

Implement a custom protocol with the window-streaming role currently associated
with xpra. Do not depend on xpra or claim compatibility with its wire format.

## Package boundaries

Use a Cargo workspace with two packages initially:

- `x11-bridge-protocol`: platform-independent library containing typed messages,
  framing, validation and subscription identity rules.
- `x11-bridge-agent`: Linux executable that captures selected X11 client windows
  and handles input over stdin/stdout. Protocol data uses stdout; logs use stderr.

Omarchy Launchpad depends on the protocol library, starts the agent over system
SSH and renders received frames using AppKit.
Keep rendering, SSH profiles and macOS settings out of the new repository.

## Existing implementation to extract

In omarchy-launchpad, `src/protocol.rs` implements bounded, versioned packet framing,
RGBA8 frame validation and aspect-fit pointer mapping. `src/messages.rs` defines
handshake, metadata, frames, input, pause/resume, errors and subscription state.
`src/lib.rs` exports these modules. Their tests pass locally.

These are groundwork, not a demonstrated remote-window engine. No Linux agent,
Mac renderer, transport integration or real capture/input demo exists. Review
and complete the protocol before treating it as stable. In particular, define
capability negotiation, direction/state validation, title/error limits, window
identity allocation, reconnect generations and input focus/release semantics.

Omarchy Launchpad removed the xpra execution paths and executable chooser.
Launch currently returns an explicit unavailable error. Saved profiles retain
legacy display semantics and must not silently become desktop DISPLAY values.

## First implementation milestone

1. Create the new repository and initialize Jujutsu. Use the configured Git author name and email; do not copy personal hostnames, addresses, credentials or settings.
2. Extract protocol code and tests into the library; keep dependencies minimal.
3. Implement an explicit authenticated X11 display connection and client-window
   enumeration. Evaluate x11rb with only necessary extension features.
4. Capture one client window using Composite-backed pixmaps. Respect visual
   masks, byte order and row stride; convert to canonical RGBA8. Handle resize,
   unmap, destruction and Damage events with explicit resource ownership.
5. Send complete frames over stdio. Bound queues and coalesce stale full frames.
   Validate lengths before allocation and reject stale session/window identity.
6. Accept targeted keysyms, pointer coordinates and buttons. Verify focus before
   injection and release held input on disconnect/focus loss. Never equate Mac
   physical keycodes with X11 keycodes. Treat global XTEST focus as a constraint.
7. Test against Xvfb/Xorg and then a real authenticated Linux desktop. Record
   extension versions, capture behavior, transferred bytes and measured latency.
8. Integrate the library and agent with Omarchy Launchpad in a separate change.

Existing X11 applications belong to the user. Disconnect must not kill them or
terminate the desktop session. Never disable X authentication. Do not add
clipboard, audio, persistent reconnect or codecs before the first capture/input
path works. Raw frames are a correctness milestone, not a performance solution.

## Rust engineering expectations

Use typed messages and errors, checked arithmetic and explicit resource limits.
Keep unsafe code small and justified. Prefer RAII for X resources and child
process cleanup. Avoid unwrap/expect on untrusted input. Keep platform-specific
code outside the protocol crate. Use synchronous I/O initially unless measured
requirements justify an async runtime.

Run formatting, Clippy with warnings denied, protocol tests and Linux integration
tests. Include malformed/truncated messages, overflow, unsupported versions,
recycled identities, pixel conversion, resize and held-input cleanup. Fixtures
must be labelled as fixtures and cannot establish remote acceptance.

## Current evidence and limitations

Omarchy Launchpad passes 18 tests and Clippy. Two removed tests covered the
deleted xpra argument construction. No reachable Linux target has been established. No native
remote rendering, Xwayland compatibility or performance claims are verified.
The original HANDOFF.md contains additional capture and lifecycle requirements.

Repository created and protocol extracted. The agent now implements a bounded,
read-only EWMH catalogue command. Capture and input remain future work.
Use `jj status` to inspect current changes.

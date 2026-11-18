//! Live tests create and destroy only their own fixture windows.
use std::{
    io::Write,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};
use x11_bridge_protocol::{
    messages::{CAP_CAPTURE, CAP_INPUT, Input, Message},
    protocol::Packet,
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME,
    connection::Connection,
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
};

struct Fixture {
    c: RustConnection,
    window: u32,
    display: String,
}
impl Fixture {
    fn new() -> Self {
        let display = std::env::var("X11_BRIDGE_DISPLAY")
            .expect("X11_BRIDGE_DISPLAY required for live tests");
        let (c, screen) =
            x11rb::connect(Some(&display)).expect("authenticated test display unavailable");
        let root = &c.setup().roots[screen];
        let window = c.generate_id().unwrap();
        c.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root.root,
            20,
            20,
            96,
            64,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(0xff0000)
                .override_redirect(1)
                .event_mask(
                    EventMask::KEY_PRESS
                        | EventMask::KEY_RELEASE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION,
                ),
        )
        .unwrap()
        .check()
        .unwrap();
        c.map_window(window).unwrap().check().unwrap();
        c.flush().unwrap();
        Self { c, window, display }
    }
    fn paint(&self, color: u32) {
        self.c
            .change_window_attributes(
                self.window,
                &ChangeWindowAttributesAux::new().background_pixel(color),
            )
            .unwrap()
            .check()
            .unwrap();
        self.c
            .clear_area(false, self.window, 0, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        self.c.flush().unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.c.destroy_window(self.window);
        let _ = self.c.flush();
    }
}
struct Agent {
    child: Child,
    input: Option<ChildStdin>,
    packets: Receiver<Packet>,
    generation: u64,
}
impl Agent {
    fn new(f: &Fixture, input: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_x11-bridge-agent"));
        command.args(["--display", &f.display, "--window", &f.window.to_string()]);
        if input {
            command.arg("--allow-input");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        thread::spawn(move || {
            while let Ok(Some(packet)) = Packet::read(&mut stdout) {
                if tx.send(packet).is_err() {
                    break;
                }
            }
        });
        let mut agent = Self {
            child,
            input: Some(stdin),
            packets: rx,
            generation: 0,
        };
        let hello = agent.next();
        agent.generation = hello.generation;
        assert_eq!(hello.window, 0);
        let caps = CAP_CAPTURE | if input { CAP_INPUT } else { 0 };
        assert_eq!(
            Message::decode(&hello).unwrap(),
            Message::Hello { capabilities: caps }
        );
        agent.send_at(Message::Hello { capabilities: caps }, 0, agent.generation);
        agent
    }
    fn next(&self) -> Packet {
        self.packets
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for agent packet")
    }
    fn send_at(&mut self, message: Message, window: u64, generation: u64) {
        message
            .packet(generation, window)
            .unwrap()
            .write(self.input.as_mut().unwrap())
            .unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }
    fn send(&mut self, message: Message) {
        self.send_at(message, 1, self.generation);
    }
    fn frame(&self, width: u32, height: u32, pixel: [u8; 4]) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "expected frame never arrived");
            let p = self.next();
            assert_eq!((p.generation, p.window), (self.generation, 1));
            if let Message::Frame {
                width: w,
                height: h,
                rgba,
            } = Message::decode(&p).unwrap()
                && (w, h) == (width, height)
                && rgba.as_chunks::<4>().0.iter().all(|p| *p == pixel)
            {
                return rgba.len();
            }
        }
    }
    fn wait(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            assert!(Instant::now() < deadline, "agent failed to exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        // Give the agent a chance to release input even when a test panics.
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires an authenticated X11 display in X11_BRIDGE_DISPLAY"]
fn capture_resize_pause_remap_destroy() {
    let f = Fixture::new();
    let started = Instant::now();
    let mut agent = Agent::new(&f, false);
    let bytes = agent.frame(96, 64, [255, 0, 0, 255]);
    println!(
        "bridge-metric first_frame_bytes={bytes} first_frame_ms={}",
        started.elapsed().as_millis()
    );
    f.paint(0x00ff00);
    agent.frame(96, 64, [0, 255, 0, 255]);
    f.c.configure_window(f.window, &ConfigureWindowAux::new().width(128).height(80))
        .unwrap()
        .check()
        .unwrap();
    f.paint(0x0000ff);
    agent.frame(128, 80, [0, 0, 255, 255]);
    agent.send(Message::Pause);
    // Drain packets already in flight before asserting pause.
    thread::sleep(Duration::from_millis(150));
    while agent.packets.try_recv().is_ok() {}
    f.paint(0xff0000);
    assert!(
        agent
            .packets
            .recv_timeout(Duration::from_millis(200))
            .is_err()
    );
    agent.send(Message::Resume);
    agent.frame(128, 80, [255, 0, 0, 255]);
    f.c.unmap_window(f.window).unwrap().check().unwrap();
    thread::sleep(Duration::from_millis(100));
    f.c.map_window(f.window).unwrap().check().unwrap();
    f.paint(0x00ff00);
    agent.frame(128, 80, [0, 255, 0, 255]);
    f.c.destroy_window(f.window).unwrap().check().unwrap();
    loop {
        if Message::decode(&agent.next()).unwrap() == Message::Destroy {
            break;
        }
    }
    assert!(agent.wait());
}
#[test]
#[ignore = "requires an authenticated X11 display in X11_BRIDGE_DISPLAY"]
fn rejects_stale_identity_and_disconnect_preserves_window() {
    let f = Fixture::new();
    for bad_generation in [true, false] {
        let mut agent = Agent::new(&f, false);
        agent.frame(96, 64, [255, 0, 0, 255]);
        agent.send_at(
            Message::Pause,
            if bad_generation { 1 } else { 2 },
            if bad_generation {
                agent.generation.wrapping_add(1).max(1)
            } else {
                agent.generation
            },
        );
        assert!(!agent.wait());
        assert!(f.c.get_geometry(f.window).unwrap().reply().is_ok());
    }
    for message in [
        Message::Hello {
            capabilities: CAP_CAPTURE,
        },
        Message::Input(Input::ReleaseAll),
        Message::Destroy,
    ] {
        let mut agent = Agent::new(&f, false);
        agent.frame(96, 64, [255, 0, 0, 255]);
        agent.send(message);
        assert!(!agent.wait(), "invalid direction/capability was accepted");
    }
    let mut agent = Agent::new(&f, false);
    agent.frame(96, 64, [255, 0, 0, 255]);
    drop(agent.input.take());
    assert!(agent.wait());
    assert!(f.c.get_geometry(f.window).unwrap().reply().is_ok());
}
#[test]
#[ignore = "temporarily focuses a fixture; requires X11_BRIDGE_TEST_INPUT=1"]
fn targeted_input_and_held_key_cleanup() {
    assert_eq!(
        std::env::var("X11_BRIDGE_TEST_INPUT").as_deref(),
        Ok("1"),
        "set X11_BRIDGE_TEST_INPUT=1 to authorize fixture focus/input"
    );
    let f = Fixture::new();
    let previous = f.c.get_input_focus().unwrap().reply().unwrap();
    let root = f.c.query_tree(f.window).unwrap().reply().unwrap().root;
    let pointer = f.c.query_pointer(root).unwrap().reply().unwrap();
    struct Restore<'a>(&'a RustConnection, GetInputFocusReply, u32, i16, i16);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = self
                .0
                .warp_pointer(0u32, self.2, 0, 0, 0, 0, self.3, self.4);
            let _ = self
                .0
                .set_input_focus(self.1.revert_to, self.1.focus, CURRENT_TIME);
            let _ = self.0.flush();
        }
    }
    let _restore = Restore(&f.c, previous, root, pointer.root_x, pointer.root_y);
    for reason in [
        "release",
        "pause",
        "focus",
        "unmap",
        "disconnect",
        "malformed",
    ] {
        f.c.map_window(f.window).unwrap().check().unwrap();
        f.c.set_input_focus(InputFocus::PARENT, f.window, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        while f.c.poll_for_event().unwrap().is_some() {}
        let mut agent = Agent::new(&f, true);
        agent.frame(96, 64, [255, 0, 0, 255]);
        agent.send(Message::Input(Input::Key {
            keysym: 0x61,
            pressed: true,
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        let code = loop {
            if let Some(Event::KeyPress(event)) = f.c.poll_for_event().unwrap() {
                break event.detail;
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not receive key press"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let state = f.c.query_keymap().unwrap().reply().unwrap();
        assert_ne!(state.keys[usize::from(code / 8)] & (1 << (code % 8)), 0);
        match reason {
            "release" => agent.send(Message::Input(Input::ReleaseAll)),
            "pause" => {
                agent.send(Message::Pause);
                agent.send(Message::Input(Input::ReleaseAll));
            }
            "focus" => {
                f.c.set_input_focus(InputFocus::POINTER_ROOT, 1u32, CURRENT_TIME)
                    .unwrap()
                    .check()
                    .unwrap();
            }
            "unmap" => {
                f.c.unmap_window(f.window).unwrap().check().unwrap();
            }
            "disconnect" => {
                drop(agent.input.take());
                assert!(agent.wait());
            }
            "malformed" => {
                agent.input.as_mut().unwrap().write_all(&[0; 32]).unwrap();
                assert!(!agent.wait());
            }
            _ => unreachable!(),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = f.c.query_keymap().unwrap().reply().unwrap();
            if state.keys[usize::from(code / 8)] & (1 << (code % 8)) == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "held key was not released on {reason}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        if agent.input.is_some() {
            drop(agent.input.take());
            if reason != "malformed" {
                assert!(agent.wait());
            } else {
                let _ = agent.wait();
            }
        }
    }
    f.c.set_input_focus(InputFocus::PARENT, f.window, CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    while f.c.poll_for_event().unwrap().is_some() {}
    let mut agent = Agent::new(&f, true);
    agent.frame(96, 64, [255, 0, 0, 255]);
    agent.send(Message::Input(Input::Motion { x: 10, y: 12 }));
    agent.send(Message::Input(Input::Button {
        button: 1,
        pressed: true,
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(Event::ButtonPress(event)) = f.c.poll_for_event().unwrap() {
            assert_eq!((event.event_x, event.event_y, event.detail), (10, 12, 1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not receive targeted button press"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // Stop consuming frames, fill the reader queue and stdout pipe, then
    // disconnect stdin. Input cleanup must not depend on stdout progress.
    f.c.configure_window(f.window, &ConfigureWindowAux::new().width(512).height(512))
        .unwrap()
        .check()
        .unwrap();
    for color in [0, 0xffffff].into_iter().cycle().take(12) {
        f.paint(color);
        thread::sleep(Duration::from_millis(50));
    }
    drop(agent.input.take());
    assert!(agent.wait());
    let pointer = f.c.query_pointer(root).unwrap().reply().unwrap();
    assert_eq!(u16::from(pointer.mask) & u16::from(KeyButMask::BUTTON1), 0);
}

#[test]
#[ignore = "requires an authenticated X11 display in X11_BRIDGE_DISPLAY"]
fn destruction_with_pending_damage() {
    let f = Fixture::new();
    let mut agent = Agent::new(&f, false);
    agent.frame(96, 64, [255, 0, 0, 255]);
    // Keep the agent from acknowledging damage until the drawable and its
    // Damage resource have both been destroyed. Events remain in wire order.
    f.c.grab_server().unwrap().check().unwrap();
    f.paint(0x00ff00);
    f.c.destroy_window(f.window).unwrap().check().unwrap();
    f.c.ungrab_server().unwrap().check().unwrap();
    loop {
        if Message::decode(&agent.next()).unwrap() == Message::Destroy {
            break;
        }
    }
    assert!(agent.wait());
}

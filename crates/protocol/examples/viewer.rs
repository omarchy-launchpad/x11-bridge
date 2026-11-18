//! Small macOS viewer for the bridge's Docker GUI demo. AppKit integration for
//! Seams remains separate; this example only displays pixels and forwards input.
#[cfg(target_os = "macos")]
mod mac {
    use minifb::{Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};
    use std::{
        collections::BTreeSet,
        error::Error,
        io,
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex, mpsc},
        thread,
        time::{Duration, Instant},
    };
    use x11_bridge_protocol::{
        client::{self, Event},
        messages::{CAP_CAPTURE, CAP_INPUT, Input},
        protocol::map_pointer,
    };
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct Frame {
        width: usize,
        height: usize,
        pixels: Vec<u32>,
    }
    #[derive(Default)]
    struct Updates {
        frame: Option<Frame>,
        title: Option<String>,
        hidden: bool,
        ended: bool,
        error: Option<String>,
    }
    fn modifier(key: Key) -> bool {
        matches!(
            key,
            Key::LeftShift
                | Key::RightShift
                | Key::LeftCtrl
                | Key::RightCtrl
                | Key::LeftAlt
                | Key::RightAlt
        )
    }
    fn keysym(key: Key) -> Option<u32> {
        let k = key as u32;
        if (Key::A as u32..=Key::Z as u32).contains(&k) {
            return Some(0x61 + k - Key::A as u32);
        }
        if (Key::Key0 as u32..=Key::Key9 as u32).contains(&k) {
            return Some(0x30 + k - Key::Key0 as u32);
        }
        if (Key::NumPad0 as u32..=Key::NumPad9 as u32).contains(&k) {
            return Some(0x30 + k - Key::NumPad0 as u32);
        }
        Some(match key {
            Key::Space => 0x20,
            Key::Enter | Key::NumPadEnter => 0xff0d,
            Key::Tab => 0xff09,
            Key::Escape => 0xff1b,
            Key::Backspace => 0xff08,
            Key::Delete => 0xffff,
            Key::Left => 0xff51,
            Key::Up => 0xff52,
            Key::Right => 0xff53,
            Key::Down => 0xff54,
            Key::Home => 0xff50,
            Key::End => 0xff57,
            Key::PageUp => 0xff55,
            Key::PageDown => 0xff56,
            Key::Minus => 0x2d,
            Key::Equal => 0x3d,
            Key::Period | Key::NumPadDot => 0x2e,
            Key::Comma => 0x2c,
            Key::Slash => 0x2f,
            Key::Semicolon => 0x3b,
            Key::Apostrophe => 0x27,
            Key::LeftBracket => 0x5b,
            Key::RightBracket => 0x5d,
            Key::Backslash => 0x5c,
            Key::Backquote => 0x60,
            Key::LeftShift => 0xffe1,
            Key::RightShift => 0xffe2,
            Key::LeftCtrl => 0xffe3,
            Key::RightCtrl => 0xffe4,
            Key::LeftAlt => 0xffe9,
            Key::RightAlt => 0xffea,
            Key::NumPadSlash => 0xffaf,
            Key::NumPadAsterisk => 0xffaa,
            Key::NumPadMinus => 0xffad,
            Key::NumPadPlus => 0xffab,
            _ => return None,
        })
    }
    pub fn run() -> Result<(), Box<dyn Error>> {
        let mut args = std::env::args().skip(1);
        let program = args.next().ok_or("usage: viewer COMMAND [ARG ...]")?;
        let mut process = Process(
            Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        let stdout = process.0.stdout.take().ok_or("missing stdout")?;
        let stdin = process.0.stdin.take().ok_or("missing stdin")?;
        let updates = Arc::new(Mutex::new(Updates::default()));
        let shared = Arc::clone(&updates);
        let (tx, rx) = mpsc::sync_channel::<Vec<Input>>(32);
        // Handshake and pipe I/O stay off the native UI thread.
        thread::spawn(move || {
            let result = (|| -> io::Result<()> {
                let (mut events, mut controls) =
                    client::connect(stdout, stdin, CAP_CAPTURE | CAP_INPUT)?;
                let commands = Arc::clone(&shared);
                thread::spawn(move || {
                    'commands: while let Ok(batch) = rx.recv() {
                        for input in batch {
                            if let Err(error) = controls.input(input) {
                                if error.kind() == io::ErrorKind::WouldBlock {
                                    continue;
                                }
                                let mut state = commands.lock().unwrap();
                                state.error = Some(error.to_string());
                                state.ended = true;
                                break 'commands;
                            }
                        }
                    }
                    controls.disconnect();
                });
                while let Some(event) = events.next_event()? {
                    let mut state = shared.lock().unwrap();
                    match event {
                        Event::Frame {
                            width,
                            height,
                            rgba,
                        } => {
                            state.frame = Some(Frame {
                                width: width as usize,
                                height: height as usize,
                                pixels: rgba
                                    .as_chunks::<4>()
                                    .0
                                    .iter()
                                    .map(|p| {
                                        (u32::from(p[0]) << 16)
                                            | (u32::from(p[1]) << 8)
                                            | u32::from(p[2])
                                    })
                                    .collect(),
                            });
                            state.hidden = false;
                        }
                        Event::Window(window) => {
                            state.title = Some(format!("{} | x11-bridge", window.title))
                        }
                        Event::Hidden => {
                            state.hidden = true;
                            state.frame = None;
                        }
                        Event::Destroyed => {
                            state.ended = true;
                            break;
                        }
                    }
                }
                Ok(())
            })();
            let mut state = shared.lock().unwrap();
            if let Err(error) = result {
                state.error = Some(error.to_string());
            }
            state.ended = true;
        });
        let mut window = Window::new(
            "Connecting to Docker calculator...",
            420,
            560,
            WindowOptions {
                resize: true,
                scale_mode: ScaleMode::AspectRatioStretch,
                ..WindowOptions::default()
            },
        )?;
        window.set_target_fps(60);
        let mut frame = Frame {
            width: 420,
            height: 560,
            pixels: vec![0x182334; 420 * 560],
        };
        let mut received = false;
        let mut keys = BTreeSet::new();
        let mut buttons = [false; 3];
        let mut last_pointer = None;
        let mut active = false;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut failure = None;
        while window.is_open() {
            let hidden;
            {
                let mut state = updates.lock().unwrap();
                if let Some(next) = state.frame.take() {
                    frame = next;
                    received = true;
                }
                if let Some(title) = state.title.take() {
                    window.set_title(&title);
                }
                if state.ended {
                    failure = state.error.take();
                    break;
                }
                hidden = state.hidden;
            }
            if !received && Instant::now() > deadline {
                failure = Some("Timed out waiting for the calculator".into());
                break;
            }
            if hidden {
                window.update();
            } else {
                window.update_with_buffer(&frame.pixels, frame.width, frame.height)?;
            }
            let size = window.get_size();
            let point = window.get_mouse_pos(MouseMode::Discard).and_then(|(x, y)| {
                map_pointer(
                    (size.0 as f64, size.1 as f64),
                    (frame.width as u32, frame.height as u32),
                    (f64::from(x), f64::from(y)),
                )
            });
            let now_active = window.is_active() && received && !hidden;
            let mut batch = Vec::new();
            if !now_active || point.is_none() {
                if active || !keys.is_empty() || buttons.iter().any(|v| *v) {
                    batch.push(Input::ReleaseAll);
                }
                keys.clear();
                buttons = [false; 3];
                last_pointer = None;
            } else {
                if point != last_pointer {
                    if let Some((x, y)) = point {
                        batch.push(Input::Motion { x, y });
                    }
                    last_pointer = point;
                }
                for (i, button) in [MouseButton::Left, MouseButton::Middle, MouseButton::Right]
                    .into_iter()
                    .enumerate()
                {
                    let pressed = window.get_mouse_down(button);
                    if pressed != buttons[i] {
                        if pressed {eprintln!("[DEBUG-pointer] size={size:?} raw={:?} mapped={point:?}", window.get_mouse_pos(MouseMode::Discard));}
                        batch.push(Input::Button {
                            button: i as u8 + 1,
                            pressed,
                        });
                        buttons[i] = pressed;
                    }
                }
                if let Some((_, y)) = window.get_scroll_wheel()
                    && y != 0.0
                {
                    let button = if y > 0.0 { 4 } else { 5 };
                    batch.extend([
                        Input::Button {
                            button,
                            pressed: true,
                        },
                        Input::Button {
                            button,
                            pressed: false,
                        },
                    ]);
                }
                let mut pressed = window.get_keys_pressed(KeyRepeat::No);
                pressed.sort_by_key(|key| !modifier(*key));
                for key in pressed {
                    if let Some(keysym) = keysym(key)
                        && keys.insert(key)
                    {
                        batch.push(Input::Key {
                            keysym,
                            pressed: true,
                        });
                    }
                }
                let mut released = window.get_keys_released();
                released.sort_by_key(|key| modifier(*key));
                for key in released {
                    if keys.remove(&key)
                        && let Some(keysym) = keysym(key)
                    {
                        batch.push(Input::Key {
                            keysym,
                            pressed: false,
                        });
                    }
                }
            }
            active = now_active && point.is_some();
            if !batch.is_empty() && tx.try_send(batch).is_err() {
                failure = Some("Input transport stopped responding".into());
                break;
            }
        }
        drop(tx); // Close agent stdin after draining accepted commands.
        if let Some(error) = failure {
            return Err(error.into());
        }
        Ok(())
    }
}
#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = mac::run() {
        eprintln!("viewer: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The native preview viewer currently requires macOS.");
    std::process::exit(1);
}

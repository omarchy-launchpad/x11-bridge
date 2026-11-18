//! Disposable integration fixture. Owns only its own X11 window and child agent.
use std::{
    error::Error,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME,
    connection::Connection,
    protocol::{Event, xproto::*},
    rust_connection::RustConnection,
};
struct Fixture {
    connection: RustConnection,
    window: u32,
    previous_focus: Option<GetInputFocusReply>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(focus) = &self.previous_focus {
            let _ = self
                .connection
                .set_input_focus(focus.revert_to, focus.focus, CURRENT_TIME);
        }
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }
}
struct Agent(Child);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn draw_preview(c: &RustConnection, window: u32) -> Result<(), Box<dyn Error>> {
    let gc = c.generate_id()?;
    let font = c.generate_id()?;
    c.open_font(font, b"fixed")?.check()?;
    c.create_gc(gc, window, &CreateGCAux::new().font(font))?
        .check()?;
    let fill = |x, y, width, height, color| -> Result<(), Box<dyn Error>> {
        c.change_gc(gc, &ChangeGCAux::new().foreground(color))?
            .check()?;
        c.poly_fill_rectangle(
            window,
            gc,
            &[Rectangle {
                x,
                y,
                width,
                height,
            }],
        )?
        .check()?;
        Ok(())
    };
    let label = |x, y, color, text: &[u8]| -> Result<(), Box<dyn Error>> {
        c.change_gc(
            gc,
            &ChangeGCAux::new().foreground(color).background(0x162333),
        )?
        .check()?;
        c.poly_text8(window, gc, x, y, &[&[text.len() as u8, 0], text].concat())?
            .check()?;
        Ok(())
    };
    fill(0, 0, 640, 400, 0x101a28)?;
    fill(0, 0, 640, 54, 0x203249)?;
    fill(20, 20, 14, 14, 0x53d6a5)?;
    label(48, 31, 0xffffff, b"X11 BRIDGE / WINDOW PREVIEW")?;
    label(
        24,
        84,
        0xa7bfd4,
        b"A real X11 window, captured inside Docker.",
    )?;
    fill(24, 108, 380, 226, 0x162333)?;
    label(40, 134, 0xffffff, b"FRAME CONTENT")?;
    for y in [166, 206, 246, 286] {
        fill(40, y, 348, 1, 0x293d51)?;
    }
    for (i, height) in [48, 86, 66, 126, 106, 148, 130, 160]
        .into_iter()
        .enumerate()
    {
        fill(
            48 + i as i16 * 42,
            310 - height as i16,
            26,
            height,
            if i % 2 == 0 { 0x53d6a5 } else { 0x52a9ef },
        )?;
    }
    fill(424, 108, 192, 104, 0x162333)?;
    label(440, 134, 0xffffff, b"PIXEL CHECK")?;
    for (i, color) in [0xf87171, 0xfbbf24, 0x53d6a5, 0x52a9ef]
        .into_iter()
        .enumerate()
    {
        fill(440 + i as i16 * 40, 154, 32, 36, color)?;
    }
    fill(424, 228, 192, 106, 0x162333)?;
    label(440, 254, 0xffffff, b"CAPTURE PATH")?;
    label(440, 278, 0x53d6a5, b"Xvfb -> agent")?;
    label(440, 300, 0x52a9ef, b"client -> PNG")?;
    label(
        24,
        366,
        0xa7bfd4,
        b"640 x 400  |  RGBA8  |  No host desktop access",
    )?;
    c.free_gc(gc)?.check()?;
    c.close_font(font)?.check()?;
    c.flush()?;
    Ok(())
}
fn run() -> Result<(), Box<dyn Error>> {
    let display = std::env::var("X11_BRIDGE_DISPLAY")?;
    let preview = std::env::var("X11_BRIDGE_PREVIEW").as_deref() == Ok("1");
    let test_input = std::env::var("X11_BRIDGE_TEST_INPUT").as_deref() == Ok("1");
    let (c, screen) = x11rb::connect(Some(&display))?;
    let root = c.setup().roots[screen].root;
    let window = c.generate_id()?;
    c.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        20,
        20,
        if preview { 640 } else { 96 },
        if preview { 400 } else { 64 },
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .background_pixel(0xff0000)
            .override_redirect(1)
            .event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
    )?
    .check()?;
    let mut fixture = Fixture {
        connection: c,
        window,
        previous_focus: None,
    };
    let c = &fixture.connection;
    let title = b"x11-bridge integration fixture";
    c.change_property(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        8,
        title.len() as u32,
        title,
    )?
    .check()?;
    c.map_window(window)?.check()?;
    if preview {
        draw_preview(c, window)?;
    }
    if test_input {
        fixture.previous_focus = Some(c.get_input_focus()?.reply()?);
        c.set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?
            .check()?;
    }
    c.flush()?;
    let path = std::env::current_exe()?
        .parent()
        .ok_or("missing example directory")?
        .parent()
        .ok_or("missing binary directory")?
        .join("x11-bridge-agent");
    let mut command = Command::new(path);
    command.args(["--display", &display, "--window", &window.to_string()]);
    if test_input {
        command.arg("--allow-input");
    }
    let mut agent = Agent(
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut pressed = None;
    let mut released = false;
    loop {
        while let Some(event) = c.poll_for_event()? {
            match event {
                Event::KeyPress(event) => pressed = Some(event.detail),
                Event::KeyRelease(event) if Some(event.detail) == pressed => {
                    released = true;
                    c.change_window_attributes(
                        window,
                        &ChangeWindowAttributesAux::new().background_pixel(0x00ff00),
                    )?
                    .check()?;
                    c.clear_area(false, window, 0, 0, 0, 0)?.check()?;
                    c.flush()?;
                }
                _ => {}
            }
        }
        if let Some(status) = agent.0.try_wait()? {
            if !status.success() {
                return Err("fixture agent failed".into());
            }
            if test_input {
                if !released {
                    return Err("fixture input was not delivered".into());
                }
                let key = pressed.ok_or("fixture missed key press")?;
                let state = c.query_keymap()?.reply()?;
                if state.keys[usize::from(key / 8)] & (1 << (key % 8)) != 0 {
                    return Err("fixture key remains held".into());
                }
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("fixture timed out".into());
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn main() {
    // Keep fixture diagnostics generic: X11 errors can contain session details.
    if run().is_err() {
        eprintln!("streaming fixture failed");
        std::process::exit(1);
    }
}

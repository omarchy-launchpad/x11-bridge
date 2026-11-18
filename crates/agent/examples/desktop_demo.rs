//! Disposable calculator used by the live Docker viewer. Never opens host apps.
use std::{
    error::Error,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use x11rb::{CURRENT_TIME, connection::Connection, protocol::xproto::*};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn run() -> Result<(), Box<dyn Error>> {
    let display = std::env::var("X11_BRIDGE_DISPLAY")?;
    let (c, screen) = x11rb::connect(Some(&display))?;
    let root = c.setup().roots[screen].root;
    let name = format!("bridge-demo-{}", std::process::id());
    let mut app = Process(
        Command::new("xcalc")
            .args(["-name", &name, "-geometry", "420x560+20+20"])
            .env("DISPLAY", &display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let window = loop {
        if app.0.try_wait()?.is_some() {
            return Err("calculator exited before opening a window".into());
        }
        let mut selected = None;
        for window in c.query_tree(root)?.reply()?.children {
            let class = c
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)?
                .reply()?;
            if class.value.split(|b| *b == 0).next() == Some(name.as_bytes())
                && c.get_window_attributes(window)?.reply()?.map_state == MapState::VIEWABLE
            {
                selected = Some(window);
                break;
            }
        }
        if let Some(window) = selected {
            break window;
        }
        if Instant::now() >= deadline {
            return Err("calculator window did not appear".into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    c.set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?
        .check()?;
    c.flush()?;
    let binary = std::env::current_exe()?
        .parent()
        .ok_or("missing examples directory")?
        .parent()
        .ok_or("missing binary directory")?
        .join("x11-bridge-agent");
    let mut agent = Process(
        Command::new(binary)
            .args([
                "--display",
                &display,
                "--window",
                &window.to_string(),
                "--allow-input",
            ])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    loop {
        if let Some(status) = agent.0.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err("bridge agent failed".into())
            };
        }
        if app.0.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("desktop demo: {error}");
        std::process::exit(1);
    }
}

//! Consumer example and end-to-end fixture assertion, usable with any command
//! that exposes agent stdin/stdout. Machine details remain command/env inputs.
use std::{
    error::Error,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use x11_bridge_protocol::{
    client::{self, Event},
    messages::{CAP_CAPTURE, CAP_INPUT, Input},
};
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .ok_or("usage: stream_smoke [--preview] COMMAND [ARG ...]")?;
    let preview = first == "--preview";
    let program = if preview {
        args.next().ok_or("preview command required")?
    } else {
        first
    };
    let input = std::env::var("X11_BRIDGE_TEST_INPUT").as_deref() == Ok("1");
    let start = Instant::now();
    let mut process = Process(
        Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let stdout = process.0.stdout.take().ok_or("missing stdout")?;
    let stdin = process.0.stdin.take().ok_or("missing stdin")?;
    let (mut events, mut controls) = client::connect(
        stdout,
        stdin,
        CAP_CAPTURE | if input { CAP_INPUT } else { 0 },
    )?;
    let mut frames = 0;
    let mut saw_title = false;
    let mut first_ms = 0;
    while let Some(event) = events.next_event()? {
        match event {
            Event::Window(window) => saw_title = window.title == "x11-bridge integration fixture",
            Event::Frame {
                width,
                height,
                rgba,
            } => {
                if preview {
                    let mut encoder = png::Encoder::new(std::io::stdout().lock(), width, height);
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    let mut writer = encoder.write_header()?;
                    writer.write_image_data(&rgba)?;
                    writer.finish()?;
                    frames = 1;
                    break;
                }
                if !saw_title || (width, height) != (96, 64) {
                    return Err("unexpected fixture metadata".into());
                }
                let expected = if frames > 0 && input {
                    [0, 255, 0, 255]
                } else {
                    [255, 0, 0, 255]
                };
                if !rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| *pixel == expected)
                {
                    if frames > 0 && input {
                        continue;
                    }
                    return Err("incorrect fixture pixels".into());
                }
                frames += 1;
                if frames == 1 {
                    first_ms = start.elapsed().as_millis();
                    if input {
                        controls.input(Input::Key {
                            keysym: 0x61,
                            pressed: true,
                        })?;
                        controls.input(Input::ReleaseAll)?;
                    } else {
                        controls.pause()?;
                        controls.resume()?;
                    }
                } else {
                    break;
                }
            }
            _ => return Err("fixture ended before receiving frames".into()),
        }
    }
    if frames != if preview { 1 } else { 2 } {
        return Err("fixture frames missing".into());
    }
    controls.disconnect();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process.0.try_wait()? {
            if !status.success() {
                return Err("fixture transport failed".into());
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err("fixture transport did not exit".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    if preview {
        return Ok(());
    }
    println!(
        "bridge-ssh frames=2 pixel_bytes=49152 first_frame_ms={first_ms} input={}",
        u8::from(input)
    );
    Ok(())
}
fn main() {
    if run().is_err() {
        eprintln!("bridge streaming smoke test failed");
        std::process::exit(1);
    }
}

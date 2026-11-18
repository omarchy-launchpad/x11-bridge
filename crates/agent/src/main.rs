//! Authenticated X11 catalogue and selected-window stdio streaming agent.
mod input;
mod pixels;
mod stream;
use std::{
    error::Error,
    io::{self, Read, Write},
};
use x11_bridge_protocol::messages::Message;
use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt},
    rust_connection::RustConnection,
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn title(connection: &RustConnection, window: u32) -> Result<String> {
    let atom = connection
        .intern_atom(false, b"_NET_WM_NAME")?
        .reply()?
        .atom;
    let utf8 = connection.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
    let mut reply = connection
        .get_property(false, window, atom, utf8, 0, 1024)?
        .reply()?;
    if reply.type_ == 0 {
        reply = connection
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?
            .reply()?;
    }
    let mut title = String::from_utf8_lossy(&reply.value).into_owned();
    // Replacement characters can expand invalid UTF-8 beyond the wire limit.
    while title.len() > x11_bridge_protocol::messages::MAX_TEXT {
        title.pop();
    }
    Ok(title)
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut display = None;
    let mut window = None;
    let mut allow_input = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--display" if display.is_none() && i + 1 < args.len() => {
                i += 1;
                display = Some(args[i].as_str());
            }
            "--window" if window.is_none() && i + 1 < args.len() => {
                i += 1;
                window = Some(if let Some(hex) = args[i].strip_prefix("0x") {
                    u32::from_str_radix(hex, 16)?
                } else {
                    args[i].parse()?
                });
            }
            "--allow-input" if !allow_input => allow_input = true,
            _ => {
                return Err(
                    "usage: x11-bridge-agent --display DISPLAY [--window XID [--allow-input]]"
                        .into(),
                );
            }
        }
        i += 1;
    }
    let display = display.ok_or("explicit --display required")?;
    if display.is_empty() || (allow_input && window.is_none()) || window == Some(0) {
        return Err("invalid agent options".into());
    }
    let (connection, screen) = x11rb::connect(Some(display))?;
    let root = connection.setup().roots[screen].root;
    let mut nonce = [0; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let generation = u64::from_ne_bytes(nonce).max(1);
    if let Some(window) = window {
        return stream::run(&connection, root, window, generation, allow_input);
    }
    let clients = connection
        .intern_atom(false, b"_NET_CLIENT_LIST")?
        .reply()?
        .atom;
    let reply = connection
        .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
        .reply()?;
    if reply.bytes_after != 0 {
        return Err("client catalogue exceeds 4096 windows".into());
    }
    let windows: Vec<_> = reply
        .value32()
        .ok_or("window manager does not provide _NET_CLIENT_LIST; use --window XID")?
        .collect();
    let mut output = io::BufWriter::new(io::stdout().lock());
    Message::Hello { capabilities: 0 }
        .packet(generation, 0)?
        .write(&mut output)?;
    for window in windows {
        let Ok(geometry) = connection.get_geometry(window)?.reply() else {
            continue;
        };
        let Ok(title) = title(&connection, window) else {
            continue;
        };
        Message::Window {
            width: geometry.width.into(),
            height: geometry.height.into(),
            title,
        }
        .packet(generation, window.into())?
        .write(&mut output)?;
    }
    output.flush()?;
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("x11-bridge-agent: {error}");
        std::process::exit(1);
    }
}

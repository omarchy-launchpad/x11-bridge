//! Read-only first agent milestone: enumerate EWMH client windows.
use std::{
    error::Error,
    io::{self, Write},
};
use x11_bridge_protocol::messages::Message;
use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt},
};

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 || args[0] != "--display" {
        return Err(
            "usage: x11-bridge-agent --display DISPLAY (authenticated X11 session required)".into(),
        );
    }
    let (connection, screen) = x11rb::connect(Some(&args[1]))?;
    let root = connection.setup().roots[screen].root;
    let atom = |name: &[u8]| -> Result<u32, Box<dyn Error>> {
        Ok(connection.intern_atom(false, name)?.reply()?.atom)
    };
    let clients = atom(b"_NET_CLIENT_LIST")?;
    let title_atom = atom(b"_NET_WM_NAME")?;
    let utf8 = atom(b"UTF8_STRING")?;
    let reply = connection
        .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
        .reply()?;
    if reply.bytes_after != 0 {
        return Err("client catalogue exceeds 4096 windows".into());
    }
    let windows: Vec<_> = reply
        .value32()
        .ok_or("window manager does not provide _NET_CLIENT_LIST")?
        .collect();
    // Each invocation is a separate transport. The consumer must not reuse subscriptions.
    let generation = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
    )?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    Message::Hello { capabilities: 0 }
        .packet(generation, 0)?
        .write(&mut output)?;
    for (index, window) in windows.into_iter().enumerate() {
        // Clients can disappear between catalogue and metadata requests.
        let Ok(geometry) = connection.get_geometry(window)?.reply() else {
            continue;
        };
        let Ok(title) = connection
            .get_property(false, window, title_atom, utf8, 0, 1024)?
            .reply()
        else {
            continue;
        };
        let title = String::from_utf8_lossy(&title.value).into_owned();
        Message::Window {
            width: geometry.width.into(),
            height: geometry.height.into(),
            title,
        }
        .packet(generation, index as u64 + 1)?
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

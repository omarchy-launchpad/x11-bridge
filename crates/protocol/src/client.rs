//! Blocking transport-independent consumer API. Run reads/writes off the UI thread.
//! Pass owned pipes to `connect`; dropping Controls closes stdin and releases input.
use crate::{
    messages::{CAP_CAPTURE, CAP_INPUT, Input, MAX_TEXT, Message},
    protocol::{Kind, Packet},
};
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// XID in a catalogue; transport-local identity in a streaming session.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub title: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInfo {
    pub generation: u64,
    pub capabilities: u32,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Window(WindowInfo),
    Frame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Hidden,
    Destroyed,
}
#[derive(Default)]
struct State {
    alive: bool,
    paused: bool,
    visible: bool,
    size: Option<(u32, u32)>,
}
type Shared = Arc<Mutex<State>>;
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "bridge session ended")
}
fn invalidate(state: &Shared) {
    state.lock().unwrap().alive = false;
}

/// Read an entire one-shot catalogue. The transport must end at EOF.
/// No titles, host details or credentials are logged by this library.
pub fn read_catalogue(mut input: impl Read) -> io::Result<Vec<WindowInfo>> {
    let hello = Packet::read_limited(&mut input, 4)?
        .ok_or_else(|| invalid("missing catalogue handshake"))?;
    if hello.window != 0 || Message::decode(&hello)? != (Message::Hello { capabilities: 0 }) {
        return Err(invalid("invalid catalogue handshake"));
    }
    let mut windows = Vec::new();
    let mut ids = HashSet::new();
    while let Some(packet) = Packet::read_limited(&mut input, MAX_TEXT + 8)? {
        if windows.len() == 4096
            || packet.generation != hello.generation
            || packet.window > u64::from(u32::MAX)
            || !ids.insert(packet.window)
        {
            return Err(invalid("invalid catalogue identity or count"));
        }
        let Message::Window {
            width,
            height,
            title,
        } = Message::decode(&packet)?
        else {
            return Err(invalid("unexpected catalogue message"));
        };
        windows.push(WindowInfo {
            id: packet.window,
            width,
            height,
            title,
        });
    }
    Ok(windows)
}

/// Negotiate one selected window. `capabilities` must contain CAP_CAPTURE;
/// CAP_INPUT is optional and required to send input. Unsupported requests fail.
/// The returned halves can live on separate threads. EOF or any protocol error
/// invalidates both halves. The caller owns transport timeouts and process reaping.
pub fn connect<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    capabilities: u32,
) -> io::Result<(Events<R>, Controls<W>)> {
    if capabilities & CAP_CAPTURE == 0 || capabilities & !(CAP_CAPTURE | CAP_INPUT) != 0 {
        return Err(invalid("invalid requested capabilities"));
    }
    let hello = Packet::read_limited(&mut input, 4)?
        .ok_or_else(|| invalid("missing streaming handshake"))?;
    let Message::Hello {
        capabilities: offered,
    } = Message::decode(&hello)?
    else {
        return Err(invalid("expected streaming handshake"));
    };
    if hello.window != 0 || offered & capabilities != capabilities {
        return Err(invalid("agent does not offer requested capabilities"));
    }
    let info = SessionInfo {
        generation: hello.generation,
        capabilities,
    };
    Message::Hello { capabilities }
        .packet(info.generation, 0)?
        .write(&mut output)?;
    output.flush()?;
    let state = Arc::new(Mutex::new(State {
        alive: true,
        ..State::default()
    }));
    Ok((
        Events {
            input,
            info,
            state: Arc::clone(&state),
        },
        Controls {
            output,
            info,
            state,
        },
    ))
}

pub struct Events<R> {
    input: R,
    info: SessionInfo,
    state: Shared,
}
impl<R: Read> Events<R> {
    pub fn info(&self) -> SessionInfo {
        self.info
    }
    /// Read the next validated event. Frames received while locally paused are
    /// discarded. None means EOF, Destroy already delivered, or explicit close.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        if !self.state.lock().unwrap().alive {
            return Ok(None);
        }
        let result = self.receive();
        if result.is_err() || matches!(result, Ok(None)) {
            invalidate(&self.state);
        }
        result
    }
    fn receive(&mut self) -> io::Result<Option<Event>> {
        loop {
            let Some(packet) = Packet::read(&mut self.input)? else {
                return Ok(None);
            };
            if packet.generation != self.info.generation || packet.window != 1 {
                return Err(invalid("stale streaming identity"));
            }
            if !matches!(
                packet.kind,
                Kind::Window | Kind::Frame | Kind::Pause | Kind::Destroy | Kind::Error
            ) {
                return Err(invalid("unexpected agent message direction"));
            }
            let message = Message::from_packet(packet)?;
            let mut state = self.state.lock().unwrap();
            if !state.alive {
                return Ok(None);
            }
            return Ok(Some(match message {
                Message::Window {
                    width,
                    height,
                    title,
                } => {
                    state.size = Some((width, height));
                    Event::Window(WindowInfo {
                        id: 1,
                        width,
                        height,
                        title,
                    })
                }
                Message::Frame {
                    width,
                    height,
                    rgba,
                } => {
                    if state.size != Some((width, height)) {
                        return Err(invalid("frame does not match window metadata"));
                    }
                    if state.paused {
                        continue;
                    }
                    state.visible = true;
                    Event::Frame {
                        width,
                        height,
                        rgba,
                    }
                }
                Message::Pause => {
                    state.visible = false;
                    state.size = None;
                    Event::Hidden
                }
                Message::Destroy => {
                    state.alive = false;
                    Event::Destroyed
                }
                Message::Error(_) => {
                    return Err(io::Error::other("remote agent reported an error"));
                }
                _ => unreachable!(),
            }));
        }
    }
}
impl<R> Drop for Events<R> {
    fn drop(&mut self) {
        invalidate(&self.state);
    }
}

pub struct Controls<W> {
    output: W,
    info: SessionInfo,
    state: Shared,
}
impl<W: Write> Controls<W> {
    pub fn info(&self) -> SessionInfo {
        self.info
    }
    pub fn input(&mut self, input: Input) -> io::Result<()> {
        {
            let state = self.state.lock().unwrap();
            if !state.alive {
                return Err(closed());
            }
            if self.info.capabilities & CAP_INPUT == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "input was not negotiated",
                ));
            }
            if input != Input::ReleaseAll {
                if state.paused || !state.visible {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "selected window is paused or hidden",
                    ));
                }
                match input {
                    Input::Motion { x, y } if state.size.is_none_or(|(w, h)| x >= w || y >= h) => {
                        return Err(invalid("pointer outside window"));
                    }
                    Input::Button { button, .. } if !(1..=5).contains(&button) => {
                        return Err(invalid("unsupported button"));
                    }
                    Input::Key { keysym: 0, .. } => return Err(invalid("invalid keysym")),
                    _ => {}
                }
            }
        }
        self.send(Message::Input(input))
    }
    pub fn pause(&mut self) -> io::Result<()> {
        self.set_paused(true)?;
        self.send(Message::Pause)
    }
    pub fn resume(&mut self) -> io::Result<()> {
        self.set_paused(false)?;
        self.send(Message::Resume)
    }
    fn set_paused(&self, paused: bool) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.alive {
            return Err(closed());
        }
        state.paused = paused;
        if paused {
            state.visible = false;
        }
        Ok(())
    }
    fn send(&mut self, message: Message) -> io::Result<()> {
        if !self.state.lock().unwrap().alive {
            return Err(closed());
        }
        let result = message
            .packet(self.info.generation, 1)?
            .write(&mut self.output)
            .and_then(|()| self.output.flush());
        if result.is_err() {
            invalidate(&self.state);
        }
        result
    }
    /// Closes an owned writer without waiting for a blocked reader. With an
    /// agent's stdin pipe this triggers held-input cleanup and process exit.
    pub fn disconnect(self) {}
}
impl<W> Drop for Controls<W> {
    fn drop(&mut self) {
        invalidate(&self.state);
    }
}

use crate::{Result, input::InputState, pixels::Layout};
use std::{
    io::{self, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use x11_bridge_protocol::{
    messages::{CAP_CAPTURE, CAP_INPUT, Message},
    protocol::{MAX_PAYLOAD, Packet},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        composite::{ConnectionExt as _, Redirect},
        damage::{ConnectionExt as _, ReportLevel},
        xproto::*,
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

// At most one queued full frame; replacing it coalesces stale updates.
type Mailbox = Arc<(Mutex<Option<Vec<Packet>>>, Condvar)>;
fn publish(mailbox: &Mailbox, messages: Vec<Packet>) {
    *mailbox.0.lock().unwrap() = Some(messages);
    mailbox.1.notify_one();
}
// Deliver terminal state when possible, but never hang on a stalled consumer.
fn finish(writer: thread::JoinHandle<()>, failed: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !writer.is_finished() {
        if Instant::now() >= deadline {
            return Err("output transport stalled during shutdown".into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    writer.join().map_err(|_| "output thread panicked")?;
    if failed.load(Ordering::Acquire) {
        Err("output transport closed".into())
    } else {
        Ok(())
    }
}
struct Capture<'a> {
    connection: &'a RustConnection,
    window: u32,
    damage: u32,
}
impl<'a> Capture<'a> {
    fn new(connection: &'a RustConnection, window: u32) -> Result<Self> {
        connection
            .composite_redirect_window(window, Redirect::AUTOMATIC)?
            .check()?;
        let damage = connection.generate_id()?;
        let resource = Self {
            connection,
            window,
            damage,
        };
        connection
            .damage_create(damage, window, ReportLevel::NON_EMPTY)?
            .check()?;
        Ok(resource)
    }
    fn frame(&self) -> Result<Message> {
        let c = self.connection;
        let attributes = c.get_window_attributes(self.window)?.reply()?;
        let geometry = c.get_geometry(self.window)?.reply()?;
        let bytes = u64::from(geometry.width) * u64::from(geometry.height) * 4;
        if bytes == 0
            || bytes > (MAX_PAYLOAD - 8) as u64
            || geometry.width > 16384
            || geometry.height > 16384
        {
            return Err("window exceeds frame limit".into());
        }
        let visual = c
            .setup()
            .roots
            .iter()
            .flat_map(|s| &s.allowed_depths)
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == attributes.visual)
            .ok_or("unknown visual")?;
        if visual.class != VisualClass::TRUE_COLOR {
            return Err("only TrueColor visuals supported".into());
        }
        let format = c
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == geometry.depth)
            .ok_or("unknown pixel format")?;
        let pixmap = c.generate_id()?;
        c.composite_name_window_pixmap(self.window, pixmap)?
            .check()?;
        // Name anew each capture: resize/remap replaces the backing storage.
        // The pixmap can include the window border; capture only client pixels.
        let result = (|| -> Result<_> {
            let image = c
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    pixmap,
                    geometry.border_width.try_into()?,
                    geometry.border_width.try_into()?,
                    geometry.width,
                    geometry.height,
                    u32::MAX,
                )?
                .reply()?;
            let rgba = Layout {
                bits: format.bits_per_pixel,
                pad: format.scanline_pad,
                little_endian: c.setup().image_byte_order == ImageOrder::LSB_FIRST,
                masks: [visual.red_mask, visual.green_mask, visual.blue_mask],
            }
            .convert(geometry.width, geometry.height, &image.data)?;
            Ok(Message::Frame {
                width: geometry.width.into(),
                height: geometry.height.into(),
                rgba,
            })
        })();
        c.free_pixmap(pixmap)?.check()?;
        result
    }
}
impl Drop for Capture<'_> {
    fn drop(&mut self) {
        let _ = self.connection.damage_destroy(self.damage);
        let _ = self
            .connection
            .composite_unredirect_window(self.window, Redirect::AUTOMATIC);
        let _ = self.connection.flush();
    }
}

pub fn run(
    c: &RustConnection,
    root: u32,
    window: u32,
    generation: u64,
    allow_input: bool,
) -> Result<()> {
    if window == root {
        return Err("select a client window, not the root".into());
    }
    let composite = c.composite_query_version(0, 4)?.reply()?;
    if composite.major_version == 0 && composite.minor_version < 2 {
        return Err("Composite 0.2 required".into());
    }
    let damage = c.damage_query_version(1, 1)?.reply()?;
    if damage.major_version < 1 {
        return Err("Damage 1.0 required".into());
    }
    if allow_input {
        c.xtest_get_version(2, 2)?.reply()?;
    }
    eprintln!(
        "Composite {}.{}, Damage {}.{}",
        composite.major_version,
        composite.minor_version,
        damage.major_version,
        damage.minor_version
    );
    c.change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(
            EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE,
        ),
    )?
    .check()?;
    let capture = Capture::new(c, window)?;
    let mut input = InputState::new(c, window, root);
    let capabilities = CAP_CAPTURE | if allow_input { CAP_INPUT } else { 0 };
    let mailbox: Mailbox = Arc::new((Mutex::new(None), Condvar::new()));
    let output_failed = Arc::new(AtomicBool::new(false));
    let writer = {
        let mailbox = Arc::clone(&mailbox);
        let failed = Arc::clone(&output_failed);
        thread::spawn(move || {
            let result = (|| -> io::Result<()> {
                let mut output = io::BufWriter::new(io::stdout().lock());
                Message::Hello { capabilities }
                    .packet(generation, 0)?
                    .write(&mut output)?;
                output.flush()?;
                loop {
                    let mut guard = mailbox.0.lock().unwrap();
                    while guard.is_none() {
                        guard = mailbox.1.wait(guard).unwrap();
                    }
                    let packets = guard.take().unwrap();
                    drop(guard);
                    let mut destroyed = false;
                    for packet in packets {
                        destroyed |= packet.kind == x11_bridge_protocol::protocol::Kind::Destroy;
                        packet.write(&mut output)?;
                    }
                    output.flush()?;
                    if destroyed {
                        return Ok(());
                    }
                }
            })();
            if result.is_err() {
                failed.store(true, Ordering::Release);
            }
        })
    };
    let (tx, rx) = mpsc::sync_channel(16);
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let packet = Packet::read_limited(&mut stdin, 64);
            let done = !matches!(packet, Ok(Some(_)));
            if tx.send(packet).is_err() || done {
                break;
            }
        }
    });
    let mut negotiated = false;
    let mut negotiated_input = false;
    let mut capture_failures = 0;
    let mut paused = false;
    let mut dirty = true;
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    loop {
        if output_failed.load(Ordering::Acquire) {
            return Err("output transport closed".into());
        }
        for _ in 0..32 {
            match rx.try_recv() {
                Ok(Ok(Some(packet))) => {
                    if packet.generation != generation {
                        return Err("stale transport generation".into());
                    }
                    let message = Message::decode(&packet)?;
                    if !negotiated {
                        if packet.window != 0
                            || !matches!(message, Message::Hello {capabilities: flags} if flags & CAP_CAPTURE != 0 && flags & !capabilities == 0)
                        {
                            return Err("expected compatible consumer handshake".into());
                        }
                        negotiated_input = matches!(message, Message::Hello {capabilities: flags} if flags & CAP_INPUT != 0);
                        negotiated = true;
                        continue;
                    }
                    if packet.window != 1 {
                        return Err("stale window identity".into());
                    }
                    match message {
                        Message::Pause => {
                            paused = true;
                            input.release()?;
                        }
                        Message::Resume => {
                            paused = false;
                            dirty = true;
                        }
                        Message::Input(event)
                            if negotiated_input
                                && (!paused
                                    || event
                                        == x11_bridge_protocol::messages::Input::ReleaseAll) =>
                        {
                            if let Err(error) = input.apply(event) {
                                input.release()?;
                                return Err(error);
                            }
                        }
                        _ => {
                            return Err("invalid consumer message or unavailable capability".into());
                        }
                    }
                }
                Ok(Ok(None)) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                Ok(Err(error)) => return Err(error.into()),
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        for _ in 0..256 {
            let Some(event) = c.poll_for_event()? else {
                break;
            };
            match event {
                Event::DestroyNotify(event) if event.window == window => {
                    input.release()?;
                    publish(&mailbox, vec![Message::Destroy.packet(generation, 1)?]);
                    return finish(writer, &output_failed);
                }
                Event::DamageNotify(_) => {
                    if let Err(error) = c.damage_subtract(capture.damage, 0u32, 0u32)?.check() {
                        use x11rb::{errors::ReplyError, protocol::ErrorKind};
                        // The server destroys Damage resources with their drawable,
                        // but an earlier DamageNotify can still be queued locally.
                        if matches!(&error, ReplyError::X11Error(e) if e.error_kind == ErrorKind::DamageBadDamage)
                            && matches!(c.get_window_attributes(window)?.reply(), Err(ReplyError::X11Error(e)) if e.error_kind == ErrorKind::Window)
                        {
                            input.release()?;
                            publish(&mailbox, vec![Message::Destroy.packet(generation, 1)?]);
                            return finish(writer, &output_failed);
                        }
                        return Err(error.into());
                    }
                    dirty = true;
                }
                Event::UnmapNotify(_) => {
                    input.release()?;
                    if negotiated {
                        publish(&mailbox, vec![Message::Pause.packet(generation, 1)?]);
                    }
                    dirty = true;
                }
                Event::ConfigureNotify(_) | Event::MapNotify(_) | Event::PropertyNotify(_) => {
                    dirty = true
                }
                Event::FocusOut(_) => input.release()?,
                Event::Error(error) => return Err(format!("X11 event error: {error:?}").into()),
                _ => {}
            }
        }
        if allow_input && !input.focused()? {
            input.release()?;
        }
        if negotiated && dirty && !paused && last_frame.elapsed() >= Duration::from_millis(33) {
            let snapshot = (|| -> Result<Option<Vec<Packet>>> {
                let attrs = c.get_window_attributes(window)?.reply()?;
                if attrs.map_state != MapState::VIEWABLE {
                    return Ok(None);
                }
                let frame = capture.frame()?;
                let (width, height) = match &frame {
                    Message::Frame { width, height, .. } => (*width, *height),
                    _ => unreachable!(),
                };
                let title = crate::title(c, window)?;
                Ok(Some(vec![
                    Message::Window {
                        width,
                        height,
                        title,
                    }
                    .packet(generation, 1)?,
                    frame.packet(generation, 1)?,
                ]))
            })();
            match snapshot {
                Ok(Some(packets)) => {
                    publish(&mailbox, packets);
                    dirty = false;
                    capture_failures = 0;
                }
                Ok(None) => {}
                Err(error) => {
                    use x11rb::{errors::ReplyError, protocol::ErrorKind};
                    let kind = error.downcast_ref::<ReplyError>().and_then(|e| match e {
                        ReplyError::X11Error(e) => Some(e.error_kind),
                        _ => None,
                    });
                    if kind == Some(ErrorKind::Window) {
                        input.release()?;
                        publish(&mailbox, vec![Message::Destroy.packet(generation, 1)?]);
                        return finish(writer, &output_failed);
                    }
                    // GetImage/NameWindowPixmap can race resize or unmap.
                    capture_failures += 1;
                    if !matches!(kind, Some(ErrorKind::Match | ErrorKind::Drawable))
                        || capture_failures >= 3
                    {
                        return Err(error);
                    }
                }
            }
            last_frame = Instant::now();
        }
        c.flush()?;
        thread::sleep(Duration::from_millis(5));
    }
}

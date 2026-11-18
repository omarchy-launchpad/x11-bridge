use std::io::{self, Cursor, Write};
use x11_bridge_protocol::{
    client::{self, Event},
    messages::{CAP_CAPTURE, CAP_INPUT, Input, Message},
    protocol::Packet,
};
fn bytes(messages: impl IntoIterator<Item = (u64, u64, Message)>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (generation, window, message) in messages {
        message
            .packet(generation, window)
            .unwrap()
            .write(&mut bytes)
            .unwrap();
    }
    bytes
}
fn hello(caps: u32) -> (u64, u64, Message) {
    (7, 0, Message::Hello { capabilities: caps })
}
fn window() -> (u64, u64, Message) {
    (
        7,
        1,
        Message::Window {
            width: 1,
            height: 1,
            title: "fixture".into(),
        },
    )
}
fn frame() -> (u64, u64, Message) {
    (
        7,
        1,
        Message::Frame {
            width: 1,
            height: 1,
            rgba: vec![255, 0, 0, 255],
        },
    )
}
#[test]
fn handshake_frames_controls_and_eof() {
    let data = bytes([hello(3), window(), frame()]);
    let mut output = Vec::new();
    let (mut events, mut controls) = client::connect(Cursor::new(data), &mut output, 3).unwrap();
    assert_eq!(events.info().generation, 7);
    assert!(controls.input(Input::Motion { x: 0, y: 0 }).is_err());
    assert!(matches!(
        events.next_event().unwrap(),
        Some(Event::Window(_))
    ));
    assert!(matches!(
        events.next_event().unwrap(),
        Some(Event::Frame { .. })
    ));
    controls.input(Input::Motion { x: 0, y: 0 }).unwrap();
    assert!(controls.input(Input::Motion { x: 1, y: 0 }).is_err());
    controls.pause().unwrap();
    controls.input(Input::ReleaseAll).unwrap();
    assert!(
        controls
            .input(Input::Key {
                keysym: 0x61,
                pressed: true
            })
            .is_err()
    );
    controls.resume().unwrap();
    assert_eq!(events.next_event().unwrap(), None);
    assert_eq!(
        controls.pause().unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
    drop(controls);
    let mut output = Cursor::new(output);
    for expected in [
        Message::Hello { capabilities: 3 },
        Message::Input(Input::Motion { x: 0, y: 0 }),
        Message::Pause,
        Message::Input(Input::ReleaseAll),
        Message::Resume,
    ] {
        assert_eq!(
            Message::decode(&Packet::read(&mut output).unwrap().unwrap()).unwrap(),
            expected
        );
    }
    assert_eq!(Packet::read(output).unwrap(), None);
}
#[test]
fn hide_remap_destroy_and_paused_frames() {
    let data = bytes([
        hello(1),
        window(),
        frame(),
        (7, 1, Message::Pause),
        window(),
        frame(),
        (7, 1, Message::Destroy),
    ]);
    let (mut events, mut controls) = client::connect(Cursor::new(data), Vec::new(), 1).unwrap();
    events.next_event().unwrap();
    events.next_event().unwrap();
    assert_eq!(events.next_event().unwrap(), Some(Event::Hidden));
    controls.pause().unwrap();
    events.next_event().unwrap();
    assert_eq!(events.next_event().unwrap(), Some(Event::Destroyed)); // paused frame discarded
    assert_eq!(events.next_event().unwrap(), None);
    assert!(controls.resume().is_err());
}
#[test]
fn rejects_capabilities_and_bad_handshakes() {
    for (message, requested) in [
        (hello(0), 1),
        (hello(1), 3),
        (hello(3), 0),
        (hello(3), 5),
        (window(), 1),
    ] {
        assert!(client::connect(Cursor::new(bytes([message])), Vec::new(), requested).is_err());
    }
    let (mut events, mut controls) = client::connect(
        Cursor::new(bytes([hello(1), window(), frame()])),
        Vec::new(),
        1,
    )
    .unwrap();
    events.next_event().unwrap();
    events.next_event().unwrap();
    assert_eq!(
        controls.input(Input::ReleaseAll).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}
#[test]
fn invalid_agent_packets_invalidate_both_halves() {
    for invalid in [
        (8, 1, Message::Pause),
        (7, 2, Message::Pause),
        (7, 1, Message::Resume),
        hello(1),
        frame(),
        (7, 1, Message::Error("private remote diagnostic".into())),
    ] {
        let (mut events, mut controls) =
            client::connect(Cursor::new(bytes([hello(1), invalid])), Vec::new(), 1).unwrap();
        let error = events.next_event().unwrap_err();
        assert!(!error.to_string().contains("private remote diagnostic"));
        assert!(controls.resume().is_err());
        assert_eq!(events.next_event().unwrap(), None);
    }
    let bad_frame = (
        7,
        1,
        Message::Frame {
            width: 2,
            height: 1,
            rgba: vec![0; 8],
        },
    );
    let (mut events, _controls) = client::connect(
        Cursor::new(bytes([hello(1), window(), bad_frame])),
        Vec::new(),
        1,
    )
    .unwrap();
    events.next_event().unwrap();
    assert!(events.next_event().is_err());
}
#[test]
fn truncation_and_reader_drop_close_controls() {
    let mut data = bytes([hello(3), window()]);
    data.pop();
    let (mut events, mut controls) =
        client::connect(Cursor::new(data), Vec::new(), CAP_CAPTURE | CAP_INPUT).unwrap();
    assert!(events.next_event().is_err());
    assert!(controls.input(Input::ReleaseAll).is_err());
    let (events, mut controls) =
        client::connect(Cursor::new(bytes([hello(1)])), Vec::new(), 1).unwrap();
    drop(events);
    assert!(controls.resume().is_err());
}
#[test]
fn write_failure_closes_reader() {
    struct FailAfterHandshake(bool);
    impl Write for FailAfterHandshake {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0 {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0 = true;
            Ok(())
        }
    }
    let (mut events, mut controls) = client::connect(
        Cursor::new(bytes([hello(1), window()])),
        FailAfterHandshake(false),
        1,
    )
    .unwrap();
    assert_eq!(
        controls.pause().unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(events.next_event().unwrap(), None);
}
#[test]
fn catalogue_checks_generation_identity_direction_and_limits() {
    let valid = bytes([hello(0), window()]);
    let catalogue = client::read_catalogue(Cursor::new(valid)).unwrap();
    assert_eq!(catalogue[0].title, "fixture");
    for data in [
        bytes([hello(1)]),
        bytes([hello(0), window(), window()]),
        bytes([hello(0), (8, 1, Message::Pause)]),
        bytes([hello(0), frame()]),
    ] {
        assert!(client::read_catalogue(Cursor::new(data)).is_err());
    }
    let mut data = bytes([hello(0)]);
    for id in 1..=4097 {
        Message::Window {
            width: 1,
            height: 1,
            title: String::new(),
        }
        .packet(7, id)
        .unwrap()
        .write(&mut data)
        .unwrap();
    }
    assert!(client::read_catalogue(Cursor::new(data)).is_err());
}

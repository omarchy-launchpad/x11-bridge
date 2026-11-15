//! Typed v1 payloads. Text is UTF-8 and bounded by the packet limit.
use crate::protocol::{Kind, Packet};
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Motion { x: u32, y: u32 },
    Button { button: u8, pressed: bool },
    Key { keysym: u32, pressed: bool },
    ReleaseAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello {
        capabilities: u32,
    },
    Window {
        width: u32,
        height: u32,
        title: String,
    },
    Destroy,
    Frame {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Input(Input),
    Pause,
    Resume,
    Error(String),
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid message payload")
}
fn integer(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}
fn dimensions(bytes: &[u8]) -> io::Result<(u32, u32)> {
    let size = (integer(bytes, 0)?, integer(bytes, 4)?);
    if size.0 == 0 || size.1 == 0 || size.0 > 16384 || size.1 > 16384 {
        return Err(invalid());
    }
    Ok(size)
}
impl Message {
    pub fn decode(packet: &Packet) -> io::Result<Self> {
        let p = &packet.payload;
        Ok(match packet.kind {
            Kind::Hello if p.len() == 4 => Self::Hello {
                capabilities: integer(p, 0)?,
            },
            Kind::Window if p.len() >= 8 => {
                let (width, height) = dimensions(p)?;
                Self::Window {
                    width,
                    height,
                    title: String::from_utf8(p[8..].to_vec()).map_err(|_| invalid())?,
                }
            }
            Kind::Frame => {
                let (width, height, rgba) = packet.frame()?;
                Self::Frame {
                    width,
                    height,
                    rgba: rgba.to_vec(),
                }
            }
            Kind::Destroy if p.is_empty() => Self::Destroy,
            Kind::Pause if p.is_empty() => Self::Pause,
            Kind::Resume if p.is_empty() => Self::Resume,
            Kind::Error => Self::Error(String::from_utf8(p.clone()).map_err(|_| invalid())?),
            Kind::Input => Self::Input(match p.as_slice() {
                [0, rest @ ..] if rest.len() == 8 => Input::Motion {
                    x: integer(rest, 0)?,
                    y: integer(rest, 4)?,
                },
                [1, button, pressed] if (1..=7).contains(button) && *pressed <= 1 => {
                    Input::Button {
                        button: *button,
                        pressed: *pressed == 1,
                    }
                }
                [2, pressed, rest @ ..] if *pressed <= 1 && rest.len() == 4 => Input::Key {
                    keysym: integer(rest, 0)?,
                    pressed: *pressed == 1,
                },
                [3] => Input::ReleaseAll,
                _ => return Err(invalid()),
            }),
            _ => return Err(invalid()),
        })
    }
    pub fn packet(self, generation: u64, window: u64) -> io::Result<Packet> {
        let mut payload = Vec::new();
        let kind = match self {
            Self::Hello { capabilities } => {
                payload.extend(capabilities.to_le_bytes());
                Kind::Hello
            }
            Self::Window {
                width,
                height,
                title,
            } => {
                payload.extend(width.to_le_bytes());
                payload.extend(height.to_le_bytes());
                payload.extend(title.as_bytes());
                Kind::Window
            }
            Self::Frame {
                width,
                height,
                rgba,
            } => {
                payload.extend(width.to_le_bytes());
                payload.extend(height.to_le_bytes());
                payload.extend(rgba);
                Kind::Frame
            }
            Self::Destroy => Kind::Destroy,
            Self::Pause => Kind::Pause,
            Self::Resume => Kind::Resume,
            Self::Error(s) => {
                payload.extend(s.as_bytes());
                Kind::Error
            }
            Self::Input(input) => {
                match input {
                    Input::Motion { x, y } => {
                        payload.push(0);
                        payload.extend(x.to_le_bytes());
                        payload.extend(y.to_le_bytes());
                    }
                    Input::Button { button, pressed } => {
                        payload.extend([1, button, u8::from(pressed)])
                    }
                    Input::Key { keysym, pressed } => {
                        payload.extend([2, u8::from(pressed)]);
                        payload.extend(keysym.to_le_bytes());
                    }
                    Input::ReleaseAll => payload.push(3),
                };
                Kind::Input
            }
        };
        let packet = Packet {
            kind,
            generation,
            window,
            payload,
        };
        Self::decode(&packet)?;
        packet.write(io::sink())?;
        Ok(packet)
    }
}

/// One subscription per transport. Reject stale generations and window identities.
pub struct Subscription {
    generation: u64,
    window: u64,
    alive: bool,
}
impl Subscription {
    pub fn new(generation: u64, window: u64) -> io::Result<Self> {
        if generation == 0 || window == 0 {
            return Err(invalid());
        }
        Ok(Self {
            generation,
            window,
            alive: true,
        })
    }
    pub fn accept(&mut self, packet: &Packet) -> io::Result<Message> {
        if !self.alive || packet.generation != self.generation || packet.window != self.window {
            return Err(invalid());
        }
        let message = Message::decode(packet)?;
        if message == Message::Destroy {
            self.alive = false;
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_messages_roundtrip() {
        for message in [
            Message::Hello { capabilities: 0 },
            Message::Window {
                width: 1,
                height: 1,
                title: "窓".into(),
            },
            Message::Destroy,
            Message::Frame {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
            },
            Message::Pause,
            Message::Resume,
            Message::Error("failure".into()),
            Message::Input(Input::Motion { x: 12, y: 34 }),
            Message::Input(Input::Key {
                keysym: 0xff0d,
                pressed: true,
            }),
            Message::Input(Input::Button {
                button: 1,
                pressed: false,
            }),
            Message::Input(Input::ReleaseAll),
        ] {
            let packet = message.clone().packet(1, 1).unwrap();
            assert_eq!(Message::decode(&packet).unwrap(), message);
        }
    }
    #[test]
    fn rejects_stale_and_destroyed_windows() {
        let mut s = Subscription::new(1, 2).unwrap();
        assert!(s.accept(&Message::Pause.packet(2, 2).unwrap()).is_err());
        assert!(s.accept(&Message::Pause.packet(1, 3).unwrap()).is_err());
        s.accept(&Message::Destroy.packet(1, 2).unwrap()).unwrap();
        assert!(s.accept(&Message::Resume.packet(1, 2).unwrap()).is_err());
    }
    #[test]
    fn rejects_malformed_input() {
        for payload in [vec![], vec![1, 0, 1], vec![1, 1, 2], vec![3, 0], vec![2, 1]] {
            assert!(
                Message::decode(&Packet {
                    kind: Kind::Input,
                    generation: 1,
                    window: 1,
                    payload
                })
                .is_err()
            );
        }
    }
}

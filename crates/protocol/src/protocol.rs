//! Version 1 framing. Integers are little endian; pixels are tightly packed RGBA8.
use std::io::{self, Read, Write};

pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
pub const HEADER_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: Kind,
    pub generation: u64,
    pub window: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Hello = 1,
    Window = 2,
    Destroy = 3,
    Frame = 4,
    Input = 5,
    Pause = 6,
    Resume = 7,
    Error = 8,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Packet {
    fn validate(&self) -> io::Result<()> {
        if self.generation == 0 || self.payload.len() > MAX_PAYLOAD {
            return Err(invalid("invalid generation or oversized payload"));
        }
        if !matches!(self.kind, Kind::Hello | Kind::Error) && self.window == 0 {
            return Err(invalid("window identity must be nonzero"));
        }
        if matches!(self.kind, Kind::Destroy | Kind::Pause | Kind::Resume)
            && !self.payload.is_empty()
        {
            return Err(invalid("control packet must be empty"));
        }
        if self.kind == Kind::Frame {
            self.frame()?;
        }
        Ok(())
    }

    pub fn write(&self, mut output: impl Write) -> io::Result<()> {
        self.validate()?;
        output.write_all(b"SEAM")?;
        output.write_all(&1u16.to_le_bytes())?;
        output.write_all(&(self.kind as u16).to_le_bytes())?;
        output.write_all(&(self.payload.len() as u32).to_le_bytes())?;
        output.write_all(&0u32.to_le_bytes())?;
        output.write_all(&self.generation.to_le_bytes())?;
        output.write_all(&self.window.to_le_bytes())?;
        output.write_all(&self.payload)
    }

    /// EOF is valid only between packets. Partial headers and bodies are errors.
    pub fn read(mut input: impl Read) -> io::Result<Option<Self>> {
        Self::read_limited(&mut input, MAX_PAYLOAD)
    }

    /// Read with a stricter payload bound, before allocating the payload.
    pub fn read_limited(mut input: impl Read, limit: usize) -> io::Result<Option<Self>> {
        let mut header = [0; HEADER_LEN];
        loop {
            match input.read(&mut header[..1]) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        input.read_exact(&mut header[1..])?;
        if &header[..4] != b"SEAM" || header[4..6] != 1u16.to_le_bytes() || header[12..16] != [0; 4]
        {
            return Err(invalid("unsupported protocol header"));
        }
        let kind = match u16::from_le_bytes(header[6..8].try_into().unwrap()) {
            1 => Kind::Hello,
            2 => Kind::Window,
            3 => Kind::Destroy,
            4 => Kind::Frame,
            5 => Kind::Input,
            6 => Kind::Pause,
            7 => Kind::Resume,
            8 => Kind::Error,
            _ => return Err(invalid("unknown message kind")),
        };
        let size = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if size > MAX_PAYLOAD || size > limit {
            return Err(invalid("oversized payload"));
        }
        let mut packet = Self {
            kind,
            generation: u64::from_le_bytes(header[16..24].try_into().unwrap()),
            window: u64::from_le_bytes(header[24..32].try_into().unwrap()),
            payload: vec![0; size],
        };
        input.read_exact(&mut packet.payload)?;
        packet.validate()?;
        Ok(Some(packet))
    }

    pub fn frame(&self) -> io::Result<(u32, u32, &[u8])> {
        if self.kind != Kind::Frame || self.payload.len() < 8 {
            return Err(invalid("missing frame dimensions"));
        }
        let w = u32::from_le_bytes(self.payload[..4].try_into().unwrap());
        let h = u32::from_le_bytes(self.payload[4..8].try_into().unwrap());
        let bytes = u64::from(w)
            .checked_mul(u64::from(h))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid("frame dimensions overflow"))?;
        if w == 0
            || h == 0
            || bytes > (MAX_PAYLOAD - 8) as u64
            || bytes != (self.payload.len() - 8) as u64
        {
            return Err(invalid("invalid frame dimensions or pixel length"));
        }
        Ok((w, h, &self.payload[8..]))
    }
}

/// Coordinates use a top-left origin and AppKit points, not backing pixels.
/// Returns None for letterbox margins and invalid or nonfinite geometry.
pub fn map_pointer(view: (f64, f64), remote: (u32, u32), point: (f64, f64)) -> Option<(u32, u32)> {
    let (vw, vh) = view;
    let (rw, rh) = remote;
    let (x, y) = point;
    if ![vw, vh, x, y].iter().all(|v| v.is_finite()) || vw <= 0.0 || vh <= 0.0 || rw == 0 || rh == 0
    {
        return None;
    }
    let scale = (vw / rw as f64).min(vh / rh as f64);
    let x = (x - (vw - rw as f64 * scale) / 2.0) / scale;
    let y = (y - (vh - rh as f64 * scale) / 2.0) / scale;
    if x < 0.0 || y < 0.0 || x >= rw as f64 || y >= rh as f64 {
        None
    } else {
        Some((x.floor() as u32, y.floor() as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Packet {
        Packet {
            kind: Kind::Frame,
            generation: 1,
            window: 9,
            payload: [
                1u32.to_le_bytes().as_slice(),
                1u32.to_le_bytes().as_slice(),
                &[255, 0, 0, 255],
            ]
            .concat(),
        }
    }
    #[test]
    fn roundtrip_and_all_truncations() {
        let p = sample();
        let mut bytes = Vec::new();
        p.write(&mut bytes).unwrap();
        assert_eq!(Packet::read(bytes.as_slice()).unwrap(), Some(p));
        assert_eq!(Packet::read(&[][..]).unwrap(), None);
        for end in 1..bytes.len() {
            assert!(Packet::read(&bytes[..end]).is_err());
        }
    }
    #[test]
    fn rejects_hostile_headers_and_dimensions() {
        let mut bytes = Vec::new();
        sample().write(&mut bytes).unwrap();
        for (offset, replacement) in [
            (4, vec![2, 0]),
            (8, u32::MAX.to_le_bytes().to_vec()),
            (24, vec![0; 8]),
            (16, vec![0; 8]),
            (32, u32::MAX.to_le_bytes().to_vec()),
        ] {
            let mut bad = bytes.clone();
            bad[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(Packet::read(bad.as_slice()).is_err());
        }
    }
    #[test]
    fn consumer_limit_rejects_before_reading_body() {
        let mut bytes = Vec::new();
        sample().write(&mut bytes).unwrap();
        bytes[8..12].copy_from_slice(&65u32.to_le_bytes());
        let mut cursor = io::Cursor::new(bytes);
        assert_eq!(
            Packet::read_limited(&mut cursor, 64).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(cursor.position(), HEADER_LEN as u64);
    }
    #[test]
    fn letterbox_and_retina_point_mapping() {
        assert_eq!(
            map_pointer((200., 200.), (400, 200), (100., 100.)),
            Some((200, 100))
        );
        assert_eq!(map_pointer((200., 200.), (400, 200), (100., 49.)), None);
        assert_eq!(map_pointer((200., 200.), (400, 200), (200., 100.)), None);
        assert_eq!(map_pointer((f64::NAN, 200.), (400, 200), (0., 0.)), None);
    }
}

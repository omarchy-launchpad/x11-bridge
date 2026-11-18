//! X11 ZPixmap to opaque RGBA8. Alpha is not inferred from unused visual bits.
use std::io;
use x11_bridge_protocol::protocol::MAX_PAYLOAD;

pub struct Layout {
    pub bits: u8,
    pub pad: u8,
    pub little_endian: bool,
    pub masks: [u32; 3],
}
impl Layout {
    pub fn convert(&self, width: u16, height: u16, data: &[u8]) -> io::Result<Vec<u8>> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unsupported X11 pixel layout");
        if ![16, 24, 32].contains(&self.bits) || ![8, 16, 32].contains(&self.pad) {
            return Err(invalid());
        }
        for (i, mask) in self.masks.iter().copied().enumerate() {
            if mask == 0 || (self.bits < 32 && mask >> self.bits != 0) {
                return Err(invalid());
            }
            let normalized = mask >> mask.trailing_zeros();
            if normalized & normalized.wrapping_add(1) != 0
                || self.masks[..i].iter().any(|other| other & mask != 0)
            {
                return Err(invalid());
            }
        }
        let size = usize::from(width) * usize::from(height) * 4;
        if size == 0 || size > MAX_PAYLOAD - 8 {
            return Err(invalid());
        }
        let stride = (usize::from(width) * usize::from(self.bits)).div_ceil(usize::from(self.pad))
            * usize::from(self.pad)
            / 8;
        if data.len() < stride * usize::from(height) {
            return Err(invalid());
        }
        let mut result = Vec::with_capacity(size);
        for row in data[..stride * usize::from(height)].chunks_exact(stride) {
            for pixel in row[..usize::from(width) * usize::from(self.bits) / 8]
                .chunks_exact(usize::from(self.bits) / 8)
            {
                let value = if self.little_endian {
                    pixel
                        .iter()
                        .rev()
                        .fold(0u32, |v, b| (v << 8) | u32::from(*b))
                } else {
                    pixel.iter().fold(0u32, |v, b| (v << 8) | u32::from(*b))
                };
                for mask in self.masks {
                    let component = (value & mask) >> mask.trailing_zeros();
                    let max = mask >> mask.trailing_zeros();
                    result.push(
                        ((u64::from(component) * 255 + u64::from(max) / 2) / u64::from(max)) as u8,
                    );
                }
                result.push(255);
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truecolor_32bit_and_invalid_masks() {
        for little_endian in [true, false] {
            let layout = Layout {
                bits: 32,
                pad: 32,
                little_endian,
                masks: [0xff0000, 0xff00, 0xff],
            };
            let data = if little_endian {
                0x127f8040u32.to_le_bytes()
            } else {
                0x127f8040u32.to_be_bytes()
            };
            assert_eq!(layout.convert(1, 1, &data).unwrap(), [127, 128, 64, 255]);
        }
        for masks in [
            [0, 0xff00, 0xff],
            [0xff, 0xff00, 0xff],
            [0x550000, 0xff00, 0xff],
        ] {
            assert!(
                Layout {
                    bits: 32,
                    pad: 32,
                    little_endian: true,
                    masks
                }
                .convert(1, 1, &[0; 4])
                .is_err()
            );
        }
    }
    #[test]
    fn byte_orders_padding_and_rgb565() {
        for little_endian in [false, true] {
            let l = Layout {
                bits: 24,
                pad: 32,
                little_endian,
                masks: [0xff0000, 0xff00, 0xff],
            };
            let bytes = if little_endian {
                vec![0, 0, 255, 42, 0, 255, 0, 42]
            } else {
                vec![255, 0, 0, 42, 0, 255, 0, 42]
            };
            assert_eq!(
                l.convert(1, 2, &bytes).unwrap(),
                [255, 0, 0, 255, 0, 255, 0, 255]
            );
        }
        let l = Layout {
            bits: 16,
            pad: 16,
            little_endian: true,
            masks: [0xf800, 0x7e0, 0x1f],
        };
        assert_eq!(l.convert(1, 1, &[0xff, 0xff]).unwrap(), [255; 4]);
        assert!(l.convert(1, 1, &[0]).is_err());
        assert!(l.convert(u16::MAX, u16::MAX, &[]).is_err());
    }
}

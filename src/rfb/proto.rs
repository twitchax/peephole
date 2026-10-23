//! RFB wire constants and the client-to-server messages (RFC 6143 §7.5).

use crate::framebuffer::Rect;

/// Security types (RFC 6143 §7.2.1, plus Apple's).
pub mod security {
    pub const INVALID: u8 = 0;
    pub const NONE: u8 = 1;
    pub const VNC: u8 = 2;
    /// Apple Remote Desktop: Diffie-Hellman, then the macOS username and password under AES.
    pub const ARD: u8 = 30;

    /// A readable name for errors and `screen_info`.
    #[must_use]
    pub fn name(security_type: u8) -> String {
        match security_type {
            NONE => "None (1)".into(),
            VNC => "VNC password (2)".into(),
            ARD => "Apple login (30)".into(),
            other => format!("type {other}"),
        }
    }
}

/// Encodings and pseudo-encodings (RFC 6143 §7.7–7.8).
pub mod encoding {
    pub const RAW: i32 = 0;
    pub const COPY_RECT: i32 = 1;
    pub const ZRLE: i32 = 16;
    pub const DESKTOP_SIZE: i32 = -223;
    pub const LAST_RECT: i32 = -224;
    pub const EXTENDED_DESKTOP_SIZE: i32 = -308;

    /// What we ask for, most preferred first. Apple's server favours ZRLE.
    pub const PREFERRED: &[i32] = &[ZRLE, COPY_RECT, RAW, DESKTOP_SIZE, EXTENDED_DESKTOP_SIZE, LAST_RECT];
}

/// Pointer button-mask bits. Wheel "buttons" are pressed and released once per notch.
pub mod buttons {
    pub const LEFT: u8 = 1;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
    pub const WHEEL_UP: u8 = 1 << 3;
    pub const WHEEL_DOWN: u8 = 1 << 4;
    pub const WHEEL_LEFT: u8 = 1 << 5;
    pub const WHEEL_RIGHT: u8 = 1 << 6;
}

/// An RFB pixel format (RFC 6143 §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// What we ask every server for: 32-bit little-endian with red in the low byte, so a pixel
    /// arrives as `[R, G, B, x]` and a ZRLE compressed pixel as `[R, G, B]`, already in storage
    /// order. libvncclient requests the same on little-endian hosts.
    pub const RGBX: Self = Self {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 0,
        green_shift: 8,
        blue_shift: 16,
    };

    /// Parses the 16-byte wire form.
    #[must_use]
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            bits_per_pixel: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_colour: b[3] != 0,
            red_max: u16::from_be_bytes([b[4], b[5]]),
            green_max: u16::from_be_bytes([b[6], b[7]]),
            blue_max: u16::from_be_bytes([b[8], b[9]]),
            red_shift: b[10],
            green_shift: b[11],
            blue_shift: b[12],
        }
    }

    /// The 16-byte wire form (three trailing padding bytes).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 16] {
        let [rh, rl] = self.red_max.to_be_bytes();
        let [gh, gl] = self.green_max.to_be_bytes();
        let [bh, bl] = self.blue_max.to_be_bytes();
        [
            self.bits_per_pixel,
            self.depth,
            u8::from(self.big_endian),
            u8::from(self.true_colour),
            rh,
            rl,
            gh,
            gl,
            bh,
            bl,
            self.red_shift,
            self.green_shift,
            self.blue_shift,
            0,
            0,
            0,
        ]
    }
}

/// A client-to-server message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMessage {
    SetPixelFormat(PixelFormat),
    SetEncodings(&'static [i32]),
    UpdateRequest { incremental: bool, area: Rect },
    Key { keysym: u32, down: bool },
    Pointer { x: u16, y: u16, buttons: u8 },
}

impl ClientMessage {
    /// Appends the wire form to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Self::SetPixelFormat(format) => {
                out.extend_from_slice(&[0, 0, 0, 0]);
                out.extend_from_slice(&format.to_bytes());
            }
            Self::SetEncodings(encodings) => {
                out.extend_from_slice(&[2, 0]);
                // The list is a short constant; a count past u16 is a programming error, so clamp.
                let count = u16::try_from(encodings.len()).unwrap_or(u16::MAX);
                out.extend_from_slice(&count.to_be_bytes());
                for encoding in encodings.iter().take(usize::from(count)) {
                    out.extend_from_slice(&encoding.to_be_bytes());
                }
            }
            Self::UpdateRequest { incremental, area } => {
                out.extend_from_slice(&[3, u8::from(incremental)]);
                for value in [area.x, area.y, area.width, area.height] {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
            Self::Key { keysym, down } => {
                out.extend_from_slice(&[4, u8::from(down), 0, 0]);
                out.extend_from_slice(&keysym.to_be_bytes());
            }
            Self::Pointer { x, y, buttons } => {
                out.push(5);
                out.push(buttons);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
        }
    }
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use pretty_assertions::assert_eq;

    use super::*;

    fn encoded(message: ClientMessage) -> Vec<u8> {
        let mut out = Vec::new();
        message.encode(&mut out);
        out
    }

    #[test]
    fn pixel_format_round_trips() {
        let bytes = PixelFormat::RGBX.to_bytes();
        assert_eq!(bytes, [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]);
        assert_eq!(PixelFormat::from_bytes(&bytes), PixelFormat::RGBX);
    }

    #[test]
    fn encodes_input_events() {
        assert_eq!(encoded(ClientMessage::Key { keysym: 0xffeb, down: true }), [4, 1, 0, 0, 0, 0, 0xff, 0xeb]);
        assert_eq!(
            encoded(ClientMessage::Pointer {
                x: 0x0102,
                y: 0x0304,
                buttons: buttons::RIGHT
            }),
            [5, 4, 1, 2, 3, 4]
        );
    }

    #[test]
    fn encodes_requests_and_setup() {
        let area = Rect::new(1, 2, 0x0300, 0x0400);
        assert_eq!(encoded(ClientMessage::UpdateRequest { incremental: true, area }), [3, 1, 0, 1, 0, 2, 3, 0, 4, 0]);
        assert_eq!(
            encoded(ClientMessage::SetEncodings(&[encoding::ZRLE, encoding::DESKTOP_SIZE])),
            [2, 0, 0, 2, 0, 0, 0, 16, 0xff, 0xff, 0xff, 0x21]
        );
        assert_eq!(encoded(ClientMessage::SetPixelFormat(PixelFormat::RGBX)).len(), 20);
    }
}

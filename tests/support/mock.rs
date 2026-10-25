//! What the hand-rolled mock servers share: the server side of a 3.8 handshake with no security,
//! a screen that sends Raw updates in the RGBX format peephole asks for, and client messages.

use std::io;

use peephole::{framebuffer::Rect, rfb::PixelFormat};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use super::Input;

/// RGB8 pixels, `width` x `height`.
pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub rgb: Vec<u8>,
}

impl Screen {
    pub fn paint(&mut self, rect: Rect, colour: [u8; 3]) {
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let at = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 3;
                self.rgb[at..at + 3].copy_from_slice(&colour);
            }
        }
    }

    /// A one-rectangle Raw `FramebufferUpdate` of `rect`, in the RGBX format peephole asks for.
    pub fn update(&self, rect: Rect) -> Vec<u8> {
        let mut out = vec![0, 0];
        out.extend(1u16.to_be_bytes());
        for v in [rect.x, rect.y, rect.width, rect.height] {
            out.extend(v.to_be_bytes());
        }
        out.extend(0i32.to_be_bytes());
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let at = (usize::from(y) * usize::from(self.width) + usize::from(x)) * 3;
                out.extend([self.rgb[at], self.rgb[at + 1], self.rgb[at + 2], 0]);
            }
        }
        out
    }
}

/// Greets with `greeting`, offers only None, and describes a `width` x `height` desktop.
pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, greeting: &[u8; 12], width: u16, height: u16) -> io::Result<()> {
    s.write_all(greeting).await?;
    s.read_exact(&mut [0; 12]).await?;
    s.write_all(&[1, 1]).await?; // one security type: None
    s.read_u8().await?;
    s.write_u32(0).await?;
    s.read_u8().await?; // ClientInit
    let name = b"mock";
    let mut init = Vec::new();
    init.extend(width.to_be_bytes());
    init.extend(height.to_be_bytes());
    init.extend(PixelFormat::RGBX.to_bytes());
    init.extend(u32::try_from(name.len()).unwrap().to_be_bytes());
    init.extend(name);
    s.write_all(&init).await
}

/// A client message the mocks act on.
pub enum Message {
    Update {
        incremental: bool,
        area: Rect,
    },
    Input(Input),
    /// `SetPixelFormat` or `SetEncodings`: peephole always asks for RGBX, which `Screen` sends.
    Setup,
}

/// Reads the rest of a client message whose type byte, `kind`, has been read: the mocks read that
/// byte inside `select!`, and the rest outside it, where a cancel cannot cut a message in half.
pub async fn read_message<S: AsyncRead + Unpin>(s: &mut S, kind: u8) -> io::Result<Message> {
    Ok(match kind {
        0 => {
            s.read_exact(&mut [0; 19]).await?;
            Message::Setup
        }
        2 => {
            s.read_u8().await?;
            let count = s.read_u16().await?;
            s.read_exact(&mut vec![0; 4 * usize::from(count)]).await?;
            Message::Setup
        }
        3 => Message::Update {
            incremental: s.read_u8().await? != 0,
            area: Rect::new(s.read_u16().await?, s.read_u16().await?, s.read_u16().await?, s.read_u16().await?),
        },
        4 => {
            let down = s.read_u8().await? != 0;
            s.read_u16().await?;
            Message::Input(Input::Key(s.read_u32().await?, down))
        }
        5 => {
            let buttons = s.read_u8().await?;
            Message::Input(Input::Pointer(s.read_u16().await?, s.read_u16().await?, buttons))
        }
        other => return Err(io::Error::other(format!("unexpected client message {other}"))),
    })
}

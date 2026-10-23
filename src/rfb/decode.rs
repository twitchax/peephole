//! Server-to-client messages (RFC 6143 §7.6) and the rectangle encodings we ask for: Raw,
//! `CopyRect` and ZRLE, plus the `DesktopSize`, `ExtendedDesktopSize` and `LastRect`
//! pseudo-encodings.
//!
//! Reading and applying are separate steps. [`MessageReader::read`] pulls a whole
//! `FramebufferUpdate` off the socket (inflating ZRLE as it goes) into reusable buffers without
//! touching the framebuffer; [`MessageReader::apply`] then paints it in one pass. The framebuffer
//! lock is held only for the paint, never across network reads, and a screenshot never sees half
//! an update.

use anyhow::{Context as _, bail, ensure};
use flate2::{Decompress, FlushDecompress};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use super::proto::encoding;
use crate::base::{Res, Void};
use crate::framebuffer::{BYTES_PER_PIXEL, Framebuffer, Rect};

/// Bytes per pixel on the wire in our pixel format ([`super::proto::PixelFormat::RGBX`]).
const WIRE_PIXEL: usize = 4;
/// Bytes per ZRLE compressed pixel in our pixel format: `[R, G, B]`, the same as storage.
const CPIXEL: usize = 3;
/// ZRLE tiles are 64x64.
const TILE: u16 = 64;
/// The most compressed data we accept for one ZRLE rectangle.
const MAX_ZRLE_LEN: u32 = 256 * 1024 * 1024;
/// The longest clipboard text we read (and discard).
const MAX_CUT_TEXT: u32 = 16 * 1024 * 1024;

/// Which message the server sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerMessage {
    /// A `FramebufferUpdate`, ready for [`MessageReader::apply`].
    FramebufferUpdate,
    SetColourMapEntries,
    Bell,
    ServerCutText,
}

/// What applying an update changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Every rectangle that was painted.
    pub damage: Vec<Rect>,
    /// The desktop changed size; ask for a full, non-incremental update.
    pub resized: bool,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    /// RGB8 pixels at `arena[start..]`, already converted from the wire.
    Pixels {
        rect: Rect,
        start: usize,
    },
    /// Inflated ZRLE tile data at `arena[start..end]`.
    Zrle {
        rect: Rect,
        start: usize,
        end: usize,
    },
    Copy {
        rect: Rect,
        src_x: u16,
        src_y: u16,
    },
    Resize {
        width: u16,
        height: u16,
    },
}

/// Reads server messages, keeping the buffers and the ZRLE zlib stream (one per connection) alive
/// between updates so steady-state updates allocate nothing.
pub struct MessageReader {
    zlib: Decompress,
    wire: Vec<u8>,
    arena: Vec<u8>,
    ops: Vec<Op>,
}

impl Default for MessageReader {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageReader {
    #[must_use]
    pub fn new() -> Self {
        Self {
            zlib: Decompress::new(true),
            wire: Vec::new(),
            arena: Vec::new(),
            ops: Vec::new(),
        }
    }

    /// Reads one server message. `size` is the framebuffer's current width and height, used to
    /// bound every rectangle before anything is allocated for it.
    ///
    /// # Errors
    ///
    /// On I/O failure, an unknown message or encoding, or data that does not fit the framebuffer.
    pub async fn read<R: AsyncRead + Unpin>(&mut self, r: &mut R, size: (u16, u16)) -> Res<ServerMessage> {
        let kind = r.read_u8().await.context("reading a server message")?;
        match kind {
            0 => {
                self.read_update(r, size).await?;
                Ok(ServerMessage::FramebufferUpdate)
            }
            1 => {
                let mut header = [0u8; 5];
                r.read_exact(&mut header).await?;
                let count = u16::from_be_bytes([header[3], header[4]]);
                discard(r, u64::from(count) * 6).await?;
                Ok(ServerMessage::SetColourMapEntries)
            }
            2 => Ok(ServerMessage::Bell),
            3 => {
                let mut padding = [0u8; 3];
                r.read_exact(&mut padding).await?;
                let len = r.read_u32().await?;
                ensure!(len <= MAX_CUT_TEXT, "the server sent {len} bytes of clipboard text");
                discard(r, u64::from(len)).await?;
                Ok(ServerMessage::ServerCutText)
            }
            other => bail!("unsupported server message type {other}"),
        }
    }

    async fn read_update<R: AsyncRead + Unpin>(&mut self, r: &mut R, (mut width, mut height): (u16, u16)) -> Void {
        self.arena.clear();
        self.ops.clear();
        let _padding = r.read_u8().await?;
        let count = r.read_u16().await?;
        // With LastRect, a count of 0xffff means "until the LastRect rectangle".
        for _ in 0..count {
            let mut header = [0u8; 12];
            r.read_exact(&mut header).await.context("reading a rectangle header")?;
            let [x0, x1, y0, y1, w0, w1, h0, h1, e0, e1, e2, e3] = header;
            let rect = Rect::new(u16::from_be_bytes([x0, x1]), u16::from_be_bytes([y0, y1]), u16::from_be_bytes([w0, w1]), u16::from_be_bytes([h0, h1]));
            let encoding = i32::from_be_bytes([e0, e1, e2, e3]);

            match encoding {
                encoding::LAST_RECT => break,
                encoding::DESKTOP_SIZE => {
                    (width, height) = (rect.width, rect.height);
                    self.ops.push(Op::Resize { width, height });
                    continue;
                }
                encoding::EXTENDED_DESKTOP_SIZE => {
                    let screens = r.read_u8().await?;
                    discard(r, 3 + u64::from(screens) * 16).await?;
                    // y carries the status of a resize we asked for; we never ask, so only a
                    // server-initiated change (status 0) means anything.
                    if rect.y == 0 {
                        (width, height) = (rect.width, rect.height);
                        self.ops.push(Op::Resize { width, height });
                    }
                    continue;
                }
                _ => {}
            }

            ensure!(
                rect.right() <= u32::from(width) && rect.bottom() <= u32::from(height),
                "the server sent a {}x{} rectangle at ({}, {}) outside the {width}x{height} desktop",
                rect.width,
                rect.height,
                rect.x,
                rect.y
            );
            match encoding {
                encoding::RAW => self.read_raw(r, rect).await?,
                encoding::COPY_RECT => {
                    let src_x = r.read_u16().await?;
                    let src_y = r.read_u16().await?;
                    self.ops.push(Op::Copy { rect, src_x, src_y });
                }
                encoding::ZRLE => self.read_zrle(r, rect).await?,
                other => bail!("the server used encoding {other}, which we did not ask for"),
            }
        }
        Ok(())
    }

    async fn read_raw<R: AsyncRead + Unpin>(&mut self, r: &mut R, rect: Rect) -> Void {
        let start = self.arena.len();
        self.arena.reserve(rect.area() * BYTES_PER_PIXEL);
        // Row by row, so the wire buffer stays one row long even for a full-screen rectangle.
        self.wire.resize(usize::from(rect.width) * WIRE_PIXEL, 0);
        for _ in 0..rect.height {
            r.read_exact(&mut self.wire).await.context("reading Raw pixels")?;
            for pixel in self.wire.as_chunks::<WIRE_PIXEL>().0 {
                self.arena.extend_from_slice(&pixel[..BYTES_PER_PIXEL]);
            }
        }
        self.ops.push(Op::Pixels { rect, start });
        Ok(())
    }

    async fn read_zrle<R: AsyncRead + Unpin>(&mut self, r: &mut R, rect: Rect) -> Void {
        let len = r.read_u32().await?;
        ensure!(len <= MAX_ZRLE_LEN, "the server sent a {len}-byte ZRLE rectangle");
        self.wire.resize(len as usize, 0);
        r.read_exact(&mut self.wire).await.context("reading ZRLE data")?;

        // The largest valid tile stream: every tile raw, plus its subencoding byte.
        let tiles = usize::from(rect.width.div_ceil(TILE)) * usize::from(rect.height.div_ceil(TILE));
        let limit = rect.area() * CPIXEL + tiles;
        let start = self.arena.len();
        let mut input = &self.wire[..];
        loop {
            if self.arena.len() == self.arena.capacity() {
                self.arena.reserve((limit + 64).min(1024 * 1024));
            }
            let (in_before, out_before) = (self.zlib.total_in(), self.arena.len());
            self.zlib.decompress_vec(input, &mut self.arena, FlushDecompress::Sync).context("inflating ZRLE data")?;
            let consumed = usize::try_from(self.zlib.total_in() - in_before)?;
            input = &input[consumed..];
            ensure!(self.arena.len() - start <= limit, "ZRLE data inflated past its rectangle");
            // Done once every input byte is in and the inflater stopped short of a full buffer.
            if input.is_empty() && self.arena.len() < self.arena.capacity() {
                break;
            }
            ensure!(consumed > 0 || self.arena.len() > out_before, "ZRLE data stalled the inflater");
        }
        self.ops.push(Op::Zrle { rect, start, end: self.arena.len() });
        Ok(())
    }

    /// Paints the last `FramebufferUpdate` onto `fb`.
    ///
    /// # Errors
    ///
    /// When a rectangle falls outside `fb` or ZRLE tile data is malformed. Rectangles before the
    /// bad one stay painted; the session drops the connection anyway.
    pub fn apply(&self, fb: &mut Framebuffer) -> Res<Applied> {
        let mut applied = Applied::default();
        for op in &self.ops {
            match *op {
                Op::Pixels { rect, start } => {
                    fb.check(&rect)?;
                    let row_len = usize::from(rect.width) * BYTES_PER_PIXEL;
                    for (row, pixels) in (rect.y..rect.y + rect.height).zip(self.arena[start..].chunks_exact(row_len)) {
                        fb.span_mut(rect.x, row, rect.width).copy_from_slice(pixels);
                    }
                    applied.damage.push(rect);
                }
                Op::Zrle { rect, start, end } => {
                    apply_zrle(fb, rect, &self.arena[start..end])?;
                    applied.damage.push(rect);
                }
                Op::Copy { rect, src_x, src_y } => {
                    fb.copy_rect(src_x, src_y, &rect)?;
                    applied.damage.push(rect);
                }
                Op::Resize { width, height } => {
                    *fb = Framebuffer::new(width, height);
                    applied.damage.push(fb.bounds());
                    applied.resized = true;
                }
            }
        }
        Ok(applied)
    }
}

async fn discard<R: AsyncRead + Unpin>(r: &mut R, len: u64) -> Void {
    let skipped = tokio::io::copy(&mut r.take(len), &mut tokio::io::sink()).await?;
    ensure!(skipped == len, "the connection closed mid-message");
    Ok(())
}

/// A cursor over inflated ZRLE data.
struct Bytes<'a> {
    data: &'a [u8],
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        ensure!(n <= self.data.len(), "ZRLE tile data ended early");
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Res<u8> {
        Ok(self.take(1)?[0])
    }

    fn cpixel(&mut self) -> Res<[u8; 3]> {
        let p = self.take(CPIXEL)?;
        Ok([p[0], p[1], p[2]])
    }

    fn palette(&mut self, size: usize) -> Res<Vec<[u8; 3]>> {
        (0..size).map(|_| self.cpixel()).collect()
    }

    /// A ZRLE run length: bytes summed until one is not 255, plus one.
    fn run_length(&mut self) -> Res<usize> {
        let mut len = 1;
        loop {
            let b = self.u8()?;
            len += usize::from(b);
            if b != 255 {
                return Ok(len);
            }
        }
    }
}

/// Writes runs of pixels into a tile in raster order.
struct TileWriter {
    tile: Rect,
    next: usize,
}

impl TileWriter {
    fn run(&mut self, fb: &mut Framebuffer, rgb: [u8; 3], len: usize) -> Void {
        ensure!(len <= self.tile.area() - self.next, "a ZRLE run overflows its tile");
        let width = usize::from(self.tile.width);
        let mut left = len;
        while left > 0 {
            let (row, col) = (self.next / width, self.next % width);
            let span = left.min(width - col);
            // Both fit in u16: they are bounded by the tile's width and height.
            let (x, y) = (self.tile.x + u16::try_from(col)?, self.tile.y + u16::try_from(row)?);
            for pixel in fb.span_mut(x, y, u16::try_from(span)?).as_chunks_mut::<BYTES_PER_PIXEL>().0 {
                *pixel = rgb;
            }
            self.next += span;
            left -= span;
        }
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.next == self.tile.area()
    }
}

fn apply_zrle(fb: &mut Framebuffer, rect: Rect, data: &[u8]) -> Void {
    fb.check(&rect)?;
    let mut bytes = Bytes { data };
    for ty in (0..rect.height).step_by(usize::from(TILE)) {
        for tx in (0..rect.width).step_by(usize::from(TILE)) {
            let tile = Rect::new(rect.x + tx, rect.y + ty, TILE.min(rect.width - tx), TILE.min(rect.height - ty));
            apply_tile(fb, tile, &mut bytes)?;
        }
    }
    ensure!(bytes.data.is_empty(), "ZRLE rectangle has {} bytes left over", bytes.data.len());
    Ok(())
}

fn apply_tile(fb: &mut Framebuffer, tile: Rect, bytes: &mut Bytes<'_>) -> Void {
    let subencoding = bytes.u8()?;
    match subencoding {
        // Raw CPIXELs, already in storage order.
        0 => {
            for row in tile.y..tile.y + tile.height {
                let src = bytes.take(usize::from(tile.width) * CPIXEL)?;
                fb.span_mut(tile.x, row, tile.width).copy_from_slice(src);
            }
        }
        // One colour.
        1 => fb.fill(&tile, bytes.cpixel()?)?,
        // Packed palette: 1, 2 or 4 bits per index, each row padded to a whole byte.
        2..=16 => {
            let palette = bytes.palette(usize::from(subencoding))?;
            let bits: u8 = match subencoding {
                2 => 1,
                3 | 4 => 2,
                _ => 4,
            };
            let row_bytes = (usize::from(tile.width) * usize::from(bits)).div_ceil(8);
            let mask = (1u8 << bits) - 1;
            for row in tile.y..tile.y + tile.height {
                let packed = bytes.take(row_bytes)?;
                let span = fb.span_mut(tile.x, row, tile.width);
                for (i, dst) in span.as_chunks_mut::<BYTES_PER_PIXEL>().0.iter_mut().enumerate() {
                    let bit = i * usize::from(bits);
                    // `bit % 8` is below 8, so the shift amount fits a u8.
                    let shift = 8 - u8::try_from(bit % 8)? - bits;
                    let index = (packed[bit / 8] >> shift) & mask;
                    let rgb = palette.get(usize::from(index)).context("ZRLE palette index out of range")?;
                    *dst = *rgb;
                }
            }
        }
        // Plain RLE: (CPIXEL, run length) pairs.
        128 => {
            let mut writer = TileWriter { tile, next: 0 };
            while !writer.is_full() {
                let rgb = bytes.cpixel()?;
                let len = bytes.run_length()?;
                writer.run(fb, rgb, len)?;
            }
        }
        // Palette RLE: an index with the top bit set is followed by a run length.
        130..=255 => {
            let palette = bytes.palette(usize::from(subencoding - 128))?;
            let mut writer = TileWriter { tile, next: 0 };
            while !writer.is_full() {
                let index = bytes.u8()?;
                let len = if index & 0x80 == 0 { 1 } else { bytes.run_length()? };
                let rgb = palette.get(usize::from(index & 0x7f)).context("ZRLE palette index out of range")?;
                writer.run(fb, *rgb, len)?;
            }
        }
        other => bail!("invalid ZRLE subencoding {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::Write as _;

    use flate2::{Compression, write::ZlibEncoder};
    use pretty_assertions::assert_eq;

    use super::*;

    /// A `FramebufferUpdate` message holding `rects` (header fields, then payload).
    fn update(rects: &[(Rect, i32, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0, 0];
        out.extend_from_slice(&u16::try_from(rects.len()).unwrap().to_be_bytes());
        for (rect, encoding, payload) in rects {
            for v in [rect.x, rect.y, rect.width, rect.height] {
                out.extend_from_slice(&v.to_be_bytes());
            }
            out.extend_from_slice(&encoding.to_be_bytes());
            out.extend_from_slice(payload);
        }
        out
    }

    /// ZRLE payload: the length prefix, then `tiles` deflated with a sync flush, as servers send.
    fn zrle(encoder: &mut ZlibEncoder<Vec<u8>>, tiles: &[u8]) -> Vec<u8> {
        encoder.write_all(tiles).unwrap();
        encoder.flush().unwrap();
        let data = std::mem::take(encoder.get_mut());
        let mut out = u32::try_from(data.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(&data);
        out
    }

    async fn read_and_apply(reader: &mut MessageReader, fb: &mut Framebuffer, bytes: &[u8]) -> Res<Applied> {
        let mut input = bytes;
        assert_eq!(reader.read(&mut input, (fb.width(), fb.height())).await?, ServerMessage::FramebufferUpdate);
        assert!(input.is_empty(), "{} bytes unread", input.len());
        reader.apply(fb)
    }

    fn pixel(fb: &Framebuffer, x: usize, y: usize) -> [u8; 3] {
        let i = (y * usize::from(fb.width()) + x) * 3;
        fb.pixels()[i..i + 3].try_into().unwrap()
    }

    #[tokio::test]
    async fn raw_drops_the_padding_byte() {
        let mut fb = Framebuffer::new(3, 2);
        let payload = [[1, 2, 3, 0], [4, 5, 6, 0]].concat();
        let applied = read_and_apply(&mut MessageReader::new(), &mut fb, &update(&[(Rect::new(1, 1, 2, 1), encoding::RAW, payload)]))
            .await
            .unwrap();
        assert_eq!(pixel(&fb, 1, 1), [1, 2, 3]);
        assert_eq!(pixel(&fb, 2, 1), [4, 5, 6]);
        assert_eq!(pixel(&fb, 0, 1), [0, 0, 0]);
        assert_eq!(applied.damage, [Rect::new(1, 1, 2, 1)]);
    }

    #[tokio::test]
    async fn copy_rect_moves_pixels() {
        let mut fb = Framebuffer::new(4, 1);
        fb.fill(&Rect::new(0, 0, 1, 1), [9, 9, 9]).unwrap();
        read_and_apply(&mut MessageReader::new(), &mut fb, &update(&[(Rect::new(3, 0, 1, 1), encoding::COPY_RECT, vec![0, 0, 0, 0])]))
            .await
            .unwrap();
        assert_eq!(pixel(&fb, 3, 0), [9, 9, 9]);
    }

    #[tokio::test]
    async fn desktop_size_resizes_and_bounds_later_rectangles() {
        let mut fb = Framebuffer::new(2, 2);
        let bytes = update(&[(Rect::new(0, 0, 8, 4), encoding::DESKTOP_SIZE, vec![]), (Rect::new(7, 3, 1, 1), encoding::RAW, vec![1, 2, 3, 0])]);
        let applied = read_and_apply(&mut MessageReader::new(), &mut fb, &bytes).await.unwrap();
        assert!(applied.resized);
        assert_eq!((fb.width(), fb.height()), (8, 4));
        assert_eq!(pixel(&fb, 7, 3), [1, 2, 3]);
    }

    #[tokio::test]
    async fn extended_desktop_size_resizes_on_a_server_change() {
        let mut fb = Framebuffer::new(2, 2);
        let screens = [vec![1, 0, 0, 0], vec![0; 16]].concat();
        let applied = read_and_apply(&mut MessageReader::new(), &mut fb, &update(&[(Rect::new(0, 0, 5, 6), encoding::EXTENDED_DESKTOP_SIZE, screens)]))
            .await
            .unwrap();
        assert!(applied.resized);
        assert_eq!((fb.width(), fb.height()), (5, 6));
    }

    #[tokio::test]
    async fn last_rect_ends_an_open_ended_update() {
        let mut fb = Framebuffer::new(1, 1);
        let mut bytes = update(&[(Rect::new(0, 0, 1, 1), encoding::RAW, vec![7, 7, 7, 0]), (Rect::default(), encoding::LAST_RECT, vec![])]);
        bytes[2..4].copy_from_slice(&[0xff, 0xff]);
        read_and_apply(&mut MessageReader::new(), &mut fb, &bytes).await.unwrap();
        assert_eq!(pixel(&fb, 0, 0), [7, 7, 7]);
    }

    #[tokio::test]
    async fn rejects_rectangles_outside_the_desktop() {
        let mut fb = Framebuffer::new(2, 2);
        let error = read_and_apply(&mut MessageReader::new(), &mut fb, &update(&[(Rect::new(1, 1, 2, 2), encoding::RAW, vec![0; 16])]))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outside"), "{error:#}");
    }

    #[tokio::test]
    async fn zrle_decodes_every_subencoding() {
        // One 70x2 rectangle is two tiles: 64x2 then 6x2. The first tile is solid; the second
        // walks through the other subencodings across successive updates on one zlib stream.
        let (red, green, blue) = ([255, 0, 0], [0, 255, 0], [0, 0, 255]);
        let mut fb = Framebuffer::new(70, 2);
        let mut reader = MessageReader::new();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let rect = Rect::new(0, 0, 70, 2);
        let solid_first = [&[1][..], &red].concat();

        // Raw tile.
        let raw: Vec<u8> = (0..12).flat_map(|i| [i, i, i]).collect();
        let tiles = [solid_first.clone(), vec![0], raw].concat();
        read_and_apply(&mut reader, &mut fb, &update(&[(rect, encoding::ZRLE, zrle(&mut encoder, &tiles))])).await.unwrap();
        assert_eq!(pixel(&fb, 0, 0), red);
        assert_eq!(pixel(&fb, 63, 1), red);
        assert_eq!(pixel(&fb, 64, 0), [0, 0, 0]);
        assert_eq!(pixel(&fb, 69, 1), [11, 11, 11]);

        // Packed palette of 2 (1 bit per pixel): rows 0b101010.. and 0b010101..
        let tiles = [solid_first.clone(), vec![2], green.to_vec(), blue.to_vec(), vec![0b1010_1000, 0b0101_0100]].concat();
        read_and_apply(&mut reader, &mut fb, &update(&[(rect, encoding::ZRLE, zrle(&mut encoder, &tiles))])).await.unwrap();
        assert_eq!([pixel(&fb, 64, 0), pixel(&fb, 65, 0), pixel(&fb, 69, 0)], [blue, green, green]);
        assert_eq!([pixel(&fb, 64, 1), pixel(&fb, 65, 1), pixel(&fb, 69, 1)], [green, blue, blue]);

        // Packed palette of 5 (4 bits per pixel).
        let palette5: Vec<u8> = [red, green, blue, [1, 1, 1], [2, 2, 2]].iter().flat_map(|c| *c).collect();
        let tiles = [solid_first.clone(), vec![5], palette5, vec![0x01, 0x23, 0x40, 0x43, 0x21, 0x00]].concat();
        read_and_apply(&mut reader, &mut fb, &update(&[(rect, encoding::ZRLE, zrle(&mut encoder, &tiles))])).await.unwrap();
        assert_eq!([pixel(&fb, 64, 0), pixel(&fb, 67, 0), pixel(&fb, 68, 0), pixel(&fb, 64, 1)], [red, [1, 1, 1], [2, 2, 2], [2, 2, 2]]);

        // Plain RLE: 5 green, then 7 blue (a run that wraps onto the second row).
        let tiles = [solid_first.clone(), vec![128], green.to_vec(), vec![4], blue.to_vec(), vec![6]].concat();
        read_and_apply(&mut reader, &mut fb, &update(&[(rect, encoding::ZRLE, zrle(&mut encoder, &tiles))])).await.unwrap();
        assert_eq!([pixel(&fb, 68, 0), pixel(&fb, 69, 0), pixel(&fb, 64, 1)], [green, blue, blue]);

        // Palette RLE: one red, then a run of 11 green.
        let tiles = [solid_first, vec![130], red.to_vec(), green.to_vec(), vec![0x00, 0x81, 10]].concat();
        read_and_apply(&mut reader, &mut fb, &update(&[(rect, encoding::ZRLE, zrle(&mut encoder, &tiles))])).await.unwrap();
        assert_eq!([pixel(&fb, 64, 0), pixel(&fb, 65, 0), pixel(&fb, 69, 1)], [red, green, green]);
    }

    #[tokio::test]
    async fn zrle_long_runs_sum_their_length_bytes() {
        // A 64x8 tile (512 pixels) as one plain-RLE run: 1 + 255 + 255 + 1 = 512.
        let mut fb = Framebuffer::new(64, 8);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let tiles = [vec![128], [4, 5, 6].to_vec(), vec![255, 255, 1]].concat();
        let bytes = update(&[(fb.bounds(), encoding::ZRLE, zrle(&mut encoder, &tiles))]);
        read_and_apply(&mut MessageReader::new(), &mut fb, &bytes).await.unwrap();
        assert!(fb.pixels().as_chunks::<3>().0.iter().all(|p| *p == [4, 5, 6]));
    }

    #[tokio::test]
    async fn zrle_rejects_a_run_that_overflows_its_tile() {
        let mut fb = Framebuffer::new(2, 2);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        let tiles = [vec![128], [1, 1, 1].to_vec(), vec![4]].concat();
        let bytes = update(&[(fb.bounds(), encoding::ZRLE, zrle(&mut encoder, &tiles))]);
        assert!(read_and_apply(&mut MessageReader::new(), &mut fb, &bytes).await.is_err());
    }

    #[tokio::test]
    async fn skips_bell_colour_map_and_clipboard() {
        let mut reader = MessageReader::new();
        let bytes = [vec![2], vec![1, 0, 0, 0, 0, 1], vec![0; 6], vec![3, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']].concat();
        let mut input = &bytes[..];
        assert_eq!(reader.read(&mut input, (1, 1)).await.unwrap(), ServerMessage::Bell);
        assert_eq!(reader.read(&mut input, (1, 1)).await.unwrap(), ServerMessage::SetColourMapEntries);
        assert_eq!(reader.read(&mut input, (1, 1)).await.unwrap(), ServerMessage::ServerCutText);
        assert_eq!(input, b"");
    }
}

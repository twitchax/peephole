//! The local copy of the remote screen. Pixels are tightly packed RGB8 rows, the layout the resizer
//! and the PNG encoder take directly, so a screenshot never converts pixel formats.

use anyhow::ensure;

use crate::base::Void;

/// Bytes per stored pixel (R, G, B).
pub const BYTES_PER_PIXEL: usize = 3;

/// A rectangle in framebuffer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self { x, y, width, height }
    }

    /// One past the right edge (wider than `u16`, so it cannot overflow).
    #[must_use]
    pub fn right(&self) -> u32 {
        u32::from(self.x) + u32::from(self.width)
    }

    /// One past the bottom edge.
    #[must_use]
    pub fn bottom(&self) -> u32 {
        u32::from(self.y) + u32::from(self.height)
    }

    #[must_use]
    pub fn area(&self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }

    #[must_use]
    pub fn intersects(&self, other: &Rect) -> bool {
        u32::from(self.x) < other.right() && u32::from(other.x) < self.right() && u32::from(self.y) < other.bottom() && u32::from(other.y) < self.bottom()
    }
}

/// The remote screen as RGB8.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Framebuffer {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

impl Framebuffer {
    /// A black framebuffer.
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; usize::from(width) * usize::from(height) * BYTES_PER_PIXEL],
        }
    }

    #[must_use]
    pub fn width(&self) -> u16 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u16 {
        self.height
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Every pixel, row by row, as RGB8.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Fails unless `rect` lies inside the framebuffer. Every write path checks first, so a buggy
    /// or hostile server gets an error rather than a panic.
    ///
    /// # Errors
    ///
    /// When `rect` reaches past the right or bottom edge.
    pub fn check(&self, rect: &Rect) -> Void {
        ensure!(
            rect.right() <= u32::from(self.width) && rect.bottom() <= u32::from(self.height),
            "rectangle {}x{} at ({}, {}) lies outside the {}x{} framebuffer",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            self.width,
            self.height
        );
        Ok(())
    }

    fn offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.width) + usize::from(x)) * BYTES_PER_PIXEL
    }

    /// `len` pixels of row `y` starting at column `x`. The caller has checked the span.
    pub(crate) fn span_mut(&mut self, x: u16, y: u16, len: u16) -> &mut [u8] {
        let start = self.offset(x, y);
        &mut self.pixels[start..start + usize::from(len) * BYTES_PER_PIXEL]
    }

    /// Paints `rect` one colour.
    ///
    /// # Errors
    ///
    /// When `rect` is out of bounds.
    pub fn fill(&mut self, rect: &Rect, rgb: [u8; 3]) -> Void {
        self.check(rect)?;
        for row in rect.y..rect.y + rect.height {
            for pixel in self.span_mut(rect.x, row, rect.width).as_chunks_mut::<BYTES_PER_PIXEL>().0 {
                *pixel = rgb;
            }
        }
        Ok(())
    }

    /// Copies the `dst`-sized block at (`src_x`, `src_y`) onto `dst` (RFB `CopyRect`). Source and
    /// destination may overlap, as when a window scrolls.
    ///
    /// # Errors
    ///
    /// When either rectangle is out of bounds.
    pub fn copy_rect(&mut self, src_x: u16, src_y: u16, dst: &Rect) -> Void {
        let src = Rect::new(src_x, src_y, dst.width, dst.height);
        self.check(&src)?;
        self.check(dst)?;
        let row_len = usize::from(dst.width) * BYTES_PER_PIXEL;
        let mut copy_row = |row: u16| {
            let from = self.offset(src_x, src_y + row);
            let to = self.offset(dst.x, dst.y + row);
            self.pixels.copy_within(from..from + row_len, to);
        };
        // Moving content down, copy bottom-up so no source row is overwritten before it is read.
        if src_y < dst.y {
            (0..dst.height).rev().for_each(&mut copy_row);
        } else {
            (0..dst.height).for_each(&mut copy_row);
        }
        Ok(())
    }

    /// The pixels of `rect`, row by row.
    ///
    /// # Errors
    ///
    /// When `rect` is out of bounds.
    pub fn crop(&self, rect: &Rect) -> crate::base::Res<Vec<u8>> {
        self.check(rect)?;
        let row_len = usize::from(rect.width) * BYTES_PER_PIXEL;
        let mut out = Vec::with_capacity(rect.area() * BYTES_PER_PIXEL);
        for row in rect.y..rect.y + rect.height {
            let start = self.offset(rect.x, row);
            out.extend_from_slice(&self.pixels[start..start + row_len]);
        }
        Ok(out)
    }

    /// Whether `rect` still holds exactly `pixels` (as returned by [`Framebuffer::crop`]). A
    /// rectangle that no longer fits, after a resize, has changed, and so has one compared with a
    /// baseline of the wrong size.
    #[must_use]
    pub fn matches(&self, rect: &Rect, pixels: &[u8]) -> bool {
        if self.check(rect).is_err() || pixels.len() != rect.area() * BYTES_PER_PIXEL {
            return false;
        }
        let row_len = usize::from(rect.width) * BYTES_PER_PIXEL;
        (rect.y..rect.y + rect.height).zip(pixels.chunks_exact(row_len)).all(|(row, expected)| {
            let start = self.offset(rect.x, row);
            &self.pixels[start..start + row_len] == expected
        })
    }
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use pretty_assertions::assert_eq;

    use super::*;

    /// A 4x3 framebuffer whose red channel numbers the pixels 0..12.
    fn numbered() -> Framebuffer {
        let mut fb = Framebuffer::new(4, 3);
        for (i, pixel) in fb.pixels.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            pixel[0] = u8::try_from(i).unwrap();
        }
        fb
    }

    fn reds(fb: &Framebuffer) -> Vec<u8> {
        fb.pixels().as_chunks::<3>().0.iter().map(|p| p[0]).collect()
    }

    #[test]
    fn fills_a_rectangle() {
        let mut fb = Framebuffer::new(3, 2);
        fb.fill(&Rect::new(1, 0, 2, 2), [9, 8, 7]).unwrap();
        assert_eq!(fb.pixels(), &[0, 0, 0, 9, 8, 7, 9, 8, 7, 0, 0, 0, 9, 8, 7, 9, 8, 7]);
    }

    #[test]
    fn rejects_out_of_bounds_writes() {
        let mut fb = Framebuffer::new(3, 2);
        assert!(fb.fill(&Rect::new(2, 0, 2, 1), [1, 1, 1]).is_err());
        assert!(fb.copy_rect(0, 1, &Rect::new(0, 0, 1, 2)).is_err());
        assert!(fb.crop(&Rect::new(0, 0, 3, 3)).is_err());
    }

    #[test]
    fn copies_overlapping_rows_downward() {
        let mut fb = numbered();
        fb.copy_rect(0, 0, &Rect::new(0, 1, 4, 2)).unwrap();
        assert_eq!(reds(&fb), [0, 1, 2, 3, 0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn copies_overlapping_rows_upward_and_sideways() {
        let mut fb = numbered();
        fb.copy_rect(1, 1, &Rect::new(0, 0, 3, 2)).unwrap();
        assert_eq!(reds(&fb), [5, 6, 7, 3, 9, 10, 11, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn crop_and_match_round_trip() {
        let mut fb = numbered();
        let rect = Rect::new(1, 1, 2, 2);
        let baseline = fb.crop(&rect).unwrap();
        assert_eq!(baseline.as_chunks::<3>().0.iter().map(|p| p[0]).collect::<Vec<_>>(), [5, 6, 9, 10]);
        assert!(fb.matches(&rect, &baseline));
        fb.fill(&Rect::new(0, 0, 1, 1), [255, 0, 0]).unwrap();
        assert!(fb.matches(&rect, &baseline), "a change outside the rectangle is not a change");
        fb.fill(&Rect::new(2, 2, 1, 1), [255, 0, 0]).unwrap();
        assert!(!fb.matches(&rect, &baseline));
        assert!(!Framebuffer::new(1, 1).matches(&rect, &baseline), "a rectangle that no longer fits has changed");
    }

    #[test]
    fn a_baseline_of_the_wrong_size_never_matches() {
        let fb = numbered();
        let rect = Rect::new(1, 1, 2, 2);
        let baseline = fb.crop(&rect).unwrap();
        assert!(!fb.matches(&rect, &[]));
        assert!(!fb.matches(&rect, &baseline[..6]));
    }

    #[test]
    fn rectangles_intersect_only_when_they_overlap() {
        let a = Rect::new(0, 0, 10, 10);
        assert!(a.intersects(&Rect::new(9, 9, 5, 5)));
        assert!(!a.intersects(&Rect::new(10, 0, 5, 5)));
        assert!(!a.intersects(&Rect::new(0, 10, 5, 5)));
    }
}

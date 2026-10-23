//! Screen space and screenshots.
//!
//! Agents never see framebuffer pixels. They work in *screen space*: the whole framebuffer scaled
//! so its long edge is at most `max_edge` (1,568 px by default, about where Claude's vision stops
//! gaining detail; a 3456x2234 Retina frame is far too big to send raw). A full screenshot at the
//! default size is exactly screen space, so its pixel coordinates can be clicked as they are. A
//! region screenshot zooms into part of screen space without changing it, the way Claude's
//! computer-use zoom works.

use anyhow::{Context as _, ensure};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image, images::ImageRef};

use crate::base::Res;
use crate::framebuffer::{Framebuffer, Rect};

/// The default long edge of screen space, in pixels.
pub const DEFAULT_MAX_EDGE: u32 = 1568;
/// A channel at or below this counts as black when deciding whether the screen is dark.
const BLACK_LEVEL: u8 = 8;

/// A rectangle in screen space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The coordinate system agents work in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenSpace {
    pub width: u32,
    pub height: u32,
    /// Framebuffer pixels per screen pixel; at least 1, since screen space never scales up.
    pub scale: f64,
    fb_width: u16,
    fb_height: u16,
}

/// Rounds a non-negative, in-range float to `u32`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // callers pass values in 0..=u16::MAX * 2
fn to_u32(value: f64) -> u32 {
    value.round().max(0.0) as u32
}

impl ScreenSpace {
    /// Screen space for a `fb_width` x `fb_height` framebuffer with a long edge of at most `max_edge`.
    #[must_use]
    pub fn new(fb_width: u16, fb_height: u16, max_edge: u32) -> Self {
        let long = u32::from(fb_width.max(fb_height));
        let scale = if long > max_edge && max_edge > 0 { f64::from(long) / f64::from(max_edge) } else { 1.0 };
        Self {
            width: to_u32(f64::from(fb_width) / scale).max(1),
            height: to_u32(f64::from(fb_height) / scale).max(1),
            scale,
            fb_width,
            fb_height,
        }
    }

    /// The framebuffer pixel under the centre of screen pixel (`x`, `y`).
    ///
    /// # Errors
    ///
    /// When the point lies outside screen space.
    pub fn to_framebuffer(&self, x: u32, y: u32) -> Res<(u16, u16)> {
        ensure!(x < self.width && y < self.height, "({x}, {y}) is outside the {}x{} screen", self.width, self.height);
        let map = |v: u32, limit: u16| -> Res<u16> {
            let mapped = to_u32(((f64::from(v) + 0.5) * self.scale).floor()).min(u32::from(limit.saturating_sub(1)));
            Ok(u16::try_from(mapped)?)
        };
        Ok((map(x, self.fb_width)?, map(y, self.fb_height)?))
    }

    /// The framebuffer pixels that `region` covers, rounded outward.
    ///
    /// # Errors
    ///
    /// When the region is empty or reaches outside screen space.
    pub fn to_framebuffer_rect(&self, region: &Region) -> Res<Rect> {
        self.check(region)?;
        let edge = |v: u32, limit: u16, round: fn(f64) -> f64| to_u32(round(f64::from(v) * self.scale)).min(u32::from(limit));
        let (x0, y0) = (edge(region.x, self.fb_width, f64::floor), edge(region.y, self.fb_height, f64::floor));
        let (x1, y1) = (edge(region.x + region.width, self.fb_width, f64::ceil), edge(region.y + region.height, self.fb_height, f64::ceil));
        Ok(Rect::new(u16::try_from(x0)?, u16::try_from(y0)?, u16::try_from(x1 - x0)?, u16::try_from(y1 - y0)?))
    }

    fn check(&self, region: &Region) -> Res<()> {
        ensure!(region.width > 0 && region.height > 0, "the region is empty");
        ensure!(
            region.x.checked_add(region.width).is_some_and(|r| r <= self.width) && region.y.checked_add(region.height).is_some_and(|b| b <= self.height),
            "the {}x{} region at ({}, {}) reaches outside the {}x{} screen",
            region.width,
            region.height,
            region.x,
            region.y,
            self.width,
            self.height
        );
        Ok(())
    }
}

/// A rendered screenshot.
#[derive(Debug, Clone)]
pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The part of screen space it shows.
    pub region: Region,
    /// Whether every pixel is (nearly) black: an asleep display or a blanked session.
    pub black: bool,
}

impl Shot {
    /// Screen pixels per image pixel: 1 for a full screenshot at the default size, below 1 for a
    /// zoomed region.
    #[must_use]
    pub fn screen_per_image_px(&self) -> f64 {
        f64::from(self.region.width) / f64::from(self.width)
    }
}

/// Renders `region` of screen space (all of it by default) as a PNG whose long edge is at most
/// `max_edge`, at no more than the framebuffer's own resolution.
///
/// # Errors
///
/// When there is no frame yet, or the region reaches outside screen space.
pub fn render(fb: &Framebuffer, space: &ScreenSpace, region: Option<Region>, max_edge: u32) -> Res<Shot> {
    ensure!(fb.width() > 0 && fb.height() > 0, "no frame has arrived yet");
    let region = region.unwrap_or(Region {
        x: 0,
        y: 0,
        width: space.width,
        height: space.height,
    });
    space.check(&region)?;

    // The crop in framebuffer pixels (fractional: the resizer samples sub-pixel crops).
    let (left, top) = (f64::from(region.x) * space.scale, f64::from(region.y) * space.scale);
    let crop_w = (f64::from(region.width) * space.scale).min(f64::from(fb.width()) - left);
    let crop_h = (f64::from(region.height) * space.scale).min(f64::from(fb.height()) - top);
    let long = crop_w.max(crop_h);
    let shrink = if long > f64::from(max_edge.max(1)) { f64::from(max_edge.max(1)) / long } else { 1.0 };
    let (width, height) = (to_u32(crop_w * shrink).max(1), to_u32(crop_h * shrink).max(1));

    let src = ImageRef::new(u32::from(fb.width()), u32::from(fb.height()), fb.pixels(), PixelType::U8x3).context("wrapping the framebuffer")?;
    let mut dst = Image::new(width, height, PixelType::U8x3);
    // Catmull-Rom keeps small text legible when a Retina frame is halved, at a fraction of
    // Lanczos3's cost.
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom)).crop(left, top, crop_w, crop_h);
    Resizer::new().resize(&src, &mut dst, &options).context("scaling the screenshot")?;

    let pixels = dst.buffer();
    Ok(Shot {
        png: encode_png(pixels, width, height)?,
        width,
        height,
        region,
        black: pixels.iter().all(|&channel| channel <= BLACK_LEVEL),
    })
}

fn encode_png(rgb: &[u8], width: u32, height: u32) -> Res<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 4);
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    // Screenshots are sent once and thrown away: speed over the last few percent of size.
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().context("writing the PNG header")?;
    writer.write_image_data(rgb).context("encoding the PNG")?;
    writer.finish().context("finishing the PNG")?;
    Ok(out)
}

#[cfg(test)]
mod unit {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn retina_frames_scale_down_to_the_default_edge() {
        let space = ScreenSpace::new(3456, 2234, DEFAULT_MAX_EDGE);
        assert_eq!((space.width, space.height), (1568, 1014));
        assert!((space.scale - 3456.0 / 1568.0).abs() < 1e-9);
    }

    #[test]
    fn small_frames_are_never_scaled_up() {
        let space = ScreenSpace::new(800, 600, DEFAULT_MAX_EDGE);
        assert_eq!((space.width, space.height, space.scale), (800, 600, 1.0));
        assert_eq!(space.to_framebuffer(799, 599).unwrap(), (799, 599));
    }

    #[test]
    fn maps_points_to_the_pixel_under_their_centre() {
        let space = ScreenSpace::new(3000, 1500, 1500);
        assert_eq!(space.scale, 2.0);
        assert_eq!(space.to_framebuffer(0, 0).unwrap(), (1, 1));
        assert_eq!(space.to_framebuffer(10, 20).unwrap(), (21, 41));
        assert_eq!(space.to_framebuffer(1499, 749).unwrap(), (2999, 1499));
        assert!(space.to_framebuffer(1500, 0).is_err());
        assert!(space.to_framebuffer(0, 750).is_err());
    }

    #[test]
    fn maps_regions_outward_and_rejects_bad_ones() {
        let space = ScreenSpace::new(3000, 1500, 1500);
        let rect = space.to_framebuffer_rect(&Region { x: 10, y: 5, width: 20, height: 10 }).unwrap();
        assert_eq!(rect, Rect::new(20, 10, 40, 20));
        assert!(space.to_framebuffer_rect(&Region { x: 0, y: 0, width: 0, height: 1 }).is_err());
        assert!(space.to_framebuffer_rect(&Region { x: 1400, y: 0, width: 101, height: 1 }).is_err());
        assert!(space.to_framebuffer_rect(&Region { x: u32::MAX, y: 0, width: 2, height: 1 }).is_err());
    }

    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[test]
    fn renders_the_whole_screen_at_screen_size() {
        let mut fb = Framebuffer::new(400, 200);
        fb.fill(&Rect::new(0, 0, 200, 200), [255, 0, 0]).unwrap();
        fb.fill(&Rect::new(200, 0, 200, 200), [0, 0, 255]).unwrap();
        let space = ScreenSpace::new(400, 200, 100);

        let shot = render(&fb, &space, None, 100).unwrap();

        assert_eq!((shot.width, shot.height), (100, 50));
        assert!(!shot.black);
        assert_eq!(shot.screen_per_image_px(), 1.0);
        let (w, h, rgb) = decode(&shot.png);
        assert_eq!((w, h), (100, 50));
        assert_eq!(&rgb[..3], &[255, 0, 0]);
        assert_eq!(&rgb[rgb.len() - 3..], &[0, 0, 255]);
    }

    #[test]
    fn zooms_into_a_region_at_framebuffer_resolution() {
        let mut fb = Framebuffer::new(400, 200);
        fb.fill(&Rect::new(300, 100, 100, 100), [0, 255, 0]).unwrap();
        let space = ScreenSpace::new(400, 200, 100);

        // The bottom-right quarter of screen space is 25x25 screen pixels but 100x100 framebuffer
        // pixels, so the zoom shows it at 100x100 (capped by max_edge = 1568).
        let shot = render(&fb, &space, Some(Region { x: 75, y: 25, width: 25, height: 25 }), DEFAULT_MAX_EDGE).unwrap();

        assert_eq!((shot.width, shot.height), (100, 100));
        assert_eq!(shot.screen_per_image_px(), 0.25);
        let (_, _, rgb) = decode(&shot.png);
        assert!(rgb.as_chunks::<3>().0.iter().all(|p| *p == [0, 255, 0]));
    }

    #[test]
    fn notices_a_black_screen() {
        let fb = Framebuffer::new(64, 64);
        let shot = render(&fb, &ScreenSpace::new(64, 64, 64), None, 64).unwrap();
        assert!(shot.black);
    }

    #[test]
    fn refuses_to_render_before_the_first_frame() {
        assert!(render(&Framebuffer::default(), &ScreenSpace::new(0, 0, 64), None, 64).is_err());
    }
}

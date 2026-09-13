//! The icon bitmaps: one for the notification area, one for the corner widget.
//!
//! Both images are generated in code rather than decoded from an embedded file
//! so that the crate does not need an image decoding dependency for two small
//! glyphs. The artwork is deliberately plain: it only has to be recognisable.
//!
//! The two share a visual language — three bars of increasing height,
//! suggesting a level meter — but not a bitmap, because they sit on very
//! different grounds. The notification area gives the system tray icon a
//! background the shell already guarantees contrast against; the corner widget
//! floats over whatever the user's desktop happens to be, so it carries its own
//! backdrop.

use crate::theme;

/// Tray icon edge length in pixels.
const SIZE: u32 = 32;

/// Corner widget edge length at rest, in pixels.
///
/// Larger than the tray icon on purpose. The tray icon is drawn into a slot the
/// shell sizes for it, whereas this one is a free-standing click target on the
/// desktop: at 32 px it would be smaller than any other control on screen and
/// awkward to hit, and the glyph inside it would have to shrink further to
/// leave room for the backdrop. 56 px leaves a legible glyph inside a
/// comfortable target while still being small enough to sit in a corner
/// unobtrusively.
pub(crate) const WIDGET_SIZE: u32 = 56;

/// Opacity of the resting badge's backdrop. Not fully opaque: the badge should
/// read as laid over the desktop rather than punched into it, which is the same
/// reasoning behind the overlay panel's own translucent surface.
pub(crate) const BACKDROP_ALPHA: f32 = 235.0;

/// The level glyph's proportions, as fractions of the square it is drawn into.
///
/// Kept as fractions rather than pixel offsets so the same drawing serves the
/// resting badge and the much smaller per-row markers in the endpoint list
/// without being retuned — and without the blur that scaling one finished
/// bitmap to another size would bring.
const GLYPH_BOX_WIDTH: f32 = 24.0 / 56.0;
const GLYPH_BOX_HEIGHT: f32 = 23.0 / 56.0;

/// Bar heights as fractions of the glyph box, shortest first.
const GLYPH_BAR_HEIGHTS: [f32; 3] = [8.0 / 23.0, 15.0 / 23.0, 1.0];

/// Three bars of increasing height, suggesting a level meter, drawn light on a
/// transparent background so the icon reads on both light and dark taskbars.
pub(crate) fn tray_icon_rgba() -> (Vec<u8>, u32, u32) {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    // Bar geometry in pixels: (x start, x end exclusive, y start).
    let bars: [(u32, u32, u32); 3] = [(6, 11, 18), (13, 18, 10), (20, 25, 4)];
    let bottom = SIZE - 4;

    for (x0, x1, y0) in bars {
        for y in y0..bottom {
            for x in x0..x1 {
                let idx = ((y * SIZE + x) * 4) as usize;
                rgba[idx] = 0xF5;
                rgba[idx + 1] = 0xF5;
                rgba[idx + 2] = 0xF3;
                rgba[idx + 3] = 0xFF;
            }
        }
    }

    (rgba, SIZE, SIZE)
}

/// A straight-alpha RGBA image the widget composes its frames into.
///
/// Straight rather than premultiplied because text is drawn over it later by
/// GDI, which knows nothing about an alpha channel; premultiplying is the last
/// step before the pixels reach the window.
pub(crate) struct Canvas {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl Canvas {
    /// A fully transparent canvas.
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            pixels: vec![0u8; (width * height * 4) as usize],
            width,
            height,
        }
    }

    fn index(&self, x: u32, y: u32) -> usize {
        ((y * self.width + x) * 4) as usize
    }

    /// Lays `colour` over one pixel, in straight alpha.
    fn blend(&mut self, x: u32, y: u32, colour: [u8; 4], coverage: f32) {
        if x >= self.width || y >= self.height {
            return;
        }

        let source_alpha = f32::from(colour[3]) / 255.0 * coverage.clamp(0.0, 1.0);
        if source_alpha <= 0.0 {
            return;
        }

        let idx = self.index(x, y);
        let under_alpha = f32::from(self.pixels[idx + 3]) / 255.0;

        // Straight-alpha "source over", which divides the weighted colours by
        // the combined alpha. Using the premultiplied form here instead would
        // darken every colour laid onto a transparent pixel towards black —
        // the backdrop would come out dimmer than the palette says it is.
        let combined = source_alpha + under_alpha * (1.0 - source_alpha);
        if combined <= 0.0 {
            return;
        }

        for (channel, over) in colour.iter().take(3).enumerate() {
            let under = f32::from(self.pixels[idx + channel]);
            let over = f32::from(*over);
            let mixed =
                (over * source_alpha + under * under_alpha * (1.0 - source_alpha)) / combined;
            self.pixels[idx + channel] = mixed.round().clamp(0.0, 255.0) as u8;
        }

        self.pixels[idx + 3] = (combined * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// The alpha channel on its own.
    ///
    /// Drawing text with GDI does not preserve the alpha byte of a 32-bit
    /// bitmap — it writes colour and leaves the fourth byte to chance — so the
    /// shape is kept here and written back after the text has been drawn,
    /// rather than trusting whatever GDI left behind.
    pub(crate) fn alpha_mask(&self) -> Vec<u8> {
        self.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[3])
            .collect()
    }

    /// Fills a rounded rectangle covering the whole canvas.
    ///
    /// One shape serves both states the widget has: at the resting size, a
    /// radius of half the edge length *is* a circle, so growing into the list
    /// is a continuous morph from badge to panel rather than a swap between
    /// two different drawings.
    pub(crate) fn fill_rounded(&mut self, radius: f32, colour: [u8; 4]) {
        let half_w = self.width as f32 / 2.0;
        let half_h = self.height as f32 / 2.0;
        let radius = radius.clamp(0.0, half_w.min(half_h));

        for y in 0..self.height {
            for x in 0..self.width {
                // Signed distance to a rounded rectangle: the distance to the
                // inner rectangle its corner arcs ride on, less the radius.
                let dx = ((x as f32 + 0.5) - half_w).abs() - (half_w - radius);
                let dy = ((y as f32 + 0.5) - half_h).abs() - (half_h - radius);
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
                let distance = outside + dx.max(dy).min(0.0) - radius;

                let coverage = (0.5 - distance).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, colour, coverage);
                }
            }
        }
    }

    /// Lays a flat rectangle over whatever is already there.
    pub(crate) fn fill_rect(&mut self, x: i32, y: i32, width: u32, height: u32, colour: [u8; 4]) {
        for row in 0..height {
            for column in 0..width {
                let px = x + column as i32;
                let py = y + row as i32;
                if px >= 0 && py >= 0 {
                    self.blend(px as u32, py as u32, colour, 1.0);
                }
            }
        }
    }

    /// Draws the three-bar level glyph inside the square box at `(x, y)`.
    ///
    /// The bars, their gaps and their heights are all fractions of the box, so
    /// this reads correctly at the resting badge's size and at the much smaller
    /// size each list row uses.
    pub(crate) fn draw_level_glyph(&mut self, x: i32, y: i32, size: u32, colour: [u8; 4]) {
        let box_width = (size as f32 * GLYPH_BOX_WIDTH).round().max(3.0);
        let box_height = (size as f32 * GLYPH_BOX_HEIGHT).round().max(3.0);

        // Three bars and two gaps: a bar is a quarter of the box, which leaves
        // an eighth for each gap.
        let bar_width = (box_width / 4.0).round().max(1.0);
        let gap = ((box_width - bar_width * 3.0) / 2.0).round().max(1.0);

        // Centre the glyph box inside the square it was given.
        let left = x + ((size as f32 - box_width) / 2.0).floor() as i32;
        let top = y + ((size as f32 - box_height) / 2.0).floor() as i32;
        let bottom = top + box_height as i32;

        for (bar, fraction) in GLYPH_BAR_HEIGHTS.iter().enumerate() {
            let height = (box_height * fraction).round().max(1.0);
            let bar_x = left + (bar as f32 * (bar_width + gap)) as i32;
            let bar_y = bottom - height as i32;
            self.fill_rect(bar_x, bar_y, bar_width as u32, height as u32, colour);
        }
    }
}

/// The resting badge — the three bars on a filled circular backdrop in the
/// overlay's own surface colour, so the glyph reads against any desktop — and
/// every intermediate shape between it and the endpoint list's panel.
///
/// The backdrop is drawn with coverage-based smoothing at its edge; a hard
/// edge at this size reads as visibly stepped, and the window it ends up in is
/// alpha-blended against the desktop, so the smoothing is not wasted.
///
/// `width`/`height`, `radius` and `backdrop_alpha` are what the grow animation
/// moves: at rest they are the resting size with a radius of half that (which
/// makes the rounded rectangle a circle), and they travel towards the panel's
/// size, its much smaller corner radius and full opacity. `glyph_opacity`
/// fades the bars out as that happens, since the grown state shows rows
/// instead.
pub(crate) fn widget_badge(
    width: u32,
    height: u32,
    radius: f32,
    backdrop_alpha: f32,
    glyph_opacity: f32,
) -> Canvas {
    let mut canvas = Canvas::new(width, height);

    let surface = theme::SURFACE.to_srgba_unmultiplied();
    canvas.fill_rounded(
        radius,
        [
            surface[0],
            surface[1],
            surface[2],
            backdrop_alpha.round().clamp(0.0, 255.0) as u8,
        ],
    );

    if glyph_opacity > 0.0 {
        // The glyph keeps its resting size throughout the morph and only
        // fades; scaling it up into the panel would draw attention to a shape
        // that is on its way out.
        let glyph = theme::TEXT_PRIMARY.to_srgba_unmultiplied();
        canvas.draw_level_glyph(
            (width as i32 - WIDGET_SIZE as i32) / 2,
            (height as i32 - WIDGET_SIZE as i32) / 2,
            WIDGET_SIZE,
            [
                glyph[0],
                glyph[1],
                glyph[2],
                (glyph_opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
            ],
        );
    }

    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge exactly as the widget composes it when nothing is hovering
    /// it: the resting size, a radius of half that — which makes the rounded
    /// rectangle a circle — and the glyph at full strength.
    fn resting_badge() -> Canvas {
        widget_badge(
            WIDGET_SIZE,
            WIDGET_SIZE,
            WIDGET_SIZE as f32 / 2.0,
            BACKDROP_ALPHA,
            1.0,
        )
    }

    #[test]
    fn icon_has_one_rgba_quad_per_pixel() {
        let (rgba, w, h) = tray_icon_rgba();
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn icon_is_not_entirely_blank() {
        let (rgba, _, _) = tray_icon_rgba();
        assert!(
            rgba.as_chunks::<4>().0.iter().any(|px| px[3] != 0),
            "icon must have at least one opaque pixel"
        );
    }

    #[test]
    fn widget_icon_has_one_rgba_quad_per_pixel() {
        let canvas = resting_badge();
        assert_eq!(
            canvas.pixels.len(),
            (canvas.width * canvas.height * 4) as usize
        );
        assert_eq!(canvas.width, canvas.height);
        assert!(
            canvas.width > SIZE,
            "the widget icon must be larger than the tray one"
        );
    }

    #[test]
    fn widget_icon_corners_are_clear_and_its_centre_is_not() {
        let canvas = resting_badge();
        let w = canvas.width;
        let alpha_at = |x: u32, y: u32| canvas.pixels[canvas.index(x, y) + 3];

        // Outside the circle: the window is the size of the whole square, so
        // the corners have to be transparent or the widget shows as a box.
        assert_eq!(alpha_at(0, 0), 0);
        assert_eq!(alpha_at(w - 1, 0), 0);
        assert_eq!(alpha_at(0, w - 1), 0);
        assert_eq!(alpha_at(w - 1, w - 1), 0);

        // Inside it.
        assert!(alpha_at(w / 2, w / 2) > 0);
    }

    #[test]
    fn widget_icon_draws_the_glyph_over_the_backdrop() {
        let canvas = resting_badge();
        let pixel_at = |x: u32, y: u32| {
            let idx = canvas.index(x, y);
            [
                canvas.pixels[idx],
                canvas.pixels[idx + 1],
                canvas.pixels[idx + 2],
                canvas.pixels[idx + 3],
            ]
        };

        let glyph = theme::TEXT_PRIMARY.to_srgba_unmultiplied();
        // Inside the tallest bar.
        assert_eq!(pixel_at(36, 20), [glyph[0], glyph[1], glyph[2], 0xFF]);

        // Between two bars: backdrop, not glyph.
        let surface = theme::SURFACE.to_srgba_unmultiplied();
        let gap = pixel_at(23, 35);
        assert_eq!(
            [gap[0], gap[1], gap[2]],
            [surface[0], surface[1], surface[2]]
        );
        assert!(gap[3] > 0 && gap[3] < 0xFF);
    }

    /// The glyph is laid out from fractions, so it has to keep its proportions
    /// at a row-marker size as well as at the badge size — the whole reason it
    /// is redrawn per size instead of being scaled from one bitmap.
    #[test]
    fn the_level_glyph_keeps_its_proportions_at_any_size() {
        for size in [16u32, 24, 56, 96] {
            let mut canvas = Canvas::new(size, size);
            canvas.draw_level_glyph(0, 0, size, [0xFF, 0xFF, 0xFF, 0xFF]);

            let column_has_ink =
                |x: u32| (0..size).any(|y| canvas.pixels[canvas.index(x, y) + 3] > 0);
            let row_has_ink = |y: u32| (0..size).any(|x| canvas.pixels[canvas.index(x, y) + 3] > 0);

            let inked_columns = (0..size).filter(|x| column_has_ink(*x)).count();
            let inked_rows = (0..size).filter(|y| row_has_ink(*y)).count();

            // Three bars plus two gaps never fill the whole box, and the
            // tallest bar spans the box's full height.
            assert!(
                inked_columns > 0 && inked_columns < size as usize,
                "size {size}"
            );
            assert!(inked_rows > 0 && inked_rows < size as usize, "size {size}");

            // The bars ascend: the rightmost inked column reaches higher than
            // the leftmost one.
            let first = (0..size).find(|x| column_has_ink(*x)).expect("ink");
            let last = (0..size).rev().find(|x| column_has_ink(*x)).expect("ink");
            let top_of = |x: u32| (0..size).find(|y| canvas.pixels[canvas.index(x, *y) + 3] > 0);
            assert!(
                top_of(last) < top_of(first),
                "size {size}: bars must ascend"
            );
        }
    }

    /// At the resting size the backdrop's radius is half its edge, which makes
    /// the rounded rectangle a circle — that equivalence is what lets the grow
    /// animation morph one shape into the other.
    #[test]
    fn a_full_radius_rounded_rectangle_is_the_circular_badge() {
        let canvas = widget_badge(
            WIDGET_SIZE,
            WIDGET_SIZE,
            WIDGET_SIZE as f32 / 2.0,
            BACKDROP_ALPHA,
            1.0,
        );
        let alpha_at = |x: u32, y: u32| canvas.pixels[canvas.index(x, y) + 3];

        assert_eq!(alpha_at(0, 0), 0);
        assert_eq!(alpha_at(WIDGET_SIZE - 1, WIDGET_SIZE - 1), 0);
        assert!(
            alpha_at(WIDGET_SIZE / 2, 1) > 0,
            "the circle touches the edge midpoints"
        );
        assert!(alpha_at(1, WIDGET_SIZE / 2) > 0);
    }

    /// A small radius keeps the corners, which is the panel the list is drawn
    /// on; only the very corner pixels are cut away.
    #[test]
    fn a_small_radius_keeps_a_panel_shape() {
        let mut canvas = Canvas::new(200, 80);
        canvas.fill_rounded(12.0, [10, 10, 10, 0xFF]);
        let alpha_at = |c: &Canvas, x: u32, y: u32| c.pixels[c.index(x, y) + 3];

        assert_eq!(alpha_at(&canvas, 0, 0), 0, "the corner is rounded away");
        assert_eq!(alpha_at(&canvas, 100, 40), 0xFF, "the middle is solid");
        assert_eq!(alpha_at(&canvas, 100, 0), 0xFF, "the top edge is straight");
        assert_eq!(alpha_at(&canvas, 0, 40), 0xFF, "the left edge is straight");
    }

    #[test]
    fn the_alpha_mask_matches_the_canvas_alpha_channel() {
        let canvas = widget_badge(
            WIDGET_SIZE,
            WIDGET_SIZE,
            WIDGET_SIZE as f32 / 2.0,
            BACKDROP_ALPHA,
            1.0,
        );
        let mask = canvas.alpha_mask();

        assert_eq!(mask.len(), (canvas.width * canvas.height) as usize);
        for (index, pixel) in canvas.pixels.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(mask[index], pixel[3]);
        }
    }
}

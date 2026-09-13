//! The tray icon bitmap.
//!
//! The image is generated in code rather than decoded from an embedded file so
//! that the crate does not need an image decoding dependency for a 32x32
//! glyph. The artwork is deliberately plain: it only has to be recognisable in
//! the notification area.

/// Icon edge length in pixels.
const SIZE: u32 = 32;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}

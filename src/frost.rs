//! The drop zone's frosted-glass look, as plain pixel math on 0xAARRGGBB
//! images (the byte order of a 32-bit Windows DIB). No Windows code here.
//!
//! The screen under the zone is captured at a fraction of its size, blurred
//! there (cheap, and the downscale itself blurs), then scaled back up with
//! smooth filtering, darkened a touch, and given rounded corners and a
//! hairline border.

/// How the zone looks.
pub struct Style {
    /// Box-blur radius, in pixels of the small capture.
    pub blur: usize,
    /// How much to darken the blur, 0 (none) to 255 (black).
    pub darken: u8,
    /// Corner radius, in screen pixels.
    pub corner: f32,
    /// Opacity of the white hairline border, 0 to 255.
    pub border: u8,
}

fn channels(p: u32) -> [u32; 3] {
    [(p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff]
}

fn pack([r, g, b]: [u32; 3]) -> u32 {
    0xff00_0000 | (r << 16) | (g << 8) | b
}

/// The cw×ch block at (x, y) of a w×h image, clamped to the image (edge
/// pixels repeat where the block runs off it).
pub fn crop(px: &[u32], w: usize, h: usize, x: isize, y: isize, cw: usize, ch: usize) -> Vec<u32> {
    if w == 0 || h == 0 {
        return vec![0; cw * ch];
    }
    let mut out = Vec::with_capacity(cw * ch);
    for j in 0..ch as isize {
        let sy = (y + j).clamp(0, h as isize - 1) as usize;
        out.extend((0..cw as isize).map(|i| px[sy * w + (x + i).clamp(0, w as isize - 1) as usize]));
    }
    out
}

/// Blur in place: three box-blur passes each way, which together look
/// close to a Gaussian.
pub fn blur(px: &mut [u32], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut line = Vec::new();
    for _ in 0..3 {
        for y in 0..h {
            line.clear();
            line.extend((0..w).map(|x| px[y * w + x]));
            box_line(&line, r, |x, p| px[y * w + x] = p);
        }
        for x in 0..w {
            line.clear();
            line.extend((0..h).map(|y| px[y * w + x]));
            box_line(&line, r, |y, p| px[y * w + x] = p);
        }
    }
}

/// One box-blur pass over a row or column, clamping at the ends.
fn box_line(src: &[u32], r: usize, mut put: impl FnMut(usize, u32)) {
    let n = src.len();
    let at = |i: isize| channels(src[i.clamp(0, n as isize - 1) as usize]);
    let mut sum = [0u32; 3];
    for i in -(r as isize)..=(r as isize) {
        let c = at(i);
        (0..3).for_each(|k| sum[k] += c[k]);
    }
    let span = 2 * r as u32 + 1;
    for i in 0..n {
        put(i, pack(sum.map(|s| s / span)));
        let (add, sub) = (at(i as isize + r as isize + 1), at(i as isize - r as isize));
        (0..3).for_each(|k| sum[k] = sum[k] + add[k] - sub[k]);
    }
}

/// Scale the blurred `small` image (sw×sh) up to w×h with bilinear
/// filtering and finish it per `style`. Returns premultiplied-alpha pixels,
/// ready for a layered window.
pub fn compose(small: &[u32], sw: usize, sh: usize, w: usize, h: usize, style: &Style) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    if sw == 0 || sh == 0 {
        return out;
    }
    // Source coordinate (8-bit fraction) for each output column/row.
    let map = |i: usize, from: usize, to: usize| -> (usize, usize, u32) {
        let f = ((((i as f32 + 0.5) * from as f32 / to as f32) - 0.5).max(0.0) * 256.0) as u32;
        let i0 = ((f >> 8) as usize).min(from - 1);
        (i0, (i0 + 1).min(from - 1), f & 0xff)
    };
    let cols: Vec<_> = (0..w).map(|x| map(x, sw, w)).collect();
    let src: Vec<[u32; 3]> = small.iter().map(|&p| channels(p)).collect();
    let keep = 255 - style.darken as u32;
    // Only pixels this close to a side can be in a rounded corner or on the
    // rim; everything further in is plain, opaque frost.
    let band = (style.corner.ceil() as usize).max(2);
    let mut row = vec![[0u32; 3]; sw];
    for y in 0..h {
        // Blend the two source rows once, then each output pixel is a blend
        // of two neighbours in that row.
        let (y0, y1, ty) = map(y, sh, h);
        for (i, v) in row.iter_mut().enumerate() {
            let (a, b) = (src[y0 * sw + i], src[y1 * sw + i]);
            *v = [0, 1, 2].map(|k| a[k] * (256 - ty) + b[k] * ty);
        }
        let edge_row = y < band || y + band >= h;
        let line = &mut out[y * w..(y + 1) * w];
        for (x, (&(x0, x1, tx), px)) in cols.iter().zip(line.iter_mut()).enumerate() {
            let (a, b) = (row[x0], row[x1]);
            let rgb = [0, 1, 2].map(|k| ((a[k] * (256 - tx) + b[k] * tx) >> 16) * keep / 255);
            if !edge_row && x >= band && x + band < w {
                *px = pack(rgb);
                continue;
            }
            let (coverage, rim) = shape(x, y, w, h, style.corner);
            if coverage == 0.0 {
                continue;
            }
            // Hairline border: white over the rim.
            let bw = style.border as f32 * rim / 255.0;
            let lit = rgb.map(|v| (v as f32 * (1.0 - bw) + 255.0 * bw) as u32);
            let alpha = (coverage * 255.0) as u32;
            *px = (alpha << 24) | pack(lit.map(|v| v * alpha / 255)) & 0x00ff_ffff;
        }
    }
    out
}

/// How much of pixel (x, y) is inside a w×h rounded rectangle (0 to 1),
/// and how much of it is on the 1px rim (0 to 1).
fn shape(x: usize, y: usize, w: usize, h: usize, corner: f32) -> (f32, f32) {
    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
    let r = corner.min(w as f32 / 2.0).min(h as f32 / 2.0);
    // Distance inside the edge: to the nearest side, or to the corner arc.
    let cx = px.clamp(r, w as f32 - r);
    let cy = py.clamp(r, h as f32 - r);
    let inside = if (px - cx).abs() > 0.0 && (py - cy).abs() > 0.0 {
        r - ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
    } else {
        px.min(py).min(w as f32 - px).min(h as f32 - py)
    };
    let coverage = (inside + 0.5).clamp(0.0, 1.0);
    let rim = (1.5 - inside).clamp(0.0, 1.0);
    (coverage, rim)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLE: Style = Style { blur: 2, darken: 0, corner: 8.0, border: 0 };

    #[test]
    fn crop_takes_a_block_and_clamps_at_the_edges() {
        let px: Vec<u32> = (0..12).collect(); // 4×3, values = index
        assert_eq!(crop(&px, 4, 3, 1, 1, 2, 2), vec![5, 6, 9, 10]);
        assert_eq!(crop(&px, 4, 3, -1, 2, 2, 2), vec![8, 8, 8, 8]);
    }

    #[test]
    fn blur_keeps_a_flat_image_flat() {
        let mut px = vec![0xff40_8020; 12 * 9];
        blur(&mut px, 12, 9, 3);
        assert!(px.iter().all(|&p| p == 0xff40_8020));
    }

    #[test]
    fn blur_spreads_a_bright_dot() {
        let mut px = vec![0xff00_0000; 9 * 9];
        px[4 * 9 + 4] = 0xffff_ffff;
        blur(&mut px, 9, 9, 1);
        assert!(channels(px[4 * 9 + 4])[0] < 255, "the dot dims");
        assert!(channels(px[4 * 9 + 5])[0] > 0, "its neighbours brighten");
    }

    #[test]
    fn compose_scales_up_and_rounds_corners() {
        let small = vec![0xff80_8080; 4 * 4];
        let out = compose(&small, 4, 4, 40, 30, &STYLE);
        assert_eq!(out.len(), 40 * 30);
        assert_eq!(out[15 * 40 + 20], 0xff80_8080, "middle is the image, opaque");
        assert_eq!(out[0] >> 24, 0, "corner pixel is cut away");
        assert_eq!(out[15 * 40] >> 24, 0xff, "side midpoint is solid");
    }

    #[test]
    fn compose_darkens_and_draws_the_rim() {
        let small = vec![0xff80_8080; 4];
        let style = Style { darken: 128, border: 255, ..STYLE };
        let out = compose(&small, 2, 2, 40, 40, &style);
        assert!(channels(out[20 * 40 + 20])[0] < 0x80, "darkened");
        assert!(channels(out[20 * 40])[0] > 0xc0, "rim is lit");
    }

    #[test]
    fn compose_output_is_premultiplied() {
        let out = compose(&[0xffff_ffff], 1, 1, 30, 30, &STYLE);
        for p in out {
            let a = p >> 24;
            assert!(channels(p).iter().all(|&c| c <= a), "{p:#x}");
        }
    }
}

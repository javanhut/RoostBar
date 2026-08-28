use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};

pub struct Canvas<'a> {
    pub buf: &'a mut [u8],
    pub width: u32,
    pub height: u32,
}

impl<'a> Canvas<'a> {
    pub fn fill(&mut self, argb: [u8; 4]) {
        let [a, r, g, b] = argb;
        let px = premul(a, r, g, b);
        for chunk in self.buf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
    }

    #[inline]
    fn blend(&mut self, x: i32, y: i32, argb: [u8; 4], cov: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let a = (argb[0] as f32 / 255.0) * cov;
        if a <= 0.0 {
            return;
        }
        let i = ((y as u32 * self.width + x as u32) * 4) as usize;
        let dst = &mut self.buf[i..i + 4];
        // wl_shm ARGB8888 is little-endian: bytes are B, G, R, A.
        let src = [argb[3] as f32 * a, argb[2] as f32 * a, argb[1] as f32 * a, 255.0 * a];
        for k in 0..4 {
            dst[k] = (src[k] + dst[k] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, argb: [u8; 4]) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, argb, 1.0);
            }
        }
    }
}

fn premul(a: u8, r: u8, g: u8, b: u8) -> [u8; 4] {
    let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    [m(b), m(g), m(r), a]
}

pub struct Text {
    font: FontVec,
    scale: PxScale,
}

impl Text {
    pub fn load(path: &std::path::Path, size: f32) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let font = FontVec::try_from_vec(bytes).map_err(|e| format!("{}: {e:?}", path.display()))?;
        Ok(Self { font, scale: PxScale::from(size) })
    }

    pub fn with_scale(&self, factor: f32) -> PxScale {
        PxScale::from(self.scale.x * factor)
    }

    pub fn width(&self, s: &str, scale: PxScale) -> f32 {
        let f = self.font.as_scaled(scale);
        let mut w = 0.0;
        let mut prev = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                w += f.kern(p, id);
            }
            w += f.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Draw `s` with its left edge at `x`, vertically centred in the canvas.
    pub fn draw(&self, canvas: &mut Canvas, s: &str, x: f32, scale: PxScale, color: [u8; 4]) -> f32 {
        let f = self.font.as_scaled(scale);
        let text_h = f.ascent() - f.descent();
        let baseline = ((canvas.height as f32 - text_h) / 2.0 + f.ascent()).round();
        let mut cx = x;
        let mut prev = None;
        for c in s.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                cx += f.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, point(cx, baseline));
            if let Some(og) = self.font.outline_glyph(glyph) {
                let b = og.px_bounds();
                og.draw(|gx, gy, cov| {
                    canvas.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, color, cov);
                });
            }
            cx += f.h_advance(id);
            prev = Some(id);
        }
        cx - x
    }
}

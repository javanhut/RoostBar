use ab_glyph::{point, Font, FontVec, GlyphId, PxScale, ScaleFont};

pub struct Canvas<'a> {
    pub buf: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    /// Rows outside `start..end` are left untouched: the clock panel's list
    /// scrolls under the header rather than over it.
    pub clip: Option<(i32, i32)>,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u8], width: u32, height: u32) -> Self {
        Self { buf, width, height, clip: None }
    }

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
        if self.clip.is_some_and(|(start, end)| y < start || y >= end) {
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

    /// A square-cornered fill: the rule under the clock panel's date.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, argb: [u8; 4]) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, argb, 1.0);
            }
        }
    }

    /// A full-width one-pixel line at row `y`: the hairline where the bar
    /// meets the desktop.
    pub fn hline(&mut self, y: i32, argb: [u8; 4]) {
        for xx in 0..self.width as i32 {
            self.blend(xx, y, argb, 1.0);
        }
    }

    /// A rectangle with rounded corners, antialiased.
    ///
    /// The corners sample distance from the arc's centre rather than
    /// stepping a scanline, the same way Huginn's canvas does it: at the
    /// radius a hover pill uses, a hard cutoff is a visible staircase.
    pub fn fill_rounded(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, argb: [u8; 4]) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let (x0, y0) = (x.floor() as i32, y.floor() as i32);
        let (x1, y1) = ((x + w).ceil() as i32, (y + h).ceil() as i32);
        for py in y0..y1 {
            let ly = py as f32 + 0.5 - y;
            // Distance past the corner arc along each axis, zero in the body.
            let dy = (radius - ly).max(ly - (h - radius)).max(0.0);
            for px in x0..x1 {
                let lx = px as f32 + 0.5 - x;
                let dx = (radius - lx).max(lx - (w - radius)).max(0.0);
                let cov = if dx == 0.0 && dy == 0.0 {
                    // Inside the body: only the rectangle's own edges matter.
                    (lx.min(w - lx) + 0.5).clamp(0.0, 1.0) * (ly.min(h - ly) + 0.5).clamp(0.0, 1.0)
                } else {
                    (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0)
                };
                if cov > 0.0 {
                    self.blend(px, py, argb, cov);
                }
            }
        }
    }
}

fn premul(a: u8, r: u8, g: u8, b: u8) -> [u8; 4] {
    let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    [m(b), m(g), m(r), a]
}

/// Which face a character is drawn with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Face {
    /// The UI typeface: everything it has a glyph for.
    Ui,
    /// The Nerd Font: the icon glyphs, and anything the UI face lacks.
    Icon,
}

/// The bar's type: a UI face for words and numbers, and the Nerd Font kept
/// for the icon glyphs the UI face cannot draw.
///
/// One face at a time reads either as a terminal (everything in the mono
/// Nerd Font) or as a bar with no icons; two faces, chosen per character,
/// is how the bar gets both.
pub struct Text {
    /// The UI face, when it loaded. `None` means the single-font bar of old:
    /// every character comes from `icon`.
    ui: Option<FontVec>,
    icon: FontVec,
    scale: PxScale,
}

impl Text {
    /// `ui` is tried first and is optional; `icon` must load.
    pub fn load(ui: &std::path::Path, icon: &std::path::Path, size: f32) -> Result<Self, String> {
        let icon = load_font(icon)?;
        let ui = match load_font(ui) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("roostbar: ui_font: {e}; drawing everything with the icon font");
                None
            }
        };
        Ok(Self { ui, icon, scale: PxScale::from(size) })
    }

    pub fn with_scale(&self, factor: f32) -> PxScale {
        PxScale::from(self.scale.x * factor)
    }

    /// The face for `c`: the UI face if it has the glyph, else the icon face.
    fn face_of(&self, c: char) -> (Face, GlyphId) {
        if let Some(ui) = &self.ui {
            let id = ui.glyph_id(c);
            if id.0 != 0 {
                return (Face::Ui, id);
            }
        }
        (Face::Icon, self.icon.glyph_id(c))
    }

    fn font(&self, face: Face) -> &FontVec {
        match face {
            Face::Ui => self.ui.as_ref().unwrap_or(&self.icon),
            Face::Icon => &self.icon,
        }
    }

    /// Walk `s`, calling `f` with each glyph's face, id and pen x, and
    /// return the total advance. Width and draw share this so they can
    /// never disagree about where a character lands.
    fn walk(&self, s: &str, scale: PxScale, mut f: impl FnMut(Face, GlyphId, f32)) -> f32 {
        let mut cx = 0.0;
        let mut prev: Option<(Face, GlyphId)> = None;
        for c in s.chars() {
            let (face, id) = self.face_of(c);
            let font = self.font(face).as_scaled(scale);
            // Kerning only means something between two glyphs of one face.
            if let Some((pf, pid)) = prev {
                if pf == face {
                    cx += font.kern(pid, id);
                }
            }
            f(face, id, cx);
            cx += font.h_advance(id);
            prev = Some((face, id));
        }
        cx
    }

    pub fn width(&self, s: &str, scale: PxScale) -> f32 {
        self.walk(s, scale, |_, _, _| {})
    }

    /// `s` cut to fit `max` pixels, ending in an ellipsis when it had to be.
    pub fn ellipsize(&self, s: &str, max: f32, scale: PxScale) -> String {
        if self.width(s, scale) <= max {
            return s.to_string();
        }
        let mut cut: String = s.to_string();
        while !cut.is_empty() {
            cut.pop();
            let candidate = format!("{}…", cut.trim_end());
            if self.width(&candidate, scale) <= max {
                return candidate;
            }
        }
        String::new()
    }

    /// Draw `s` with its left edge at `x`, vertically centred in the canvas
    /// on the UI face's metrics, so a line of text sits where it would in
    /// any other Raven panel and the icons fall in beside it.
    pub fn draw(&self, canvas: &mut Canvas, s: &str, x: f32, scale: PxScale, color: [u8; 4]) -> f32 {
        let height = canvas.height as f32;
        self.draw_line(canvas, s, x, 0.0, height, scale, color)
    }

    /// Draw `s` with its left edge at `x`, vertically centred in the band
    /// `top..top + height` the same way [`Text::draw`] centres it in the bar.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_line(
        &self,
        canvas: &mut Canvas,
        s: &str,
        x: f32,
        top: f32,
        height: f32,
        scale: PxScale,
        color: [u8; 4],
    ) -> f32 {
        let metrics = self.font(Face::Ui).as_scaled(scale);
        let text_h = metrics.ascent() - metrics.descent();
        let baseline = (top + (height - text_h) / 2.0 + metrics.ascent()).round();
        self.walk(s, scale, |face, id, cx| {
            let font = self.font(face);
            let glyph = id.with_scale_and_position(scale, point(x + cx, baseline));
            if let Some(og) = font.outline_glyph(glyph) {
                let b = og.px_bounds();
                og.draw(|gx, gy, cov| {
                    canvas.blend(b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, color, cov);
                });
            }
        })
    }
}

fn load_font(path: &std::path::Path) -> Result<FontVec, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    FontVec::try_from_vec(bytes).map_err(|e| format!("{}: {e:?}", path.display()))
}

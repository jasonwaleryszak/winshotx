//! 32bpp DIB sections, rectangle helpers and a tiny anti-aliased software
//! rasteriser for drawing premultiplied BGRA pixels.

use std::ffi::c_void;

use windows::core::Result;
use windows::Win32::Foundation::{COLORREF, LPARAM, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;

/// Selection highlight shared by both capture overlays.
pub const SELECTION: Color = rgba(0xD9D9D9, 32);

/// A 32bpp top-down DIB section selected into its own memory DC.
pub struct Dib {
    pub dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    pub width: i32,
    pub height: i32,
}

impl Dib {
    pub unsafe fn new(reference: HDC, width: i32, height: i32) -> Result<Self> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(reference);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bmp = match CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bmp) => bmp,
            Err(e) => {
                let _ = DeleteDC(dc);
                return Err(e);
            }
        };
        let old = SelectObject(dc, bmp);
        Ok(Self {
            dc,
            bmp,
            old,
            bits: bits.cast(),
            width,
            height,
        })
    }

    pub fn pixels(&self) -> &[u32] {
        unsafe { std::slice::from_raw_parts(self.bits, (self.width * self.height) as usize) }
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.width * self.height) as usize) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp);
            let _ = DeleteDC(self.dc);
        }
    }
}

/// A straight (non-premultiplied) colour: 0xRRGGBB plus opacity 0-255.
#[derive(Clone, Copy)]
pub struct Color {
    pub rgb: u32,
    pub a: u32,
}

pub const fn rgba(rgb: u32, a: u32) -> Color {
    Color { rgb, a }
}

/// Source-over blends `c`, scaled by `cover` (0-255), onto a premultiplied pixel.
pub fn blend(dst: u32, c: Color, cover: u32) -> u32 {
    let a = c.a * cover / 255;
    if a == 0 {
        return dst;
    }
    let inv = 255 - a;
    let mut out = (a + ((dst >> 24) * inv + 127) / 255) << 24;
    for shift in [0, 8, 16] {
        let s = ((c.rgb >> shift) & 0xFF) * a;
        let d = ((dst >> shift) & 0xFF) * inv;
        out |= ((s + d + 127) / 255) << shift;
    }
    out
}

/// Width in pixels of single-line text drawn with `font`.
pub fn text_width(font: HFONT, s: &str) -> i32 {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut size = SIZE::default();
    unsafe {
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, font);
        let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
    }
    size.cx
}

pub fn point_from_lparam(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    }
}

pub fn is_empty(r: RECT) -> bool {
    r.right <= r.left || r.bottom <= r.top
}

pub fn intersect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

pub fn union(a: RECT, b: RECT) -> RECT {
    if is_empty(a) {
        return b;
    }
    if is_empty(b) {
        return a;
    }
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

pub fn contains(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

pub fn offset(r: RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right + dx,
        bottom: r.bottom + dy,
    }
}

pub fn inflate(r: RECT, d: i32) -> RECT {
    RECT {
        left: r.left - d,
        top: r.top - d,
        right: r.right + d,
        bottom: r.bottom + d,
    }
}

#[derive(Clone, Copy)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub fn inset(self, d: f32) -> Self {
        Self {
            l: self.l + d,
            t: self.t + d,
            r: self.r - d,
            b: self.b - d,
        }
    }
}

impl From<RECT> for RectF {
    fn from(r: RECT) -> Self {
        Self {
            l: r.left as f32,
            t: r.top as f32,
            r: r.right as f32,
            b: r.bottom as f32,
        }
    }
}

fn sd_round_rect(x: f32, y: f32, r: RectF, radius: f32) -> f32 {
    let (cx, cy) = ((r.l + r.r) / 2.0, (r.t + r.b) / 2.0);
    let qx = (x - cx).abs() - ((r.r - r.l) / 2.0 - radius);
    let qy = (y - cy).abs() - ((r.b - r.t) / 2.0 - radius);
    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
}

fn dist_segment(x: f32, y: f32, (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (px, py, dx, dy) = (x - ax, y - ay, bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let h = if len2 > 0.0 { ((px * dx + py * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    (px - dx * h).hypot(py - dy * h)
}

/// Draws onto a premultiplied BGRA DIB, restricted to a clip rectangle.
pub struct Canvas<'a> {
    px: &'a mut [u32],
    width: i32,
    clip: RECT,
}

impl<'a> Canvas<'a> {
    pub fn new(dib: &'a mut Dib, clip: RECT) -> Self {
        let bounds = RECT {
            left: 0,
            top: 0,
            right: dib.width,
            bottom: dib.height,
        };
        Self {
            clip: intersect(clip, bounds),
            width: dib.width,
            px: dib.pixels_mut(),
        }
    }

    pub fn clip(&self) -> RECT {
        self.clip
    }

    fn index(&self, x: i32, y: i32) -> usize {
        (y * self.width + x) as usize
    }

    /// Overwrites a rectangle with a premultiplied pixel value.
    pub fn fill(&mut self, r: RECT, px: u32) {
        let r = intersect(r, self.clip);
        if is_empty(r) {
            return;
        }
        for y in r.top..r.bottom {
            let i = self.index(r.left, y);
            self.px[i..i + (r.right - r.left) as usize].fill(px);
        }
    }

    pub fn blend_rect(&mut self, r: RECT, c: Color) {
        let r = intersect(r, self.clip);
        for y in r.top..r.bottom {
            for x in r.left..r.right {
                let i = self.index(x, y);
                self.px[i] = blend(self.px[i], c, 255);
            }
        }
    }

    /// Overwrites the outermost pixels of `r`.
    pub fn frame(&mut self, r: RECT, px: u32) {
        let (l, t, rt, b) = (r.left, r.top, r.right, r.bottom);
        self.fill(RECT { left: l, top: t, right: rt, bottom: t + 1 }, px);
        self.fill(RECT { left: l, top: b - 1, right: rt, bottom: b }, px);
        self.fill(RECT { left: l, top: t, right: l + 1, bottom: b }, px);
        self.fill(RECT { left: rt - 1, top: t, right: rt, bottom: b }, px);
    }

    /// Blends `c` over every pixel near `bounds`, weighted by `coverage`
    /// (0.0-1.0) evaluated at the pixel centre.
    fn shade(&mut self, bounds: RectF, c: Color, coverage: impl Fn(f32, f32) -> f32) {
        let x0 = (bounds.l.floor() as i32 - 1).max(self.clip.left);
        let y0 = (bounds.t.floor() as i32 - 1).max(self.clip.top);
        let x1 = (bounds.r.ceil() as i32 + 1).min(self.clip.right);
        let y1 = (bounds.b.ceil() as i32 + 1).min(self.clip.bottom);
        for y in y0..y1 {
            for x in x0..x1 {
                let k = coverage(x as f32 + 0.5, y as f32 + 0.5);
                if k > 0.0 {
                    let i = self.index(x, y);
                    self.px[i] = blend(self.px[i], c, (k.min(1.0) * 255.0).round() as u32);
                }
            }
        }
    }

    pub fn round_rect(&mut self, r: RectF, radius: f32, c: Color) {
        self.shade(r, c, |x, y| (0.5 - sd_round_rect(x, y, r, radius)).clamp(0.0, 1.0));
    }

    /// A rounded-rectangle outline lying just inside `r`.
    pub fn round_rect_stroke(&mut self, r: RectF, radius: f32, width: f32, c: Color) {
        let half = width / 2.0;
        let path = r.inset(half);
        let radius = (radius - half).max(0.0);
        self.shade(r, c, |x, y| {
            (half + 0.5 - sd_round_rect(x, y, path, radius).abs()).clamp(0.0, 1.0)
        });
    }

    /// Connected line segments with round caps and joins.
    pub fn polyline(&mut self, pts: &[(f32, f32)], width: f32, c: Color) {
        let half = width / 2.0;
        let mut b = RectF {
            l: f32::MAX,
            t: f32::MAX,
            r: f32::MIN,
            b: f32::MIN,
        };
        for &(x, y) in pts {
            b = RectF {
                l: b.l.min(x),
                t: b.t.min(y),
                r: b.r.max(x),
                b: b.b.max(y),
            };
        }
        self.shade(b.inset(-half), c, |x, y| {
            let d = pts
                .windows(2)
                .map(|s| dist_segment(x, y, s[0], s[1]))
                .fold(f32::MAX, f32::min);
            (half + 0.5 - d).clamp(0.0, 1.0)
        });
    }

    /// Draws single-line text. GDI can't write alpha, so the text is rendered
    /// white-on-black into a scratch bitmap and used as a coverage mask.
    pub unsafe fn text(&mut self, s: &str, r: RECT, font: HFONT, c: Color, format: DRAW_TEXT_FORMAT) {
        let visible = intersect(r, self.clip);
        if is_empty(visible) {
            return;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let Ok(mask) = Dib::new(HDC::default(), w, h) else {
            return;
        };
        let old = SelectObject(mask.dc, font);
        SetTextColor(mask.dc, COLORREF(0xFFFFFF));
        SetBkMode(mask.dc, TRANSPARENT);
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        DrawTextW(mask.dc, &mut wide, &mut rc, format | DT_SINGLELINE | DT_NOPREFIX);
        SelectObject(mask.dc, old);
        let _ = GdiFlush();

        let src = mask.pixels();
        for y in visible.top..visible.bottom {
            for x in visible.left..visible.right {
                let k = (src[((y - r.top) * w + x - r.left) as usize] >> 8) & 0xFF;
                if k > 0 {
                    let i = self.index(x, y);
                    self.px[i] = blend(self.px[i], c, k);
                }
            }
        }
    }
}

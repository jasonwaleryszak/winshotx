//! Alt+Shift+5 capture menu. Unlike the Alt+Shift+4 overlay the desktop stays
//! live: a layered window draws an editable area selection and a toolbar with
//! Area, Fullscreen and Window capture, size fields and aspect-ratio presets.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmFlush, DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, Sleep};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_ESCAPE, VK_RETURN, VK_TAB};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture;
use crate::gfx::{
    blend, contains, inflate, intersect, is_empty, offset, point_from_lparam, rgba, union, Canvas, Color, Dib, RectF,
    SELECTION,
};

const CLASS_NAME: PCWSTR = w!("WinShotXMenu");

/// Fully transparent pixels of a layered window let clicks through to the
/// windows below, so "empty" overlay pixels get the lowest visible alpha.
const BACKGROUND: u32 = 0x0100_0000;
const WHITE: u32 = 0xFFFF_FFFF;

const PANEL_FILL: Color = rgba(0x1C1C1E, 248);
const PANEL_BORDER: Color = rgba(0xFFFFFF, 30);
const BUTTON_ACTIVE: Color = rgba(0xFFFFFF, 30);
const BUTTON_HOVER: Color = rgba(0xFFFFFF, 18);
const FIELD_FILL: Color = rgba(0x000000, 110);
const FIELD_HOVER: Color = rgba(0xFFFFFF, 50);
const ACCENT: Color = rgba(0x2D7DF6, 255);
const TEXT: Color = rgba(0xFFFFFF, 235);
const TEXT_DIM: Color = rgba(0xFFFFFF, 130);
const HANDLE_FILL: Color = rgba(0xFFFFFF, 255);
const HANDLE_BORDER: Color = rgba(0x000000, 70);

const MIN_SIZE: i32 = 2;
const BUTTONS: [&str; 3] = ["Area", "Fullscreen", "Window"];
const ASPECTS: [(&str, Option<(i32, i32)>); 7] = [
    ("Freeform", None),
    ("1:1", Some((1, 1))),
    ("4:3", Some((4, 3))),
    ("3:2", Some((3, 2))),
    ("16:9", Some((16, 9))),
    ("16:10", Some((16, 10))),
    ("9:16", Some((9, 16))),
];

thread_local! {
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
    /// Last area in screen coordinates, restored the next time the menu opens.
    static LAST_AREA: Cell<Option<RECT>> = const { Cell::new(None) };
    static LAST_ASPECT: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Area,
    Window,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Width,
    Height,
}

impl Field {
    fn other(self) -> Self {
        match self {
            Field::Width => Field::Height,
            Field::Height => Field::Width,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    None,
    Toolbar,
    Button(usize),
    Field(Field),
    Aspect,
    Item(usize),
}

/// Which edges a resize handle moves: -1 left/top, 1 right/bottom, 0 neither.
#[derive(Clone, Copy)]
struct Handle {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy)]
enum Drag {
    New { anchor: POINT, previous: RECT },
    Move { start: POINT, orig: RECT },
    Resize { handle: Handle, orig: RECT },
}

struct Edit {
    field: Field,
    text: String,
    /// The first digit typed replaces the value instead of appending to it.
    fresh: bool,
}

/// What to capture, in overlay coordinates.
enum Target {
    Rect(RECT),
    Window(HWND, RECT),
}

enum Action {
    None,
    BeginDrag,
    Capture(Target),
    Close,
}

struct Fonts {
    label: HFONT,
    value: HFONT,
    item: HFONT,
}

impl Fonts {
    unsafe fn new(scale: f32) -> Self {
        Self {
            label: font(12.0 * scale, 400),
            value: font(15.0 * scale, 600),
            item: font(13.0 * scale, 400),
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            for f in [self.label, self.value, self.item] {
                let _ = DeleteObject(f);
            }
        }
    }
}

unsafe fn font(px: f32, weight: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -(px.round() as i32),
        lfWeight: weight,
        lfQuality: ANTIALIASED_QUALITY,
        ..Default::default()
    };
    for (d, s) in lf.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
        *d = s;
    }
    CreateFontIndirectW(&lf)
}

/// Toolbar geometry in overlay coordinates.
struct Layout {
    left: RECT,
    buttons: [RECT; 3],
    right: RECT,
    fields: [RECT; 2],
    times: RECT,
    divider: RECT,
    aspect: RECT,
    dropdown: RECT,
    items: Vec<RECT>,
}

impl Layout {
    /// Centres the toolbar near the bottom of the given work area.
    fn new(work: RECT, scale: f32) -> Self {
        let px = |v: f32| (v * scale).round() as i32;
        let rect = |l: i32, t: i32, w: i32, h: i32| RECT {
            left: l,
            top: t,
            right: l + w,
            bottom: t + h,
        };

        let pad = px(6.0);
        let (bw, bh, bgap) = (px(78.0), px(52.0), px(2.0));
        let panel_h = bh + 2 * pad;
        let left_w = 3 * bw + 2 * bgap + 2 * pad;

        let (fw, fh) = (px(64.0), px(32.0));
        let (times_w, gap) = (px(22.0), px(10.0));
        let divider_w = px(1.0).max(1);
        let aspect_w = px(62.0);
        let right_w = px(12.0) + fw + times_w + fw + gap + divider_w + gap + aspect_w + px(8.0);

        let total = left_w + px(12.0) + right_w;
        let x0 = (work.left + work.right - total) / 2;
        let top = work.bottom - px(28.0) - panel_h;
        let cy = top + panel_h / 2;

        let left = rect(x0, top, left_w, panel_h);
        let buttons = [0, 1, 2].map(|i| rect(left.left + pad + i * (bw + bgap), top + pad, bw, bh));

        let right = rect(left.right + px(12.0), top, right_w, panel_h);
        let mut x = right.left + px(12.0);
        let field_w = rect(x, cy - fh / 2, fw, fh);
        x += fw;
        let times = rect(x, cy - fh / 2, times_w, fh);
        x += times_w;
        let field_h = rect(x, cy - fh / 2, fw, fh);
        x += fw + gap;
        let divider = rect(x, cy - px(16.0), divider_w, px(32.0));
        x += divider_w + gap;
        let aspect = rect(x, cy - fh / 2, aspect_w, fh);

        let (item_w, item_h) = (px(132.0), px(28.0));
        let dd_h = ASPECTS.len() as i32 * item_h + 2 * pad;
        let dropdown = rect(right.right - item_w - 2 * pad, top - px(8.0) - dd_h, item_w + 2 * pad, dd_h);
        let items = (0..ASPECTS.len() as i32)
            .map(|i| rect(dropdown.left + pad, dropdown.top + pad + i * item_h, item_w, item_h))
            .collect();

        Self {
            left,
            buttons,
            right,
            fields: [field_w, field_h],
            times,
            divider,
            aspect,
            dropdown,
            items,
        }
    }
}

struct Menu {
    hwnd: HWND,
    /// Virtual-screen position of the overlay's top-left corner.
    origin: POINT,
    width: i32,
    height: i32,
    /// Taken out while drawing so the rest of the state can be borrowed.
    canvas: Option<Dib>,
    tint: u32,
    scale: f32,
    monitor: RECT,
    layout: Layout,
    fonts: Fonts,
    mode: Mode,
    area: RECT,
    aspect: usize,
    drag: Option<Drag>,
    hover: Hit,
    edit: Option<Edit>,
    dropdown: bool,
    windows: Vec<(HWND, RECT)>,
    window: Option<usize>,
    /// Bounds of everything drawn last frame, so it can be erased.
    drawn: RECT,
    dirty: bool,
}

impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Menu {
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    fn bounds(&self) -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: self.width,
            bottom: self.height,
        }
    }

    fn ratio(&self) -> Option<(i32, i32)> {
        ASPECTS[self.aspect].1
    }

    /// The rectangle currently shown with the selection tint and border.
    fn highlight(&self) -> Option<RECT> {
        match self.mode {
            Mode::Area => Some(self.area),
            Mode::Window => self
                .window
                .map(|i| intersect(self.windows[i].1, self.bounds()))
                .filter(|r| !is_empty(*r)),
        }
    }

    fn handle_radius(&self) -> f32 {
        4.5 * self.scale
    }

    /// Centres of the resize handles, on the selection's border pixels.
    fn handle_points(&self) -> Vec<(f32, f32)> {
        let r = self.area;
        let (l, t) = (r.left as f32 + 0.5, r.top as f32 + 0.5);
        let (rt, b) = (r.right as f32 - 0.5, r.bottom as f32 - 0.5);
        let (cx, cy) = ((l + rt) / 2.0, (t + b) / 2.0);
        let mut pts = vec![(l, t), (rt, t), (rt, b), (l, b)];
        let min_side = self.handle_radius() * 6.0;
        if rt - l >= min_side {
            pts.extend([(cx, t), (cx, b)]);
        }
        if b - t >= min_side {
            pts.extend([(l, cy), (rt, cy)]);
        }
        pts
    }

    fn decor_bounds(&self) -> RECT {
        let pad = self.handle_radius().ceil() as i32 + 2;
        let mut r = self.highlight().map(|h| inflate(h, pad)).unwrap_or_default();
        r = union(r, union(self.layout.left, self.layout.right));
        if self.dropdown {
            r = union(r, self.layout.dropdown);
        }
        r
    }

    unsafe fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let now = self.decor_bounds();
        let dirty = intersect(union(self.drawn, now), self.bounds());
        self.drawn = now;
        if is_empty(dirty) {
            return;
        }
        let Some(mut dib) = self.canvas.take() else {
            return;
        };
        {
            let mut c = Canvas::new(&mut dib, dirty);
            self.draw(&mut c);
        }
        self.present(&dib, dirty);
        self.canvas = Some(dib);
    }

    unsafe fn present(&self, dib: &Dib, dirty: RECT) {
        let size = SIZE {
            cx: self.width,
            cy: self.height,
        };
        let src = POINT::default();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: std::ptr::null(),
            psize: &size,
            hdcSrc: dib.dc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: &dirty,
        };
        let _ = UpdateLayeredWindowIndirect(self.hwnd, &info);
    }

    unsafe fn draw(&self, c: &mut Canvas) {
        c.fill(c.clip(), BACKGROUND);
        if let Some(r) = self.highlight() {
            c.fill(r, self.tint);
            c.frame(r, WHITE);
        }
        if self.mode == Mode::Area {
            let rad = self.handle_radius();
            for (x, y) in self.handle_points() {
                let dot = RectF {
                    l: x - rad,
                    t: y - rad,
                    r: x + rad,
                    b: y + rad,
                };
                c.round_rect(dot, rad, HANDLE_FILL);
                c.round_rect_stroke(dot, rad, 1.0, HANDLE_BORDER);
            }
        }
        self.draw_toolbar(c);
    }

    unsafe fn draw_toolbar(&self, c: &mut Canvas) {
        let l = &self.layout;
        let s = self.scale;
        let panel = |c: &mut Canvas, r: RECT, radius: f32| {
            c.round_rect(r.into(), radius, PANEL_FILL);
            c.round_rect_stroke(r.into(), radius, 1.0, PANEL_BORDER);
        };

        panel(c, l.left, 12.0 * s);
        for (i, &r) in l.buttons.iter().enumerate() {
            let active = (i == 0 && self.mode == Mode::Area) || (i == 2 && self.mode == Mode::Window);
            if active {
                c.round_rect(r.into(), 8.0 * s, BUTTON_ACTIVE);
            } else if self.hover == Hit::Button(i) {
                c.round_rect(r.into(), 8.0 * s, BUTTON_HOVER);
            }
            let cx = (r.left + r.right) as f32 / 2.0;
            draw_button_icon(c, i, cx, r.top as f32 + 18.0 * s, s);
            let label = RECT {
                top: r.top + self.px(31.0),
                bottom: r.bottom - self.px(3.0),
                ..r
            };
            c.text(BUTTONS[i], label, self.fonts.label, TEXT, DT_CENTER | DT_VCENTER);
        }

        panel(c, l.right, 12.0 * s);
        let editable = self.mode == Mode::Area;
        for (f, &r) in [Field::Width, Field::Height].iter().zip(&l.fields) {
            c.round_rect(r.into(), 6.0 * s, FIELD_FILL);
            if self.edit.as_ref().is_some_and(|e| e.field == *f) {
                c.round_rect_stroke(r.into(), 6.0 * s, 1.5 * s, ACCENT);
            } else if self.hover == Hit::Field(*f) {
                c.round_rect_stroke(r.into(), 6.0 * s, 1.0, FIELD_HOVER);
            }
            let color = if editable { TEXT } else { TEXT_DIM };
            c.text(&self.field_text(*f), r, self.fonts.value, color, DT_CENTER | DT_VCENTER);
        }
        c.text("x", l.times, self.fonts.label, TEXT_DIM, DT_CENTER | DT_VCENTER);
        c.blend_rect(l.divider, PANEL_BORDER);

        let a = l.aspect;
        if editable && (self.dropdown || self.hover == Hit::Aspect) {
            c.round_rect(a.into(), 8.0 * s, BUTTON_HOVER);
        }
        let color = if editable { TEXT } else { TEXT_DIM };
        let cy = (a.top + a.bottom) as f32 / 2.0;
        let chevron_x = a.right as f32 - 12.0 * s;
        if self.ratio().is_some() {
            let label = RECT {
                right: a.right - self.px(20.0),
                ..a
            };
            c.text(ASPECTS[self.aspect].0, label, self.fonts.item, color, DT_CENTER | DT_VCENTER);
        } else {
            draw_crop_icon(c, a.left as f32 + 22.0 * s, cy, s, color);
        }
        c.polyline(
            &[(chevron_x - 3.5 * s, cy - 1.5 * s), (chevron_x, cy + 2.0 * s), (chevron_x + 3.5 * s, cy - 1.5 * s)],
            1.5 * s,
            color,
        );

        if self.dropdown {
            panel(c, l.dropdown, 10.0 * s);
            for (i, &r) in l.items.iter().enumerate() {
                if self.hover == Hit::Item(i) {
                    c.round_rect(r.into(), 6.0 * s, BUTTON_HOVER);
                }
                let cy = (r.top + r.bottom) as f32 / 2.0;
                if i == self.aspect {
                    let x = r.left as f32 + 13.0 * s;
                    c.polyline(
                        &[(x - 4.0 * s, cy), (x - 1.2 * s, cy + 3.0 * s), (x + 4.5 * s, cy - 3.5 * s)],
                        1.6 * s,
                        TEXT,
                    );
                }
                let label = RECT {
                    left: r.left + self.px(28.0),
                    ..r
                };
                c.text(ASPECTS[i].0, label, self.fonts.item, TEXT, DT_LEFT | DT_VCENTER);
            }
        }
    }

    fn field_text(&self, f: Field) -> String {
        if let Some(e) = self.edit.as_ref().filter(|e| e.field == f) {
            return e.text.clone();
        }
        match self.highlight() {
            Some(r) => match f {
                Field::Width => (r.right - r.left).to_string(),
                Field::Height => (r.bottom - r.top).to_string(),
            },
            None => "-".into(),
        }
    }

    fn hit(&self, p: POINT) -> Hit {
        let l = &self.layout;
        if self.dropdown {
            if let Some(i) = l.items.iter().position(|r| contains(*r, p)) {
                return Hit::Item(i);
            }
            if contains(l.dropdown, p) {
                return Hit::Toolbar;
            }
        }
        if let Some(i) = l.buttons.iter().position(|r| contains(*r, p)) {
            return Hit::Button(i);
        }
        if self.mode == Mode::Area {
            if contains(l.fields[0], p) {
                return Hit::Field(Field::Width);
            }
            if contains(l.fields[1], p) {
                return Hit::Field(Field::Height);
            }
            if contains(l.aspect, p) {
                return Hit::Aspect;
            }
        }
        if contains(l.left, p) || contains(l.right, p) {
            return Hit::Toolbar;
        }
        Hit::None
    }

    /// The resize handle (or edge) under `p`, if any.
    fn handle_at(&self, p: POINT) -> Option<Handle> {
        let r = self.area;
        let reach = self.px(6.0);
        let (right, bottom) = (r.right - 1, r.bottom - 1);
        let in_x = p.x >= r.left - reach && p.x <= right + reach;
        let in_y = p.y >= r.top - reach && p.y <= bottom + reach;
        let pick = |lo: i32, hi: i32, v: i32| {
            let (dl, dh) = ((v - lo).abs(), (v - hi).abs());
            match (dl <= reach, dh <= reach) {
                (true, true) if dh < dl => 1,
                (true, _) => -1,
                (false, true) => 1,
                _ => 0,
            }
        };
        let x = if in_y { pick(r.left, right, p.x) } else { 0 };
        let y = if in_x { pick(r.top, bottom, p.y) } else { 0 };
        (x != 0 || y != 0).then_some(Handle { x, y })
    }

    fn cursor_at(&self, p: POINT) -> PCWSTR {
        if self.hit(p) != Hit::None || self.mode == Mode::Window {
            return IDC_ARROW;
        }
        match self.handle_at(p) {
            Some(h) => resize_cursor(h),
            None if contains(self.area, p) => IDC_SIZEALL,
            None => IDC_CROSS,
        }
    }

    fn area_target(&self) -> Action {
        Action::Capture(Target::Rect(self.area))
    }

    fn window_target(&self) -> Action {
        match self.window {
            Some(i) => Action::Capture(Target::Window(self.windows[i].0, self.windows[i].1)),
            None => Action::None,
        }
    }

    unsafe fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.edit = None;
        self.dropdown = false;
        self.window = None;
        if mode == Mode::Window {
            self.windows = list_windows(self.origin);
            let mut p = POINT::default();
            if GetCursorPos(&mut p).is_ok() {
                self.update_window(offset_point(p, self.origin, -1));
            }
        }
        self.dirty = true;
    }

    fn update_window(&mut self, p: POINT) {
        let w = match self.hit(p) {
            Hit::None => self.windows.iter().position(|(_, r)| contains(*r, p)),
            _ => None,
        };
        if w != self.window {
            self.window = w;
            self.dirty = true;
        }
    }

    fn set_aspect(&mut self, i: usize) {
        self.aspect = i;
        LAST_ASPECT.set(i);
        if let Some(ratio) = self.ratio() {
            self.area = apply_ratio(self.area, ratio, self.bounds());
        }
        self.dirty = true;
    }

    fn begin_edit(&mut self, field: Field) {
        self.commit_edit();
        self.edit = Some(Edit {
            field,
            text: self.field_text(field),
            fresh: true,
        });
        self.dirty = true;
    }

    fn commit_edit(&mut self) {
        if let Some(e) = self.edit.take() {
            if let Ok(v) = e.text.parse::<i32>() {
                self.set_size(e.field, v);
            }
            self.dirty = true;
        }
    }

    /// Resizes the area from its top-left corner, keeping it on screen.
    fn set_size(&mut self, field: Field, v: i32) {
        let b = self.bounds();
        let r = self.area;
        let (mut w, mut h) = (r.right - r.left, r.bottom - r.top);
        match field {
            Field::Width => w = v,
            Field::Height => h = v,
        }
        let (bw, bh) = (b.right - b.left, b.bottom - b.top);
        if let Some((rw, rh)) = self.ratio() {
            match field {
                Field::Width => h = div_round(w * rh, rw),
                Field::Height => w = div_round(h * rw, rh),
            }
            let k = (bw as f32 / w as f32).min(bh as f32 / h as f32);
            if k < 1.0 {
                w = (w as f32 * k) as i32;
                h = (h as f32 * k) as i32;
            }
        }
        self.area = place(r.left, r.top, w, h, b);
    }

    unsafe fn mouse_down(&mut self, p: POINT) -> Action {
        let hit = self.hit(p);
        if self.dropdown {
            self.dropdown = false;
            self.dirty = true;
            if let Hit::Item(i) = hit {
                self.set_aspect(i);
            }
            return Action::None;
        }
        if !matches!(hit, Hit::Field(_)) {
            self.commit_edit();
        }
        match hit {
            Hit::Button(0) if self.mode == Mode::Area => self.area_target(),
            Hit::Button(0) => {
                self.set_mode(Mode::Area);
                Action::None
            }
            Hit::Button(1) => Action::Capture(Target::Rect(self.monitor)),
            Hit::Button(_) => {
                let mode = if self.mode == Mode::Window { Mode::Area } else { Mode::Window };
                self.set_mode(mode);
                Action::None
            }
            Hit::Field(f) => {
                self.begin_edit(f);
                Action::None
            }
            Hit::Aspect => {
                self.dropdown = true;
                self.dirty = true;
                Action::None
            }
            Hit::Toolbar | Hit::Item(_) => Action::None,
            Hit::None if self.mode == Mode::Window => self.window_target(),
            Hit::None => {
                let orig = self.area;
                self.drag = Some(match self.handle_at(p) {
                    Some(handle) => Drag::Resize { handle, orig },
                    None if contains(orig, p) => Drag::Move { start: p, orig },
                    None => Drag::New {
                        anchor: p,
                        previous: orig,
                    },
                });
                Action::BeginDrag
            }
        }
    }

    fn mouse_move(&mut self, p: POINT) {
        let b = self.bounds();
        let ratio = self.ratio();
        let hit = if self.drag.is_some() { Hit::None } else { self.hit(p) };
        if hit != self.hover {
            self.hover = hit;
            self.dirty = true;
        }
        match self.drag {
            Some(Drag::New { anchor, .. }) => self.area = span(anchor, p, ratio, b),
            Some(Drag::Move { start, orig }) => {
                let (w, h) = (orig.right - orig.left, orig.bottom - orig.top);
                self.area = place(orig.left + p.x - start.x, orig.top + p.y - start.y, w, h, b);
            }
            Some(Drag::Resize { handle, orig }) => self.area = resize(orig, handle, p, ratio, b),
            None => {}
        }
        if self.drag.is_some() {
            self.dirty = true;
        }
        if self.mode == Mode::Window {
            self.update_window(p);
        }
    }

    fn mouse_up(&mut self) {
        if let Some(Drag::New { previous, .. }) = self.drag.take() {
            // A click without a drag keeps the existing area.
            if self.area.right - self.area.left <= MIN_SIZE || self.area.bottom - self.area.top <= MIN_SIZE {
                self.area = previous;
            }
        }
        self.dirty = true;
    }

    unsafe fn key_down(&mut self, key: VIRTUAL_KEY) -> Action {
        match key {
            VK_ESCAPE => {
                if self.dropdown {
                    self.dropdown = false;
                } else if self.edit.is_some() {
                    self.edit = None;
                } else if self.mode == Mode::Window {
                    self.set_mode(Mode::Area);
                } else {
                    return Action::Close;
                }
                self.dirty = true;
                Action::None
            }
            VK_RETURN if self.edit.is_some() => {
                self.commit_edit();
                Action::None
            }
            VK_RETURN if self.mode == Mode::Area => self.area_target(),
            VK_RETURN => self.window_target(),
            VK_TAB => {
                if let Some(field) = self.edit.as_ref().map(|e| e.field.other()) {
                    self.begin_edit(field);
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn char_typed(&mut self, ch: u32) {
        let Some(e) = self.edit.as_mut() else {
            return;
        };
        match char::from_u32(ch) {
            Some(d @ '0'..='9') => {
                if e.fresh {
                    e.text.clear();
                    e.fresh = false;
                }
                if e.text.len() < 5 {
                    e.text.push(d);
                }
            }
            Some('\u{8}') => {
                e.fresh = false;
                e.text.pop();
            }
            _ => return,
        }
        self.dirty = true;
    }
}

fn div_round(n: i32, d: i32) -> i32 {
    (n + d / 2) / d
}

fn offset_point(p: POINT, by: POINT, sign: i32) -> POINT {
    POINT {
        x: p.x + sign * by.x,
        y: p.y + sign * by.y,
    }
}

fn clamp_point(p: POINT, b: RECT) -> POINT {
    POINT {
        x: p.x.clamp(b.left, b.right - 1),
        y: p.y.clamp(b.top, b.bottom - 1),
    }
}

/// A `w` x `h` rectangle at (x, y), shrunk and shifted to fit inside `b`.
fn place(x: i32, y: i32, w: i32, h: i32, b: RECT) -> RECT {
    let w = w.clamp(MIN_SIZE, b.right - b.left);
    let h = h.clamp(MIN_SIZE, b.bottom - b.top);
    let left = x.clamp(b.left, b.right - w);
    let top = y.clamp(b.top, b.bottom - h);
    RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

/// The rectangle dragged out from anchor pixel `a` to cursor pixel `c`
/// (both included), optionally locked to an aspect ratio.
fn span(a: POINT, c: POINT, ratio: Option<(i32, i32)>, b: RECT) -> RECT {
    let c = clamp_point(c, b);
    let sx = if c.x < a.x { -1 } else { 1 };
    let sy = if c.y < a.y { -1 } else { 1 };
    let (mut w, mut h) = ((c.x - a.x).abs() + 1, (c.y - a.y).abs() + 1);
    if let Some((rw, rh)) = ratio {
        let room_w = if sx < 0 { a.x - b.left + 1 } else { b.right - a.x };
        let room_h = if sy < 0 { a.y - b.top + 1 } else { b.bottom - a.y };
        w = w.max(div_round(h * rw, rh)).min(room_w);
        h = div_round(w * rh, rw);
        if h > room_h {
            h = room_h;
            w = div_round(h * rw, rh);
        }
    }
    let (x2, y2) = (a.x + sx * (w - 1), a.y + sy * (h - 1));
    RECT {
        left: a.x.min(x2),
        top: a.y.min(y2),
        right: a.x.max(x2) + 1,
        bottom: a.y.max(y2) + 1,
    }
}

fn resize(orig: RECT, h: Handle, p: POINT, ratio: Option<(i32, i32)>, b: RECT) -> RECT {
    let p = clamp_point(p, b);
    if ratio.is_some() && h.x != 0 && h.y != 0 {
        let anchor = POINT {
            x: if h.x < 0 { orig.right - 1 } else { orig.left },
            y: if h.y < 0 { orig.bottom - 1 } else { orig.top },
        };
        return span(anchor, p, ratio, b);
    }
    let mut r = orig;
    match h.x {
        -1 => r.left = p.x.min(r.right - MIN_SIZE),
        1 => r.right = (p.x + 1).max(r.left + MIN_SIZE),
        _ => {}
    }
    match h.y {
        -1 => r.top = p.y.min(r.bottom - MIN_SIZE),
        1 => r.bottom = (p.y + 1).max(r.top + MIN_SIZE),
        _ => {}
    }
    let Some((rw, rh)) = ratio else {
        return r;
    };
    // An edge handle with a locked ratio: derive the other side, centred.
    if h.x != 0 {
        let mut w = r.right - r.left;
        let mut hh = div_round(w * rh, rw);
        if hh > b.bottom - b.top {
            hh = b.bottom - b.top;
            w = div_round(hh * rw, rh);
            if h.x < 0 {
                r.left = r.right - w;
            } else {
                r.right = r.left + w;
            }
        }
        let cy = (orig.top + orig.bottom) / 2;
        r.top = (cy - hh / 2).clamp(b.top, b.bottom - hh);
        r.bottom = r.top + hh;
    } else {
        let mut hh = r.bottom - r.top;
        let mut w = div_round(hh * rw, rh);
        if w > b.right - b.left {
            w = b.right - b.left;
            hh = div_round(w * rh, rw);
            if h.y < 0 {
                r.top = r.bottom - hh;
            } else {
                r.bottom = r.top + hh;
            }
        }
        let cx = (orig.left + orig.right) / 2;
        r.left = (cx - w / 2).clamp(b.left, b.right - w);
        r.right = r.left + w;
    }
    r
}

/// Reshapes `r` to the ratio around its centre, keeping roughly its area.
fn apply_ratio(r: RECT, (rw, rh): (i32, i32), b: RECT) -> RECT {
    let area = ((r.right - r.left) as f32) * ((r.bottom - r.top) as f32);
    let mut w = (area * rw as f32 / rh as f32).sqrt();
    let mut h = w * rh as f32 / rw as f32;
    let k = ((b.right - b.left) as f32 / w).min((b.bottom - b.top) as f32 / h);
    if k < 1.0 {
        w *= k;
        h *= k;
    }
    let (w, h) = (w as i32, h as i32);
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    place(cx - w / 2, cy - h / 2, w, h, b)
}

fn resize_cursor(h: Handle) -> PCWSTR {
    match (h.x, h.y) {
        (-1, -1) | (1, 1) => IDC_SIZENWSE,
        (1, -1) | (-1, 1) => IDC_SIZENESW,
        (_, 0) => IDC_SIZEWE,
        _ => IDC_SIZENS,
    }
}

fn draw_button_icon(c: &mut Canvas, i: usize, cx: f32, cy: f32, s: f32) {
    let t = 1.6 * s;
    match i {
        0 => {
            let (h, arm) = (8.0 * s, 4.5 * s);
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let (x, y) = (cx + sx * h, cy + sy * h);
                c.polyline(&[(x, y - sy * arm), (x, y), (x - sx * arm, y)], t, TEXT);
            }
        }
        1 => {
            let screen = RectF {
                l: cx - 9.5 * s,
                t: cy - 8.0 * s,
                r: cx + 9.5 * s,
                b: cy + 5.0 * s,
            };
            c.round_rect_stroke(screen, 2.0 * s, t, TEXT);
            c.polyline(&[(cx, cy + 5.0 * s), (cx, cy + 8.0 * s)], t, TEXT);
            c.polyline(&[(cx - 4.5 * s, cy + 8.5 * s), (cx + 4.5 * s, cy + 8.5 * s)], t, TEXT);
        }
        _ => {
            let frame = RectF {
                l: cx - 9.5 * s,
                t: cy - 7.5 * s,
                r: cx + 9.5 * s,
                b: cy + 7.5 * s,
            };
            c.round_rect_stroke(frame, 2.5 * s, t, TEXT);
            c.polyline(&[(cx - 9.0 * s, cy - 3.0 * s), (cx + 9.0 * s, cy - 3.0 * s)], t, TEXT);
        }
    }
}

fn draw_crop_icon(c: &mut Canvas, cx: f32, cy: f32, s: f32, color: Color) {
    let t = 1.6 * s;
    let (a, b) = (4.0 * s, 8.0 * s);
    c.polyline(&[(cx - a, cy - b), (cx - a, cy + a), (cx + b, cy + a)], t, color);
    c.polyline(&[(cx - b, cy - a), (cx + a, cy - a), (cx + a, cy + b)], t, color);
}

/// Visible top-level windows of other processes, front to back, in overlay
/// coordinates.
unsafe fn list_windows(origin: POINT) -> Vec<(HWND, RECT)> {
    let mut found: Vec<(HWND, RECT)> = Vec::new();
    let _ = EnumWindows(Some(collect_window), LPARAM(&mut found as *mut _ as isize));
    found
        .into_iter()
        .map(|(hwnd, r)| (hwnd, offset(r, -origin.x, -origin.y)))
        .collect()
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = &mut *(lparam.0 as *mut Vec<(HWND, RECT)>);
    if let Some(r) = window_bounds(hwnd) {
        found.push((hwnd, r));
    }
    TRUE
}

unsafe fn window_bounds(hwnd: HWND) -> Option<RECT> {
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return None;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return None;
    }
    // Click-through overlays (e.g. GPU or recording HUDs) aren't real targets.
    if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 != 0 {
        return None;
    }
    // Suspended UWP apps and windows on other virtual desktops are cloaked.
    let mut cloaked = 0u32;
    let size = std::mem::size_of::<u32>() as u32;
    if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut c_void, size).is_ok()
        && cloaked != 0
    {
        return None;
    }
    let mut class = [0u16; 16];
    let n = GetClassNameW(hwnd, &mut class).max(0) as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if class == "Progman" || class == "WorkerW" {
        return None;
    }
    // The extended frame excludes the invisible resize borders.
    let mut r = RECT::default();
    let size = std::mem::size_of::<RECT>() as u32;
    if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as *mut c_void, size).is_err() {
        GetWindowRect(hwnd, &mut r).ok()?;
    }
    (!is_empty(r)).then_some(r)
}

/// Runs `f` on the open menu, if any. `try_borrow_mut` guards against
/// re-entrant window messages arriving while the menu is already borrowed.
fn with_menu<R>(f: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    MENU.with(|m| m.try_borrow_mut().ok()?.as_mut().map(f))
}

pub fn is_open() -> bool {
    MENU.with(|m| m.try_borrow().map_or(true, |m| m.is_some()))
}

pub unsafe fn register(instance: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSW {
        hInstance: instance,
        lpszClassName: CLASS_NAME,
        lpfnWndProc: Some(wndproc),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32());
    }
    Ok(())
}

/// Opens the capture menu, reporting any failure to the user.
pub fn open() {
    if is_open() || crate::overlay::is_open() {
        return;
    }
    let result = unsafe { GetModuleHandleW(None).and_then(|m| start(m.into())) };
    if let Err(e) = result {
        crate::show_error(&format!("Failed to open capture menu:\n\n{e}"));
    }
}

pub fn close() {
    finish(None);
}

unsafe fn start(instance: HINSTANCE) -> Result<()> {
    let origin = POINT {
        x: GetSystemMetrics(SM_XVIRTUALSCREEN),
        y: GetSystemMetrics(SM_YVIRTUALSCREEN),
    };
    let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let bounds = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };

    let mut cursor = POINT::default();
    GetCursorPos(&mut cursor)?;
    let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return Err(Error::from_win32());
    }
    let (mut dpi, mut dpi_y) = (96, 96);
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
    let scale = dpi as f32 / 96.0;
    let to_local = |r: RECT| offset(r, -origin.x, -origin.y);
    let monitor_rect = to_local(info.rcMonitor);

    let aspect = LAST_ASPECT.get();
    let area = LAST_AREA
        .get()
        .map(to_local)
        .filter(|r| intersect(*r, bounds) == *r && !is_empty(*r))
        .unwrap_or_else(|| {
            let (mw, mh) = (monitor_rect.right - monitor_rect.left, monitor_rect.bottom - monitor_rect.top);
            let r = place(monitor_rect.left + mw / 4, monitor_rect.top + mh / 4, mw / 2, mh / 2, bounds);
            match ASPECTS[aspect].1 {
                Some(ratio) => apply_ratio(r, ratio, bounds),
                None => r,
            }
        });

    let canvas = Dib::new(HDC::default(), width, height)?;
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS_NAME,
        w!("WinShotX"),
        WS_POPUP,
        origin.x,
        origin.y,
        width,
        height,
        None,
        None,
        instance,
        None,
    )?;

    let mut menu = Menu {
        hwnd,
        origin,
        width,
        height,
        canvas: Some(canvas),
        tint: blend(BACKGROUND, SELECTION, 255),
        scale,
        monitor: monitor_rect,
        layout: Layout::new(to_local(info.rcWork), scale),
        fonts: Fonts::new(scale),
        mode: Mode::Area,
        area,
        aspect,
        drag: None,
        hover: Hit::None,
        edit: None,
        dropdown: false,
        windows: Vec::new(),
        window: None,
        drawn: bounds,
        dirty: true,
    };
    menu.refresh();
    MENU.with_borrow_mut(|m| *m = Some(menu));

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    Ok(())
}

/// Closes the menu and, given a target, captures it from the live screen.
fn finish(target: Option<Target>) {
    let Some(menu) = MENU.with(|m| m.try_borrow_mut().ok().and_then(|mut m| m.take())) else {
        return;
    };
    let origin = menu.origin;
    let virtual_screen = offset(menu.bounds(), origin.x, origin.y);
    LAST_AREA.set(Some(offset(menu.area, origin.x, origin.y)));
    drop(menu);

    let Some(target) = target else {
        return;
    };
    let rect = match target {
        Target::Rect(r) | Target::Window(_, r) => intersect(offset(r, origin.x, origin.y), virtual_screen),
    };
    unsafe {
        // Wait until the compositor has drawn a frame without the overlay.
        let _ = DwmFlush();
        let _ = DwmFlush();
        if let Target::Window(hwnd, _) = target {
            // Raise the window so overlapping windows don't end up in the shot.
            if SetForegroundWindow(hwnd).as_bool() {
                Sleep(100);
                let _ = DwmFlush();
            }
        }
    }
    match capture::grab(rect) {
        Ok(image) => capture::deliver(&image),
        Err(e) => crate::show_error(&format!("Failed to capture screen:\n\n{e}")),
    }
}

/// Applies the outcome of an input event, then redraws if needed.
unsafe fn act(hwnd: HWND, action: Option<Action>) {
    match action {
        Some(Action::BeginDrag) => {
            SetCapture(hwnd);
        }
        Some(Action::Capture(target)) => finish(Some(target)),
        Some(Action::Close) => finish(None),
        Some(Action::None) | None => {}
    }
    with_menu(|m| m.refresh());
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                if let Some(id) = with_menu(|m| m.cursor_at(offset_point(p, m.origin, -1))) {
                    if let Ok(cursor) = LoadCursorW(None, id) {
                        SetCursor(cursor);
                    }
                    return LRESULT(1);
                }
            }
            WM_LBUTTONDOWN => {
                let p = point_from_lparam(lparam);
                let action = with_menu(|m| m.mouse_down(p));
                act(hwnd, action);
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let p = point_from_lparam(lparam);
                with_menu(|m| m.mouse_move(p));
                act(hwnd, None);
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                with_menu(|m| m.mouse_up());
                let _ = ReleaseCapture();
                act(hwnd, None);
                return LRESULT(0);
            }
            WM_RBUTTONDOWN => {
                let action = with_menu(|m| m.key_down(VK_ESCAPE));
                act(hwnd, action);
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let action = with_menu(|m| m.key_down(VIRTUAL_KEY(wparam.0 as u16)));
                act(hwnd, action);
                return LRESULT(0);
            }
            WM_CHAR => {
                with_menu(|m| m.char_typed(wparam.0 as u32));
                act(hwnd, None);
                return LRESULT(0);
            }
            WM_KILLFOCUS => {
                // Another window took focus (e.g. Alt+Tab); don't leave a
                // topmost overlay behind.
                finish(None);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

//! Alt+Shift+5 capture menu. Unlike the Alt+Shift+4 overlay the desktop stays
//! live: a layered window draws an editable area selection and a toolbar with
//! Area, Fullscreen and Window capture, screen recording, size fields and
//! aspect-ratio presets.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, TRUE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, Sleep};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_ESCAPE, VK_RETURN, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::audio::{self, Device};
use crate::capture;
use crate::gfx::{
    contains, inflate, intersect, is_empty, offset, point_from_lparam, rgba, text_width,
    toolbar_origin, union, Canvas, Color, Dib, Inside, RectF,
};
use crate::record::{Quality, Settings};

const CLASS_NAME: PCWSTR = w!("WinShotXMenu");

/// Fully transparent pixels of a layered window let clicks through to the
/// windows below, so "empty" overlay pixels get the lowest visible alpha.
const BACKGROUND: u32 = 0x0100_0000;
/// Darkens everything outside the selection (premultiplied black).
const DIM: u32 = 0x7A00_0000;

pub(crate) const PANEL_FILL: Color = rgba(0x121212, 255);
pub(crate) const PANEL_BORDER: Color = rgba(0x3A3A3A, 255);
const BUTTON_ACTIVE: Color = rgba(0x383838, 255);
pub(crate) const BUTTON_HOVER: Color = rgba(0x262626, 255);
const FIELD_FILL: Color = rgba(0x222222, 255);
const FIELD_HOVER: Color = rgba(0x4A4A4A, 255);
const ACCENT: Color = rgba(0x2D8CFF, 255);
pub(crate) const TEXT: Color = rgba(0xFFFFFF, 235);
const TEXT_DIM: Color = rgba(0xFFFFFF, 130);
const HANDLE_FILL: Color = rgba(0xFFFFFF, 255);
const HANDLE_SHADOW: Color = rgba(0x000000, 70);
/// Handle stroke thickness, corner arm length and edge bar length, in
/// logical pixels.
const HANDLE_THICKNESS: f32 = 4.0;
const HANDLE_ARM: f32 = 18.0;
const HANDLE_BAR: f32 = 24.0;

const RECORD_RED: Color = rgba(0xFF5A52, 255);

const MIN_SIZE: i32 = 16;
const TOOLTIPS: [&str; 4] = [
    "Capture Area",
    "Capture Fullscreen",
    "Capture Window",
    "Record Screen",
];
const TOOLTIP_TIMER: usize = 1;
pub(crate) const TOOLTIP_DELAY_MS: u32 = 400;
const NO_MIC: &str = "Do not record microphone";
const NO_MICS_FOUND: &str = "No microphones found";

/// Buttons on the recording island; the last one starts recording.
const REC_SYSTEM_AUDIO: usize = 0;
const REC_MIC: usize = 1;
const REC_QUALITY: usize = 2;
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
    static LAST_SYSTEM_AUDIO: Cell<bool> = const { Cell::new(false) };
    /// Endpoint ID of the microphone last chosen for recording.
    static LAST_MIC: RefCell<Option<String>> = const { RefCell::new(None) };
    static LAST_QUALITY: Cell<Quality> = const { Cell::new(Quality::Medium) };
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Area,
    Window,
    Record,
}

#[derive(Clone, Copy, PartialEq)]
enum Popup {
    Aspect,
    Mic,
    Quality,
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
    RecButton(usize),
    /// 0 is "Do not record microphone", then one per microphone.
    MicItem(usize),
    /// Index into `Quality::ALL`.
    QualityItem(usize),
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
    Record(Settings),
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
            value: font(14.0 * scale, 600),
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

pub(crate) unsafe fn font(px: f32, weight: i32) -> HFONT {
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
    buttons: [RECT; 4],
    right: RECT,
    fields: [RECT; 2],
    times: RECT,
    divider: RECT,
    aspect: RECT,
    dropdown: RECT,
    items: Vec<RECT>,
    /// Shown to the right of the others in Record mode, without recentring
    /// them, so the buttons don't jump when switching modes.
    record: RECT,
    record_buttons: [RECT; 4],
    record_divider: RECT,
    /// The rectangle the toolbar is attached to.
    target: RECT,
}

impl Layout {
    /// Two pill-shaped islands attached to `target` (see `toolbar_origin`),
    /// plus the Record island for Record mode (see `record_origin`).
    fn new(target: RECT, work: RECT, scale: f32) -> Self {
        let px = |v: f32| (v * scale).round() as i32;
        let rect = |l: i32, t: i32, w: i32, h: i32| RECT {
            left: l,
            top: t,
            right: l + w,
            bottom: t + h,
        };

        let pad = px(6.0);
        let (btn, bgap) = (px(36.0), px(4.0));
        let panel_h = btn + 2 * pad;
        let left_w = 4 * btn + 3 * bgap + 2 * pad;

        let (fw, fh) = (px(60.0), px(32.0));
        let times_w = px(18.0);
        let divider_w = px(1.0).max(1);
        let right_w =
            (panel_h - fh) / 2 + fw + times_w + fw + px(8.0) + divider_w + px(6.0) + btn + pad;

        let island_gap = px(10.0);
        let total = left_w + island_gap + right_w;
        let POINT { x: x0, y: top } =
            toolbar_origin(target, work, total, panel_h, total, Inside::Centre, scale);
        let cy = top + panel_h / 2;
        // Popups open above the toolbar, or below it if there's no room.
        let popup_top = |h: i32| {
            let above = top - px(10.0) - h;
            if above >= work.top {
                above
            } else {
                top + panel_h + px(10.0)
            }
        };

        let left = rect(x0, top, left_w, panel_h);
        let buttons =
            [0, 1, 2, 3].map(|i| rect(left.left + pad + i * (btn + bgap), top + pad, btn, btn));

        let right = rect(left.right + island_gap, top, right_w, panel_h);
        // Inset the first field so its rounded end sits concentric with the island's.
        let mut x = right.left + (panel_h - fh) / 2;
        let field_w = rect(x, cy - fh / 2, fw, fh);
        x += fw;
        let times = rect(x, cy - fh / 2, times_w, fh);
        x += times_w;
        let field_h = rect(x, cy - fh / 2, fw, fh);
        x += fw + px(8.0);
        let divider = rect(x, cy - px(12.0), divider_w, px(24.0));
        x += divider_w + px(6.0);
        let aspect = rect(x, top + pad, btn, btn);

        let (item_w, item_h) = (px(132.0), px(28.0));
        let dd_h = ASPECTS.len() as i32 * item_h + 2 * pad;
        let dropdown = rect(
            right.right - item_w - 2 * pad,
            popup_top(dd_h),
            item_w + 2 * pad,
            dd_h,
        );
        let items = (0..ASPECTS.len() as i32)
            .map(|i| {
                rect(
                    dropdown.left + pad,
                    dropdown.top + pad + i * item_h,
                    item_w,
                    item_h,
                )
            })
            .collect();

        let record_w = 2 * pad + 4 * btn + 2 * bgap + px(12.0) + divider_w;
        let POINT { x: rx, y: ry } =
            record_origin(target, work, union(left, right), record_w, panel_h, scale);
        let record = rect(rx, ry, record_w, panel_h);
        let (rtop, rcy) = (ry + pad, ry + panel_h / 2);
        let mut x = record.left + pad;
        let system_audio = rect(x, rtop, btn, btn);
        x += btn + bgap;
        let mic = rect(x, rtop, btn, btn);
        x += btn + bgap;
        let quality = rect(x, rtop, btn, btn);
        x += btn + px(6.0);
        let record_divider = rect(x, rcy - px(12.0), divider_w, px(24.0));
        x += divider_w + px(6.0);
        let start = rect(x, rtop, btn, btn);

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
            record,
            record_buttons: [system_audio, mic, quality, start],
            record_divider,
            target,
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
    scale: f32,
    monitor: RECT,
    /// Work area of the monitor, in overlay coordinates.
    work: RECT,
    layout: Layout,
    fonts: Fonts,
    mode: Mode,
    area: RECT,
    aspect: usize,
    drag: Option<Drag>,
    hover: Hit,
    edit: Option<Edit>,
    dropdown: Option<Popup>,
    system_audio: bool,
    mic: Option<String>,
    mics: Vec<Device>,
    /// Microphone menu geometry: the panel, its rows and the separator.
    mic_popup: RECT,
    mic_items: Vec<RECT>,
    mic_separator: RECT,
    quality: Quality,
    /// Quality menu geometry: the panel and its rows.
    quality_popup: RECT,
    quality_items: Vec<RECT>,
    windows: Vec<(HWND, RECT)>,
    window: Option<usize>,
    /// The control whose tooltip is showing (shown after hovering a moment).
    tooltip: Option<Hit>,
    /// Bounds of everything drawn last frame, so it can be erased.
    drawn: RECT,
    dirty: bool,
    /// A capture or close triggered by a button press, applied on release so
    /// the release doesn't reach whatever is under the menu once it closes.
    pending: Option<Action>,
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

    /// The rectangle left undimmed.
    fn highlight(&self) -> Option<RECT> {
        match self.mode {
            Mode::Area | Mode::Record => Some(self.area),
            Mode::Window => self
                .window
                .map(|i| intersect(self.windows[i].1, self.bounds()))
                .filter(|r| !is_empty(*r)),
        }
    }

    /// The resize handles, each as rounded-rectangle parts centred on the
    /// selection's edges: L-shaped brackets with square corners at the
    /// corners, and bars at the middle of each edge with room for one.
    /// Every free end is rounded.
    fn handle_shapes(&self) -> Vec<Vec<(RectF, f32)>> {
        let r = self.area;
        let (l, t, rt, b) = (r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
        let (w, h) = (rt - l, b - t);
        let s = self.scale;
        let (th, bar) = (HANDLE_THICKNESS * s, HANDLE_BAR * s);
        let half = th / 2.0;
        let arm_x = (HANDLE_ARM * s).min(w / 2.0);
        let arm_y = (HANDLE_ARM * s).min(h / 2.0);
        let span = |a: f32, b: f32| (a.min(b), a.max(b));
        let rect = |(l, r): (f32, f32), (t, b): (f32, f32)| RectF { l, t, r, b };

        let mut shapes = Vec::new();
        for (x, dx) in [(l, 1.0), (rt, -1.0)] {
            for (y, dy) in [(t, 1.0), (b, -1.0)] {
                let across_x = span(x - half, x + half);
                let across_y = span(y - half, y + half);
                shapes.push(vec![
                    (rect(across_x, across_y), 0.0),
                    (rect(span(x - dx * half, x + dx * arm_x), across_y), half),
                    (rect(across_x, span(y - dy * half, y + dy * arm_y)), half),
                ]);
            }
        }
        let (cx, cy) = ((l + rt) / 2.0, (t + b) / 2.0);
        let min_side = 2.0 * HANDLE_ARM * s + bar + 16.0 * s;
        if w >= min_side {
            for y in [t, b] {
                shapes.push(vec![(
                    rect(
                        span(cx - bar / 2.0, cx + bar / 2.0),
                        span(y - half, y + half),
                    ),
                    half,
                )]);
            }
        }
        if h >= min_side {
            for x in [l, rt] {
                shapes.push(vec![(
                    rect(
                        span(x - half, x + half),
                        span(cy - bar / 2.0, cy + bar / 2.0),
                    ),
                    half,
                )]);
            }
        }
        shapes
    }

    fn decor_bounds(&self) -> RECT {
        let pad = (HANDLE_THICKNESS * self.scale / 2.0).ceil() as i32 + 3;
        let mut r = self
            .highlight()
            .map(|h| inflate(h, pad))
            .unwrap_or_default();
        r = union(r, union(self.layout.left, self.layout.right));
        if self.mode == Mode::Record {
            r = union(r, self.layout.record);
        }
        match self.dropdown {
            Some(Popup::Aspect) => r = union(r, self.layout.dropdown),
            Some(Popup::Mic) => r = union(r, self.mic_popup),
            Some(Popup::Quality) => r = union(r, self.quality_popup),
            None => {}
        }
        if let Some((tip, _)) = self.tooltip_rect() {
            r = union(r, tip);
        }
        r
    }

    /// Whether the area can be edited (it's what gets captured).
    fn editable(&self) -> bool {
        self.mode != Mode::Window
    }

    /// Index into `mics` of the chosen microphone, if it's still present.
    fn mic_index(&self) -> Option<usize> {
        let id = self.mic.as_ref()?;
        self.mics.iter().position(|d| &d.id == id)
    }

    fn tooltip_text(&self, hit: Hit) -> Option<String> {
        if self.dropdown.is_some() {
            return None;
        }
        match hit {
            Hit::Button(i) => Some(TOOLTIPS[i].to_string()),
            Hit::RecButton(REC_SYSTEM_AUDIO) => Some(format!(
                "System Audio: {}",
                if self.system_audio { "On" } else { "Off" }
            )),
            Hit::RecButton(REC_MIC) => Some(match self.mic_index() {
                Some(i) => format!("Microphone: {}", self.mics[i].name),
                None => "Microphone: Off".into(),
            }),
            Hit::RecButton(REC_QUALITY) => Some(format!("Quality: {}", self.quality.name())),
            Hit::RecButton(_) => Some("Start Recording".into()),
            Hit::Field(Field::Width) => Some("Width".into()),
            Hit::Field(Field::Height) => Some("Height".into()),
            Hit::Aspect => Some(format!("Aspect Ratio: {}", ASPECTS[self.aspect].0)),
            _ => None,
        }
    }

    /// The tooltip bubble, centred above the hovered control.
    fn tooltip_rect(&self) -> Option<(RECT, String)> {
        let hit = self.tooltip.filter(|h| *h == self.hover)?;
        let text = self.tooltip_text(hit)?;
        let l = &self.layout;
        let anchor = match hit {
            Hit::Button(i) => l.buttons[i],
            Hit::RecButton(i) => l.record_buttons[i],
            Hit::Field(Field::Width) => l.fields[0],
            Hit::Field(Field::Height) => l.fields[1],
            Hit::Aspect => l.aspect,
            _ => return None,
        };
        let w = text_width(self.fonts.label, &text) + 2 * self.px(10.0);
        let h = self.px(26.0);
        let left =
            ((anchor.left + anchor.right - w) / 2).clamp(self.monitor.left, self.monitor.right - w);
        let island = if matches!(hit, Hit::RecButton(_)) {
            l.record
        } else {
            l.left
        };
        let top = self.popup_top(island, h, self.px(8.0));
        let r = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
        Some((r, text))
    }

    /// Hides the tooltip on hover changes. Once one has been shown, moving
    /// to another control shows its tooltip immediately.
    fn schedule_tooltip(&mut self) {
        let was_showing = self.tooltip.take().is_some();
        if was_showing {
            self.dirty = true;
        }
        unsafe {
            if self.tooltip_text(self.hover).is_none() {
                let _ = KillTimer(self.hwnd, TOOLTIP_TIMER);
            } else if was_showing {
                self.tooltip = Some(self.hover);
            } else {
                SetTimer(self.hwnd, TOOLTIP_TIMER, TOOLTIP_DELAY_MS, None);
            }
        }
    }

    fn show_tooltip(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TOOLTIP_TIMER);
        }
        if self.drag.is_none() && self.tooltip_text(self.hover).is_some() {
            self.tooltip = Some(self.hover);
            self.dirty = true;
        }
    }

    fn hide_tooltip(&mut self) {
        unsafe {
            let _ = KillTimer(self.hwnd, TOOLTIP_TIMER);
        }
        if self.tooltip.take().is_some() {
            self.dirty = true;
        }
    }

    unsafe fn refresh(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.relayout();
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
        c.fill(c.clip(), DIM);
        if let Some(r) = self.highlight() {
            c.fill(r, BACKGROUND);
        }
        if self.editable() {
            for parts in self.handle_shapes() {
                // A faint outline keeps the handles visible over white content.
                c.round_rects(&parts, 1.0, HANDLE_SHADOW);
                c.round_rects(&parts, 0.0, HANDLE_FILL);
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
        let pill = |r: RECT| (r.bottom - r.top) as f32 / 2.0;
        let centre = |r: RECT| {
            (
                (r.left + r.right) as f32 / 2.0,
                (r.top + r.bottom) as f32 / 2.0,
            )
        };

        panel(c, l.left, pill(l.left));
        for (i, &r) in l.buttons.iter().enumerate() {
            let active = match i {
                0 => self.mode == Mode::Area,
                2 => self.mode == Mode::Window,
                3 => self.mode == Mode::Record,
                _ => false,
            };
            if active {
                c.round_rect(r.into(), pill(r), BUTTON_ACTIVE);
            } else if self.hover == Hit::Button(i) {
                c.round_rect(r.into(), pill(r), BUTTON_HOVER);
            }
            let (cx, cy) = centre(r);
            draw_button_icon(c, i, cx, cy, s);
        }

        panel(c, l.right, pill(l.right));
        let editable = self.editable();
        let color = if editable { TEXT } else { TEXT_DIM };
        for (f, &r) in [Field::Width, Field::Height].iter().zip(&l.fields) {
            c.round_rect(r.into(), pill(r), FIELD_FILL);
            if self.edit.as_ref().is_some_and(|e| e.field == *f) {
                c.round_rect_stroke(r.into(), pill(r), 1.5 * s, ACCENT);
            } else if self.hover == Hit::Field(*f) {
                c.round_rect_stroke(r.into(), pill(r), 1.0, FIELD_HOVER);
            }
            c.text(
                &self.field_text(*f),
                r,
                self.fonts.value,
                color,
                DT_CENTER | DT_VCENTER,
            );
        }
        c.text(
            "\u{00D7}",
            l.times,
            self.fonts.label,
            TEXT_DIM,
            DT_CENTER | DT_VCENTER,
        );
        c.blend_rect(l.divider, PANEL_BORDER);

        let a = l.aspect;
        if editable && self.dropdown == Some(Popup::Aspect) {
            c.round_rect(a.into(), pill(a), BUTTON_ACTIVE);
        } else if editable && self.hover == Hit::Aspect {
            c.round_rect(a.into(), pill(a), BUTTON_HOVER);
        }
        let (cx, cy) = centre(a);
        draw_crop_icon(c, cx, cy, s, color);
        if self.ratio().is_some() {
            // Badge marking a locked aspect ratio; the tooltip names it.
            let (bx, by, br) = (a.right as f32 - 8.0 * s, a.top as f32 + 8.0 * s, 3.5 * s);
            let dot = RectF {
                l: bx - br,
                t: by - br,
                r: bx + br,
                b: by + br,
            };
            c.round_rect(dot, br, ACCENT);
        }

        if self.mode == Mode::Record {
            panel(c, l.record, pill(l.record));
            for (i, &r) in l.record_buttons.iter().enumerate() {
                let on = match i {
                    REC_SYSTEM_AUDIO => self.system_audio,
                    REC_MIC => self.mic_index().is_some(),
                    _ => false,
                };
                let open = match i {
                    REC_MIC => self.dropdown == Some(Popup::Mic),
                    REC_QUALITY => self.dropdown == Some(Popup::Quality),
                    _ => false,
                };
                if on || open {
                    c.round_rect(r.into(), pill(r), BUTTON_ACTIVE);
                } else if self.hover == Hit::RecButton(i) {
                    c.round_rect(r.into(), pill(r), BUTTON_HOVER);
                }
                let (cx, cy) = centre(r);
                match i {
                    REC_SYSTEM_AUDIO => draw_speaker_icon(c, cx, cy, s, on),
                    REC_MIC => draw_mic_icon(c, cx, cy, s, on),
                    REC_QUALITY => draw_quality_icon(c, cx, cy, s, self.quality),
                    _ => {
                        let rad = 7.0 * s;
                        let dot = RectF {
                            l: cx - rad,
                            t: cy - rad,
                            r: cx + rad,
                            b: cy + rad,
                        };
                        c.round_rect(dot, rad, RECORD_RED);
                    }
                }
            }
            c.blend_rect(l.record_divider, PANEL_BORDER);
        }

        if let Some((r, text)) = self.tooltip_rect() {
            panel(c, r, pill(r));
            c.text(&text, r, self.fonts.label, TEXT, DT_CENTER | DT_VCENTER);
        }

        if self.dropdown == Some(Popup::Mic) {
            panel(c, self.mic_popup, 14.0 * s);
            c.blend_rect(self.mic_separator, PANEL_BORDER);
            let selected = self.mic_index().map_or(0, |i| i + 1);
            for (i, &r) in self.mic_items.iter().enumerate() {
                let empty = self.mics.is_empty() && i == 1;
                if self.hover == Hit::MicItem(i) && !empty {
                    c.round_rect(r.into(), 8.0 * s, BUTTON_HOVER);
                }
                if i == selected {
                    draw_check(c, r, s);
                }
                let label = RECT {
                    left: r.left + self.px(28.0),
                    right: r.right - self.px(8.0),
                    ..r
                };
                let (text, color) = match i {
                    0 => (NO_MIC, TEXT),
                    _ if empty => (NO_MICS_FOUND, TEXT_DIM),
                    _ => (self.mics[i - 1].name.as_str(), TEXT),
                };
                c.text(
                    text,
                    label,
                    self.fonts.item,
                    color,
                    DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
                );
            }
        }

        if self.dropdown == Some(Popup::Quality) {
            panel(c, self.quality_popup, 14.0 * s);
            for (i, (&r, q)) in self.quality_items.iter().zip(Quality::ALL).enumerate() {
                if self.hover == Hit::QualityItem(i) {
                    c.round_rect(r.into(), 8.0 * s, BUTTON_HOVER);
                }
                if q == self.quality {
                    draw_check(c, r, s);
                }
                let icon_x = r.left as f32 + 36.0 * s;
                draw_quality_icon(c, icon_x, (r.top + r.bottom) as f32 / 2.0, s, q);
                let label = RECT {
                    left: r.left + self.px(52.0),
                    right: r.right - self.px(10.0),
                    ..r
                };
                c.text(q.name(), label, self.fonts.item, TEXT, DT_LEFT | DT_VCENTER);
                c.text(
                    q.description(),
                    label,
                    self.fonts.label,
                    TEXT_DIM,
                    DT_RIGHT | DT_VCENTER,
                );
            }
        }

        if self.dropdown == Some(Popup::Aspect) {
            panel(c, l.dropdown, 14.0 * s);
            for (i, &r) in l.items.iter().enumerate() {
                if self.hover == Hit::Item(i) {
                    c.round_rect(r.into(), 8.0 * s, BUTTON_HOVER);
                }
                if i == self.aspect {
                    draw_check(c, r, s);
                }
                let label = RECT {
                    left: r.left + self.px(28.0),
                    ..r
                };
                c.text(
                    ASPECTS[i].0,
                    label,
                    self.fonts.item,
                    TEXT,
                    DT_LEFT | DT_VCENTER,
                );
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
        match self.dropdown {
            Some(Popup::Aspect) => {
                if let Some(i) = l.items.iter().position(|r| contains(*r, p)) {
                    return Hit::Item(i);
                }
                if contains(l.dropdown, p) {
                    return Hit::Toolbar;
                }
            }
            Some(Popup::Mic) => {
                let empty = self.mics.is_empty();
                match self.mic_items.iter().position(|r| contains(*r, p)) {
                    Some(i) if !(empty && i == 1) => return Hit::MicItem(i),
                    _ if contains(self.mic_popup, p) => return Hit::Toolbar,
                    _ => {}
                }
            }
            Some(Popup::Quality) => {
                if let Some(i) = self.quality_items.iter().position(|r| contains(*r, p)) {
                    return Hit::QualityItem(i);
                }
                if contains(self.quality_popup, p) {
                    return Hit::Toolbar;
                }
            }
            None => {}
        }
        if let Some(i) = l.buttons.iter().position(|r| contains(*r, p)) {
            return Hit::Button(i);
        }
        if self.mode == Mode::Record {
            if let Some(i) = l.record_buttons.iter().position(|r| contains(*r, p)) {
                return Hit::RecButton(i);
            }
            if contains(l.record, p) {
                return Hit::Toolbar;
            }
        }
        if self.editable() {
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

    fn record_target(&self) -> Action {
        Action::Capture(Target::Record(Settings {
            rect: self.area,
            system_audio: self.system_audio,
            mic: self.mic_index().map(|i| self.mics[i].id.clone()),
            quality: self.quality,
        }))
    }

    fn set_quality(&mut self, item: usize) {
        self.quality = Quality::ALL[item];
        LAST_QUALITY.set(self.quality);
        self.dirty = true;
    }

    fn set_mic(&mut self, item: usize) {
        self.mic = item
            .checked_sub(1)
            .and_then(|i| self.mics.get(i))
            .map(|d| d.id.clone());
        LAST_MIC.set(self.mic.clone());
        self.dirty = true;
    }

    /// Top of an `h`-tall popup `gap` above `island`, or below it if there's
    /// no room above.
    fn popup_top(&self, island: RECT, h: i32, gap: i32) -> i32 {
        let above = island.top - gap - h;
        if above >= self.work.top {
            above
        } else {
            island.bottom + gap
        }
    }

    /// Re-attaches the toolbar when the area has changed. In Window mode it
    /// stays where it was rather than chasing the hovered window.
    fn relayout(&mut self) {
        if !self.editable() || self.layout.target == self.area {
            return;
        }
        self.layout = Layout::new(self.area, self.work, self.scale);
        if self.mode == Mode::Record {
            self.layout_mic_popup();
            self.layout_quality_popup();
        }
    }

    /// Lays out the quality menu above its button, wide enough for each
    /// level's name and description side by side.
    fn layout_quality_popup(&mut self) {
        let (pad, item_h) = (self.px(6.0), self.px(28.0));
        let text_w = Quality::ALL
            .iter()
            .map(|q| {
                text_width(self.fonts.item, q.name())
                    + text_width(self.fonts.label, q.description())
            })
            .max()
            .unwrap_or(0);
        let item_w = (text_w + self.px(86.0)).max(self.px(220.0));
        let w = item_w + 2 * pad;
        let h = 2 * pad + Quality::ALL.len() as i32 * item_h;

        let anchor = self.layout.record_buttons[REC_QUALITY];
        let left = (anchor.left - pad).clamp(
            self.monitor.left,
            (self.monitor.right - w).max(self.monitor.left),
        );
        let top = self.popup_top(self.layout.record, h, self.px(10.0));
        self.quality_popup = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
        self.quality_items = (0..Quality::ALL.len() as i32)
            .map(|i| RECT {
                left: left + pad,
                top: top + pad + i * item_h,
                right: left + pad + item_w,
                bottom: top + pad + (i + 1) * item_h,
            })
            .collect();
    }

    /// Lays out the microphone menu above its button, sized to fit the
    /// longest device name (within limits).
    fn layout_mic_popup(&mut self) {
        let (pad, item_h, sep_h) = (self.px(6.0), self.px(28.0), self.px(9.0));
        let mut labels = vec![NO_MIC];
        if self.mics.is_empty() {
            labels.push(NO_MICS_FOUND);
        }
        labels.extend(self.mics.iter().map(|d| d.name.as_str()));
        let text_w = labels
            .iter()
            .map(|s| text_width(self.fonts.item, s))
            .max()
            .unwrap_or(0);
        let item_w = (text_w + self.px(36.0)).clamp(self.px(200.0), self.px(360.0));
        let w = item_w + 2 * pad;
        let h = 2 * pad + labels.len() as i32 * item_h + sep_h;

        let anchor = self.layout.record_buttons[REC_MIC];
        let left = (anchor.left - pad).clamp(
            self.monitor.left,
            (self.monitor.right - w).max(self.monitor.left),
        );
        let top = self.popup_top(self.layout.record, h, self.px(10.0));
        self.mic_popup = RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        };
        let mut y = self.mic_popup.top + pad;
        self.mic_items.clear();
        for i in 0..labels.len() {
            self.mic_items.push(RECT {
                left: left + pad,
                top: y,
                right: left + pad + item_w,
                bottom: y + item_h,
            });
            y += item_h;
            if i == 0 {
                self.mic_separator = RECT {
                    left: left + pad + self.px(8.0),
                    top: y + sep_h / 2,
                    right: left + pad + item_w - self.px(8.0),
                    bottom: y + sep_h / 2 + 1,
                };
                y += sep_h;
            }
        }
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
        self.dropdown = None;
        self.window = None;
        if mode == Mode::Record {
            self.mics = audio::microphones();
            self.layout_mic_popup();
            self.layout_quality_popup();
        }
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
            self.area = apply_ratio(self.area, ratio, self.monitor);
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

    /// Resizes the area from its top-left corner, keeping it on its monitor.
    /// Values larger than the monitor are clamped to the monitor's size.
    fn set_size(&mut self, field: Field, v: i32) {
        let b = self.monitor;
        let r = self.area;
        let (bw, bh) = (b.right - b.left, b.bottom - b.top);
        let (mut w, mut h) = (r.right - r.left, r.bottom - r.top);
        match field {
            Field::Width => w = v.min(bw),
            Field::Height => h = v.min(bh),
        }
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
        self.hide_tooltip();
        let hit = self.hit(p);
        if self.dropdown.take().is_some() {
            self.dirty = true;
            match hit {
                Hit::Item(i) => self.set_aspect(i),
                Hit::MicItem(i) => self.set_mic(i),
                Hit::QualityItem(i) => self.set_quality(i),
                _ => {}
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
            Hit::Button(2) => {
                let mode = if self.mode == Mode::Window {
                    Mode::Area
                } else {
                    Mode::Window
                };
                self.set_mode(mode);
                Action::None
            }
            Hit::Button(_) if self.mode == Mode::Record => self.record_target(),
            Hit::Button(_) => {
                self.set_mode(Mode::Record);
                Action::None
            }
            Hit::RecButton(REC_SYSTEM_AUDIO) => {
                self.system_audio = !self.system_audio;
                LAST_SYSTEM_AUDIO.set(self.system_audio);
                self.dirty = true;
                Action::None
            }
            Hit::RecButton(REC_MIC) => {
                self.dropdown = Some(Popup::Mic);
                self.dirty = true;
                Action::None
            }
            Hit::RecButton(REC_QUALITY) => {
                self.dropdown = Some(Popup::Quality);
                self.dirty = true;
                Action::None
            }
            Hit::RecButton(_) => self.record_target(),
            Hit::Field(f) => {
                self.begin_edit(f);
                Action::None
            }
            Hit::Aspect => {
                self.dropdown = Some(Popup::Aspect);
                self.dirty = true;
                Action::None
            }
            Hit::Toolbar | Hit::Item(_) | Hit::MicItem(_) | Hit::QualityItem(_) => Action::None,
            Hit::None if self.mode == Mode::Window => self.window_target(),
            Hit::None => {
                let orig = self.area;
                self.drag = Some(match self.handle_at(p) {
                    Some(handle) => Drag::Resize { handle, orig },
                    None if contains(orig, p) => Drag::Move { start: p, orig },
                    None => Drag::New {
                        anchor: clamp_point(p, self.monitor),
                        previous: orig,
                    },
                });
                Action::BeginDrag
            }
        }
    }

    fn mouse_move(&mut self, p: POINT) {
        let b = self.monitor;
        let ratio = self.ratio();
        let hit = if self.drag.is_some() {
            Hit::None
        } else {
            self.hit(p)
        };
        if hit != self.hover {
            self.hover = hit;
            self.dirty = true;
            self.schedule_tooltip();
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
        if let Some(Drag::New { anchor, previous }) = self.drag.take() {
            // A click without a drag keeps the existing area.
            if self.area == span(anchor, anchor, self.ratio(), self.monitor) {
                self.area = previous;
            }
        }
        self.dirty = true;
    }

    unsafe fn key_down(&mut self, key: VIRTUAL_KEY) -> Action {
        match key {
            VK_ESCAPE => {
                if self.dropdown.is_some() {
                    self.dropdown = None;
                } else if self.edit.is_some() {
                    self.edit = None;
                } else if self.mode != Mode::Area {
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
            VK_RETURN => match self.mode {
                Mode::Area => self.area_target(),
                Mode::Record => self.record_target(),
                Mode::Window => self.window_target(),
            },
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

/// Top-left corner for the `w` x `h` Record island: inside `target` along
/// its bottom edge, otherwise just above it, otherwise below the toolbar
/// (or above the toolbar if that's off screen). Spots overlapping the
/// toolbar are skipped.
fn record_origin(target: RECT, work: RECT, toolbar: RECT, w: i32, h: i32, scale: f32) -> POINT {
    let px = |v: f32| (v * scale).round() as i32;
    let (inset, gap, margin) = (px(28.0), px(12.0), px(8.0));
    let inner = intersect(target, work);
    let at = |x: i32, y: i32| RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let centre_on = |l: i32, r: i32| {
        ((l + r - w) / 2).clamp(
            work.left + margin,
            (work.right - margin - w).max(work.left + margin),
        )
    };
    let x = centre_on(inner.left, inner.right);
    let free = |r: RECT| is_empty(intersect(r, toolbar));

    let fits_inside =
        inner.right - inner.left >= w + 2 * inset && inner.bottom - inner.top >= h + 2 * inset;
    let inside = at(x, inner.bottom - inset - h);
    if fits_inside && free(inside) {
        return POINT { x, y: inside.top };
    }
    let above = at(x, target.top - gap - h);
    if above.top >= work.top + margin && free(above) {
        return POINT { x, y: above.top };
    }
    let tx = centre_on(toolbar.left, toolbar.right);
    let below = toolbar.bottom + px(10.0);
    let y = if below + h <= work.bottom - margin {
        below
    } else {
        toolbar.top - px(10.0) - h
    };
    POINT { x: tx, y }
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
    let room_w = if sx < 0 {
        a.x - b.left + 1
    } else {
        b.right - a.x
    };
    let room_h = if sy < 0 {
        a.y - b.top + 1
    } else {
        b.bottom - a.y
    };
    let mut w = ((c.x - a.x).abs() + 1).max(MIN_SIZE).min(room_w);
    let mut h = ((c.y - a.y).abs() + 1).max(MIN_SIZE).min(room_h);
    if let Some((rw, rh)) = ratio {
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
            c.polyline(
                &[(cx - 4.5 * s, cy + 8.5 * s), (cx + 4.5 * s, cy + 8.5 * s)],
                t,
                TEXT,
            );
        }
        2 => {
            let frame = RectF {
                l: cx - 9.5 * s,
                t: cy - 7.5 * s,
                r: cx + 9.5 * s,
                b: cy + 7.5 * s,
            };
            c.round_rect_stroke(frame, 2.5 * s, t, TEXT);
            c.polyline(
                &[(cx - 9.0 * s, cy - 3.0 * s), (cx + 9.0 * s, cy - 3.0 * s)],
                t,
                TEXT,
            );
        }
        _ => {
            // A video camera: body plus a lens flaring out to the right.
            let body = RectF {
                l: cx - 10.0 * s,
                t: cy - 6.0 * s,
                r: cx + 3.0 * s,
                b: cy + 6.0 * s,
            };
            c.round_rect_stroke(body, 2.5 * s, t, TEXT);
            c.polyline(
                &[
                    (cx + 3.0 * s, cy - 1.5 * s),
                    (cx + 9.5 * s, cy - 5.0 * s),
                    (cx + 9.5 * s, cy + 5.0 * s),
                    (cx + 3.0 * s, cy + 1.5 * s),
                ],
                t,
                TEXT,
            );
        }
    }
}

/// A check mark at the left of a menu row.
fn draw_check(c: &mut Canvas, row: RECT, s: f32) {
    let x = row.left as f32 + 13.0 * s;
    let cy = (row.top + row.bottom) as f32 / 2.0;
    c.polyline(
        &[
            (x - 4.0 * s, cy),
            (x - 1.2 * s, cy + 3.0 * s),
            (x + 4.5 * s, cy - 3.5 * s),
        ],
        1.6 * s,
        TEXT,
    );
}

/// A speaker with sound waves when on, or a cross when muted.
fn draw_speaker_icon(c: &mut Canvas, cx: f32, cy: f32, s: f32, on: bool) {
    let t = 1.6 * s;
    let x = cx - 3.0 * s;
    c.polyline(
        &[
            (x - 6.0 * s, cy - 3.0 * s),
            (x - 2.5 * s, cy - 3.0 * s),
            (x + 2.0 * s, cy - 7.0 * s),
            (x + 2.0 * s, cy + 7.0 * s),
            (x - 2.5 * s, cy + 3.0 * s),
            (x - 6.0 * s, cy + 3.0 * s),
            (x - 6.0 * s, cy - 3.0 * s),
        ],
        t,
        TEXT,
    );
    let wx = x + 2.0 * s;
    if on {
        c.arc(wx, cy, 4.5 * s, -45.0, 45.0, t, TEXT);
        c.arc(wx, cy, 8.5 * s, -45.0, 45.0, t, TEXT);
    } else {
        let (mx, d) = (wx + 6.0 * s, 2.8 * s);
        c.polyline(&[(mx - d, cy - d), (mx + d, cy + d)], t, TEXT);
        c.polyline(&[(mx - d, cy + d), (mx + d, cy - d)], t, TEXT);
    }
}

/// A microphone, struck through when off.
fn draw_mic_icon(c: &mut Canvas, cx: f32, cy: f32, s: f32, on: bool) {
    let t = 1.6 * s;
    let capsule = RectF {
        l: cx - 3.5 * s,
        t: cy - 9.0 * s,
        r: cx + 3.5 * s,
        b: cy + 2.5 * s,
    };
    c.round_rect_stroke(capsule, 3.5 * s, t, TEXT);
    c.arc(cx, cy - 1.0 * s, 6.5 * s, 0.0, 180.0, t, TEXT);
    c.polyline(&[(cx, cy + 5.5 * s), (cx, cy + 8.5 * s)], t, TEXT);
    if !on {
        c.polyline(
            &[(cx - 7.5 * s, cy - 8.5 * s), (cx + 7.5 * s, cy + 8.5 * s)],
            t,
            TEXT,
        );
    }
}

/// A 5x5 lattice that thins out evenly at lower quality, staying symmetric:
/// a checkerboard at High, every other cell at Medium, and the corners and
/// centre (like a die's five) at Low. Cell edges are snapped to whole pixels
/// so the blocks stay crisp.
fn draw_quality_icon(c: &mut Canvas, cx: f32, cy: f32, s: f32, quality: Quality) {
    const CELLS: i32 = 5;
    let filled = |row: i32, col: i32| match quality {
        Quality::High => (row + col) % 2 == 0,
        Quality::Medium => row % 2 == 0 && col % 2 == 0,
        Quality::Low => (row % 4 == 0 && col % 4 == 0) || (row == 2 && col == 2),
    };
    // 3 px cells at 100% scale.
    let size = (15.0 * s).round();
    let (l, t) = ((cx - size / 2.0).round(), (cy - size / 2.0).round());
    let edge = |i: i32| (i as f32 * size / CELLS as f32).round();
    for row in 0..CELLS {
        for col in (0..CELLS).filter(|&col| filled(row, col)) {
            let r = RectF {
                l: l + edge(col),
                t: t + edge(row),
                r: l + edge(col + 1),
                b: t + edge(row + 1),
            };
            c.round_rect(r, 0.0, TEXT);
        }
    }
}

fn draw_crop_icon(c: &mut Canvas, cx: f32, cy: f32, s: f32, color: Color) {
    let t = 1.6 * s;
    let (a, b) = (4.0 * s, 8.0 * s);
    c.polyline(
        &[(cx - a, cy - b), (cx - a, cy + a), (cx + b, cy + a)],
        t,
        color,
    );
    c.polyline(
        &[(cx - b, cy - a), (cx + a, cy - a), (cx + a, cy + b)],
        t,
        color,
    );
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
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut c_void,
        size,
    )
    .is_ok()
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
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut RECT as *mut c_void,
        size,
    )
    .is_err()
    {
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
    if is_open() || crate::overlay::is_open() || crate::recbar::is_active() {
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
        .filter(|r| intersect(*r, monitor_rect) == *r && !is_empty(*r))
        .unwrap_or_else(|| {
            let (mw, mh) = (
                monitor_rect.right - monitor_rect.left,
                monitor_rect.bottom - monitor_rect.top,
            );
            let r = place(
                monitor_rect.left + mw / 4,
                monitor_rect.top + mh / 4,
                mw / 2,
                mh / 2,
                monitor_rect,
            );
            match ASPECTS[aspect].1 {
                Some(ratio) => apply_ratio(r, ratio, monitor_rect),
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

        scale,
        monitor: monitor_rect,
        work: to_local(info.rcWork),
        layout: Layout::new(area, to_local(info.rcWork), scale),
        fonts: Fonts::new(scale),
        mode: Mode::Area,
        area,
        aspect,
        drag: None,
        hover: Hit::None,
        edit: None,
        dropdown: None,
        system_audio: LAST_SYSTEM_AUDIO.get(),
        mic: LAST_MIC.with_borrow(|m| m.clone()),
        mics: Vec::new(),
        mic_popup: RECT::default(),
        mic_items: Vec::new(),
        mic_separator: RECT::default(),
        quality: LAST_QUALITY.get(),
        quality_popup: RECT::default(),
        quality_items: Vec::new(),
        windows: Vec::new(),
        window: None,
        tooltip: None,
        drawn: bounds,
        dirty: true,
        pending: None,
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
    let to_screen = |r: RECT| intersect(offset(r, origin.x, origin.y), virtual_screen);
    unsafe {
        // Wait until the compositor has drawn a frame without the overlay.
        let _ = DwmFlush();
        let _ = DwmFlush();
    }
    let rect = match target {
        Target::Rect(r) | Target::Window(_, r) => to_screen(r),
        Target::Record(settings) => {
            let rect = to_screen(settings.rect);
            crate::recbar::start(Settings { rect, ..settings });
            return;
        }
    };
    unsafe {
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

/// Holds back actions that close the menu until the mouse button is
/// released, capturing the mouse so the release comes here.
unsafe fn defer(hwnd: HWND, action: Option<Action>) -> Option<Action> {
    match action {
        Some(Action::Capture(_) | Action::Close) => {
            with_menu(|m| m.pending = action);
            SetCapture(hwnd);
            None
        }
        other => other,
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
                act(hwnd, defer(hwnd, action));
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
                act(hwnd, with_menu(|m| m.pending.take()).flatten());
                return LRESULT(0);
            }
            WM_RBUTTONDOWN => {
                let action = with_menu(|m| m.key_down(VK_ESCAPE));
                act(hwnd, defer(hwnd, action));
                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                let _ = ReleaseCapture();
                act(hwnd, with_menu(|m| m.pending.take()).flatten());
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let action = with_menu(|m| m.key_down(VIRTUAL_KEY(wparam.0 as u16)));
                act(hwnd, action);
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TOOLTIP_TIMER => {
                with_menu(|m| m.show_tooltip());
                act(hwnd, None);
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

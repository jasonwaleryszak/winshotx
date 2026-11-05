//! Floating controls shown while recording: stop (with the elapsed time),
//! pause/resume and delete. The window is excluded from screen capture, so
//! it doesn't show up in the video. A second, click-through window dims
//! everything outside the recorded area.
//!
//! Display affinity isn't supported for per-pixel-alpha layered windows, so
//! unlike the capture menu this one is colour-keyed: pixels of `KEY` are
//! transparent (and click-through), everything else is opaque.

use std::cell::RefCell;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::Sleep;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{
    contains, intersect, is_empty, offset, point_from_lparam, rgba, text_width, toolbar_origin,
    union, Inside,
};
use crate::gfx::{Canvas, Color, Dib, RectF};
use crate::menu::{font, BUTTON_HOVER, PANEL_BORDER, PANEL_FILL, TEXT, TOOLTIP_DELAY_MS};
use crate::record::{Recorder, Settings};

const CLASS_NAME: PCWSTR = w!("WinShotXRecordBar");
const RED: Color = rgba(0xFF5A52, 255);
/// Transparent colour, near black so anti-aliased edges blend toward a dark
/// fringe. Pixel (0xAARRGGBB) and COLORREF (0x00BBGGRR) forms are identical.
const KEY: u32 = 0x0001_0001;
const KEY_PX: u32 = 0xFF00_0000 | KEY;
const DIM_CLASS_NAME: PCWSTR = w!("WinShotXRecordDim");
/// Opacity of the black dimming window, matching the capture menu's dim.
const DIM_ALPHA: u8 = 0x7A;

const WM_MOUSELEAVE: u32 = 0x02A3;
const TICK_TIMER: usize = 1;
const TOOLTIP_TIMER: usize = 2;
const TICK_MS: u32 = 200;
/// Recordings stop automatically once the timer reaches 59:59.
const MAX_DURATION: Duration = Duration::from_secs(59 * 60 + 59);

const STOP: usize = 0;
const PAUSE: usize = 1;
const DELETE: usize = 2;

thread_local! {
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
    /// Stopped recordings still being finalised in the background.
    static FINISHING: RefCell<Vec<JoinHandle<()>>> = const { RefCell::new(Vec::new()) };
}

enum Action {
    None,
    Stop,
    Discard,
}

struct Bar {
    hwnd: HWND,
    dim: HWND,
    width: i32,
    height: i32,
    canvas: Option<Dib>,
    scale: f32,
    time_font: HFONT,
    label_font: HFONT,
    /// Work area of the monitor, in window coordinates.
    work: RECT,
    island: RECT,
    buttons: [RECT; 3],
    dividers: [RECT; 2],
    /// The recorded area, in window coordinates.
    area: RECT,
    hover: Option<usize>,
    tooltip: Option<usize>,
    tracking: bool,
    /// The button pressed, which acts if the mouse is released over it.
    /// Acting on release keeps the button-up from reaching whatever is
    /// under the bar once it closes.
    pressed: Option<usize>,

    recorder: Option<Recorder>,
    started: Instant,
    paused_at: Option<Instant>,
    paused: Duration,
    time: String,
    drawn: RECT,
    dirty: bool,
}

impl Drop for Bar {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyWindow(self.dim);
            let _ = DeleteObject(self.time_font);
            let _ = DeleteObject(self.label_font);
        }
    }
}

impl Bar {
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

    fn elapsed(&self) -> Duration {
        self.paused_at.unwrap_or_else(Instant::now) - self.started - self.paused
    }

    fn time_text(&self) -> String {
        let s = self.elapsed().min(MAX_DURATION).as_secs();
        format!("{}:{:02}", s / 60, s % 60)
    }

    fn tooltip_text(&self, i: usize) -> &'static str {
        match i {
            STOP => "Stop Recording",
            PAUSE if self.paused_at.is_some() => "Resume",
            PAUSE => "Pause",

            _ => "Delete",
        }
    }

    fn tooltip_rect(&self) -> Option<(RECT, &'static str)> {
        let i = self.tooltip.filter(|t| Some(*t) == self.hover)?;
        let text = self.tooltip_text(i);
        let anchor = self.buttons[i];
        let w = text_width(self.label_font, text) + 2 * self.px(10.0);
        let h = self.px(26.0);
        let left = ((anchor.left + anchor.right - w) / 2).clamp(0, self.width - w);
        let gap = self.px(8.0);
        let bottom = if self.island.top - gap - h >= self.work.top {
            self.island.top - gap
        } else {
            self.island.bottom + gap + h
        };
        let r = RECT {
            left,
            top: bottom - h,
            right: left + w,
            bottom,
        };
        Some((r, text))
    }

    fn decor_bounds(&self) -> RECT {
        let mut r = self.island;
        if let Some((tip, _)) = self.tooltip_rect() {
            r = union(r, tip);
        }
        r
    }

    fn set_hover(&mut self, hover: Option<usize>) {
        if hover == self.hover {
            return;
        }
        self.hover = hover;
        self.dirty = true;
        let was_showing = self.tooltip.take().is_some();
        unsafe {
            if hover.is_none() {
                let _ = KillTimer(self.hwnd, TOOLTIP_TIMER);
            } else if was_showing {
                self.tooltip = hover;
            } else {
                SetTimer(self.hwnd, TOOLTIP_TIMER, TOOLTIP_DELAY_MS, None);
            }
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

    fn tick(&mut self) -> Action {
        if self.elapsed() >= MAX_DURATION {
            return Action::Stop;
        }
        self.update_time();
        Action::None
    }

    /// Refreshes the timer text, resizing the stop button when it gains or
    /// loses a digit.
    fn update_time(&mut self) {
        let time = self.time_text();
        if time == self.time {
            return;
        }
        if time.len() != self.time.len() {
            (self.island, self.buttons, self.dividers) =
                layout(self.area, self.work, self.scale, self.time_font, &time);
        }
        self.time = time;
        self.dirty = true;
    }

    fn toggle_pause(&mut self) {
        let Some(rec) = &self.recorder else {
            return;
        };
        match self.paused_at.take() {
            Some(t) => {
                self.paused += t.elapsed();
                rec.resume();
            }
            None => {
                self.paused_at = Some(Instant::now());
                rec.pause();
            }
        }
        self.dirty = true;
    }

    fn mouse_down(&mut self, p: POINT) {
        self.hide_tooltip();
        self.pressed = self.buttons.iter().position(|r| contains(*r, p));
        if self.pressed.is_some() {
            unsafe {
                SetCapture(self.hwnd);
            }
        }
    }

    fn mouse_up(&mut self, p: POINT) -> Action {
        let Some(pressed) = self.pressed.take() else {
            return Action::None;
        };
        unsafe {
            let _ = ReleaseCapture();
        }
        if !contains(self.buttons[pressed], p) {
            return Action::None;
        }
        match pressed {
            STOP => Action::Stop,
            PAUSE => {
                self.toggle_pause();
                Action::None
            }
            DELETE => Action::Discard,
            _ => Action::None,
        }
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
        let dc = GetDC(self.hwnd);
        blit(dc, &dib, dirty);
        ReleaseDC(self.hwnd, dc);
        self.canvas = Some(dib);
    }

    unsafe fn paint(&self) {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(self.hwnd, &mut ps);
        if let Some(dib) = &self.canvas {
            blit(dc, dib, ps.rcPaint);
        }
        let _ = EndPaint(self.hwnd, &ps);
    }

    unsafe fn draw(&self, c: &mut Canvas) {
        c.fill(c.clip(), KEY_PX);

        let s = self.scale;
        let pill = |r: RECT| (r.bottom - r.top) as f32 / 2.0;
        let panel = |c: &mut Canvas, r: RECT| {
            c.round_rect(r.into(), pill(r), PANEL_FILL);
            c.round_rect_stroke(r.into(), pill(r), 1.0, PANEL_BORDER);
        };
        panel(c, self.island);
        for (i, &r) in self.buttons.iter().enumerate() {
            if self.hover == Some(i) {
                c.round_rect(r.into(), pill(r), BUTTON_HOVER);
            }
            let cy = (r.top + r.bottom) as f32 / 2.0;
            if i == STOP {
                let cx = r.left as f32 + pill(r);
                draw_stop_icon(c, cx, cy, s);
                let text = RECT {
                    left: r.left + self.px(34.0),
                    ..r
                };
                c.text(&self.time, text, self.time_font, RED, DT_LEFT | DT_VCENTER);
            } else {
                let cx = (r.left + r.right) as f32 / 2.0;
                draw_icon(c, i, self.paused_at.is_some(), cx, cy, s);
            }
        }
        for &d in &self.dividers {
            c.blend_rect(d, PANEL_BORDER);
        }
        if let Some((r, text)) = self.tooltip_rect() {
            panel(c, r);
            c.text(text, r, self.label_font, TEXT, DT_CENTER | DT_VCENTER);
        }
    }
}

unsafe fn blit(dc: HDC, dib: &Dib, r: RECT) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let _ = BitBlt(dc, r.left, r.top, w, h, dib.dc, r.left, r.top, SRCCOPY);
}

fn draw_stop_icon(c: &mut Canvas, cx: f32, cy: f32, s: f32) {
    c.arc(cx, cy, 8.5 * s, 0.0, 360.0, 1.6 * s, RED);
    let h = 3.5 * s;
    let square = RectF {
        l: cx - h,
        t: cy - h,
        r: cx + h,
        b: cy + h,
    };
    c.round_rect(square, 1.2 * s, RED);
}

fn draw_icon(c: &mut Canvas, i: usize, paused: bool, cx: f32, cy: f32, s: f32) {
    let t = 1.6 * s;
    match i {
        PAUSE => {
            c.arc(cx, cy, 9.0 * s, 0.0, 360.0, t, TEXT);
            if paused {
                let (l, r, h) = (cx - 2.5 * s, cx + 3.5 * s, 3.8 * s);
                c.polyline(&[(l, cy - h), (r, cy), (l, cy + h), (l, cy - h)], t, TEXT);
            } else {
                for x in [cx - 2.2 * s, cx + 2.2 * s] {
                    c.polyline(&[(x, cy - 3.5 * s), (x, cy + 3.5 * s)], 1.8 * s, TEXT);
                }
            }
        }

        _ => {
            c.polyline(
                &[(cx - 7.5 * s, cy - 5.0 * s), (cx + 7.5 * s, cy - 5.0 * s)],
                t,
                TEXT,
            );
            c.polyline(
                &[
                    (cx - 2.5 * s, cy - 5.0 * s),
                    (cx - 2.5 * s, cy - 7.5 * s),
                    (cx + 2.5 * s, cy - 7.5 * s),
                    (cx + 2.5 * s, cy - 5.0 * s),
                ],
                t,
                TEXT,
            );
            c.polyline(
                &[
                    (cx - 5.5 * s, cy - 5.0 * s),
                    (cx - 4.5 * s, cy + 8.0 * s),
                    (cx + 4.5 * s, cy + 8.0 * s),
                    (cx + 5.5 * s, cy - 5.0 * s),
                ],
                t,
                TEXT,
            );
            for x in [cx - 1.6 * s, cx + 1.6 * s] {
                c.polyline(&[(x, cy - 1.5 * s), (x, cy + 4.5 * s)], t, TEXT);
            }
        }
    }
}

fn with_bar<R>(f: impl FnOnce(&mut Bar) -> R) -> Option<R> {
    BAR.with(|b| b.try_borrow_mut().ok()?.as_mut().map(f))
}

fn take_bar() -> Option<Bar> {
    BAR.with(|b| b.try_borrow_mut().ok().and_then(|mut b| b.take()))
}

pub fn is_active() -> bool {
    BAR.with(|b| b.try_borrow().map_or(true, |b| b.is_some()))
}

pub unsafe fn register(instance: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSW {
        hInstance: instance,
        lpszClassName: CLASS_NAME,
        lpfnWndProc: Some(wndproc),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return Err(Error::from_win32());
    }
    let dim = WNDCLASSW {
        hInstance: instance,
        lpszClassName: DIM_CLASS_NAME,
        lpfnWndProc: Some(dim_wndproc),
        hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
        ..Default::default()
    };
    if RegisterClassW(&dim) == 0 {
        return Err(Error::from_win32());
    }
    Ok(())
}

extern "system" fn dim_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A translucent black, click-through window over the virtual screen with
/// a hole for `area` (screen coordinates).
unsafe fn create_dim(instance: HINSTANCE, area: RECT) -> Result<HWND> {
    let (x, y) = (
        GetSystemMetrics(SM_XVIRTUALSCREEN),
        GetSystemMetrics(SM_YVIRTUALSCREEN),
    );
    let (w, h) = (
        GetSystemMetrics(SM_CXVIRTUALSCREEN),
        GetSystemMetrics(SM_CYVIRTUALSCREEN),
    );
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        DIM_CLASS_NAME,
        w!(""),
        WS_POPUP,
        x,
        y,
        w,
        h,
        None,
        None,
        instance,
        None,
    )?;
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), DIM_ALPHA, LWA_ALPHA);
    let hole = offset(area, -x, -y);
    let rgn = CreateRectRgn(0, 0, w, h);
    let cut = CreateRectRgn(hole.left, hole.top, hole.right, hole.bottom);
    CombineRgn(rgn, rgn, cut, RGN_DIFF);
    let _ = DeleteObject(cut);
    // The window owns the region from here on.
    SetWindowRgn(hwnd, rgn, false);
    Ok(hwnd)
}

/// Starts recording and shows the controls, reporting any failure.
pub fn start(settings: Settings) {
    if is_active() {
        return;
    }
    if let Err(e) = unsafe { open(settings) } {
        crate::show_error(&format!("Failed to start recording:\n\n{e}"));
    }
}

/// Stops and saves the current recording, if any.
pub fn stop() {
    if let Some(mut bar) = take_bar() {
        if let Some(handle) = bar.recorder.take().and_then(Recorder::stop) {
            FINISHING.with_borrow_mut(|f| f.push(handle));
        }
    }
}

/// Stops recording and waits for every recording to be saved. Messages are
/// pumped while waiting, since saving puts the file on the clipboard, which
/// may notify the previous clipboard owner on this thread.
pub fn close() {
    stop();
    for handle in FINISHING.take() {
        while !handle.is_finished() {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&msg);
                }
                Sleep(10);
            }
        }
        let _ = handle.join();
    }
}

unsafe fn open(settings: Settings) -> Result<()> {
    let monitor = MonitorFromRect(&settings.rect, MONITOR_DEFAULTTONEAREST);
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

    let mon = info.rcMonitor;
    let (width, height) = (mon.right - mon.left, mon.bottom - mon.top);
    let to_local = |r: RECT| offset(r, -mon.left, -mon.top);
    let time_font = font(15.0 * scale, 600);
    let label_font = font(12.0 * scale, 400);
    let work = to_local(info.rcWork);
    let area = to_local(settings.rect);
    let (island, buttons, dividers) = layout(area, work, scale, time_font, "0:00");

    let canvas = Dib::new(HDC::default(), width, height)?;
    let instance: HINSTANCE = GetModuleHandleW(None)?.into();
    let dim = create_dim(instance, settings.rect)?;
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        CLASS_NAME,
        w!("WinShotX Recording"),
        WS_POPUP,
        mon.left,
        mon.top,
        width,
        height,
        None,
        None,
        instance,
        None,
    )?;
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(KEY), 0, LWA_COLORKEY);
    // Keep the controls out of the video.
    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

    let recorder = match Recorder::start(&settings) {
        Ok(r) => r,
        Err(e) => {
            let _ = DestroyWindow(hwnd);
            let _ = DestroyWindow(dim);
            let _ = DeleteObject(time_font);
            let _ = DeleteObject(label_font);
            return Err(e);
        }
    };

    let mut bar = Bar {
        hwnd,
        dim,
        width,
        height,
        canvas: Some(canvas),
        scale,
        time_font,
        label_font,
        work,
        island,
        buttons,
        dividers,
        area,
        hover: None,
        tooltip: None,
        tracking: false,
        pressed: None,

        recorder: Some(recorder),
        started: Instant::now(),
        paused_at: None,
        paused: Duration::ZERO,
        time: String::new(),
        drawn: RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        dirty: true,
    };
    bar.time = bar.time_text();
    bar.refresh();
    BAR.with_borrow_mut(|b| *b = Some(bar));
    // Shown first so the controls end up above it.
    let _ = ShowWindow(dim, SW_SHOWNOACTIVATE);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, TICK_TIMER, TICK_MS, None);
    Ok(())
}

/// A pill-shaped island attached to the recorded area (see
/// `toolbar_origin`): the stop button with the timer, then pause and delete,
/// with dividers.
fn layout(
    area: RECT,
    work: RECT,
    scale: f32,
    time_font: HFONT,
    time: &str,
) -> (RECT, [RECT; 3], [RECT; 2]) {
    let px = |v: f32| (v * scale).round() as i32;
    let (pad, btn, gap) = (px(6.0), px(36.0), px(4.0));
    let divider_w = px(1.0).max(1);
    // Sized to the time with every digit as 0, so the width only changes
    // when a digit is added (at 10:00).
    let template: String = time
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    let stop_w = px(34.0) + text_width(time_font, &template) + px(12.0);
    let width = 2 * pad + stop_w + 2 * (2 * gap + divider_w + btn);
    let height = btn + 2 * pad;
    let POINT { x: left, y: top } =
        toolbar_origin(area, work, width, height, width, Inside::Bottom, scale);
    let island = RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    };

    let cy = top + height / 2;
    let button = |x: i32, w: i32| RECT {
        left: x,
        top: top + pad,
        right: x + w,
        bottom: top + pad + btn,
    };
    let mut buttons = [button(left + pad, stop_w); 3];
    let mut dividers = [RECT::default(); 2];
    let mut x = left + pad + stop_w;
    for i in 0..2 {
        x += gap;
        dividers[i] = RECT {
            left: x,
            top: cy - px(12.0),
            right: x + divider_w,
            bottom: cy + px(12.0),
        };
        x += divider_w + gap;
        buttons[i + 1] = button(x, btn);
        x += btn;
    }
    (island, buttons, dividers)
}

unsafe fn act(action: Option<Action>) {
    match action {
        Some(Action::Stop) => stop(),
        Some(Action::Discard) => {
            if let Some(mut bar) = take_bar() {
                if let Some(rec) = bar.recorder.take() {
                    rec.discard();
                }
            }
        }

        Some(Action::None) | None => {}
    }
    with_bar(|b| b.refresh());
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                if with_bar(|b| b.paint()).is_some() {
                    return LRESULT(0);
                }
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_MOUSEMOVE => {
                let p = point_from_lparam(lparam);
                with_bar(|b| {
                    if !b.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        b.tracking = TrackMouseEvent(&mut tme).is_ok();
                    }
                    let hover = b.buttons.iter().position(|r| contains(*r, p));
                    b.set_hover(hover);
                });
                act(None);
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                with_bar(|b| {
                    b.tracking = false;
                    b.set_hover(None);
                });
                act(None);
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let p = point_from_lparam(lparam);
                with_bar(|b| b.mouse_down(p));
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let p = point_from_lparam(lparam);
                let action = with_bar(|b| b.mouse_up(p));
                act(action);
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TICK_TIMER => {
                let action = with_bar(|b| b.tick());
                act(action);
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TOOLTIP_TIMER => {
                with_bar(|b| {
                    let _ = KillTimer(b.hwnd, TOOLTIP_TIMER);
                    if b.hover.is_some() {
                        b.tooltip = b.hover;
                        b.dirty = true;
                    }
                });
                act(None);
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

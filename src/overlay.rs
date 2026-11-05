//! Alt+Shift+4 overlay: freezes the screen, drag to select, release to capture.

use std::cell::RefCell;

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::capture::{self, Image};
use crate::gfx::{blend, point_from_lparam, Dib, SELECTION};

const CLASS_NAME: PCWSTR = w!("WinShotXOverlay");

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

/// Fullscreen window showing a frozen screenshot of the virtual desktop.
struct Overlay {
    hwnd: HWND,
    width: i32,
    height: i32,
    shot: Dib,
    tinted: Dib,
    back: Dib,
    anchor: Option<POINT>,
    cursor: POINT,
}

impl Overlay {
    /// Selected rectangle in overlay/bitmap coordinates (right/bottom exclusive).
    fn selection(&self) -> Option<RECT> {
        let a = self.anchor?;
        let c = self.cursor;
        let r = RECT {
            left: a.x.min(c.x).clamp(0, self.width),
            top: a.y.min(c.y).clamp(0, self.height),
            right: (a.x.max(c.x) + 1).clamp(0, self.width),
            bottom: (a.y.max(c.y) + 1).clamp(0, self.height),
        };
        (r.right - r.left > 1 && r.bottom - r.top > 1).then_some(r)
    }

    unsafe fn paint(&self, hdc: HDC) {
        let (w, h) = (self.width, self.height);
        let back = self.back.dc;
        let _ = BitBlt(back, 0, 0, w, h, self.shot.dc, 0, 0, SRCCOPY);
        if let Some(r) = self.selection() {
            let (sw, sh) = (r.right - r.left, r.bottom - r.top);
            let _ = BitBlt(back, r.left, r.top, sw, sh, self.tinted.dc, r.left, r.top, SRCCOPY);
            // Drawn on the selection's outermost pixels so the edges pass exactly
            // through the anchor and the crosshair hotspot.
            FrameRect(back, &r, HBRUSH(GetStockObject(WHITE_BRUSH).0));
        }
        let _ = BitBlt(hdc, 0, 0, w, h, back, 0, 0, SRCCOPY);
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

pub unsafe fn register(instance: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSW {
        hCursor: LoadCursorW(None, IDC_CROSS)?,
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

pub fn is_open() -> bool {
    OVERLAY.with(|o| o.try_borrow().map_or(true, |o| o.is_some()))
}

/// Opens the selection overlay, reporting any failure to the user.
pub fn open() {
    if is_open() || crate::menu::is_open() {
        return;
    }
    let result = unsafe { GetModuleHandleW(None).and_then(|m| start(m.into())) };
    if let Err(e) = result {
        crate::show_error(&format!("Failed to start capture:\n\n{e}"));
    }
}

pub fn close() {
    finish(false);
}

unsafe fn start(instance: HINSTANCE) -> Result<()> {
    let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);

    let screen = GetDC(None);
    let buffers = (|| -> Result<_> {
        let shot = Dib::new(screen, width, height)?;
        BitBlt(shot.dc, 0, 0, width, height, screen, x, y, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0))?;
        Ok((shot, Dib::new(screen, width, height)?, Dib::new(screen, width, height)?))
    })();
    ReleaseDC(None, screen);
    let (shot, mut tinted, back) = buffers?;

    for (d, &s) in tinted.pixels_mut().iter_mut().zip(shot.pixels()) {
        *d = blend(s, SELECTION, 255);
    }

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        CLASS_NAME,
        w!("WinShotX"),
        WS_POPUP,
        x,
        y,
        width,
        height,
        None,
        None,
        instance,
        None,
    )?;

    OVERLAY.with_borrow_mut(|o| {
        *o = Some(Overlay {
            hwnd,
            width,
            height,
            shot,
            tinted,
            back,
            anchor: None,
            cursor: POINT::default(),
        })
    });

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    Ok(())
}

/// Tears down the overlay and, if requested, saves the current selection.
fn finish(save: bool) {
    let Some(overlay) = OVERLAY.with(|o| o.try_borrow_mut().ok().and_then(|mut o| o.take())) else {
        return;
    };
    let image = if save {
        overlay
            .selection()
            .map(|r| Image::crop(overlay.shot.pixels(), overlay.width, r))
    } else {
        None
    };
    drop(overlay);
    if let Some(image) = image {
        capture::deliver(&image);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                OVERLAY.with_borrow(|o| {
                    if let Some(o) = o {
                        o.paint(hdc);
                    }
                });
                let _ = EndPaint(hwnd, &ps);
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let pt = point_from_lparam(lparam);
                OVERLAY.with_borrow_mut(|o| {
                    if let Some(o) = o {
                        o.anchor = Some(pt);
                        o.cursor = pt;
                    }
                });
                SetCapture(hwnd);
                let _ = InvalidateRect(hwnd, None, false);
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let pt = point_from_lparam(lparam);
                let dragging = OVERLAY.with_borrow_mut(|o| match o {
                    Some(o) if o.anchor.is_some() => {
                        o.cursor = pt;
                        true
                    }
                    _ => false,
                });
                if dragging {
                    let _ = InvalidateRect(hwnd, None, false);
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                finish(true);
                return LRESULT(0);
            }
            WM_RBUTTONDOWN => {
                finish(false);
                return LRESULT(0);
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                finish(false);
                return LRESULT(0);
            }
            WM_KILLFOCUS => {
                // Another window stole focus (e.g. Alt+Tab); bail out rather than
                // leave an unresponsive topmost overlay on screen.
                finish(false);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

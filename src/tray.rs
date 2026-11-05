//! Notification-area icon with a small context menu.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::autostart;

const CLASS_NAME: PCWSTR = w!("WinShotXTray");
const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const TOOLTIP: &str = "WinShotX (Alt+Shift+4, Alt+Shift+5)";

const CMD_CAPTURE: usize = 1;
const CMD_AUTOSTART: usize = 2;
const CMD_QUIT: usize = 3;
const CMD_MENU: usize = 4;

/// Tray window handle, readable from other threads (e.g. the console Ctrl+C handler).
static TRAY_HWND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    static ICON: Cell<HICON> = Cell::new(HICON::default());
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
}

impl Tray {
    pub unsafe fn new(instance: HINSTANCE) -> Result<Self> {
        let wc = WNDCLASSW {
            hInstance: instance,
            lpszClassName: CLASS_NAME,
            lpfnWndProc: Some(wndproc),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_win32());
        }
        // Broadcast when Explorer (re)starts; the icon has to be re-added then.
        TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));

        let icon = create_icon()?;
        ICON.set(icon);

        // A hidden top-level window rather than a message-only one, because
        // message-only windows don't receive the TaskbarCreated broadcast.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("WinShotX"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?;
        TRAY_HWND.store(hwnd.0, Ordering::Release);
        // If Explorer isn't up yet this fails; TaskbarCreated will retry it.
        let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
        Ok(Self { hwnd, icon })
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            TRAY_HWND.store(std::ptr::null_mut(), Ordering::Release);
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(self.hwnd));
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyIcon(self.icon);
        }
    }
}

/// The tray window, also used as the clipboard owner.
pub fn hwnd() -> HWND {
    HWND(TRAY_HWND.load(Ordering::Acquire))
}

/// Asks the UI thread to exit cleanly. Safe to call from any thread.
pub fn request_quit() {
    let hwnd = TRAY_HWND.load(Ordering::Acquire);
    if !hwnd.is_null() {
        unsafe {
            let _ = PostMessageW(HWND(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: ICON.get(),
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip(TOOLTIP.encode_utf16()) {
        *dst = src;
    }
    nid
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                let event = (lparam.0 & 0xFFFF) as u32;
                if event == WM_LBUTTONUP || event == WM_RBUTTONUP {
                    show_menu(hwnd);
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            m if m != 0 && m == TASKBAR_CREATED.get() => {
                let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let autostart_on = autostart::is_enabled();
    let check = if autostart_on { MF_CHECKED } else { MF_UNCHECKED };
    let recording = crate::recbar::is_active();
    let region_flags = if recording { MF_STRING | MF_GRAYED } else { MF_STRING };
    let _ = AppendMenuW(menu, region_flags, CMD_CAPTURE, w!("Capture Region\tAlt+Shift+4"));
    let menu_label = if recording { w!("Stop Recording\tAlt+Shift+5") } else { w!("Capture...\tAlt+Shift+5") };
    let _ = AppendMenuW(menu, MF_STRING, CMD_MENU, menu_label);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING | check, CMD_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit WinShotX"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Without these two calls the menu won't close when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match cmd.0 as usize {
        CMD_CAPTURE => crate::overlay::open(),
        CMD_MENU if crate::recbar::is_active() => crate::recbar::stop(),
        CMD_MENU => crate::menu::open(),
        CMD_AUTOSTART => {
            if let Err(e) = autostart::set_enabled(!autostart_on) {
                crate::show_error(&format!("Failed to update Start with Windows:\n\n{e}"));
            }
        }
        CMD_QUIT => PostQuitMessage(0),
        _ => {}
    }
}

/// Builds the tray icon in code: white selection-corner marks on a blue
/// rounded square, so no resource compiler is needed.
unsafe fn create_icon() -> Result<HICON> {
    let size = GetSystemMetrics(SM_CXSMICON).max(16);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let n = (size * size) as usize;
    draw_icon(std::slice::from_raw_parts_mut(bits.cast(), n), size as usize);

    // The alpha channel defines transparency; the mask just has to exist.
    let mask_bits = vec![0u8; (size as usize + 15) / 16 * 2 * size as usize];
    let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr().cast()));
    let info = ICONINFO {
        fIcon: TRUE,
        hbmMask: mask,
        hbmColor: color,
        ..Default::default()
    };
    let icon = CreateIconIndirect(&info);
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}

fn draw_icon(pixels: &mut [u32], size: usize) {
    const SS: usize = 4; // supersampling per axis, for anti-aliasing
    const BLUE: [f32; 3] = [0x2D as f32, 0x7D as f32, 0xF6 as f32];

    let in_tile = |u: f32, v: f32| {
        let r = 0.22;
        let dx = ((u - 0.5).abs() - (0.5 - r)).max(0.0);
        let dy = ((v - 0.5).abs() - (0.5 - r)).max(0.0);
        dx.hypot(dy) <= r
    };
    let in_marks = |u: f32, v: f32| {
        let (lo, hi, thick, arm) = (0.24, 0.76, 0.09, 0.2);
        if !(lo..=hi).contains(&u) || !(lo..=hi).contains(&v) {
            return false;
        }
        let on_edge = |p: f32| p <= lo + thick || p >= hi - thick;
        let near_corner = |p: f32| p <= lo + arm || p >= hi - arm;
        (on_edge(u) && near_corner(v)) || (on_edge(v) && near_corner(u))
    };

    let s = size as f32;
    for y in 0..size {
        for x in 0..size {
            let (mut tile, mut mark) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    if in_tile(u, v) {
                        tile += 1;
                        mark += in_marks(u, v) as u32;
                    }
                }
            }
            let px = &mut pixels[y * size + x];
            if tile == 0 {
                *px = 0;
                continue;
            }
            let alpha = tile * 255 / (SS * SS) as u32;
            let white = mark as f32 / tile as f32;
            let [r, g, b] = BLUE.map(|c| (c + (255.0 - c) * white).round() as u32);
            *px = alpha << 24 | r << 16 | g << 8 | b;
        }
    }
}

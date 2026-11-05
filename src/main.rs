// Release builds have no console window; debug builds keep one so Ctrl+C works.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{w, Error, Result, HSTRING, PCWSTR, PWSTR, VARIANT};
use windows::Win32::Foundation::{GlobalFree, E_FAIL, GENERIC_WRITE, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::Threading::Sleep;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, ReleaseCapture, SetCapture, MOD_ALT, MOD_NOREPEAT, MOD_SHIFT, VK_ESCAPE,
};
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: PCWSTR = w!("WinShotXOverlay");
const HOTKEY_ID: i32 = 1;
const JPEG_QUALITY: f32 = 0.92;
/// Selection fill colour (0xRRGGBB) and its opacity (0-255).
const SELECTION_FILL: u32 = 0xD9D9D9;
const SELECTION_ALPHA: u32 = 32;

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

/// A 32bpp top-down DIB section selected into its own memory DC.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    len: usize,
}

impl Dib {
    unsafe fn new(reference: HDC, width: i32, height: i32) -> Result<Self> {
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
            len: (width * height) as usize,
        })
    }

    fn pixels(&self) -> &[u32] {
        unsafe { std::slice::from_raw_parts(self.bits, self.len) }
    }

    fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
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

/// Fullscreen window showing a frozen, dimmed screenshot of the virtual desktop.
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

    /// Copies the selected region out as tightly packed 24bpp BGR.
    fn crop_bgr(&self, r: RECT) -> (u32, u32, Vec<u8>) {
        let (w, h) = ((r.right - r.left) as usize, (r.bottom - r.top) as usize);
        let src = self.shot.pixels();
        let mut out = Vec::with_capacity(w * h * 3);
        for y in r.top as usize..r.bottom as usize {
            let row = &src[y * self.width as usize..][r.left as usize..r.right as usize];
            for &px in row {
                out.extend_from_slice(&px.to_le_bytes()[..3]);
            }
        }
        (w as u32, h as u32, out)
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
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

        if let Err(e) = RegisterHotKey(None, HOTKEY_ID, MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, '4' as u32) {
            show_error(&format!(
                "Could not register Alt+Shift+4. Is WinShotX already running?\n\n{e}"
            ));
            return Err(e);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_HOTKEY && msg.wParam.0 == HOTKEY_ID as usize {
                if let Err(e) = start_capture(instance) {
                    show_error(&format!("Failed to start capture:\n\n{e}"));
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe fn start_capture(instance: HINSTANCE) -> Result<()> {
    if OVERLAY.with_borrow(|o| o.is_some()) {
        return Ok(());
    }

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

    let a = SELECTION_ALPHA;
    for (d, &s) in tinted.pixels_mut().iter_mut().zip(shot.pixels()) {
        let mut px = 0;
        for shift in [0, 8, 16] {
            let src = (s >> shift) & 0xFF;
            let fill = (SELECTION_FILL >> shift) & 0xFF;
            px |= ((src * (255 - a) + fill * a) / 255) << shift;
        }
        *d = px;
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
    let Some(overlay) = OVERLAY.with_borrow_mut(|o| o.take()) else {
        return;
    };
    let crop = if save {
        overlay.selection().map(|r| overlay.crop_bgr(r))
    } else {
        None
    };
    // The clipboard should be opened with a live window as owner, so copy
    // before the overlay is destroyed.
    let clip_result = crop
        .as_ref()
        .map(|(w, h, pixels)| copy_to_clipboard(overlay.hwnd, *w, *h, pixels));
    drop(overlay);

    if let Some(Err(e)) = clip_result {
        show_error(&format!("Failed to copy screenshot to clipboard:\n\n{e}"));
    }
    if let Some((w, h, pixels)) = crop {
        if let Err(e) = output_path().and_then(|path| save_jpeg(&path, w, h, &pixels)) {
            show_error(&format!("Failed to save screenshot:\n\n{e}"));
        }
    }
}

fn point_from_lparam(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xFFFF) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
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

fn output_path() -> Result<PathBuf> {
    let dir = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const c_void));
        PathBuf::from(s.map_err(|e| Error::new(E_FAIL, e.to_string()))?).join("WinShotX")
    };
    std::fs::create_dir_all(&dir).map_err(|e| Error::new(E_FAIL, e.to_string()))?;

    // Windows forbids ':' in file names, so the time uses '.' separators.
    let t = unsafe { GetLocalTime() };
    let stem = format!(
        "WinShotX {:04}-{:02}-{:02} {:02}.{:02}.{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let mut path = dir.join(format!("{stem}.jpg"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).jpg"));
        n += 1;
    }
    Ok(path)
}

fn save_jpeg(path: &Path, width: u32, height: u32, bgr: &[u8]) -> Result<()> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), GENERIC_WRITE.0)?;

        let encoder = factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

        let mut frame: Option<IWICBitmapFrameEncode> = None;
        let mut props: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut props)?;
        let frame = frame.ok_or_else(Error::empty)?;
        let props = props.ok_or_else(Error::empty)?;

        let option = PROPBAG2 {
            pstrName: PWSTR(w!("ImageQuality").as_ptr() as *mut u16),
            ..Default::default()
        };
        props.Write(1, &option, &VARIANT::from(JPEG_QUALITY))?;

        frame.Initialize(&props)?;
        frame.SetSize(width, height)?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        frame.WritePixels(height, width * 3, bgr)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

/// Places the image on the clipboard as a bottom-up 24bpp CF_DIB.
fn copy_to_clipboard(owner: HWND, width: u32, height: u32, bgr: &[u8]) -> Result<()> {
    let row = width as usize * 3;
    let stride = (row + 3) & !3;
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: height as i32,
        biPlanes: 1,
        biBitCount: 24,
        biCompression: BI_RGB.0,
        biSizeImage: (stride * height as usize) as u32,
        ..Default::default()
    };
    let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let total = header_size + stride * height as usize;

    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total)?;
        let dst = GlobalLock(mem) as *mut u8;
        if dst.is_null() {
            let _ = GlobalFree(mem);
            return Err(Error::from_win32());
        }
        std::ptr::copy_nonoverlapping(&header as *const _ as *const u8, dst, header_size);
        let pixels = dst.add(header_size);
        for (y, src_row) in bgr.chunks_exact(row).enumerate() {
            let dst_row = pixels.add((height as usize - 1 - y) * stride);
            std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, row);
        }
        let _ = GlobalUnlock(mem);

        // Another app may briefly hold the clipboard open; retry a few times.
        let mut opened = OpenClipboard(owner);
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            Sleep(20);
            opened = OpenClipboard(owner);
        }
        if let Err(e) = opened {
            let _ = GlobalFree(mem);
            return Err(e);
        }

        let result = EmptyClipboard().and_then(|_| SetClipboardData(CF_DIB.0 as u32, HANDLE(mem.0)));
        let _ = CloseClipboard();
        if let Err(e) = result {
            // Ownership only transfers to the system on success.
            let _ = GlobalFree(mem);
            return Err(e);
        }
    }
    Ok(())
}

fn show_error(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("WinShotX"), MB_ICONERROR | MB_OK);
    }
}

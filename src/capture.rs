//! Grabbing screen pixels and delivering them: clipboard plus a JPEG file.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{w, Error, Result, PCWSTR, PWSTR, VARIANT};
use windows::Win32::Foundation::{GlobalFree, E_FAIL, E_INVALIDARG, GENERIC_WRITE, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::{IPropertyBag2, PROPBAG2};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::Sleep;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

use crate::gfx::Dib;

const JPEG_QUALITY: f32 = 0.92;

/// Tightly packed 24bpp BGR pixels, top row first.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bgr: Vec<u8>,
}

impl Image {
    /// Copies `r` out of a 32bpp pixel buffer that is `stride` pixels wide.
    pub fn crop(pixels: &[u32], stride: i32, r: RECT) -> Self {
        let (w, h) = ((r.right - r.left) as usize, (r.bottom - r.top) as usize);
        let mut bgr = Vec::with_capacity(w * h * 3);
        for y in r.top as usize..r.bottom as usize {
            let row = &pixels[y * stride as usize..][r.left as usize..r.right as usize];
            for &px in row {
                bgr.extend_from_slice(&px.to_le_bytes()[..3]);
            }
        }
        Self {
            width: w as u32,
            height: h as u32,
            bgr,
        }
    }
}

/// Captures a rectangle of the live screen (virtual-screen coordinates).
pub fn grab(r: RECT) -> Result<Image> {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return Err(Error::new(E_INVALIDARG, "Nothing to capture"));
    }
    unsafe {
        let screen = GetDC(None);
        let result = (|| -> Result<Image> {
            let dib = Dib::new(screen, w, h)?;
            BitBlt(dib.dc, 0, 0, w, h, screen, r.left, r.top, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0))?;
            let all = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            Ok(Image::crop(dib.pixels(), w, all))
        })();
        ReleaseDC(None, screen);
        result
    }
}

/// Copies the image to the clipboard and saves it, reporting failures.
pub fn deliver(image: &Image) {
    if let Err(e) = copy_to_clipboard(crate::tray::hwnd(), image) {
        crate::show_error(&format!("Failed to copy screenshot to clipboard:\n\n{e}"));
    }
    if let Err(e) = output_path().and_then(|path| save_jpeg(&path, image)) {
        crate::show_error(&format!("Failed to save screenshot:\n\n{e}"));
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

fn save_jpeg(path: &Path, image: &Image) -> Result<()> {
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
        frame.SetSize(image.width, image.height)?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        frame.SetPixelFormat(&mut format)?;
        frame.WritePixels(image.height, image.width * 3, &image.bgr)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

/// Places the image on the clipboard as a bottom-up 24bpp CF_DIB.
fn copy_to_clipboard(owner: HWND, image: &Image) -> Result<()> {
    let (width, height) = (image.width, image.height);
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
        for (y, src_row) in image.bgr.chunks_exact(row).enumerate() {
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

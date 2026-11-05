// Release builds have no console window; debug builds keep one so Ctrl+C works.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod capture;
mod gfx;
mod menu;
mod overlay;
mod recbar;
mod record;
mod tray;

use windows::core::{w, Result, HSTRING};
use windows::Win32::Foundation::{BOOL, HINSTANCE, TRUE};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_NOREPEAT, MOD_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::*;

const HOTKEY_REGION: i32 = 1;
const HOTKEY_MENU: i32 = 2;

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        overlay::register(instance)?;
        menu::register(instance)?;
        recbar::register(instance)?;

        for (id, key, name) in [(HOTKEY_REGION, '4', "Alt+Shift+4"), (HOTKEY_MENU, '5', "Alt+Shift+5")] {
            if let Err(e) = RegisterHotKey(None, id, MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, key as u32) {
                show_error(&format!("Could not register {name}. Is WinShotX already running?\n\n{e}"));
                return Err(e);
            }
        }

        let tray = tray::Tray::new(instance)?;
        // In debug builds Ctrl+C would otherwise kill the process and leave a
        // ghost tray icon behind.
        let _ = SetConsoleCtrlHandler(Some(on_console_ctrl), true);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_HOTKEY {
                match msg.wParam.0 as i32 {
                    HOTKEY_REGION => overlay::open(),
                    // While recording, the menu hotkey stops the recording.
                    HOTKEY_MENU if recbar::is_active() => recbar::stop(),
                    HOTKEY_MENU => menu::open(),
                    _ => {}
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        overlay::close();
        menu::close();
        recbar::close();
        drop(tray);
    }
    Ok(())
}

unsafe extern "system" fn on_console_ctrl(_ctrl_type: u32) -> BOOL {
    tray::request_quit();
    TRUE
}

fn show_error(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("WinShotX"), MB_ICONERROR | MB_OK);
    }
}

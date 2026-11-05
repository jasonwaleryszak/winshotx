//! "Start with Windows" via the per-user `Run` registry key.

use windows::core::{w, Error, Result, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("WinShotX");

/// The command line registered to run at sign-in: the current executable, quoted.
fn command() -> Result<String> {
    let exe = std::env::current_exe().map_err(|e| Error::new(E_FAIL, e.to_string()))?;
    Ok(format!("\"{}\"", exe.display()))
}

/// True only if the Run entry points at this executable.
pub fn is_enabled() -> bool {
    let Ok(expected) = command() else {
        return false;
    };
    let mut buf = [0u16; 1024];
    let mut len = std::mem::size_of_val(&buf) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if err != ERROR_SUCCESS {
        return false;
    }
    // `len` is in bytes and includes the terminating NUL.
    let chars = (len as usize / 2).saturating_sub(1);
    String::from_utf16_lossy(&buf[..chars]).eq_ignore_ascii_case(&expected)
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let err = unsafe {
        if enabled {
            let wide: Vec<u16> = command()?.encode_utf16().chain(Some(0)).collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            )
        } else {
            match RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                e => e,
            }
        }
    };
    err.ok()
}

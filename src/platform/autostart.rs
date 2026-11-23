use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_NONE, REG_OPTION_NON_VOLATILE,
    REG_SAM_FLAGS, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::w;

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: windows::core::PCWSTR = w!("RPin");

pub fn is_enabled() -> bool {
    let Some(current) = current_value().filter(|value| !value.trim().is_empty()) else {
        return false;
    };
    if let Ok(expected) = run_command() {
        if !current.eq_ignore_ascii_case(&expected) {
            if let Err(err) = set_enabled(true) {
                crate::logging::warn(format!("failed to update autostart path: {err}"));
            }
        }
    }
    true
}

fn run_command() -> anyhow::Result<String> {
    Ok(format!(
        "\"{}\" --minimized",
        std::env::current_exe()?.display()
    ))
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    let mut key = HKEY::default();
    // SAFETY: only enabling may create a missing Run key. Both APIs fill key on success.
    let status = unsafe {
        if enabled {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        } else {
            RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key)
        }
    };
    if !enabled && status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    status.ok()?;
    let result = (|| {
        if enabled {
            let value = run_command()?;
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
            // SAFETY: key is open for KEY_SET_VALUE; bytes points to a valid
            // UTF-16 string including the terminating NUL.
            let status = unsafe { RegSetValueExW(key, VALUE_NAME, None, REG_SZ, Some(bytes)) };
            if status != ERROR_SUCCESS {
                anyhow::bail!("RegSetValueExW failed: {}", status.0);
            }
        } else {
            // SAFETY: key is open for KEY_SET_VALUE.
            let status = unsafe { RegDeleteValueW(key, VALUE_NAME) };
            if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
                anyhow::bail!("RegDeleteValueW failed: {}", status.0);
            }
        }
        Ok(())
    })();

    // SAFETY: key was opened by open_run_key and is closed exactly once.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn current_value() -> Option<String> {
    let key = open_run_key(KEY_QUERY_VALUE).ok()?;
    let value = query_string(key, VALUE_NAME);
    // SAFETY: key was opened by open_run_key and is closed exactly once.
    unsafe {
        let _ = RegCloseKey(key);
    }
    value
}

fn open_run_key(access: REG_SAM_FLAGS) -> anyhow::Result<HKEY> {
    let mut key = HKEY::default();
    // SAFETY: RUN_KEY is a static NUL-terminated string; key is valid output.
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, access, &mut key) };
    if status != ERROR_SUCCESS {
        anyhow::bail!("RegOpenKeyExW failed: {}", status.0);
    }
    Ok(key)
}

fn query_string(key: HKEY, name: windows::core::PCWSTR) -> Option<String> {
    let mut ty = REG_NONE;
    let mut size = 0u32;
    // SAFETY: key is valid; size receives the required byte count.
    let status = unsafe { RegQueryValueExW(key, name, None, Some(&mut ty), None, Some(&mut size)) };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }

    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf is large enough for `size` bytes as reported above.
    let status = unsafe {
        RegQueryValueExW(
            key,
            name,
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || ty != REG_SZ {
        return None;
    }

    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

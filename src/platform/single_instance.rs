use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_COPYDATA,
};
use windows::core::{PCWSTR, w};

use crate::app::Command;

use super::window::MAIN_WINDOW_CLASS;

const MAX_COMMAND_BYTES: u32 = 64;

pub enum AcquireResult {
    Primary(SingleInstance),
    Secondary,
}

pub struct SingleInstance {
    handle: HANDLE,
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: handle is owned by this struct and closed exactly once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Creates the per-session named mutex. If another instance exists, tries to
/// forward `command` via WM_COPYDATA and reports `Secondary`.
pub fn acquire(command: &str) -> anyhow::Result<AcquireResult> {
    // SAFETY: name is a static NUL-terminated string; NULL security attributes.
    let handle = unsafe { CreateMutexW(None, false, w!("Local\\RPin.SingleInstance"))? };

    // SAFETY: GetLastError reads the thread-local last-error value.
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        // SAFETY: we did not create the mutex, so release our duplicate handle.
        unsafe {
            let _ = CloseHandle(handle);
        }
        anyhow::ensure!(
            send_command(command),
            "running instance did not accept {command}"
        );
        Ok(AcquireResult::Secondary)
    } else {
        Ok(AcquireResult::Primary(SingleInstance { handle }))
    }
}

fn send_command(command: &str) -> bool {
    for attempt in 0..10 {
        // SAFETY: class name is static; the first instance creates this window
        // before adding the tray icon.
        if let Ok(hwnd) = unsafe { FindWindowW(MAIN_WINDOW_CLASS, PCWSTR::null()) } {
            match send_copy_data(hwnd, command) {
                Some(true) => return true,
                None => return false,
                Some(false) => {} // The hidden window may still be initializing.
            }
        }
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    crate::logging::warn("another RPin instance exists but its window was not found");
    false
}

fn send_copy_data(hwnd: HWND, command: &str) -> Option<bool> {
    let bytes = command.as_bytes();
    let data = COPYDATASTRUCT {
        dwData: 1,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut _,
    };

    // SAFETY: the COPYDATASTRUCT and its buffer stay alive for the synchronous
    // SendMessageW call; the receiver copies the data before returning.
    let mut accepted = 0;
    let result = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&data as *const _ as isize),
            SMTO_ABORTIFHUNG,
            1000,
            Some(&mut accepted),
        )
    };
    (result.0 != 0).then_some(accepted != 0)
}

/// The lparam must be from a live WM_COPYDATA callback.
pub unsafe fn read_copy_data(lparam: LPARAM) -> Option<Command> {
    if lparam.0 == 0 {
        return None;
    }
    // SAFETY: WM_COPYDATA guarantees lparam points to a valid COPYDATASTRUCT
    // for the duration of the message handler.
    let data = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
    if data.dwData != 1 || !(1..=MAX_COMMAND_BYTES).contains(&data.cbData) || data.lpData.is_null()
    {
        return None;
    }
    // SAFETY: the sender guarantees cbData bytes at lpData.
    let bytes =
        unsafe { std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize) };
    Command::from_ipc(std::str::from_utf8(bytes).ok()?)
}

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA,
    GetMessageW, IDC_ARROW, LoadCursorW, MSG, RegisterClassW, SetWindowLongPtrW, TranslateMessage,
    WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::app;

pub const MAIN_WINDOW_CLASS: PCWSTR = w!("RPin.MainWindow");
const MAIN_WINDOW_TITLE: PCWSTR = w!("RPin");

pub fn hinstance() -> anyhow::Result<HINSTANCE> {
    // SAFETY: passing NULL asks for the current executable's module handle.
    let module = unsafe { GetModuleHandleW(PCWSTR::null())? };
    Ok(HINSTANCE(module.0))
}

pub fn create_hidden_window() -> anyhow::Result<HWND> {
    let instance = hinstance()?;

    // SAFETY: the class name and cursor are static, and the WNDCLASSW lives
    // until RegisterClassW returns.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW)? };
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(app::wndproc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: MAIN_WINDOW_CLASS,
        ..Default::default()
    };

    // SAFETY: `wc` is fully initialized and valid for this call.
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 {
        let err = windows::core::Error::from_thread();
        // ERROR_CLASS_ALREADY_EXISTS is fine: another code path registered it.
        if err.code().0 as u32 & 0xFFFF != 1410 {
            return Err(err.into());
        }
    }

    // SAFETY: the class is registered; the window is a hidden top-level popup
    // used for tray callbacks, hotkeys, clipboard ownership and WM_COPYDATA.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            MAIN_WINDOW_CLASS,
            MAIN_WINDOW_TITLE,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?
    };

    Ok(hwnd)
}

pub fn run_message_loop() -> i32 {
    let mut msg = MSG::default();
    // SAFETY: standard Win32 message pump on the UI thread.
    unsafe {
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0);
            if result.0 < 0 {
                crate::logging::error(format!(
                    "GetMessageW failed: {}",
                    windows::core::Error::from_thread()
                ));
                return 1;
            }
            if result.0 == 0 {
                return msg.wParam.0 as i32;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The state must live until the window is destroyed or detached on this thread.
pub(crate) unsafe fn attach_app(hwnd: HWND, app: *const app::WindowState) {
    // SAFETY: the caller keeps the attached pointer alive for native callbacks.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
    }
}

pub fn detach_app(hwnd: HWND) {
    // SAFETY: reads the pointer previously stored by `attach_app`.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
}

/// Default handler for messages that arrive before the App pointer is attached.
pub fn default_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: forwards to the system default window procedure.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

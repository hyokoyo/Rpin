use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, MONITORINFOF_PRIMARY};
use windows::core::BOOL;

use super::{CaptureError, MonitorInfo, frame::Rect};

pub fn at_cursor() -> Result<HMONITOR, CaptureError> {
    let mut point = POINT::default();
    // SAFETY: GetCursorPos receives a writable POINT and MonitorFromPoint takes it by value.
    unsafe {
        GetCursorPos(&mut point)
            .map_err(|err| CaptureError::Win32(format!("GetCursorPos: {err}")))?;
        Ok(MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST))
    }
}

pub(super) fn enumerate() -> Result<Vec<MonitorInfo>, CaptureError> {
    let mut handles = Vec::<HMONITOR>::new();
    // SAFETY: enumeration is synchronous, and the callback receives a pointer
    // to a live Vec for the duration of the call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&mut handles as *mut Vec<HMONITOR>) as isize),
        )
    };
    if !ok.as_bool() {
        return Err(CaptureError::win32("EnumDisplayMonitors"));
    }
    if handles.is_empty() {
        return Err(CaptureError::NoMonitor);
    }
    handles.into_iter().map(info).collect()
}

unsafe extern "system" fn collect_monitor(
    handle: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: enumerate passes a mutable Vec pointer and callbacks run in series.
    let handles = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    handles.push(handle);
    BOOL(1)
}

pub(super) fn info(handle: HMONITOR) -> Result<MonitorInfo, CaptureError> {
    if handle.is_invalid() {
        return Err(CaptureError::NoMonitor);
    }
    let mut monitor = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: monitor is properly sized writable storage for the Win32 call.
    if !unsafe { GetMonitorInfoW(handle, &mut monitor) }.as_bool() {
        return Err(CaptureError::win32("GetMonitorInfoW"));
    }
    let rect = from_rect(monitor.rcMonitor);
    if rect.is_empty() {
        return Err(CaptureError::NoMonitor);
    }
    let (mut dpi, mut dpi_y) = (0, 0);
    // SAFETY: handle was just validated and both DPI outputs are writable.
    if let Err(err) = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) } {
        crate::logging::debug(format!("GetDpiForMonitor failed: {err}"));
    }
    Ok(MonitorInfo {
        handle,
        rect,
        work: from_rect(monitor.rcWork),
        dpi: if dpi > 0 { dpi } else { 96 },
        primary: monitor.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

fn from_rect(rect: RECT) -> Rect {
    Rect::new(rect.left, rect.top, rect.right, rect.bottom)
}

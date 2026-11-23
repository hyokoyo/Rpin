//! Capture backends and the frozen-frame data model.
//!
//! v1 implements `GdiCapture` for monitors. The `Window` source is the reserved
//! integration point for v1.4 (WGC or `PrintWindow`) and must keep the same
//! output type so the overlay/output pipeline does not need to change.
#![allow(dead_code)]

pub mod frame;
pub mod gdi;
mod monitor;
mod pixels;

pub use monitor::at_cursor as monitor_at_cursor;

use thiserror::Error;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;

use frame::{Frame, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    VirtualDesktop,
    Monitor(HMONITOR),
    /// Reserved for v1.4.
    Window(HWND),
    /// Reserved for fixed-region capture.
    Region(Rect),
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest {
    pub source: CaptureSource,
    pub include_cursor: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MonitorInfo {
    pub handle: HMONITOR,
    /// Monitor rectangle in virtual-desktop physical pixels.
    pub rect: Rect,
    pub work: Rect,
    pub dpi: u32,
    pub primary: bool,
}

pub struct CapturedMonitor {
    pub info: MonitorInfo,
    pub frame: Frame,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("Win32 call failed: {0}")]
    Win32(String),
    #[error("BitBlt failed for {rect:?}: SRCCOPY: {srccopy}, CAPTUREBLT: {layered}")]
    BitBlt {
        rect: Rect,
        srccopy: windows::core::Error,
        layered: windows::core::Error,
    },
    #[error("no monitor found")]
    NoMonitor,
    #[error("invalid capture dimensions: {width} × {height}")]
    InvalidDimensions { width: i32, height: i32 },
    #[error("could not allocate {0} bytes for the captured frame")]
    Allocation(usize),
    #[error("capture source is not implemented in this version")]
    Unsupported,
}

impl CaptureError {
    fn win32(api: &str) -> Self {
        Self::Win32(format!("{api}: {}", windows::core::Error::from_thread()))
    }
}

pub trait CaptureBackend {
    fn capture_all(&self) -> Result<Vec<CapturedMonitor>, CaptureError>;

    fn capture_monitor(&self, handle: HMONITOR) -> Result<CapturedMonitor, CaptureError>;

    /// Reserved for v1.4. The default keeps v1 callers honest.
    fn capture_window(&self, _hwnd: HWND) -> Result<CapturedMonitor, CaptureError> {
        Err(CaptureError::Unsupported)
    }
}

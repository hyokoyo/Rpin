use anyhow::{Context, ensure};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_OVER, BLENDFUNCTION, DIB_RGB_COLORS, GdiFlush, HALFTONE, HDC, SRCCOPY, SetBrushOrgEx,
    SetStretchBltMode, StretchDIBits,
};
use windows::Win32::UI::WindowsAndMessaging::{ULW_ALPHA, UpdateLayeredWindow};

use crate::capture::{
    frame::{Frame, Point},
    gdi::{MemoryDc, Selection, bgra_header, dib},
};

use super::layout::Size;

/// Present one opaque screenshot with uniform window opacity. The scratch DIB
/// is freed on return; the window manager keeps the current layered surface.
pub(super) fn present(
    hwnd: HWND,
    frame: &Frame,
    size: Size,
    at: Point,
    alpha: u8,
) -> anyhow::Result<()> {
    let (bitmap, bits) = dib(HDC::default(), size.width, size.height)?;
    let same_size = size.width as u32 == frame.width && size.height as u32 == frame.height;
    if same_size {
        // SAFETY: the fresh DIB fits the frame exactly and has no active DC.
        unsafe {
            bits.as_ptr()
                .copy_from_nonoverlapping(frame.pixels().as_ptr(), frame.pixels().len())
        };
    }
    let dc = MemoryDc::create(None)?;
    let _selection = Selection::new(&dc, &bitmap)?;
    if !same_size {
        let width = frame.width as i32;
        let height = frame.height as i32;
        let info = bgra_header(width, height);
        // SAFETY: the source frame and the selected destination each fit their dimensions.
        unsafe {
            let _ = SetStretchBltMode(dc.0, HALFTONE);
            let _ = SetBrushOrgEx(dc.0, 0, 0, None);
            let rows = StretchDIBits(
                dc.0,
                0,
                0,
                size.width,
                size.height,
                0,
                0,
                width,
                height,
                Some(frame.pixels().as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            ensure!(
                rows > 0 && GdiFlush().as_bool(),
                "scaling the pin bitmap failed"
            );
        }
    }
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        SourceConstantAlpha: alpha,
        ..Default::default()
    };
    let destination = POINT { x: at.x, y: at.y };
    let bounds = SIZE {
        cx: size.width,
        cy: size.height,
    };
    // SAFETY: the source DC and the geometry stay live for this synchronous upload.
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&destination),
            Some(&bounds),
            Some(dc.0),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }
    .context("updating the layered pin window")
}

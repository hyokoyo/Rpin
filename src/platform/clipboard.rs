use std::{mem::size_of, thread, time::Duration};

use anyhow::Context;
use windows::Win32::Foundation::{ERROR_SUCCESS, GlobalFree, HANDLE, HGLOBAL, HWND, SetLastError};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

use crate::{capture::frame::Frame, output::OutputError};

const CF_DIB: u32 = 8;

pub fn copy_image(owner: HWND, frame: &Frame) -> anyhow::Result<()> {
    let (width, height) = (i32::try_from(frame.width)?, i32::try_from(frame.height)?);
    anyhow::ensure!(width > 0 && height > 0, "empty clipboard image");
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let size = size_of::<BITMAPINFOHEADER>()
        .checked_add(frame.pixels().len())
        .ok_or(OutputError::InvalidDimensions)?;
    // SAFETY: movable memory is required for transferring a DIB to Windows.
    let mut data =
        ClipboardMemory(unsafe { GlobalAlloc(GMEM_MOVEABLE, size) }.context("GlobalAlloc")?);
    // SAFETY: data owns a block of size bytes. The handle is unlocked before
    // it is passed to SetClipboardData.
    unsafe {
        let buffer = GlobalLock(data.0).cast::<u8>();
        if buffer.is_null() {
            return Err(windows::core::Error::from_thread()).context("GlobalLock");
        }
        buffer.cast::<BITMAPINFOHEADER>().write(header);
        copy_bottom_up(buffer.add(size_of::<BITMAPINFOHEADER>()), frame);
        SetLastError(ERROR_SUCCESS);
        if let Err(err) = GlobalUnlock(data.0) {
            if err.code().is_err() {
                return Err(err).context("GlobalUnlock");
            }
        }
    }
    let _clipboard = Clipboard::open(owner)?;
    // SAFETY: the clipboard is open on this thread and data is unlocked.
    unsafe {
        EmptyClipboard().context("EmptyClipboard")?;
        SetClipboardData(CF_DIB, Some(HANDLE(data.0.0))).context("SetClipboardData(CF_DIB)")?;
    }
    // Windows owns the allocation after a successful transfer.
    data.0 = HGLOBAL::default();
    Ok(())
}

unsafe fn copy_bottom_up(target: *mut u8, frame: &Frame) {
    let stride = frame.stride as usize;
    for (destination_y, source_y) in (0..frame.height).rev().enumerate() {
        // SAFETY: the caller provides space for the whole frame and it
        // does not alias the source. Initialize each row before borrowing it.
        let row = unsafe {
            let row = target.add(destination_y * stride);
            row.copy_from_nonoverlapping(frame.row(source_y).as_ptr(), stride);
            std::slice::from_raw_parts_mut(row, stride)
        };
        for pixel in row.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
}

struct Clipboard;

impl Clipboard {
    fn open(owner: HWND) -> anyhow::Result<Self> {
        for retry in 0..=5 {
            // SAFETY: owner is the app's hidden window on this UI thread.
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => return Ok(Self),
                Err(err) if retry == 5 => return Err(err).context(OutputError::ClipboardBusy),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        Err(OutputError::ClipboardBusy.into())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: paired with OpenClipboard on the same thread.
        let _ = unsafe { CloseClipboard() };
    }
}

struct ClipboardMemory(HGLOBAL);

impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Windows did not take ownership of this block.
            let _ = unsafe { GlobalFree(Some(self.0)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_rows_are_bottom_up_and_opaque() {
        let frame = Frame::new(1, 2, vec![1, 2, 3, 0, 4, 5, 6, 0]).unwrap();
        let mut output = [0; 8];
        // SAFETY: output is a separate buffer big enough for the two rows.
        unsafe {
            copy_bottom_up(output.as_mut_ptr(), &frame);
        }
        assert_eq!(output, [4, 5, 6, 255, 1, 2, 3, 255]);
    }
}

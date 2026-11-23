use std::ptr::NonNull;

use windows::Win32::Graphics::Gdi::{DeleteObject, GdiFlush, HBITMAP, HGDIOBJ};

#[derive(Debug)]
pub(crate) struct Bitmap(pub HBITMAP);

impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: the selection guard restores the old bitmap before this runs.
        // Flush the deletion on this thread, which may be about to exit.
        let deleted = unsafe { DeleteObject(HGDIOBJ(self.0.0)) }.as_bool();
        let flushed = unsafe { GdiFlush() }.as_bool();
        if !deleted || !flushed {
            crate::logging::warn("could not release capture bitmap");
            #[cfg(test)]
            panic!("Windows could not release the capture bitmap");
        }
    }
}

/// Pixel memory owned by a DIB that has been flushed and deselected.
#[derive(Debug)]
pub(super) struct DibPixels {
    _bitmap: Bitmap,
    bits: NonNull<u8>,
    len: usize,
}

impl DibPixels {
    /// The bitmap owns len initialized bytes at bits. The caller must flush
    /// all GDI writes and deselect it before transferring exclusive ownership.
    pub(super) unsafe fn new(bitmap: Bitmap, bits: NonNull<u8>, len: usize) -> Self {
        Self {
            _bitmap: bitmap,
            bits,
            len,
        }
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        // SAFETY: the owned bitmap keeps these immutable pixels alive.
        unsafe { std::slice::from_raw_parts(self.bits.as_ptr(), self.len) }
    }
}

// SAFETY: the detached, process-owned bitmap is never passed to GDI again until
// destruction. The PNG worker may read and destroy it after ownership transfer.
unsafe impl Send for DibPixels {}
// SAFETY: shared references expose only immutable pixels. Dropping the bitmap
// requires ownership, so it cannot race with readers.
unsafe impl Sync for DibPixels {}

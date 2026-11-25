//! Capture desktop pixels with GDI and transfer the completed frame to the caller.

use std::{
    ptr::{self, NonNull},
    time::{Duration, Instant},
};

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, GdiFlush, GetDC, GetDIBits,
    HDC, HGDIOBJ, HMONITOR, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};

use super::{
    CaptureBackend, CaptureError, CapturedMonitor, MonitorInfo,
    frame::{Frame, Rect},
    monitor,
    pixels::{Bitmap, DibPixels},
};
use crate::logging::{self, Level};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GdiMethod {
    #[default]
    Direct,
    Copy,
    Compatible,
}

impl GdiMethod {
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Copy => "copy",
            Self::Compatible => "compatible",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GdiStages {
    pub setup: Duration,
    pub blit: Duration,
    pub flush: Duration,
    pub readback: Duration,
    pub layered_retries: usize,
}

impl GdiStages {
    fn add(&mut self, other: Self) {
        self.setup += other.setup;
        self.blit += other.blit;
        self.flush += other.flush;
        self.readback += other.readback;
        self.layered_retries += other.layered_retries;
    }
}

#[derive(Debug, Default)]
pub struct GdiCapture(GdiMethod);

impl GdiCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_method(method: GdiMethod) -> Self {
        Self(method)
    }

    pub fn capture_profiled(
        &self,
        monitor: Option<HMONITOR>,
    ) -> Result<(Vec<CapturedMonitor>, GdiStages), CaptureError> {
        let mut stages = GdiStages::default();
        let frames = match monitor {
            Some(handle) => vec![self.capture(
                &DesktopDc::acquire()?,
                monitor::info(handle)?,
                Some(&mut stages),
            )?],
            None => self.capture_all_inner(Some(&mut stages))?,
        };
        Ok((frames, stages))
    }

    fn capture_all_inner(
        &self,
        mut totals: Option<&mut GdiStages>,
    ) -> Result<Vec<CapturedMonitor>, CaptureError> {
        let monitors = monitor::enumerate()?;
        let source = DesktopDc::acquire()?;
        monitors
            .into_iter()
            .map(|info| self.capture(&source, info, totals.as_deref_mut()))
            .collect()
    }

    fn capture(
        &self,
        source: &DesktopDc,
        info: MonitorInfo,
        totals: Option<&mut GdiStages>,
    ) -> Result<CapturedMonitor, CaptureError> {
        let debug = logging::current_level() >= Level::Debug;
        let start = debug.then(Instant::now);
        let (frame, stages) =
            capture_frame(source.0, info.rect, self.0, debug || totals.is_some())?;
        if let Some(totals) = totals {
            totals.add(stages);
        }
        if let Some(start) = start {
            let elapsed = milliseconds(start.elapsed());
            logging::debug(format!(
                "GDI stages: method={}, setup_ms={:.2}, bitblt_ms={:.2}, flush_ms={:.2}, readback_ms={:.2}, layered_retries={}",
                self.0.name(),
                milliseconds(stages.setup),
                milliseconds(stages.blit),
                milliseconds(stages.flush),
                milliseconds(stages.readback),
                stages.layered_retries,
            ));
            logging::debug(format!(
                "GDI capture: {}x{} at ({}, {}), {elapsed:.2} ms",
                frame.width, frame.height, info.rect.left, info.rect.top,
            ));
        }
        Ok(CapturedMonitor { info, frame })
    }
}

impl CaptureBackend for GdiCapture {
    fn capture_all(&self) -> Result<Vec<CapturedMonitor>, CaptureError> {
        self.capture_all_inner(None)
    }

    fn capture_monitor(&self, handle: HMONITOR) -> Result<CapturedMonitor, CaptureError> {
        let info = monitor::info(handle)?;
        self.capture(&DesktopDc::acquire()?, info, None)
    }
}

pub fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

struct StageClock(Option<Instant>);

impl StageClock {
    fn lap(&mut self) -> Duration {
        self.0.as_mut().map_or(Duration::ZERO, |last| {
            let now = Instant::now();
            now - std::mem::replace(last, now)
        })
    }
}

fn capture_frame(
    source: HDC,
    rect: Rect,
    method: GdiMethod,
    profile: bool,
) -> Result<(Frame, GdiStages), CaptureError> {
    let mut clock = StageClock(profile.then(Instant::now));
    let (width, height) = (rect.width(), rect.height());
    let invalid = || CaptureError::InvalidDimensions { width, height };
    let stride = (width as u32).checked_mul(4).ok_or_else(invalid)?;
    let len = (stride as usize)
        .checked_mul(height as usize)
        .filter(|&len| width > 0 && height > 0 && len <= isize::MAX as usize)
        .ok_or_else(invalid)?;
    let (bitmap, bits) = if method == GdiMethod::Compatible {
        (compatible_bitmap(source, width, height)?, None)
    } else {
        let (bitmap, bits) = dib(source, width, height)?;
        (bitmap, Some(bits))
    };
    let dest = MemoryDc::create(Some(source))?;
    let selected = Selection::new(&dest, &bitmap)?;
    let setup = clock.lap();
    let layered_retries = usize::from(blit(source, dest.0, rect)?);
    let blit = clock.lap();

    // SAFETY: flush on the thread that issued the blit before accessing DIB pixels.
    if !unsafe { GdiFlush() }.as_bool() {
        return Err(CaptureError::win32("GdiFlush"));
    }
    let flush = clock.lap();
    selected.restore()?;
    drop(dest);

    let frame = match (method, bits) {
        (GdiMethod::Direct, Some(bits)) => {
            // SAFETY: the flushed, deselected DIB owns len bytes. No DC can
            // access this bitmap after the transfer.
            let pixels = unsafe { DibPixels::new(bitmap, bits, len) };
            Frame::from_dib(width as u32, height as u32, pixels)
        }
        (GdiMethod::Copy, Some(bits)) => {
            let mut pixels = reserve_pixels(len)?;
            // SAFETY: the flushed bitmap is live and initialized for len bytes.
            pixels.extend_from_slice(unsafe { std::slice::from_raw_parts(bits.as_ptr(), len) });
            Frame::new(width as u32, height as u32, pixels)
        }
        (GdiMethod::Compatible, _) => Some(read_compatible(source, &bitmap, width, height, len)?),
        _ => unreachable!("DIB methods always provide pixel storage"),
    }
    .ok_or_else(invalid)?;

    Ok((
        frame,
        GdiStages {
            setup,
            blit,
            flush,
            readback: clock.lap(),
            layered_retries,
        },
    ))
}

fn blit(source: HDC, dest: HDC, rect: Rect) -> Result<bool, CaptureError> {
    let copy = |operation| {
        // SAFETY: both DCs are live and the destination bitmap fits this rect.
        unsafe {
            BitBlt(
                dest,
                0,
                0,
                rect.width(),
                rect.height(),
                Some(source),
                rect.left,
                rect.top,
                operation,
            )
        }
    };
    match copy(SRCCOPY) {
        Ok(()) => Ok(false),
        Err(srccopy) => copy(ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0))
            .map(|()| true)
            .map_err(|layered| CaptureError::BitBlt {
                rect,
                srccopy,
                layered,
            }),
    }
}

fn reserve_pixels(len: usize) -> Result<Vec<u8>, CaptureError> {
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(len)
        .map_err(|_| CaptureError::Allocation(len))?;
    Ok(pixels)
}

fn read_compatible(
    source: HDC,
    bitmap: &Bitmap,
    width: i32,
    height: i32,
    len: usize,
) -> Result<Frame, CaptureError> {
    let mut header = bgra_header(width, height);
    let mut pixels = reserve_pixels(len)?;
    pixels.resize(len, 0);
    // SAFETY: the bitmap is deselected, the source DC is live, and the buffer
    // fits all rows in the requested 32-bit layout.
    let lines = unsafe {
        GetDIBits(
            source,
            bitmap.0,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut header,
            DIB_RGB_COLORS,
        )
    };
    if lines != height {
        return Err(CaptureError::Win32(format!(
            "GetDIBits: read {lines} of {height} lines: {}",
            windows::core::Error::from_thread(),
        )));
    }
    Frame::new(width as u32, height as u32, pixels)
        .ok_or(CaptureError::InvalidDimensions { width, height })
}

pub(crate) fn bgra_header(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Draw into a new top-down DIB on this thread.
///
/// # Safety
/// A successful draw must initialize the entire frame before returning.
pub(crate) unsafe fn render_to_frame(
    bounds: Rect,
    draw: impl FnOnce(HDC) -> anyhow::Result<()>,
) -> anyhow::Result<Frame> {
    let (width, height) = (bounds.width(), bounds.height());
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|area| area.checked_mul(4))
        .filter(|len| width > 0 && height > 0 && *len <= isize::MAX as usize)
        .ok_or(CaptureError::InvalidDimensions { width, height })?;
    let dc = MemoryDc::create(None)?;
    let (bitmap, bits) = dib(dc.0, width, height)?;
    let selected = Selection::new(&dc, &bitmap)?;
    draw(dc.0)?;
    // SAFETY: flush writes on the owning thread before transferring the pixels.
    if !unsafe { GdiFlush() }.as_bool() {
        return Err(CaptureError::win32("GdiFlush").into());
    }
    selected.restore()?;
    // SAFETY: draw completed and this flushed DIB is no longer selected.
    let pixels = unsafe { DibPixels::new(bitmap, bits, len) };
    Frame::from_dib(width as u32, height as u32, pixels)
        .ok_or_else(|| CaptureError::InvalidDimensions { width, height }.into())
}

pub(crate) fn dib(
    source: HDC,
    width: i32,
    height: i32,
) -> Result<(Bitmap, NonNull<u8>), CaptureError> {
    let header = bgra_header(width, height);
    let mut bits = ptr::null_mut();
    // SAFETY: the header describes a top-down 32-bit DIB and bits is writable.
    let bitmap =
        unsafe { CreateDIBSection(Some(source), &header, DIB_RGB_COLORS, &mut bits, None, 0) }
            .map(Bitmap)
            .map_err(|err| CaptureError::Win32(format!("CreateDIBSection: {err}")))?;
    let bits = NonNull::new(bits.cast())
        .ok_or_else(|| CaptureError::Win32("CreateDIBSection returned no pixel buffer".into()))?;
    Ok((bitmap, bits))
}

fn compatible_bitmap(source: HDC, width: i32, height: i32) -> Result<Bitmap, CaptureError> {
    // SAFETY: source is a live DC and the dimensions were validated.
    let bitmap = unsafe { CreateCompatibleBitmap(source, width, height) };
    if bitmap.is_invalid() {
        return Err(CaptureError::win32("CreateCompatibleBitmap"));
    }
    Ok(Bitmap(bitmap))
}

struct DesktopDc(HDC);

impl DesktopDc {
    fn acquire() -> Result<Self, CaptureError> {
        // SAFETY: a null HWND requests the desktop DC for the calling thread.
        let dc = unsafe { GetDC(None) };
        if dc.is_invalid() {
            return Err(CaptureError::win32("GetDC"));
        }
        Ok(Self(dc))
    }
}

impl Drop for DesktopDc {
    fn drop(&mut self) {
        // SAFETY: matches GetDC on this thread, with the same null HWND.
        unsafe { ReleaseDC(None, self.0) };
    }
}

pub(crate) struct MemoryDc(pub HDC);

impl MemoryDc {
    pub(crate) fn create(source: Option<HDC>) -> Result<Self, CaptureError> {
        // SAFETY: a source DC, if supplied, is live on this thread.
        let dc = unsafe { CreateCompatibleDC(source) };
        if dc.is_invalid() {
            return Err(CaptureError::win32("CreateCompatibleDC"));
        }
        Ok(Self(dc))
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        // SAFETY: this guard owns the DC.
        let _ = unsafe { DeleteDC(self.0) };
    }
}

pub(crate) struct Selection<'a> {
    dc: &'a MemoryDc,
    _bitmap: &'a Bitmap,
    previous: HGDIOBJ,
}

impl<'a> Selection<'a> {
    pub(crate) fn new(dc: &'a MemoryDc, bitmap: &'a Bitmap) -> Result<Self, CaptureError> {
        // SAFETY: both objects are live. This guard holds both borrows.
        let previous = unsafe { SelectObject(dc.0, HGDIOBJ(bitmap.0.0)) };
        if previous.is_invalid() {
            return Err(CaptureError::win32("SelectObject"));
        }
        Ok(Self {
            dc,
            _bitmap: bitmap,
            previous,
        })
    }

    fn restore(mut self) -> Result<(), CaptureError> {
        // SAFETY: the previous bitmap remains valid for this live DC.
        let result = unsafe { SelectObject(self.dc.0, self.previous) };
        if result.is_invalid() {
            return Err(CaptureError::win32("restoring the original DC bitmap"));
        }
        self.previous = HGDIOBJ::default();
        Ok(())
    }
}

impl Drop for Selection<'_> {
    fn drop(&mut self) {
        if !self.previous.is_invalid() {
            // SAFETY: this unwinds the selection before either object is dropped.
            unsafe { SelectObject(self.dc.0, self.previous) };
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::{
        Foundation::COLORREF,
        Graphics::Gdi::{PATCOPY, PatBlt, SetPixel},
    };

    use super::*;

    #[test]
    fn methods_copy_source_rectangle_as_top_down_bgra() {
        let screen = DesktopDc::acquire().expect("screen DC");
        let bitmap = compatible_bitmap(screen.0, 3, 3).expect("source bitmap");
        let source = MemoryDc::create(Some(screen.0)).expect("memory DC");
        let _selected = Selection::new(&source, &bitmap).expect("select source bitmap");
        // SAFETY: the source DC has a 3x3 bitmap selected.
        unsafe {
            assert_eq!(
                SetPixel(source.0, 1, 1, COLORREF(0x0000ff)),
                COLORREF(0x0000ff)
            );
            assert_eq!(
                SetPixel(source.0, 1, 2, COLORREF(0xff0000)),
                COLORREF(0xff0000)
            );
        }
        for method in [GdiMethod::Direct, GdiMethod::Copy, GdiMethod::Compatible] {
            let (frame, _) = capture_frame(source.0, Rect::new(1, 1, 2, 3), method, true)
                .unwrap_or_else(|err| panic!("{method:?}: {err}"));
            assert_eq!((frame.width, frame.height, frame.stride), (1, 2, 4));
            assert_eq!(&frame.row(0)[..3], &[0, 0, 255]);
            assert_eq!(&frame.row(1)[..3], &[255, 0, 0]);
        }
    }

    #[test]
    fn frozen_dib_keeps_its_pixels_until_worker_releases_it() {
        let (bitmap, bits) = dib(HDC::default(), 1, 1).expect("DIB");
        let dc = MemoryDc::create(None).expect("memory DC");
        let selected = Selection::new(&dc, &bitmap).expect("select bitmap");
        // SAFETY: the DC points to a one-pixel bitmap on this thread.
        unsafe {
            assert_eq!(SetPixel(dc.0, 0, 0, COLORREF(0x123456)), COLORREF(0x123456));
            assert!(GdiFlush().as_bool());
        }
        selected.restore().expect("deselect bitmap");
        drop(dc);
        // SAFETY: the flushed and deselected DIB owns four bytes.
        let pixels = unsafe { DibPixels::new(bitmap, bits, 4) };
        let frame = Frame::from_dib(1, 1, pixels).unwrap();
        std::thread::spawn(move || assert_eq!(&frame.row(0)[..3], &[0x12, 0x34, 0x56]))
            .join()
            .expect("read and release frame on worker");
    }

    #[test]
    #[ignore = "4K synthetic benchmark; run on its own for handle counts"]
    fn compare_methods_on_4k_memory_dc() {
        use crate::platform::process::resources;

        let (width, height) = (3840, 2160);
        let screen = DesktopDc::acquire().expect("screen DC");
        let bitmap = compatible_bitmap(screen.0, width, height).expect("4K bitmap");
        let source = MemoryDc::create(Some(screen.0)).expect("memory DC");
        let _selection = Selection::new(&source, &bitmap).expect("select source bitmap");
        // SAFETY: the source DC owns the selected 4K bitmap being initialized.
        unsafe { PatBlt(source.0, 0, 0, width, height, PATCOPY) }.expect("fill 4K bitmap");
        assert!(unsafe { GdiFlush() }.as_bool());
        let rect = Rect::new(0, 0, width, height);
        println!("before: {}", resources().unwrap());
        for method in [GdiMethod::Direct, GdiMethod::Copy, GdiMethod::Compatible] {
            let mut totals = Vec::new();
            let mut readbacks = Vec::new();
            for _ in 0..24 {
                let start = Instant::now();
                let (frame, stages) = capture_frame(source.0, rect, method, true).unwrap();
                totals.push(start.elapsed());
                readbacks.push(stages.readback);
                assert_eq!(frame.pixels().len(), 33_177_600);
                drop(frame);
            }
            totals.sort_unstable();
            readbacks.sort_unstable();
            println!(
                "{}: median_total_ms={:.2}, median_readback_ms={:.2}; after: {}",
                method.name(),
                milliseconds(totals[totals.len() / 2]),
                milliseconds(readbacks[readbacks.len() / 2]),
                resources().unwrap(),
            );
        }
    }
}

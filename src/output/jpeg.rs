use std::fs::File;

use anyhow::Context;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::IUnknown;

use crate::capture::frame::Frame;

use super::file_stream;

pub(super) fn write(frame: &Frame, file: &File) -> anyhow::Result<()> {
    anyhow::ensure!(frame.width > 0 && frame.height > 0, "empty JPEG image");
    let _com = EncoderCom::new()?;
    let stream = file_stream::from_file(file.try_clone()?);
    // SAFETY: COM is initialized on this worker thread. The stream takes
    // ownership of a duplicate of the existing temporary file handle.
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(
            &CLSID_WICImagingFactory,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
        .context("creating WIC imaging factory")?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut output = None;
        encoder.CreateNewFrame(&mut output, std::ptr::null_mut())?;
        let output = output.context("WIC did not create a JPEG frame")?;
        // The Windows JPEG codec defaults to ImageQuality=0.9.
        output.Initialize(None)?;
        output.SetSize(frame.width, frame.height)?;
        let mut format = GUID_WICPixelFormat24bppBGR;
        output.SetPixelFormat(&mut format)?;
        anyhow::ensure!(
            format == GUID_WICPixelFormat24bppBGR,
            "WIC rejected 24-bit BGR"
        );
        // Convert in small batches; never hold another raw image in memory.
        let stride = frame.width.checked_mul(3).context("JPEG stride overflow")?;
        let rows_per_batch = (65536 / stride).max(1).min(frame.height);
        let mut bgr = vec![0; stride as usize * rows_per_batch as usize];
        for y in (0..frame.height).step_by(rows_per_batch as usize) {
            let rows = rows_per_batch.min(frame.height - y);
            let pixels = &frame.pixels()
                [y as usize * frame.stride as usize..(y + rows) as usize * frame.stride as usize];
            let bgr = &mut bgr[..rows as usize * stride as usize];
            for (src, dst) in pixels.chunks_exact(4).zip(bgr.chunks_exact_mut(3)) {
                dst.copy_from_slice(&src[..3]);
            }
            output.WritePixels(rows, stride, bgr)?;
        }
        output.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

struct EncoderCom;

impl EncoderCom {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: per-thread initialization, paired in Drop.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}

impl Drop for EncoderCom {
    fn drop(&mut self) {
        // SAFETY: EncoderCom is created and dropped on this encoding thread.
        unsafe {
            CoUninitialize();
        }
    }
}

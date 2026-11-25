//! Adapt an already-open temporary file for Windows Imaging Component.
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use windows::Win32::Foundation::{
    E_FAIL, E_NOTIMPL, E_POINTER, S_FALSE, S_OK, STG_E_INVALIDFUNCTION,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC,
    STGM_READWRITE, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows::core::{Error, HRESULT, Ref, Result, implement};

#[implement(IStream)]
struct FileStream(Mutex<File>);

pub(super) fn from_file(file: File) -> IStream {
    FileStream(Mutex::new(file)).into()
}

impl FileStream_Impl {
    fn with_file<T>(&self, op: impl FnOnce(&mut File) -> std::io::Result<T>) -> Result<T> {
        let mut file = self.0.lock().map_err(|_| Error::from(E_FAIL))?;
        op(&mut file).map_err(|err| {
            err.raw_os_error()
                .map_or(E_FAIL, |code| HRESULT::from_win32(code as u32))
                .into()
        })
    }
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, ptr: *mut c_void, len: u32, read: *mut u32) -> HRESULT {
        set_count(read, 0);
        if len == 0 {
            return S_OK;
        }
        if ptr.is_null() {
            return E_POINTER;
        }
        // SAFETY: ISequentialStream provides a writable buffer of len bytes.
        // Initialize it before borrowing it as a Rust Read buffer.
        let bytes = unsafe {
            let ptr = ptr.cast::<u8>();
            ptr.write_bytes(0, len as usize);
            std::slice::from_raw_parts_mut(ptr, len as usize)
        };
        match self.with_file(|file| file.read(bytes)) {
            Ok(count) => {
                set_count(read, count as u32);
                if count == len as usize { S_OK } else { S_FALSE }
            }
            Err(err) => err.code(),
        }
    }

    fn Write(&self, ptr: *const c_void, len: u32, written: *mut u32) -> HRESULT {
        set_count(written, 0);
        if len == 0 {
            return S_OK;
        }
        if ptr.is_null() {
            return E_POINTER;
        }
        // SAFETY: ISequentialStream provides a readable buffer of len bytes.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast(), len as usize) };
        match self.with_file(|file| file.write_all(bytes)) {
            Ok(()) => {
                set_count(written, len);
                S_OK
            }
            Err(err) => err.code(),
        }
    }
}

fn set_count(ptr: *mut u32, value: u32) {
    if !ptr.is_null() {
        // SAFETY: an optional output argument of ISequentialStream.
        unsafe {
            ptr.write(value);
        }
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, position: *mut u64) -> Result<()> {
        let from = match origin {
            STREAM_SEEK_SET if offset >= 0 => SeekFrom::Start(offset as u64),
            STREAM_SEEK_CUR => SeekFrom::Current(offset),
            STREAM_SEEK_END => SeekFrom::End(offset),
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let offset = self.with_file(|file| file.seek(from))?;
        if !position.is_null() {
            // SAFETY: IStream provides an optional writable new position.
            unsafe {
                position.write(offset);
            }
        }
        Ok(())
    }

    fn SetSize(&self, size: u64) -> Result<()> {
        self.with_file(|file| file.set_len(size))
    }

    fn Commit(&self, _: &STGC) -> Result<()> {
        self.with_file(|file| file.flush())
    }

    fn Stat(&self, info: *mut STATSTG, _: &STATFLAG) -> Result<()> {
        if info.is_null() {
            return Err(E_POINTER.into());
        }
        let size = self.with_file(|file| Ok(file.metadata()?.len()))?;
        // SAFETY: IStream provides writable storage. This stream has no COM name.
        unsafe {
            info.write(STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: size,
                grfMode: STGM_READWRITE,
                ..Default::default()
            });
        }
        Ok(())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Revert(&self) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Clone(&self) -> Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

use std::fs::{self, File};
use std::io::{BufWriter, ErrorKind};
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_INFO, FILE_GENERIC_WRITE, FileDispositionInfo,
    MOVEFILE_REPLACE_EXISTING, MoveFileExW, SetFileInformationByHandle,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::PCWSTR;

use crate::capture::frame::Frame;

use super::filename::wide;
use super::png::{Compression, write_rgb_with_compression};

pub const WM_SAVE_DONE: u32 = WM_APP + 4;
pub const THREAD_NAME: &str = "rpin-png";
static NEXT_TEMP: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
pub struct SaveWorker {
    pending: Option<Receiver<anyhow::Result<PathBuf>>>,
}

impl SaveWorker {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn start(
        &mut self,
        owner: HWND,
        frame: Frame,
        path: PathBuf,
        compression: Compression,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.busy(), "PNG worker is busy");
        let (sender, receiver) = mpsc::channel();
        let owner = owner.0 as isize;
        std::thread::Builder::new()
            .name(THREAD_NAME.into())
            .spawn(move || {
                let start = Instant::now();
                let result = std::panic::catch_unwind(|| save_png(&frame, path, compression))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("PNG worker panicked")));
                drop(frame);
                crate::logging::debug(format!(
                    "PNG worker finished in {:.2} ms",
                    start.elapsed().as_secs_f64() * 1000.0
                ));
                if sender.send(result).is_ok() {
                    // SAFETY: the HWND is only used to queue a value-less notification.
                    let _ = unsafe {
                        PostMessageW(Some(HWND(owner as _)), WM_SAVE_DONE, WPARAM(0), LPARAM(0))
                    };
                }
            })
            .context("starting PNG worker")?;
        self.pending = Some(receiver);
        Ok(())
    }

    pub fn take_result(&mut self) -> Option<anyhow::Result<PathBuf>> {
        let result = match self.pending.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("PNG worker disconnected")),
        };
        self.pending = None;
        Some(result)
    }

    pub fn shutdown(&mut self) {
        if let Some(receiver) = self.pending.take() {
            match receiver.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(path)) => crate::logging::info(format!("saved {}", path.display())),
                Ok(Err(err)) => crate::logging::error(format!("save failed: {err:#}")),
                Err(err) => crate::logging::warn(format!("PNG worker during shutdown: {err}")),
            }
        }
    }
}

fn save_png(frame: &Frame, path: PathBuf, compression: Compression) -> anyhow::Result<PathBuf> {
    let folder = path.parent().context("save path has no directory")?;
    let folder = fs::canonicalize(folder).context("resolving output directory")?;
    let destination = folder.join(path.file_name().context("save path has no file name")?);
    let (mut temp, file) = TemporaryFile::create(&folder)?;
    let mut writer = BufWriter::new(file);
    write_rgb_with_compression(frame, &mut writer, compression).context("encoding PNG")?;
    // Flush before publishing the file so buffered I/O errors reach the caller.
    let source = wide(temp.path.as_os_str())?;
    let target = wide(destination.as_os_str())?;
    let file = writer.into_inner().context("flushing PNG")?;
    // Keep automatic deletion enabled while encoding and preparing the paths.
    if temp.delete_on_close {
        set_delete_on_close(&file, false).context("retaining the completed PNG")?;
    }
    drop(file);
    // SAFETY: both paths are null-terminated. The save dialog has handled
    // overwrite confirmation for the selected path.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    }
    .with_context(|| format!("publishing {}", path.display()))?;
    temp.published = true;
    Ok(path)
}

struct TemporaryFile {
    path: PathBuf,
    published: bool,
    delete_on_close: bool,
}

impl TemporaryFile {
    fn create(folder: &Path) -> anyhow::Result<(Self, File)> {
        for _ in 0..32 {
            let index = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = folder.join(format!(".rpin-{}-{index}.tmp", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .access_mode(FILE_GENERIC_WRITE.0 | DELETE.0)
                .open(&path)
            {
                Ok(file) => {
                    let delete_on_close = match set_delete_on_close(&file, true) {
                        Ok(()) => true,
                        Err(err) => {
                            crate::logging::debug(format!(
                                "automatic PNG temp cleanup unavailable: {err}"
                            ));
                            false
                        }
                    };
                    return Ok((
                        Self {
                            path,
                            published: false,
                            delete_on_close,
                        },
                        file,
                    ));
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        anyhow::bail!("could not reserve a temporary PNG file")
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.published {
            if let Err(err) = fs::remove_file(&self.path) {
                if err.kind() != ErrorKind::NotFound {
                    crate::logging::warn(format!(
                        "could not clean PNG temp {}: {err}",
                        self.path.display()
                    ));
                }
            }
        }
    }
}

fn set_delete_on_close(file: &File, delete: bool) -> windows::core::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: delete };
    // SAFETY: this live file handle has DELETE access and info matches the class.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

    struct TestFolder(PathBuf);

    impl TestFolder {
        fn new() -> std::io::Result<Self> {
            let folder = std::env::temp_dir().join(format!(
                "rpin-save-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&folder)?;
            Ok(Self(folder))
        }
    }

    impl Drop for TestFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn interrupted_encoding_leaves_no_temp_file() -> anyhow::Result<()> {
        use std::io::Write;
        let (temp, mut file) = TemporaryFile::create(&std::env::temp_dir())?;
        let path = temp.path.clone();
        file.write_all(b"partial PNG")?;
        drop(file);
        if temp.delete_on_close {
            // The filesystem has deleted it before running our Rust guard.
            assert!(!path.exists());
            println!("filesystem removed the unfinished file on handle close");
        }
        drop(temp);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn publishes_complete_png_at_chosen_path() -> anyhow::Result<()> {
        let folder = TestFolder::new()?;
        let path = folder.0.join("截图.png");
        fs::write(&path, b"old file")?;
        let frame = Frame::new(1, 1, vec![7, 2, 3, 0]).unwrap();
        let saved = save_png(&frame, path.clone(), Compression::Small)?;
        assert_eq!(saved, path);
        let mut reader = ::png::Decoder::new(std::io::Cursor::new(fs::read(&path)?)).read_info()?;
        let mut pixel = [0; 3];
        reader.next_frame(&mut pixel)?;
        assert_eq!(pixel, [3, 2, 7]);
        assert_eq!(fs::read_dir(&folder.0)?.count(), 1);
        Ok(())
    }

    #[test]
    fn failed_publish_keeps_destination_and_cleans_temp() -> anyhow::Result<()> {
        let folder = TestFolder::new()?;
        let path = folder.0.join("locked.png");
        fs::write(&path, b"old file")?;
        let _locked = File::options()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&path)?;
        let frame = Frame::new(1, 1, vec![7, 2, 3, 0]).unwrap();
        let err = save_png(&frame, path.clone(), Compression::Fast).unwrap_err();
        assert!(err.to_string().starts_with("publishing "));
        assert_eq!(fs::read(path)?, b"old file");
        assert_eq!(fs::read_dir(&folder.0)?.count(), 1);
        Ok(())
    }
}

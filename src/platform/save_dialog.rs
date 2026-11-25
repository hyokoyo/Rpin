use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IBindCtx,
};
use windows::Win32::UI::Shell::{
    Common::COMDLG_FILTERSPEC, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_OVERWRITEPROMPT,
    FOS_PATHMUSTEXIST, FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH,
};
use windows::core::{IUnknown, PCWSTR, PWSTR};

use crate::output::ImageFormat;
use crate::output::filename::wide;

pub fn choose_image(
    owner: HWND,
    folder: Option<&Path>,
    name: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let name = wide(OsStr::new(name))?;
    let default_suffix = ImageFormat::SUPPORTED[0].extensions()[0];
    let default_extension = wide(OsStr::new(default_suffix))?;
    let filter_strings = ImageFormat::SUPPORTED.map(|format| {
        let extensions = format.extensions();
        let label = format!("{} 图片 (*.{})", format.label(), extensions.join(", *."));
        let pattern = format!("*.{}", extensions.join(";*."));
        (
            label.encode_utf16().chain([0]).collect::<Vec<_>>(),
            pattern.encode_utf16().chain([0]).collect::<Vec<_>>(),
        )
    });
    let filters = filter_strings
        .each_ref()
        .map(|(label, pattern)| COMDLG_FILTERSPEC {
            pszName: PCWSTR(label.as_ptr()),
            pszSpec: PCWSTR(pattern.as_ptr()),
        });
    // SAFETY: the UI thread has initialized COM, the owner is live, and the
    // input strings remain null-terminated for the duration of these calls.
    unsafe {
        let dialog: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None::<&IUnknown>, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(
            dialog.GetOptions()?
                | FOS_OVERWRITEPROMPT
                | FOS_FORCEFILESYSTEM
                | FOS_PATHMUSTEXIST
                | FOS_NOCHANGEDIR,
        )?;
        dialog.SetFileTypes(&filters)?;
        dialog.SetFileTypeIndex(1)?;
        dialog.SetDefaultExtension(PCWSTR(default_extension.as_ptr()))?;
        dialog.SetFileName(PCWSTR(name.as_ptr()))?;
        if let Some(folder) = folder {
            if let Err(err) = set_initial_folder(&dialog, folder) {
                crate::logging::warn(format!("could not set initial save folder: {err:#}"));
            }
        }
        match dialog.Show(Some(owner)) {
            Ok(()) => {}
            Err(err) if err.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
            Err(err) => return Err(err).context("showing the Save As dialog"),
        }
        let path = take_shell_path(dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?);
        anyhow::ensure!(
            ImageFormat::from_path(&path).is_some(),
            "请选择“保存类型”中支持的文件扩展名"
        );
        Ok(Some(path))
    }
}

fn set_initial_folder(dialog: &IFileSaveDialog, folder: &Path) -> anyhow::Result<()> {
    let folder = std::path::absolute(folder)?;
    anyhow::ensure!(
        folder.is_dir(),
        "save dialog folder does not exist: {}",
        folder.display()
    );
    let folder = wide(folder.as_os_str())?;
    // SAFETY: the path is null-terminated and the dialog belongs to this UI thread.
    unsafe {
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(folder.as_ptr()), None::<&IBindCtx>)?;
        dialog.SetFolder(&item)?;
    }
    Ok(())
}

/// Takes ownership of a null-terminated string allocated by the Shell.
unsafe fn take_shell_path(path: PWSTR) -> PathBuf {
    // SAFETY: GetDisplayName returns a COM-allocated UTF-16 string on success.
    unsafe {
        let owned = PathBuf::from(OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast()));
        owned
    }
}

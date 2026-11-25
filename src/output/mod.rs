//! Screenshot output actions and backends.
//!
//! `App::execute_output` is the single entry point for every output path.
//!
pub mod crop;
mod file_stream;
pub mod filename;
mod jpeg;
pub mod png;
pub mod save;

use thiserror::Error;

use crate::capture::frame::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }

    pub(crate) fn from_path(path: &std::path::Path) -> Option<Self> {
        let extension = path.extension()?;
        if extension.eq_ignore_ascii_case("png") {
            Some(Self::Png)
        } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
            Some(Self::Jpeg)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputAction {
    Copy,
    SaveAs,
    SaveAsOnly,
    Pin,
    PinAndCopy,
}

#[derive(Debug, Clone, Copy)]
pub enum SelectionOutcome {
    Confirmed { rect: Rect, action: OutputAction },
    Cancelled,
}

#[derive(Debug, Error)]
pub enum OutputError {
    #[error("clipboard is busy")]
    ClipboardBusy,
    #[error("invalid screenshot dimensions")]
    InvalidDimensions,
    #[error("could not allocate {0} bytes for the cropped screenshot")]
    Allocation(usize),
}

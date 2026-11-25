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
    /// Dialog order, with the default format first.
    pub(crate) const SUPPORTED: [Self; 2] = [Self::Png, Self::Jpeg];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }

    pub(crate) fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Png => &["png"],
            Self::Jpeg => &["jpg", "jpeg"],
        }
    }

    pub(crate) fn from_path(path: &std::path::Path) -> Option<Self> {
        let extension = path.extension()?;
        Self::SUPPORTED.into_iter().find(|format| {
            format
                .extensions()
                .iter()
                .any(|suffix| extension.eq_ignore_ascii_case(suffix))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputAction {
    Copy,
    SaveAs,
    /// Open the shared Save As dialog without copying to the clipboard.
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::ImageFormat;

    #[test]
    fn recognizes_supported_extensions_without_regard_to_case() {
        for (name, expected) in [
            ("screen.PNG", Some(ImageFormat::Png)),
            ("screen.JpG", Some(ImageFormat::Jpeg)),
            ("screen.jpeg", Some(ImageFormat::Jpeg)),
            ("screen.bmp", None),
            ("screen", None),
        ] {
            assert_eq!(ImageFormat::from_path(Path::new(name)), expected);
        }
    }
}

//! Screenshot output actions and backends.
//!
//! `App::execute_output` is the single entry point for every output path.
//!
pub mod crop;
pub mod filename;
pub mod png;
pub mod png_writer;

use thiserror::Error;

use crate::capture::frame::Rect;

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

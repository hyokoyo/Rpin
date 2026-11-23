use crate::capture::{
    CapturedMonitor,
    frame::{Frame, Rect},
};

use super::OutputError;

/// Compose a compact physical-pixel selection and release the source frames.
/// An exact whole-monitor selection moves the existing frame instead.
pub fn compose(mut frames: Vec<CapturedMonitor>, selection: Rect) -> Result<Frame, OutputError> {
    if selection.is_empty() {
        return Err(OutputError::InvalidDimensions);
    }
    frames.retain(|monitor| monitor.info.rect.intersect(selection).is_some());
    if frames.len() == 1 && frames[0].info.rect == selection {
        return Ok(frames.swap_remove(0).frame);
    }

    let width = selection
        .right
        .checked_sub(selection.left)
        .ok_or(OutputError::InvalidDimensions)? as u32;
    let height = selection
        .bottom
        .checked_sub(selection.top)
        .ok_or(OutputError::InvalidDimensions)? as u32;
    let stride = width.checked_mul(4).ok_or(OutputError::InvalidDimensions)? as usize;
    let length = stride
        .checked_mul(height as usize)
        .ok_or(OutputError::InvalidDimensions)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(length)
        .map_err(|_| OutputError::Allocation(length))?;

    if let [monitor] = frames.as_slice() {
        let bounds = monitor.info.rect;
        if bounds.intersect(selection) == Some(selection) {
            let source_x = (selection.left - bounds.left) as usize * 4;
            let source_y = (selection.top - bounds.top) as u32;
            if stride == monitor.frame.stride as usize {
                let start = source_y as usize * stride;
                pixels.extend_from_slice(&monitor.frame.pixels()[start..start + length]);
            } else {
                for y in source_y..source_y + height {
                    let source = &monitor.frame.row(y)[source_x..source_x + stride];
                    pixels.extend_from_slice(source);
                }
            }
            return Frame::new(width, height, pixels).ok_or(OutputError::InvalidDimensions);
        }
    }

    // A selection that spans displays can include gaps, which stay black.
    pixels.resize(length, 0);
    for monitor in frames {
        let rect = monitor.info.rect;
        let Some(overlap) = rect.intersect(selection) else {
            continue;
        };
        let source_x = (overlap.left - rect.left) as usize * 4;
        let target_x = (overlap.left - selection.left) as usize * 4;
        let source_y = (overlap.top - rect.top) as u32;
        let target_y = (overlap.top - selection.top) as usize;
        let row_bytes = overlap.width() as usize * 4;
        for row in 0..overlap.height() as usize {
            let source = &monitor.frame.row(source_y + row as u32)[source_x..source_x + row_bytes];
            let offset = (target_y + row) * stride + target_x;
            pixels[offset..offset + row_bytes].copy_from_slice(source);
        }
    }
    Frame::new(width, height, pixels).ok_or(OutputError::InvalidDimensions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::MonitorInfo;

    fn monitor(rect: Rect, color: u8) -> CapturedMonitor {
        let (width, height) = (rect.width() as u32, rect.height() as u32);
        CapturedMonitor {
            info: MonitorInfo {
                rect,
                work: rect,
                handle: Default::default(),
                dpi: 96,
                primary: false,
            },
            frame: Frame::new(width, height, vec![color; (width * height * 4) as usize]).unwrap(),
        }
    }

    #[test]
    fn composes_negative_origin_and_monitor_gap() {
        let frames = vec![
            monitor(Rect::new(-3, -1, -1, 1), 10),
            monitor(Rect::new(0, 0, 2, 2), 20),
        ];
        let frame = compose(frames, Rect::new(-2, -1, 2, 1)).unwrap();
        assert_eq!(frame.row(0), &[[10; 4], [0; 4], [0; 4], [0; 4]].concat());
        assert_eq!(frame.row(1), &[[10; 4], [0; 4], [20; 4], [20; 4]].concat());
    }

    #[test]
    fn moves_whole_monitor_buffer_without_allocating() {
        let rect = Rect::new(-2, 0, 0, 2);
        let monitor = monitor(rect, 3);
        let original = monitor.frame.pixels().as_ptr();
        let frame = compose(vec![monitor], rect).unwrap();
        assert_eq!(frame.pixels().as_ptr(), original);
    }

    #[test]
    fn copies_only_the_selected_rows_and_columns() {
        let bounds = Rect::new(-3, 4, 1, 7);
        let mut monitor = monitor(bounds, 0);
        let pixels: Vec<u8> = (0..12).flat_map(|pixel| [pixel; 4]).collect();
        monitor.frame = Frame::new(4, 3, pixels).unwrap();
        let frame = compose(vec![monitor], Rect::new(-2, 5, 0, 7)).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (2, 2, 8));
        assert_eq!(frame.pixels(), &[[5; 4], [6; 4], [9; 4], [10; 4]].concat());
    }

    #[test]
    fn copies_a_contiguous_band_when_the_selection_spans_the_source_width() {
        let bounds = Rect::new(-3, 4, 1, 7);
        let mut monitor = monitor(bounds, 0);
        let pixels: Vec<u8> = (0..48).collect();
        monitor.frame = Frame::new(4, 3, pixels.clone()).unwrap();
        let frame = compose(vec![monitor], Rect::new(-3, 5, 1, 7)).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (4, 2, 16));
        assert_eq!(frame.pixels(), &pixels[16..]);
    }

    #[test]
    fn keeps_black_pixels_outside_a_single_monitor() {
        let frames = vec![monitor(Rect::new(-1, -1, 1, 1), 2)];
        let frame = compose(frames, Rect::new(0, -1, 2, 1)).unwrap();
        assert_eq!(frame.pixels(), &[[2; 4], [0; 4], [2; 4], [0; 4]].concat());
    }
}

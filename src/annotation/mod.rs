//! Editable commands in selection-local physical pixels.
mod render;

pub(crate) use render::{Painter, flatten, rgb_color, text_format};

use std::mem::size_of;

use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};

const HISTORY_BYTES: usize = 8 * 1024 * 1024;
const HISTORY_COUNT: usize = 512;
const MAX_STROKE_POINTS: usize = 8192;
const MAX_MOSAIC_SIDE: i32 = 128;
const DEFAULT_INK: [u8; 3] = [255, 51, 31];
pub(crate) const MAX_TEXT_UNITS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextStyle {
    pub(crate) size_px: u16,
    pub(crate) rgb: [u8; 3],
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            size_px: 24,
            rgb: DEFAULT_INK,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum StrokeWidth {
    Thin,
    #[default]
    Medium,
    Thick,
}

impl StrokeWidth {
    pub(crate) const OPTIONS: [Self; 3] = [Self::Thin, Self::Medium, Self::Thick];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Thin => "细",
            Self::Medium => "中",
            Self::Thick => "粗",
        }
    }

    fn pixels(self) -> i32 {
        match self {
            Self::Thin => 2,
            Self::Medium => 4,
            Self::Thick => 8,
        }
    }

    fn paint_margin(self) -> i32 {
        self.pixels() / 2 + 2
    }

    fn arrow_head_limit(self) -> i32 {
        12 + self.pixels()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StrokeStyle {
    pub(crate) width: StrokeWidth,
    pub(crate) rgb: [u8; 3],
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            width: StrokeWidth::default(),
            rgb: DEFAULT_INK,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Pen,
    Arrow,
    Rectangle,
    Ellipse,
    Text,
    Mosaic,
}

impl Tool {
    pub(crate) fn uses_stroke(self) -> bool {
        matches!(
            self,
            Self::Pen | Self::Arrow | Self::Rectangle | Self::Ellipse
        )
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pen => "画笔",
            Self::Arrow => "箭头",
            Self::Rectangle => "矩形",
            Self::Ellipse => "椭圆",
            Self::Text => "文字",
            Self::Mosaic => "马赛克",
        }
    }

    pub(crate) fn compact_label(self) -> &'static str {
        match self {
            Self::Pen => "笔",
            Self::Arrow => "↗",
            Self::Rectangle => "□",
            Self::Ellipse => "○",
            Self::Text => "字",
            Self::Mosaic => "▦",
        }
    }
}

struct Annotation {
    shape: Shape,
    damage: Rect,
}

enum Shape {
    Stroke(StrokeShape, StrokeStyle),
    Text {
        area: Rect,
        units: Vec<u16>,
        style: TextStyle,
    },
    Mosaic(Mosaic),
}

enum StrokeShape {
    Pen(Vec<Point>),
    Arrow(Point, Point),
    Rectangle(Rect),
    Ellipse(Rect),
}

struct Mosaic {
    rect: Rect,
    tile: i32,
    columns: usize,
    colors: Vec<[u8; 4]>,
}

impl Annotation {
    fn bytes(&self) -> usize {
        size_of::<Self>()
            + match &self.shape {
                Shape::Stroke(StrokeShape::Pen(points), _) => {
                    points.capacity() * size_of::<Point>()
                }
                Shape::Text { units, .. } => units.capacity() * size_of::<u16>(),
                Shape::Mosaic(mosaic) => mosaic.colors.capacity() * size_of::<[u8; 4]>(),
                _ => 0,
            }
    }
}

#[derive(Default)]
pub(crate) struct Document {
    commands: Vec<Annotation>,
    applied: usize,
    bytes: usize,
}

impl Document {
    pub(crate) fn is_empty(&self) -> bool {
        self.applied == 0
    }

    fn push(&mut self, annotation: Annotation) {
        let redo_bytes: usize = self.commands[self.applied..]
            .iter()
            .map(Annotation::bytes)
            .sum();
        let bytes = annotation.bytes();
        if self.applied >= HISTORY_COUNT || self.bytes - redo_bytes + bytes > HISTORY_BYTES {
            crate::logging::debug("annotation history budget reached");
            return;
        }
        for entry in self.commands.drain(self.applied..) {
            self.bytes -= entry.bytes();
        }
        self.bytes += bytes;
        self.commands.push(annotation);
        self.applied += 1;
    }

    fn undo(&mut self) -> Option<Rect> {
        self.applied = self.applied.checked_sub(1)?;
        Some(self.commands[self.applied].damage)
    }

    fn redo(&mut self) -> Option<Rect> {
        let entry = self.commands.get(self.applied)?;
        self.applied += 1;
        Some(entry.damage)
    }
}

struct Gesture {
    start: Point,
    end: Point,
    tool: Tool,
    style: StrokeStyle,
    points: Vec<Point>,
    damage: Rect,
}

impl Gesture {
    fn update(&mut self, point: Point, final_point: bool) -> Option<Rect> {
        if point == self.end
            || (self.tool == Tool::Pen
                && !final_point
                && self.end.x.abs_diff(point.x) + self.end.y.abs_diff(point.y) < 2)
        {
            return None;
        }
        let previous = std::mem::replace(&mut self.end, point);
        if self.tool == Tool::Pen {
            let segment =
                stroke_damage(pixel_rect(previous, point), self.style.width.paint_margin());
            if self.points.len() < MAX_STROKE_POINTS {
                self.points.push(point);
            } else {
                // Join the last stored point to the new endpoint. Repaint the
                // full stroke when replacing its last segment.
                *self.points.last_mut().expect("bounded pen stroke") = point;
                self.damage = self.damage.union(segment);
                return Some(self.damage);
            }
            self.damage = self.damage.union(segment);
            return Some(segment);
        }
        let old = self.damage;
        self.damage = stroke_damage(pixel_rect(self.start, point), self.margin());
        Some(old.union(self.damage))
    }

    fn margin(&self) -> i32 {
        let width = self.style.width;
        match self.tool {
            Tool::Arrow => width.paint_margin() + (width.arrow_head_limit() + 1) / 2,
            Tool::Mosaic => StrokeWidth::Medium.paint_margin(),
            _ => width.paint_margin(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Editor {
    pub(crate) tool: Option<Tool>,
    pub(crate) text_style: TextStyle,
    pub(crate) stroke_style: StrokeStyle,
    document: Document,
    gesture: Option<Gesture>,
}

impl Editor {
    pub(crate) fn locks_selection(&self) -> bool {
        self.tool.is_some() || !self.document.commands.is_empty()
    }

    pub(crate) fn dragging(&self) -> bool {
        self.gesture.is_some()
    }

    pub(crate) fn has_marks(&self) -> bool {
        !self.document.is_empty() || self.dragging()
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.document.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        self.document.applied < self.document.commands.len()
    }

    pub(crate) fn begin(&mut self, tool: Tool, point: Point) -> Rect {
        let mut gesture = Gesture {
            start: point,
            end: point,
            tool,
            style: self.stroke_style,
            points: if tool == Tool::Pen {
                vec![point]
            } else {
                Vec::new()
            },
            damage: Rect::default(),
        };
        let bounds = stroke_damage(pixel_rect(point, point), gesture.margin());
        gesture.damage = bounds;
        self.gesture = Some(gesture);
        bounds
    }

    pub(crate) fn update(&mut self, point: Point) -> Option<Rect> {
        self.gesture.as_mut()?.update(point, false)
    }

    pub(crate) fn finish(
        &mut self,
        point: Point,
        selection: Rect,
        frames: &[CapturedMonitor],
    ) -> Option<Rect> {
        let mut gesture = self.gesture.take()?;
        let redraw = gesture.update(point, true);
        let rect = pixel_rect(gesture.start, gesture.end);
        let shape = match gesture.tool {
            Tool::Pen => Some(Shape::Stroke(
                StrokeShape::Pen(gesture.points),
                gesture.style,
            )),
            Tool::Arrow if gesture.start != gesture.end => Some(Shape::Stroke(
                StrokeShape::Arrow(gesture.start, gesture.end),
                gesture.style,
            )),
            Tool::Rectangle if rect.width() > 1 && rect.height() > 1 => {
                Some(Shape::Stroke(StrokeShape::Rectangle(rect), gesture.style))
            }
            Tool::Ellipse if rect.width() > 1 && rect.height() > 1 => {
                Some(Shape::Stroke(StrokeShape::Ellipse(rect), gesture.style))
            }
            Tool::Mosaic if rect.width() > 1 && rect.height() > 1 => {
                Some(Shape::Mosaic(Mosaic::sample(rect, selection, frames)))
            }
            _ => None,
        };
        if let Some(shape) = shape {
            self.document.push(Annotation {
                shape,
                damage: gesture.damage,
            });
        }
        Some(redraw.map_or(gesture.damage, |redraw| redraw.union(gesture.damage)))
    }

    pub(crate) fn cancel_gesture(&mut self) -> Option<Rect> {
        self.gesture.take().map(|gesture| gesture.damage)
    }

    pub(crate) fn add_text(
        &mut self,
        area: Rect,
        units: Vec<u16>,
        style: TextStyle,
    ) -> Option<Rect> {
        if units.is_empty() || area.is_empty() {
            return None;
        }
        self.document.push(Annotation {
            shape: Shape::Text { area, units, style },
            damage: area,
        });
        Some(area)
    }

    pub(crate) fn undo(&mut self) -> Option<Rect> {
        self.document.undo()
    }

    pub(crate) fn redo(&mut self) -> Option<Rect> {
        self.document.redo()
    }

    pub(crate) fn take_document(&mut self) -> Document {
        std::mem::take(&mut self.document)
    }
}

pub(crate) fn local_point(point: Point, selection: Rect) -> Point {
    Point::new(
        (point.x - selection.left).clamp(0, selection.width() - 1),
        (point.y - selection.top).clamp(0, selection.height() - 1),
    )
}

fn pixel_rect(a: Point, b: Point) -> Rect {
    let mut rect = Rect::normalize(a, b);
    rect.right += 1;
    rect.bottom += 1;
    rect
}

fn stroke_damage(rect: Rect, margin: i32) -> Rect {
    Rect::new(
        rect.left - margin,
        rect.top - margin,
        rect.right + margin,
        rect.bottom + margin,
    )
}

impl Mosaic {
    fn sample(rect: Rect, selection: Rect, frames: &[CapturedMonitor]) -> Self {
        // Keep even a large cross-monitor selection within 128 × 128 tiles.
        let tile =
            ((rect.width().max(rect.height()) + MAX_MOSAIC_SIDE - 1) / MAX_MOSAIC_SIDE).max(12);
        let columns = ((rect.width() + tile - 1) / tile) as usize;
        let rows = ((rect.height() + tile - 1) / tile) as usize;
        let mut colors = Vec::with_capacity(columns * rows);
        for y in (rect.top..rect.bottom).step_by(tile as usize) {
            for x in (rect.left..rect.right).step_by(tile as usize) {
                let cell = Rect::new(x, y, x + tile, y + tile)
                    .clamp(rect)
                    .offset(selection.left, selection.top);
                colors.push(sample_tile(cell, frames));
            }
        }
        Self {
            rect,
            tile,
            columns,
            colors,
        }
    }
}

fn sample_tile(cell: Rect, frames: &[CapturedMonitor]) -> [u8; 4] {
    // Sample inside each cell. Work is bounded even for long, wide regions.
    let mut total = [0u32; 3];
    let count_x = cell.width().min(4);
    let count_y = cell.height().min(4);
    for y in 0..count_y {
        let y = cell.top + (2 * y + 1) * cell.height() / (2 * count_y);
        for x in 0..count_x {
            let x = cell.left + (2 * x + 1) * cell.width() / (2 * count_x);
            if let Some(monitor) = frames
                .iter()
                .find(|frame| frame.info.rect.contains(Point::new(x, y)))
            {
                let row = monitor.frame.row((y - monitor.info.rect.top) as u32);
                let offset = (x - monitor.info.rect.left) as usize * 4;
                for channel in 0..3 {
                    total[channel] += u32::from(row[offset + channel]);
                }
            }
        }
    }
    let count = (count_x * count_y) as u32;
    [
        (total[0] / count) as u8,
        (total[1] / count) as u8,
        (total[2] / count) as u8,
        255,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{MonitorInfo, frame::Frame};
    use windows::Win32::Graphics::Direct2D::{
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1CreateFactory,
    };
    use windows::Win32::Graphics::Gdi::HMONITOR;

    #[test]
    fn a_new_gesture_discards_the_redo_branch() {
        let mut editor = Editor::default();
        let selection = Rect::new(0, 0, 50, 50);
        for x in [1, 3] {
            editor.begin(Tool::Pen, Point::new(x, 6));
            assert!(editor.update(Point::new(x, 14)).is_some());
            editor.finish(Point::new(x, 20), selection, &[]);
        }
        assert_eq!(editor.document.applied, 2);
        assert!(editor.undo().is_some());
        assert!(editor.redo().is_some());
        assert!(editor.undo().is_some());
        editor.begin(Tool::Arrow, Point::new(0, 0));
        editor.finish(Point::new(25, 25), selection, &[]);
        assert_eq!(editor.document.applied, 2);
        assert!(editor.redo().is_none());
    }

    #[test]
    fn export_keeps_each_text_objects_size_and_color() -> anyhow::Result<()> {
        let mut editor = Editor::default();
        editor.add_text(
            Rect::new(4, 4, 76, 80),
            "M".encode_utf16().collect(),
            TextStyle {
                size_px: 18,
                rgb: [20, 230, 40],
            },
        );
        editor.add_text(
            Rect::new(80, 4, 156, 80),
            "M".encode_utf16().collect(),
            TextStyle {
                size_px: 48,
                rgb: [30, 80, 240],
            },
        );
        assert!(editor.undo().is_some());
        assert!(editor.redo().is_some());
        // SAFETY: the factory is created and released on this test thread.
        let factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let base = Frame::new(160, 84, [8, 10, 12, 255].repeat(160 * 84)).unwrap();
        let frame = flatten(&factory, base, &editor.take_document())?;
        let ink_rows = |left: usize, right: usize, channel: usize| {
            (0..84)
                .filter(|&y| {
                    frame.row(y)[left * 4..right * 4]
                        .chunks_exact(4)
                        .any(|pixel| pixel[channel] > 150)
                })
                .count()
        };
        let small = ink_rows(4, 76, 1); // Green in BGRA.
        let large = ink_rows(80, 156, 0); // Blue in BGRA.
        assert!(small > 5 && large > small * 2);
        assert_eq!(ink_rows(4, 76, 0), 0);
        assert_eq!(ink_rows(80, 156, 1), 0);
        assert_eq!(frame.row(75)[8 * 4..8 * 4 + 3], [8, 10, 12]);
        Ok(())
    }

    #[test]
    fn exported_text_breaks_only_at_explicit_newlines() -> anyhow::Result<()> {
        // SAFETY: the factory is created and released on this test thread.
        let factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let shows_second_line = |text: &str| -> anyhow::Result<bool> {
            let mut editor = Editor::default();
            editor.add_text(
                Rect::new(0, 0, 37, 84),
                text.encode_utf16().collect(),
                TextStyle::default(),
            );
            let base = Frame::new(64, 84, [0, 0, 0, 255].repeat(64 * 84)).unwrap();
            let frame = flatten(&factory, base, &editor.take_document())?;
            Ok((30..84).any(|y| frame.row(y).chunks_exact(4).any(|pixel| pixel[2] > 150)))
        };
        assert!(!shows_second_line("MMM MMM MMM MMM")?);
        assert!(shows_second_line("M\r\nM")?);
        Ok(())
    }

    #[test]
    fn all_four_stroke_tools_export_their_chosen_color_and_width() -> anyhow::Result<()> {
        // SAFETY: the Direct2D factory is created and released on this thread.
        let factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let bounds = Rect::new(0, 0, 100, 192);
        let colors = [[20, 50, 230], [30, 220, 50], [230, 40, 20]];
        for tool in [Tool::Pen, Tool::Arrow, Tool::Rectangle, Tool::Ellipse] {
            let mut editor = Editor::default();
            for (index, (width, rgb)) in StrokeWidth::OPTIONS.into_iter().zip(colors).enumerate() {
                let top = (index as i32 * 64) + 12;
                let bottom = if matches!(tool, Tool::Pen | Tool::Arrow) {
                    top
                } else {
                    top + 32
                };
                editor.stroke_style = StrokeStyle { width, rgb };
                editor.begin(tool, Point::new(12, top));
                // A gesture snapshots its style even if settings change before mouse-up.
                editor.stroke_style = StrokeStyle::default();
                editor.finish(Point::new(76, bottom), bounds, &[]);
            }
            assert!(editor.undo().is_some());
            assert!(editor.redo().is_some());
            let base = Frame::new(100, 192, [0, 0, 0, 255].repeat(100 * 192)).unwrap();
            let image = flatten(&factory, base, &editor.take_document())?;
            let mut thickness = [0; 3];
            for (index, &rgb) in colors.iter().enumerate() {
                let top = index as u32 * 64 + 12;
                let expected = [rgb[2], rgb[1], rgb[0]];
                thickness[index] = (top - 6..top + 7)
                    .filter(|&y| image.row(y)[44 * 4..44 * 4 + 3] == expected)
                    .count();
            }
            assert!(
                thickness[0] >= 1 && thickness[0] < thickness[1] && thickness[1] < thickness[2],
                "{tool:?} has incorrect line widths or colors: {thickness:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn flattened_frame_contains_all_six_tools_and_preserves_unmarked_pixels() -> anyhow::Result<()>
    {
        let width = 128;
        let height = 96;
        let bounds = Rect::new(-128, -96, 0, 0);
        let pixels: Vec<_> = (0..height as u8)
            .flat_map(|y| (0..width as u8).flat_map(move |x| [x, y, 24, 255]))
            .collect();
        let frames = [CapturedMonitor {
            info: MonitorInfo {
                handle: HMONITOR::default(),
                rect: bounds,
                work: bounds,
                dpi: 144,
                primary: true,
            },
            frame: Frame::new(width, height, pixels.clone()).unwrap(),
        }];
        let mut editor = Editor::default();
        for (tool, start, end) in [
            (Tool::Pen, (5, 8), (20, 8)),
            (Tool::Arrow, (30, 8), (50, 8)),
            (Tool::Rectangle, (4, 24), (20, 40)),
            (Tool::Ellipse, (28, 24), (48, 42)),
            (Tool::Mosaic, (65, 8), (96, 25)),
        ] {
            editor.begin(tool, Point::new(start.0, start.1));
            editor.finish(Point::new(end.0, end.1), bounds, &frames);
        }
        editor.add_text(
            Rect::new(4, 48, 110, 90),
            "RPin 中文".encode_utf16().collect(),
            TextStyle::default(),
        );
        // SAFETY: the single-threaded factory is kept on this test thread.
        let factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let edited = flatten(
            &factory,
            Frame::new(width, height, pixels).unwrap(),
            &editor.take_document(),
        )?;
        let pixel = |x: usize, y| &edited.row(y)[x * 4..x * 4 + 3];
        assert_eq!(pixel(120, 4), [120, 4, 24]);
        for (x, y) in [(12, 8), (39, 8), (10, 24), (38, 24)] {
            assert!(pixel(x, y)[2] > 190, "missing stroke at ({x}, {y})");
        }
        assert_eq!(pixel(65, 8), pixel(70, 9));
        assert_ne!(pixel(65, 8), [65, 8, 24]);
        assert!((48..90).any(|y| (4..110).any(|x| pixel(x, y)[2] > 190)));
        Ok(())
    }
}

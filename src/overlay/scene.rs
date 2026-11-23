use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};
use crate::config::UiConfig;

use super::input::{Handle, Hit, Selection};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Label {
    pub rect: Rect,
    pub dpi: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Magnifier {
    pub monitor: usize,
    pub image: Rect,
    pub sample: Rect,
    pub pixel: Point,
    pub label: Option<Label>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolbarButton {
    Copy,
    Save,
    Pin,
}

impl ToolbarButton {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Copy => "复制",
            Self::Save => "保存",
            Self::Pin => "贴图",
        }
    }
}

// Insert future annotation controls before the output buttons.
const TOOLBAR_BUTTONS: [ToolbarButton; 3] =
    [ToolbarButton::Pin, ToolbarButton::Save, ToolbarButton::Copy];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Toolbar {
    pub rect: Rect,
    pub dpi: u32,
    pub hovered: Option<ToolbarButton>,
    pub pressed: Option<ToolbarButton>,
    padding: i32,
}

impl Toolbar {
    fn new(
        frames: &[CapturedMonitor],
        selection: Rect,
        cursor: Point,
        pressed: Option<ToolbarButton>,
    ) -> Option<Self> {
        let corner = Point::new(selection.right - 1, selection.bottom - 1);
        // In a gap between monitors, prefer the closest screen intersecting
        // the selection, then the closest screen overall.
        let monitor = frames.iter().min_by_key(|monitor| {
            let bounds = monitor.info.rect;
            let x = corner.x.clamp(bounds.left, bounds.right - 1);
            let y = corner.y.clamp(bounds.top, bounds.bottom - 1);
            (
                bounds.intersect(selection).is_none(),
                corner.x.abs_diff(x) + corner.y.abs_diff(y),
            )
        })?;
        let bounds = monitor.info.rect;
        let dpi = monitor.info.dpi;
        let count = TOOLBAR_BUTTONS.len() as i32;
        let padding = scaled(4, dpi).min((bounds.width() - count).max(0) / 2);
        let width = (scaled(54, dpi) * count + padding * 2).min(bounds.width());
        let height = scaled(34, dpi).min(bounds.height());
        if width < count || height < 1 {
            return None;
        }
        let gap = scaled(8, dpi);
        let below = selection.bottom + gap;
        let inside = selection.bottom.min(bounds.bottom) - gap - height;
        let above = selection.top - gap - height;
        let y = if below >= bounds.top && below + height <= bounds.bottom {
            below
        } else if inside >= selection.top.max(bounds.top) {
            inside
        } else {
            above
        };
        let rect = fit(
            Point::new(selection.right - width, y),
            width,
            height,
            bounds,
        );
        let mut toolbar = Self {
            rect,
            dpi,
            hovered: None,
            pressed,
            padding,
        };
        toolbar.hovered = toolbar.button_at(cursor);
        Some(toolbar)
    }

    pub(super) fn buttons(self) -> impl Iterator<Item = (ToolbarButton, Rect)> {
        let vertical = self.padding.min((self.rect.height() - 1) / 2);
        let left = self.rect.left + self.padding;
        let inner_width = self.rect.width() - 2 * self.padding;
        let count = TOOLBAR_BUTTONS.len() as i32;
        TOOLBAR_BUTTONS
            .into_iter()
            .enumerate()
            .map(move |(index, button)| {
                let index = index as i32;
                let area = Rect::new(
                    left + inner_width * index / count,
                    self.rect.top + vertical,
                    left + inner_width * (index + 1) / count,
                    self.rect.bottom - vertical,
                );
                (button, area)
            })
    }

    pub(super) fn button_at(self, cursor: Point) -> Option<ToolbarButton> {
        self.buttons()
            .find(|(_, area)| area.contains(cursor))
            .map(|(button, _)| button)
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Scene {
    pub selection: Option<Rect>,
    pub label: Option<Label>,
    pub magnifier: Option<Magnifier>,
    pub toolbar: Option<Toolbar>,
}

impl Scene {
    pub(super) fn new(
        frames: &[CapturedMonitor],
        ui: &UiConfig,
        selection: &Selection,
        cursor: Point,
        pressed: Option<ToolbarButton>,
    ) -> Self {
        let toolbar = if selection.dragging() {
            None
        } else {
            selection
                .saved_rect()
                .and_then(|rect| Toolbar::new(frames, rect, cursor, pressed))
        };
        let selection = selection.rect;
        let label = selection.filter(|_| ui.show_size).and_then(|rect| {
            let monitor = monitor_for_rect(frames, rect)?;
            let bounds = monitor.info.rect;
            let dpi = monitor.info.dpi;
            let top = rect.top - scaled(26, dpi);
            let y = if top >= bounds.top {
                top
            } else {
                rect.top + scaled(8, dpi)
            };
            Some(Label {
                rect: fit(
                    Point::new(rect.left, y),
                    scaled(140, dpi),
                    scaled(22, dpi),
                    bounds,
                ),
                dpi,
            })
        });
        let magnifier = if ui.magnifier && pressed.is_none() {
            magnifier(frames, ui, cursor).filter(|zoom| {
                !toolbar.is_some_and(|bar| {
                    bar.rect.contains(cursor) || bar.rect.intersect(zoom.area()).is_some()
                })
            })
        } else {
            None
        };
        Self {
            selection,
            label,
            magnifier,
            toolbar,
        }
    }

    pub(super) fn damage_from(&self, previous: &Self, frames: &[CapturedMonitor]) -> Option<Rect> {
        let selection_changed =
            self.selection != previous.selection || self.label != previous.label;
        let magnifier_changed = self.magnifier != previous.magnifier;
        let toolbar_changed = self.toolbar != previous.toolbar;
        [self, previous]
            .into_iter()
            .flat_map(|scene| {
                let selection = scene.selection.filter(|_| selection_changed).map(|rect| {
                    handles(frames, rect)
                        .into_iter()
                        .fold(inflate(rect, 3), |area, (_, _, handle)| {
                            area.union(inflate(handle, 2))
                        })
                });
                let label = scene
                    .label
                    .filter(|_| selection_changed)
                    .map(|label| label.rect);
                let magnifier = scene
                    .magnifier
                    .filter(|_| magnifier_changed)
                    .map(|zoom| inflate(zoom.area(), 3));
                let toolbar = scene
                    .toolbar
                    .filter(|_| toolbar_changed)
                    .map(|bar| bar.rect);
                [selection, label, magnifier, toolbar].into_iter().flatten()
            })
            .reduce(|a, b| a.union(b))
    }
}

impl Magnifier {
    fn area(self) -> Rect {
        self.label
            .map_or(self.image, |label| self.image.union(label.rect))
    }
}

pub(super) fn handles(frames: &[CapturedMonitor], rect: Rect) -> [(Handle, Point, Rect); 8] {
    Handle::positions(rect).map(|(handle, center)| {
        let inside = Point::new(center.x.min(rect.right - 1), center.y.min(rect.bottom - 1));
        let dpi = frames
            .iter()
            .find(|m| m.info.rect.contains(inside))
            .map_or(96, |m| m.info.dpi);
        let size = scaled(4, dpi).max(3);
        (
            handle,
            center,
            Rect::new(
                center.x - size,
                center.y - size,
                center.x + size,
                center.y + size,
            ),
        )
    })
}

pub(super) fn hit_test(frames: &[CapturedMonitor], rect: Option<Rect>, cursor: Point) -> Hit {
    let Some(rect) = rect else {
        return Hit::Outside;
    };
    handles(frames, rect)
        .into_iter()
        .filter(|&(_, _, area)| inflate(area, (area.width() / 4).max(1)).contains(cursor))
        .min_by_key(|&(_, center, _)| center.x.abs_diff(cursor.x) + center.y.abs_diff(cursor.y))
        .map_or_else(
            || {
                if rect.contains(cursor) {
                    Hit::Inside
                } else {
                    Hit::Outside
                }
            },
            |(handle, _, _)| Hit::Handle(handle),
        )
}

fn monitor_for_rect(frames: &[CapturedMonitor], rect: Rect) -> Option<&CapturedMonitor> {
    let origin = Point::new(rect.left, rect.top);
    frames
        .iter()
        .find(|m| m.info.rect.contains(origin))
        .or_else(|| {
            frames
                .iter()
                .find(|m| m.info.rect.intersect(rect).is_some())
        })
}

fn magnifier(frames: &[CapturedMonitor], ui: &UiConfig, cursor: Point) -> Option<Magnifier> {
    let (monitor, frame) = frames
        .iter()
        .enumerate()
        .find(|(_, m)| m.info.rect.contains(cursor))?;
    let bounds = frame.info.rect;
    let dpi = frame.info.dpi;
    let label_height = if ui.show_rgb { scaled(20, dpi) } else { 0 };
    let size = scaled(120, dpi)
        .min(bounds.width())
        .min(bounds.height() - label_height);
    if size < 2 {
        return None;
    }
    let width = if ui.show_rgb {
        size.max(scaled(164, dpi)).min(bounds.width())
    } else {
        size
    };
    let gap = scaled(20, dpi);
    let x = if cursor.x + gap + width <= bounds.right {
        cursor.x + gap
    } else {
        cursor.x - gap - width
    };
    let y = if cursor.y + gap + size + label_height <= bounds.bottom {
        cursor.y + gap
    } else {
        cursor.y - gap - size - label_height
    };
    let panel = fit(Point::new(x, y), width, size + label_height, bounds);
    let image = Rect::new(panel.left, panel.top, panel.left + size, panel.top + size);
    let label = ui.show_rgb.then_some(Label {
        rect: Rect::new(panel.left, image.bottom, panel.right, panel.bottom),
        dpi,
    });
    // Zoom operates on physical source pixels. Keep an odd footprint so the
    // cursor lies at the center of a pixel when the sample fits around it.
    let samples = ((size / ui.magnifier_zoom.clamp(2, 12) as i32).max(1) / 2) * 2 + 1;
    let source = fit(
        Point::new(cursor.x - samples / 2, cursor.y - samples / 2),
        samples,
        samples,
        bounds,
    );
    Some(Magnifier {
        monitor,
        image,
        sample: source.offset(-bounds.left, -bounds.top),
        pixel: Point::new(cursor.x - bounds.left, cursor.y - bounds.top),
        label,
    })
}

fn fit(point: Point, width: i32, height: i32, bounds: Rect) -> Rect {
    let width = width.min(bounds.width());
    let height = height.min(bounds.height());
    let x = point.x.clamp(bounds.left, bounds.right - width);
    let y = point.y.clamp(bounds.top, bounds.bottom - height);
    Rect::new(x, y, x + width, y + height)
}

fn inflate(rect: Rect, margin: i32) -> Rect {
    Rect::new(
        rect.left - margin,
        rect.top - margin,
        rect.right + margin,
        rect.bottom + margin,
    )
}

pub(super) fn scaled(size: i32, dpi: u32) -> i32 {
    (size * dpi as i32 + 48) / 96
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Gdi::HMONITOR;

    use crate::capture::{MonitorInfo, frame::Frame};

    use super::*;

    fn monitor(rect: Rect, dpi: u32) -> CapturedMonitor {
        CapturedMonitor {
            info: MonitorInfo {
                handle: HMONITOR::default(),
                rect,
                work: rect,
                dpi,
                primary: false,
            },
            frame: Frame::empty(),
        }
    }

    #[test]
    fn toolbar_uses_lower_right_screen_dpi_and_stays_inside_at_bottom_edge() {
        let right = Rect::new(0, 0, 1920, 1080);
        let frames = [
            monitor(Rect::new(-1600, 0, 0, 900), 96),
            monitor(right, 144),
        ];
        let selection = Rect::new(-200, 500, 140, 1080);
        let bar = Toolbar::new(&frames, selection, Point::default(), None).expect("toolbar");
        assert_eq!(bar.dpi, 144);
        assert_eq!(bar.rect.intersect(right), Some(bar.rect));
        assert!(bar.rect.bottom < selection.bottom);
        for (button, area) in bar.buttons() {
            assert_eq!(bar.button_at(Point::new(area.left, area.top)), Some(button));
        }
    }

    #[test]
    fn toolbar_drag_and_hover_damage_repaints_old_and_new_areas() {
        let bounds = Rect::new(-1000, -700, 0, 0);
        let frames = [monitor(bounds, 120)];
        let ui = UiConfig {
            magnifier: false,
            ..UiConfig::default()
        };
        let mut selection = Selection::default();
        selection.rect = Some(Rect::new(-600, -400, -300, -200));
        let first = Scene::new(&frames, &ui, &selection, Point::default(), None);
        let bar = first.toolbar.expect("toolbar");
        let (_, copy) = bar
            .buttons()
            .find(|(button, _)| *button == ToolbarButton::Copy)
            .expect("copy button");
        let cursor = Point::new(copy.left, copy.top);
        let hovered = Scene::new(&frames, &ui, &selection, cursor, Some(ToolbarButton::Copy));
        assert_eq!(hovered.damage_from(&first, &frames), Some(bar.rect));

        let inside = Point::new(-400, -300);
        selection.begin(inside, bounds, Hit::Inside);
        let dragging = Scene::new(&frames, &ui, &selection, inside, None);
        assert!(dragging.toolbar.is_none());
        assert_eq!(dragging.damage_from(&hovered, &frames), Some(bar.rect));
    }
}

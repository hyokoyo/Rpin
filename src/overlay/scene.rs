use crate::annotation::{StrokeStyle, TextStyle, Tool};
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
    TextSize,
    StrokeWidth,
    Color,
    Undo,
    Redo,
    Tool(Tool),
    Copy,
    Save,
    Pin,
}

impl ToolbarButton {
    pub(super) fn label(self, compact: bool) -> &'static str {
        match self {
            Self::TextSize => "字号",
            Self::StrokeWidth => "线宽",
            Self::Color => "颜色",
            Self::Undo => {
                if compact {
                    "↶"
                } else {
                    "撤销"
                }
            }
            Self::Redo => {
                if compact {
                    "↷"
                } else {
                    "重做"
                }
            }
            Self::Tool(tool) => {
                if compact {
                    tool.compact_label()
                } else {
                    tool.label()
                }
            }
            Self::Copy => "复制",
            Self::Save => "保存",
            Self::Pin => "贴图",
        }
    }

    pub(super) fn is_style_option(self) -> bool {
        matches!(self, Self::TextSize | Self::StrokeWidth | Self::Color)
    }
}

const TOOLBAR_BUTTONS: [ToolbarButton; 11] = [
    ToolbarButton::Undo,
    ToolbarButton::Redo,
    ToolbarButton::Tool(Tool::Pen),
    ToolbarButton::Tool(Tool::Arrow),
    ToolbarButton::Tool(Tool::Rectangle),
    ToolbarButton::Tool(Tool::Ellipse),
    ToolbarButton::Tool(Tool::Text),
    ToolbarButton::Tool(Tool::Mosaic),
    ToolbarButton::Pin,
    ToolbarButton::Save,
    ToolbarButton::Copy,
];
const TEXT_OPTIONS: [ToolbarButton; 2] = [ToolbarButton::TextSize, ToolbarButton::Color];
const STROKE_OPTIONS: [ToolbarButton; 2] = [ToolbarButton::StrokeWidth, ToolbarButton::Color];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ToolbarState {
    pub pressed: Option<ToolbarButton>,
    pub active_tool: Option<Tool>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub editing: bool,
    pub text_style: TextStyle,
    pub stroke_style: StrokeStyle,
}

impl ToolbarState {
    fn options(self) -> &'static [ToolbarButton] {
        match self.active_tool {
            Some(Tool::Text) => &TEXT_OPTIONS,
            Some(tool) if tool.uses_stroke() => &STROKE_OPTIONS,
            _ => &[],
        }
    }

    pub(super) fn active_color(self) -> [u8; 3] {
        if self.active_tool == Some(Tool::Text) {
            self.text_style.rgb
        } else {
            self.stroke_style.rgb
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Toolbar {
    pub rect: Rect,
    pub dpi: u32,
    pub hovered: Option<ToolbarButton>,
    pub state: ToolbarState,
    pub compact: bool,
    padding: i32,
}

impl Toolbar {
    fn new(
        frames: &[CapturedMonitor],
        selection: Rect,
        cursor: Point,
        state: ToolbarState,
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
        let count = (state.options().len() + TOOLBAR_BUTTONS.len()) as i32;
        let padding = scaled(4, dpi).min((bounds.width() - count).max(0) / 2);
        let full_width = scaled(54, dpi) * count + padding * 2;
        let compact = full_width > bounds.width();
        let width = if compact {
            scaled(34, dpi) * count + padding * 2
        } else {
            full_width
        };
        let width = width.min(bounds.width());
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
        } else if above >= bounds.top {
            above
        } else {
            inside.max(bounds.top)
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
            state,
            compact,
            padding,
        };
        toolbar.hovered = toolbar.button_at(cursor);
        Some(toolbar)
    }

    pub(super) fn buttons(self) -> impl Iterator<Item = (ToolbarButton, Rect)> {
        let vertical = self.padding.min((self.rect.height() - 1) / 2);
        let left = self.rect.left + self.padding;
        let inner_width = self.rect.width() - 2 * self.padding;
        let options = self.state.options();
        let count = (options.len() + TOOLBAR_BUTTONS.len()) as i32;
        options
            .iter()
            .chain(TOOLBAR_BUTTONS.iter())
            .copied()
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
            .filter(|&button| self.enabled(button))
    }

    pub(super) fn enabled(self, button: ToolbarButton) -> bool {
        match button {
            ToolbarButton::Undo => self.state.can_undo,
            ToolbarButton::Redo => self.state.can_redo,
            _ => true,
        }
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Scene {
    pub selection: Option<Rect>,
    pub label: Option<Label>,
    pub magnifier: Option<Magnifier>,
    pub toolbar: Option<Toolbar>,
    pub editing: bool,
    pub text_grip: Option<Rect>,
}

impl Scene {
    pub(super) fn new(
        frames: &[CapturedMonitor],
        ui: &UiConfig,
        selection: &Selection,
        cursor: Point,
        controls: ToolbarState,
    ) -> Self {
        let toolbar = if selection.dragging() {
            None
        } else {
            selection
                .saved_rect()
                .and_then(|rect| Toolbar::new(frames, rect, cursor, controls))
        };
        let selection = selection.rect;
        let label = selection
            .filter(|_| ui.show_size && !controls.editing)
            .and_then(|rect| {
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
        let magnifier = if ui.magnifier && controls.pressed.is_none() && !controls.editing {
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
            editing: controls.editing,
            text_grip: None,
        }
    }

    pub(super) fn damage_from(&self, previous: &Self, frames: &[CapturedMonitor]) -> Option<Rect> {
        let selection_changed = self.selection != previous.selection
            || self.label != previous.label
            || self.editing != previous.editing;
        let magnifier_changed = self.magnifier != previous.magnifier;
        let toolbar_changed = self.toolbar != previous.toolbar;
        let grip_changed = self.text_grip != previous.text_grip;
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
                let grip = scene.text_grip.filter(|_| grip_changed);
                [selection, label, magnifier, toolbar, grip]
                    .into_iter()
                    .flatten()
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
        let bar = Toolbar::new(
            &frames,
            selection,
            Point::default(),
            ToolbarState::default(),
        )
        .expect("toolbar");
        assert_eq!(bar.dpi, 144);
        assert_eq!(bar.rect.intersect(right), Some(bar.rect));
        assert!(bar.rect.bottom < selection.bottom);
        for (button, area) in bar.buttons() {
            assert_eq!(
                bar.button_at(Point::new(area.left, area.top)),
                bar.enabled(button).then_some(button)
            );
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
        let first = Scene::new(
            &frames,
            &ui,
            &selection,
            Point::default(),
            ToolbarState::default(),
        );
        let bar = first.toolbar.expect("toolbar");
        let (_, copy) = bar
            .buttons()
            .find(|(button, _)| *button == ToolbarButton::Copy)
            .expect("copy button");
        let cursor = Point::new(copy.left, copy.top);
        let hovered = Scene::new(
            &frames,
            &ui,
            &selection,
            cursor,
            ToolbarState {
                pressed: Some(ToolbarButton::Copy),
                ..Default::default()
            },
        );
        assert_eq!(hovered.damage_from(&first, &frames), Some(bar.rect));

        let inside = Point::new(-400, -300);
        selection.begin(inside, bounds, Hit::Inside);
        let dragging = Scene::new(&frames, &ui, &selection, inside, ToolbarState::default());
        assert!(dragging.toolbar.is_none());
        assert_eq!(dragging.damage_from(&hovered, &frames), Some(bar.rect));
    }

    #[test]
    fn compact_editor_tools_stay_left_of_pin_save_and_copy() {
        let bounds = Rect::new(-500, -480, 0, 0);
        let frames = [monitor(bounds, 192)];
        let rect = Rect::new(-220, -200, -20, -20);
        let bar = Toolbar::new(&frames, rect, Point::default(), ToolbarState::default()).unwrap();
        assert!(bar.compact);
        assert_eq!(bar.rect.intersect(bounds), Some(bar.rect));
        let controls: Vec<_> = bar.buttons().map(|(button, _)| button).collect();
        assert_eq!(
            &controls[2..],
            &[
                ToolbarButton::Tool(Tool::Pen),
                ToolbarButton::Tool(Tool::Arrow),
                ToolbarButton::Tool(Tool::Rectangle),
                ToolbarButton::Tool(Tool::Ellipse),
                ToolbarButton::Tool(Tool::Text),
                ToolbarButton::Tool(Tool::Mosaic),
                ToolbarButton::Pin,
                ToolbarButton::Save,
                ToolbarButton::Copy,
            ]
        );
    }

    #[test]
    fn tool_style_controls_match_the_selected_tool() {
        let frames = [monitor(Rect::new(0, 0, 550, 480), 144)];
        let rect = Rect::new(10, 20, 500, 100);
        let plain = Toolbar::new(&frames, rect, Point::default(), ToolbarState::default()).unwrap();
        let text = Toolbar::new(
            &frames,
            rect,
            Point::default(),
            ToolbarState {
                active_tool: Some(Tool::Text),
                ..Default::default()
            },
        )
        .unwrap();
        let buttons = |toolbar: Toolbar| {
            toolbar
                .buttons()
                .map(|(button, _)| button)
                .collect::<Vec<_>>()
        };
        let text_buttons = buttons(text);
        assert_eq!(
            &text_buttons[..2],
            &[ToolbarButton::TextSize, ToolbarButton::Color]
        );
        assert_eq!(&text_buttons[2..], buttons(plain));
        for tool in [
            Tool::Pen,
            Tool::Arrow,
            Tool::Rectangle,
            Tool::Ellipse,
            Tool::Mosaic,
        ] {
            let state = ToolbarState {
                active_tool: Some(tool),
                ..Default::default()
            };
            let bar = Toolbar::new(&frames, rect, Point::default(), state).unwrap();
            let tool_buttons = buttons(bar);
            if tool == Tool::Mosaic {
                assert_eq!(tool_buttons, buttons(plain));
            } else {
                assert_eq!(
                    &tool_buttons[..2],
                    &[ToolbarButton::StrokeWidth, ToolbarButton::Color]
                );
                assert_eq!(&tool_buttons[2..], buttons(plain));
            }
        }
    }
}

use crate::annotation::{Document, Editor, StrokeStyle, TextStyle, Tool, local_point};
use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};
use crate::config::UiConfig;
use crate::output::{OutputAction, SelectionOutcome};
use crate::platform::capture_windows::WindowTargets;

use super::input::{Hit, Selection};
use super::scene::{Scene, ToolbarButton, ToolbarState, hit_test};

#[derive(Default)]
enum PointerMode {
    #[default]
    Idle,
    WindowPress {
        origin: Point,
        rect: Rect,
        threshold: Point,
    },
    Selection,
    Annotation(Rect),
    Toolbar(ToolbarButton),
    TextResize {
        width_origin: i32,
    },
}

pub(super) enum Change {
    None,
    Refresh,
    Annotation(Option<Rect>),
    OpenText { selection: Rect, local: Point },
    ResizeText { width: i32, focus: bool },
    ClickToolbar(ToolbarButton),
    Finish(SelectionOutcome),
}

pub(super) struct Interaction {
    // Selection and Editor own their geometry and gesture data. Only pointer
    // transitions below start and end those operations.
    selection: Selection,
    window_targets: WindowTargets,
    editor: Editor,
    pointer: PointerMode,
    cursor: Point,
    default_action: OutputAction,
}

impl Interaction {
    pub(super) fn new(
        cursor: Point,
        default_action: OutputAction,
        window_targets: WindowTargets,
    ) -> Self {
        Self {
            selection: Selection::default(),
            window_targets,
            editor: Editor::default(),
            pointer: PointerMode::Idle,
            cursor,
            default_action,
        }
    }

    pub(super) fn idle(&self) -> bool {
        matches!(self.pointer, PointerMode::Idle)
    }

    fn annotating(&self) -> bool {
        matches!(self.pointer, PointerMode::Annotation(_))
    }

    pub(super) fn dragging_canvas(&self) -> bool {
        matches!(
            self.pointer,
            PointerMode::Selection | PointerMode::Annotation(_)
        )
    }

    pub(super) fn resizing_text(&self) -> bool {
        matches!(self.pointer, PointerMode::TextResize { .. })
    }

    pub(super) fn cursor(&self) -> Point {
        self.cursor
    }

    pub(super) fn selection(&self) -> Option<Rect> {
        self.selection.saved_rect()
    }

    pub(super) fn editor(&self) -> &Editor {
        &self.editor
    }

    pub(super) fn take_document(&mut self) -> Document {
        self.editor.take_document()
    }

    pub(super) fn scene(&self, frames: &[CapturedMonitor], ui: &UiConfig) -> Scene {
        let window_preview = match self.pointer {
            PointerMode::WindowPress { rect, .. } => Some(rect),
            PointerMode::Idle if self.selection.rect.is_none() => {
                self.window_targets.at(self.cursor)
            }
            _ => None,
        };
        Scene::new(
            frames,
            ui,
            &self.selection,
            window_preview,
            self.cursor,
            ToolbarState {
                pressed: match self.pointer {
                    PointerMode::Toolbar(button) => Some(button),
                    _ => None,
                },
                active_tool: self.editor.tool,
                can_undo: self.editor.can_undo(),
                can_redo: self.editor.can_redo(),
                editing: self.editor.locks_selection(),
                text_style: self.editor.text_style,
                stroke_style: self.editor.stroke_style,
            },
        )
    }

    pub(super) fn press_toolbar(
        &mut self,
        point: Point,
        button: Option<ToolbarButton>,
        capture: impl FnOnce() -> bool,
    ) -> Change {
        debug_assert!(self.idle());
        self.cursor = point;
        if let Some(button) = button.filter(|_| capture()) {
            self.pointer = PointerMode::Toolbar(button);
        }
        Change::Refresh
    }

    pub(super) fn begin_text_resize(&mut self, grab_x: i32, grip_left: i32, text_left: i32) {
        debug_assert!(self.idle());
        let width_origin = grab_x - (grip_left - text_left);
        self.pointer = PointerMode::TextResize { width_origin };
    }

    pub(super) fn end_text_resize(&mut self) -> bool {
        if self.resizing_text() {
            self.pointer = PointerMode::Idle;
            return true;
        }
        false
    }

    pub(super) fn press_canvas(
        &mut self,
        point: Point,
        double_click: bool,
        bounds: Rect,
        frames: &[CapturedMonitor],
        capture: impl FnOnce() -> bool,
        drag_threshold: impl FnOnce(u32) -> Point,
    ) -> Change {
        debug_assert!(self.idle());
        if double_click && self.editor.tool.is_none() {
            return if self.selection().is_some_and(|rect| rect.contains(point)) {
                self.confirm(self.default_action)
            } else {
                Change::None
            };
        }

        self.cursor = point;
        if self.editor.locks_selection() {
            let (Some(tool), Some(selection)) = (self.editor.tool, self.selection()) else {
                return Change::None;
            };
            if !selection.contains(point) {
                return Change::None;
            }
            let local = local_point(point, selection);
            if tool == Tool::Text {
                return Change::OpenText { selection, local };
            }
            if !capture() {
                return Change::None;
            }
            let damage = self.editor.begin(tool, local);
            self.pointer = PointerMode::Annotation(selection);
            Change::Annotation(Some(damage))
        } else {
            if !capture() {
                return Change::None;
            }
            if self.selection.rect.is_none() {
                if let Some(rect) = self.window_targets.at(point) {
                    let dpi = frames
                        .iter()
                        .find(|monitor| monitor.info.rect.contains(point))
                        .map_or(96, |monitor| monitor.info.dpi);
                    self.pointer = PointerMode::WindowPress {
                        origin: point,
                        rect,
                        threshold: drag_threshold(dpi),
                    };
                    return Change::Refresh;
                }
            }
            let hit = hit_test(frames, self.selection.rect, point);
            self.selection.begin(point, bounds, hit);
            self.pointer = PointerMode::Selection;
            Change::Refresh
        }
    }

    pub(super) fn move_pointer(&mut self, point: Point, bounds: Rect) -> Change {
        self.cursor = point;
        match self.pointer {
            PointerMode::WindowPress {
                origin, threshold, ..
            } if beyond_drag_threshold(origin, point, threshold) => {
                self.selection.begin(origin, bounds, Hit::Outside);
                self.selection.update(point, bounds);
                self.pointer = PointerMode::Selection;
                Change::Refresh
            }
            PointerMode::WindowPress { .. } => Change::Refresh,
            PointerMode::Selection => {
                self.selection.update(point, bounds);
                Change::Refresh
            }
            PointerMode::Annotation(selection) => {
                Change::Annotation(self.editor.update(local_point(point, selection)))
            }
            PointerMode::TextResize { width_origin } => Change::ResizeText {
                width: point.x - width_origin,
                focus: false,
            },
            PointerMode::Idle | PointerMode::Toolbar(_) => Change::Refresh,
        }
    }

    /// Returns a change only if the window had an active capture to release.
    pub(super) fn release_pointer(
        &mut self,
        point: Point,
        bounds: Rect,
        frames: &[CapturedMonitor],
        scene: &Scene,
    ) -> Option<Change> {
        let change = match std::mem::take(&mut self.pointer) {
            PointerMode::Idle => return None,
            PointerMode::WindowPress {
                origin,
                rect,
                threshold,
            } => {
                if beyond_drag_threshold(origin, point, threshold) {
                    self.selection.begin(origin, bounds, Hit::Outside);
                    self.finish_drag(point, bounds)
                } else {
                    self.selection.rect = Some(rect);
                    Change::Refresh
                }
            }
            PointerMode::Selection => self.finish_drag(point, bounds),
            PointerMode::Annotation(selection) => Change::Annotation(self.editor.finish(
                local_point(point, selection),
                selection,
                frames,
            )),
            PointerMode::Toolbar(button) => {
                if scene.toolbar.and_then(|bar| bar.button_at(point)) == Some(button) {
                    Change::ClickToolbar(button)
                } else {
                    Change::Refresh
                }
            }
            PointerMode::TextResize { width_origin } => Change::ResizeText {
                width: point.x - width_origin,
                focus: true,
            },
        };
        self.cursor = point;
        Some(change)
    }

    fn finish_drag(&mut self, point: Point, bounds: Rect) -> Change {
        self.selection.update(point, bounds);
        if self.selection.end() {
            Change::Refresh
        } else {
            Change::Finish(SelectionOutcome::Cancelled)
        }
    }

    pub(super) fn lose_capture(&mut self) -> Option<Change> {
        Some(match std::mem::take(&mut self.pointer) {
            PointerMode::Idle => return None,
            PointerMode::WindowPress { .. } => Change::Finish(SelectionOutcome::Cancelled),
            PointerMode::Selection => {
                self.selection.end();
                Change::Finish(SelectionOutcome::Cancelled)
            }
            PointerMode::Annotation(_) => Change::Annotation(self.editor.cancel_gesture()),
            PointerMode::Toolbar(_) | PointerMode::TextResize { .. } => Change::Refresh,
        })
    }

    pub(super) fn escape(&mut self) -> (Change, bool) {
        if self.annotating() {
            // Escape during a stroke clears the preview and keeps the overlay.
            (self.lose_capture().unwrap_or(Change::None), true)
        } else {
            (self.cancel(), false)
        }
    }

    pub(super) fn cancel(&mut self) -> Change {
        let _ = self.lose_capture();
        Change::Finish(SelectionOutcome::Cancelled)
    }

    pub(super) fn confirm_default(&self) -> Change {
        self.confirm(self.default_action)
    }

    pub(super) fn confirm(&self, action: OutputAction) -> Change {
        match (self.idle(), self.selection()) {
            (true, Some(rect)) => Change::Finish(SelectionOutcome::Confirmed { rect, action }),
            _ => Change::None,
        }
    }

    pub(super) fn nudge(&mut self, bounds: Rect, delta: Point, resize: bool) -> Change {
        if self.editor.locks_selection() {
            return Change::None;
        }
        self.selection.nudge(bounds, delta, resize);
        Change::Refresh
    }

    pub(super) fn history(&mut self, redo: bool) -> Change {
        if self.annotating() {
            return Change::None;
        }
        Change::Annotation(if redo {
            self.editor.redo()
        } else {
            self.editor.undo()
        })
    }

    pub(super) fn toggle_tool(&mut self, tool: Tool) {
        self.editor.tool = (self.editor.tool != Some(tool)).then_some(tool);
    }

    pub(super) fn set_styles(&mut self, text: TextStyle, stroke: StrokeStyle) {
        self.editor.text_style = text;
        self.editor.stroke_style = stroke;
    }

    pub(super) fn add_text(&mut self, area: Rect, units: Vec<u16>) -> Option<Rect> {
        self.editor.add_text(area, units, self.editor.text_style)
    }
}

fn beyond_drag_threshold(origin: Point, point: Point, threshold: Point) -> bool {
    point.x.abs_diff(origin.x) > threshold.x as u32
        || point.y.abs_diff(origin.y) > threshold.y as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{MonitorInfo, frame::Frame};

    const BOUNDS: Rect = Rect::new(-640, -100, 640, 600);
    const START: Point = Point::new(-120, 40);
    const END: Point = Point::new(80, 200);
    const WINDOW: Rect = Rect::new(-230, -20, 20, 120);

    fn press(
        interaction: &mut Interaction,
        point: Point,
        double_click: bool,
        capture: impl FnOnce() -> bool,
    ) -> Change {
        interaction.press_canvas(point, double_click, BOUNDS, &[], capture, |_| {
            Point::new(4, 4)
        })
    }

    fn with_selection() -> Interaction {
        let mut interaction = Interaction::new(START, OutputAction::Pin, WindowTargets::default());
        press(&mut interaction, START, false, || true);
        assert!(matches!(
            interaction.release_pointer(END, BOUNDS, &[], &Scene::default()),
            Some(Change::Refresh)
        ));
        interaction
    }

    #[test]
    fn window_hover_is_a_preview_until_click_locks_it_as_the_selection() {
        let behind = Rect::new(-240, -30, 120, 210);
        let targets = WindowTargets::from_rects(vec![WINDOW, behind]);
        let mut interaction = Interaction::new(START, OutputAction::Copy, targets);
        let frames = [test_monitor(144)];
        let ui = UiConfig::default();

        let preview = interaction.scene(&frames, &ui);
        assert_eq!(preview.highlighted_rect(), Some(WINDOW));
        assert!(preview.selection.is_none());
        assert!(preview.toolbar.is_none());
        interaction.move_pointer(END, BOUNDS);
        assert_eq!(
            interaction.scene(&frames, &ui).highlighted_rect(),
            Some(behind)
        );

        assert!(matches!(
            interaction.press_canvas(
                START,
                false,
                BOUNDS,
                &frames,
                || true,
                |dpi| {
                    assert_eq!(dpi, 144);
                    Point::new(4, 4)
                }
            ),
            Change::Refresh
        ));
        let near = Point::new(START.x + 4, START.y - 3);
        interaction.move_pointer(near, BOUNDS);
        assert_eq!(
            interaction.scene(&frames, &ui).highlighted_rect(),
            Some(WINDOW)
        );
        assert!(matches!(
            interaction.release_pointer(near, BOUNDS, &frames, &preview),
            Some(Change::Refresh)
        ));
        interaction.move_pointer(END, BOUNDS);
        let committed = interaction.scene(&frames, &ui);
        assert_eq!(committed.selection, Some(WINDOW));
        assert!(committed.window_preview.is_none());
        assert!(committed.toolbar.is_some());
        assert!(matches!(
            interaction.confirm_default(),
            Change::Finish(SelectionOutcome::Confirmed {
                rect: WINDOW,
                action: OutputAction::Copy,
            })
        ));
    }

    #[test]
    fn window_drag_uses_the_original_press_even_when_move_messages_are_coalesced() {
        for send_move in [true, false] {
            let targets = WindowTargets::from_rects(vec![WINDOW]);
            let mut interaction = Interaction::new(START, OutputAction::Copy, targets);
            press(&mut interaction, START, false, || true);
            let end = Point::new(-178, 15);
            let expected = Rect::new(-178, 15, -119, 41);
            if send_move {
                interaction.move_pointer(end, BOUNDS);
                assert_eq!(interaction.selection(), Some(expected));
                assert!(interaction.dragging_canvas());
            }
            assert!(matches!(
                interaction.release_pointer(end, BOUNDS, &[], &Scene::default()),
                Some(Change::Refresh)
            ));
            assert_eq!(interaction.selection(), Some(expected));
        }
    }

    #[test]
    fn returning_to_the_origin_after_dragging_does_not_select_the_window() {
        let targets = WindowTargets::from_rects(vec![WINDOW]);
        let mut interaction = Interaction::new(START, OutputAction::Copy, targets);
        press(&mut interaction, START, false, || true);
        assert!(matches!(
            interaction.lose_capture(),
            Some(Change::Finish(SelectionOutcome::Cancelled))
        ));
        assert!(interaction.selection().is_none());

        press(&mut interaction, START, false, || true);
        interaction.move_pointer(END, BOUNDS);
        assert!(matches!(
            interaction.release_pointer(START, BOUNDS, &[], &Scene::default()),
            Some(Change::Finish(SelectionOutcome::Cancelled))
        ));
    }

    #[test]
    fn selection_requires_capture_and_a_complete_drag_before_confirming() {
        let mut interaction = Interaction::new(START, OutputAction::Pin, WindowTargets::default());
        assert!(matches!(
            press(&mut interaction, START, false, || false),
            Change::None
        ));
        assert_eq!(interaction.selection(), None);
        assert!(interaction.idle());

        press(&mut interaction, START, false, || true);
        interaction.move_pointer(END, BOUNDS);
        assert!(matches!(interaction.confirm_default(), Change::None));
        assert!(matches!(
            interaction.lose_capture(),
            Some(Change::Finish(SelectionOutcome::Cancelled))
        ));
        assert!(interaction.idle());
        assert!(interaction.lose_capture().is_none());

        let mut click = Interaction::new(START, OutputAction::Pin, WindowTargets::default());
        press(&mut click, START, false, || true);
        assert!(matches!(
            click.release_pointer(START, BOUNDS, &[], &Scene::default()),
            Some(Change::Finish(SelectionOutcome::Cancelled))
        ));

        let mut interaction = with_selection();
        let inside = Point::new(0, 100);
        assert!(matches!(
            press(&mut interaction, inside, true, || unreachable!()),
            Change::Finish(SelectionOutcome::Confirmed {
                action: OutputAction::Pin,
                rect,
            }) if rect == Rect::new(-120, 40, 81, 201)
        ));
    }

    #[test]
    fn cancelling_an_annotation_preserves_the_selection_and_committed_strokes() {
        let mut interaction = with_selection();
        let selection = interaction.selection();
        let start = Point::new(-80, 80);
        let end = Point::new(-60, 100);
        interaction.toggle_tool(Tool::Pen);
        assert!(matches!(
            press(&mut interaction, start, false, || true),
            Change::Annotation(Some(_))
        ));
        assert!(matches!(
            interaction.move_pointer(end, BOUNDS),
            Change::Annotation(Some(damage)) if damage.width() < selection.unwrap().width()
        ));
        assert!(matches!(
            interaction.release_pointer(end, BOUNDS, &[], &Scene::default()),
            Some(Change::Annotation(Some(_)))
        ));

        press(&mut interaction, start, false, || true);
        assert!(matches!(interaction.confirm_default(), Change::None));
        assert!(matches!(interaction.history(false), Change::None));
        assert!(matches!(
            interaction.lose_capture(),
            Some(Change::Annotation(Some(_)))
        ));
        press(&mut interaction, start, false, || true);
        assert!(matches!(
            interaction.escape(),
            (Change::Annotation(Some(_)), true)
        ));
        assert!(interaction.idle());
        assert_eq!(interaction.selection(), selection);
        assert!(matches!(
            interaction.history(false),
            Change::Annotation(Some(_))
        ));
        assert!(matches!(
            interaction.history(false),
            Change::Annotation(None)
        ));
        assert!(matches!(
            interaction.history(true),
            Change::Annotation(Some(_))
        ));
    }

    fn test_monitor(dpi: u32) -> CapturedMonitor {
        CapturedMonitor {
            info: MonitorInfo {
                handle: Default::default(),
                rect: BOUNDS,
                work: BOUNDS,
                dpi,
                primary: true,
            },
            frame: Frame::empty(),
        }
    }

    #[test]
    fn toolbar_activates_only_when_capture_releases_on_the_same_button() {
        let mut interaction = with_selection();
        let frames = [test_monitor(96)];
        let scene = interaction.scene(&frames, &UiConfig::default());
        let button = ToolbarButton::Copy;
        let area = scene
            .toolbar
            .unwrap()
            .buttons()
            .find(|(item, _)| *item == button)
            .unwrap()
            .1;
        let point = Point::new(area.left + 1, area.top + 1);
        interaction.press_toolbar(point, None, || unreachable!());
        assert!(interaction.idle());
        interaction.press_toolbar(point, Some(button), || false);
        assert!(interaction.idle());

        interaction.press_toolbar(point, Some(button), || true);
        assert!(matches!(interaction.confirm_default(), Change::None));
        assert!(matches!(interaction.lose_capture(), Some(Change::Refresh)));
        assert!(
            interaction
                .release_pointer(point, BOUNDS, &frames, &scene)
                .is_none()
        );

        interaction.press_toolbar(point, Some(button), || true);
        assert!(matches!(
            interaction.release_pointer(START, BOUNDS, &frames, &scene),
            Some(Change::Refresh)
        ));
        interaction.press_toolbar(point, Some(button), || true);
        assert!(matches!(
            interaction.release_pointer(point, BOUNDS, &frames, &scene),
            Some(Change::ClickToolbar(ToolbarButton::Copy))
        ));
        assert!(interaction.idle());
    }

    #[test]
    fn text_resize_uses_the_grab_offset_and_does_not_finish_after_capture_loss() {
        let mut interaction = with_selection();
        interaction.begin_text_resize(-18, -20, -100);
        assert!(matches!(
            interaction.move_pointer(Point::new(-18, 80), BOUNDS),
            Change::ResizeText {
                width: 80,
                focus: false
            }
        ));
        assert!(matches!(interaction.confirm_default(), Change::None));
        assert!(matches!(interaction.lose_capture(), Some(Change::Refresh)));
        assert!(!interaction.end_text_resize());
        assert!(
            interaction
                .release_pointer(END, BOUNDS, &[], &Scene::default())
                .is_none()
        );

        interaction.begin_text_resize(-18, -20, -100);
        assert!(matches!(
            interaction.release_pointer(Point::new(12, 100), BOUNDS, &[], &Scene::default()),
            Some(Change::ResizeText {
                width: 110,
                focus: true
            })
        ));
        assert!(matches!(
            interaction.confirm_default(),
            Change::Finish(SelectionOutcome::Confirmed { .. })
        ));
    }
}

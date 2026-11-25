use crate::annotation::{Document, Editor, StrokeStyle, TextStyle, Tool, local_point};
use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};
use crate::config::UiConfig;
use crate::output::{OutputAction, SelectionOutcome};

use super::input::Selection;
use super::scene::{Scene, ToolbarButton, ToolbarState, hit_test};

#[derive(Default)]
enum PointerMode {
    #[default]
    Idle,
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
    editor: Editor,
    pointer: PointerMode,
    cursor: Point,
    default_action: OutputAction,
}

impl Interaction {
    pub(super) fn new(cursor: Point, default_action: OutputAction) -> Self {
        Self {
            selection: Selection::default(),
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
        Scene::new(
            frames,
            ui,
            &self.selection,
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
            let hit = hit_test(frames, self.selection.rect, point);
            self.selection.begin(point, bounds, hit);
            self.pointer = PointerMode::Selection;
            Change::Refresh
        }
    }

    pub(super) fn move_pointer(&mut self, point: Point, bounds: Rect) -> Change {
        self.cursor = point;
        match self.pointer {
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
            PointerMode::Selection => {
                self.selection.update(point, bounds);
                if self.selection.end() {
                    Change::Refresh
                } else {
                    Change::Finish(SelectionOutcome::Cancelled)
                }
            }
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

    pub(super) fn lose_capture(&mut self) -> Option<Change> {
        Some(match std::mem::take(&mut self.pointer) {
            PointerMode::Idle => return None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{MonitorInfo, frame::Frame};

    const BOUNDS: Rect = Rect::new(-640, -100, 640, 600);
    const START: Point = Point::new(-120, 40);
    const END: Point = Point::new(80, 200);

    fn with_selection() -> Interaction {
        let mut interaction = Interaction::new(START, OutputAction::Pin);
        interaction.press_canvas(START, false, BOUNDS, &[], || true);
        assert!(matches!(
            interaction.release_pointer(END, BOUNDS, &[], &Scene::default()),
            Some(Change::Refresh)
        ));
        interaction
    }

    #[test]
    fn selection_requires_capture_and_a_complete_drag_before_confirming() {
        let mut interaction = Interaction::new(START, OutputAction::Pin);
        assert!(matches!(
            interaction.press_canvas(START, false, BOUNDS, &[], || false),
            Change::None
        ));
        assert_eq!(interaction.selection(), None);
        assert!(interaction.idle());

        interaction.press_canvas(START, false, BOUNDS, &[], || true);
        interaction.move_pointer(END, BOUNDS);
        assert!(matches!(interaction.confirm_default(), Change::None));
        assert!(matches!(
            interaction.lose_capture(),
            Some(Change::Finish(SelectionOutcome::Cancelled))
        ));
        assert!(interaction.idle());
        assert!(interaction.lose_capture().is_none());

        let mut click = Interaction::new(START, OutputAction::Pin);
        click.press_canvas(START, false, BOUNDS, &[], || true);
        assert!(matches!(
            click.release_pointer(START, BOUNDS, &[], &Scene::default()),
            Some(Change::Finish(SelectionOutcome::Cancelled))
        ));

        let mut interaction = with_selection();
        let inside = Point::new(0, 100);
        assert!(matches!(
            interaction.press_canvas(inside, true, BOUNDS, &[], || unreachable!()),
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
            interaction.press_canvas(start, false, BOUNDS, &[], || true),
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

        interaction.press_canvas(start, false, BOUNDS, &[], || true);
        assert!(matches!(interaction.confirm_default(), Change::None));
        assert!(matches!(interaction.history(false), Change::None));
        assert!(matches!(
            interaction.lose_capture(),
            Some(Change::Annotation(Some(_)))
        ));
        interaction.press_canvas(start, false, BOUNDS, &[], || true);
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

    #[test]
    fn toolbar_activates_only_when_capture_releases_on_the_same_button() {
        let mut interaction = with_selection();
        let frames = [CapturedMonitor {
            info: MonitorInfo {
                handle: Default::default(),
                rect: BOUNDS,
                work: BOUNDS,
                dpi: 96,
                primary: true,
            },
            frame: Frame::empty(),
        }];
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

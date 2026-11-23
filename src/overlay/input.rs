use crate::capture::frame::{Point, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Handle {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

impl Handle {
    pub(super) fn positions(rect: Rect) -> [(Self, Point); 8] {
        let x = rect.left + rect.width() / 2;
        let y = rect.top + rect.height() / 2;
        [
            (Self::NorthWest, Point::new(rect.left, rect.top)),
            (Self::NorthEast, Point::new(rect.right, rect.top)),
            (Self::SouthWest, Point::new(rect.left, rect.bottom)),
            (Self::SouthEast, Point::new(rect.right, rect.bottom)),
            (Self::North, Point::new(x, rect.top)),
            (Self::South, Point::new(x, rect.bottom)),
            (Self::East, Point::new(rect.right, y)),
            (Self::West, Point::new(rect.left, y)),
        ]
    }

    fn resize(self, rect: Rect, bounds: Rect, delta: Point) -> Rect {
        use Handle::*;
        let mut next = rect;
        if matches!(self, West | NorthWest | SouthWest) {
            next.left = (rect.left + delta.x).clamp(bounds.left, rect.right - 1);
        }
        if matches!(self, East | NorthEast | SouthEast) {
            next.right = (rect.right + delta.x).clamp(rect.left + 1, bounds.right);
        }
        if matches!(self, North | NorthEast | NorthWest) {
            next.top = (rect.top + delta.y).clamp(bounds.top, rect.bottom - 1);
        }
        if matches!(self, South | SouthEast | SouthWest) {
            next.bottom = (rect.bottom + delta.y).clamp(rect.top + 1, bounds.bottom);
        }
        next
    }
}

#[derive(Clone, Copy)]
pub(super) enum Hit {
    Outside,
    Inside,
    Handle(Handle),
}

#[derive(Default)]
enum Drag {
    #[default]
    None,
    Create(Point),
    Adjust {
        origin: Point,
        original: Rect,
        handle: Option<Handle>,
    },
}

#[derive(Default)]
pub(super) struct Selection {
    pub(super) rect: Option<Rect>,
    drag: Drag,
}

impl Selection {
    pub(super) fn dragging(&self) -> bool {
        !matches!(self.drag, Drag::None)
    }

    pub(super) fn saved_rect(&self) -> Option<Rect> {
        self.rect.filter(|rect| !rect.is_empty())
    }

    pub(super) fn begin(&mut self, point: Point, bounds: Rect, hit: Hit) {
        let point = pixel_in(point, bounds);
        self.drag = match (self.rect, hit) {
            (Some(original), Hit::Handle(handle)) => Drag::Adjust {
                origin: point,
                original,
                handle: Some(handle),
            },
            (Some(original), Hit::Inside) => Drag::Adjust {
                origin: point,
                original,
                handle: None,
            },
            _ => {
                self.rect = Some(Rect::new(point.x, point.y, point.x + 1, point.y + 1));
                Drag::Create(point)
            }
        };
    }

    pub(super) fn update(&mut self, point: Point, bounds: Rect) {
        self.rect = match self.drag {
            Drag::None => return,
            Drag::Create(origin) => {
                let mut rect = Rect::normalize(origin, pixel_in(point, bounds));
                rect.right += 1;
                rect.bottom += 1;
                Some(rect)
            }
            Drag::Adjust {
                origin,
                original,
                handle,
            } => {
                let delta = Point::new(point.x - origin.x, point.y - origin.y);
                Some(match handle {
                    Some(handle) => handle.resize(original, bounds, delta),
                    None => move_rect(original, bounds, delta),
                })
            }
        };
    }

    /// Returns false when a new selection was only a click.
    pub(super) fn end(&mut self) -> bool {
        let created = matches!(std::mem::take(&mut self.drag), Drag::Create(_));
        !created || self.rect.is_some_and(|r| r.width() >= 3 && r.height() >= 3)
    }

    pub(super) fn nudge(&mut self, bounds: Rect, delta: Point, resize: bool) {
        if self.dragging() {
            return;
        }
        self.rect = self.saved_rect().map(|rect| {
            if resize {
                Handle::SouthEast.resize(rect, bounds, delta)
            } else {
                move_rect(rect, bounds, delta)
            }
        });
    }
}

fn pixel_in(point: Point, bounds: Rect) -> Point {
    Point::new(
        point.x.clamp(bounds.left, bounds.right - 1),
        point.y.clamp(bounds.top, bounds.bottom - 1),
    )
}

fn move_rect(rect: Rect, bounds: Rect, delta: Point) -> Rect {
    rect.offset(
        delta
            .x
            .clamp(bounds.left - rect.left, bounds.right - rect.right),
        delta
            .y
            .clamp(bounds.top - rect.top, bounds.bottom - rect.bottom),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_drag_across_negative_origin_preserves_endpoint_pixels() {
        let bounds = Rect::new(-1920, -100, 3840, 2160);
        let mut selection = Selection::default();
        selection.begin(Point::new(20, 10), bounds, Hit::Outside);
        selection.update(Point::new(-25, -20), bounds);
        assert!(selection.end());
        assert_eq!(selection.saved_rect(), Some(Rect::new(-25, -20, 21, 11)));
    }

    #[test]
    fn resizing_from_handle_preserves_grab_offset_and_clamps_to_one_pixel() {
        let bounds = Rect::new(-300, -100, 100, 100);
        let original = Rect::new(-220, -30, -120, 30);
        let mut selection = Selection {
            rect: Some(original),
            drag: Drag::None,
        };
        let grab = Point::new(-222, 0);
        selection.begin(grab, bounds, Hit::Handle(Handle::West));
        selection.update(grab, bounds);
        assert_eq!(selection.saved_rect(), Some(original));
        selection.update(Point::new(0, 0), bounds);
        assert!(selection.end());
        assert_eq!(selection.saved_rect(), Some(Rect::new(-121, -30, -120, 30)));
        selection.nudge(bounds, Point::new(-500, 0), false);
        assert_eq!(selection.saved_rect(), Some(Rect::new(-300, -30, -299, 30)));
    }
}

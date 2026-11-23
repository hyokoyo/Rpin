use crate::capture::frame::{Point, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Size {
    pub width: i32,
    pub height: i32,
}

impl Size {
    pub fn pixels(self) -> usize {
        self.width as usize * self.height as usize
    }

    pub fn scaled(self, requested: f64, work: Rect) -> Self {
        let scale = requested
            .clamp(0.1, 4.0)
            .min(f64::from(work.width()) / f64::from(self.width))
            .min(f64::from(work.height()) / f64::from(self.height));
        Self {
            width: (f64::from(self.width) * scale).round() as i32,
            height: (f64::from(self.height) * scale).round() as i32,
        }
        .at_least_one()
    }

    fn at_least_one(self) -> Self {
        Self {
            width: self.width.max(1),
            height: self.height.max(1),
        }
    }

    pub fn position(self, wanted: Point, work: Rect) -> Point {
        Point::new(
            wanted
                .x
                .clamp(work.left, (work.right - self.width).max(work.left)),
            wanted
                .y
                .clamp(work.top, (work.bottom - self.height).max(work.top)),
        )
    }

    pub fn zoom_position(self, previous: Rect, anchor: Point, work: Rect) -> Point {
        let x = (f64::from(anchor.x) - f64::from(previous.left)) / f64::from(previous.width());
        let y = (f64::from(anchor.y) - f64::from(previous.top)) / f64::from(previous.height());
        self.position(
            Point::new(
                anchor
                    .x
                    .saturating_sub((x * f64::from(self.width)).round() as i32),
                anchor
                    .y
                    .saturating_sub((y * f64::from(self.height)).round() as i32),
            ),
            work,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_large_pins_to_the_work_area_with_negative_coordinates() {
        let native = Size {
            width: 3840,
            height: 2160,
        };
        let work = Rect::new(-1920, 0, 0, 1040);
        let display = native.scaled(1.0, work);
        assert_eq!(
            display,
            Size {
                width: 1849,
                height: 1040
            }
        );
        assert_eq!(
            display.position(Point::new(-100, 100), work),
            Point::new(-1849, 0)
        );
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let display = Size {
            width: 800,
            height: 600,
        };
        let work = Rect::new(0, 0, 1920, 1080);
        let old = Rect::new(500, 300, 900, 600);
        let anchor = Point::new(600, 450);
        assert_eq!(
            display.zoom_position(old, anchor, work),
            Point::new(400, 150)
        );
    }
}

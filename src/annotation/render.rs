use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::{
    Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_ROUND,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ELLIPSE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_STROKE_STYLE_PROPERTIES, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, ID2D1Factory,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::w;
use windows_numerics::Vector2;

use crate::capture::{
    frame::{Frame, Point, Rect},
    gdi,
};

use super::{
    DEFAULT_INK, Document, Editor, Gesture, Mosaic, Shape, StrokeShape, StrokeStyle, StrokeWidth,
    Tool, pixel_rect, stroke_damage,
};

const PIXELS: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT {
    format: DXGI_FORMAT_B8G8R8A8_UNORM,
    alphaMode: D2D1_ALPHA_MODE_IGNORE,
};

pub(crate) struct Painter {
    ink: ID2D1SolidColorBrush,
    ink_color: Cell<[u8; 3]>,
    round: ID2D1StrokeStyle,
    dwrite: IDWriteFactory,
    text_formats: RefCell<BTreeMap<u16, IDWriteTextFormat>>,
}

impl Painter {
    pub(crate) fn new(
        factory: &ID2D1Factory,
        target: &ID2D1RenderTarget,
    ) -> windows::core::Result<Self> {
        // SAFETY: resources are created for the live render target on its owning thread.
        unsafe {
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    miterLimit: 10.0,
                    ..Default::default()
                },
                None,
            )?;
            Ok(Self {
                ink: target.CreateSolidColorBrush(&rgb_color(DEFAULT_INK), None)?,
                ink_color: Cell::new(DEFAULT_INK),
                round,
                dwrite: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
                text_formats: RefCell::new(BTreeMap::new()),
            })
        }
    }

    pub(crate) fn preview(
        &self,
        target: &ID2D1RenderTarget,
        editor: &Editor,
        origin: Point,
        dirty: Rect,
    ) -> windows::core::Result<()> {
        self.document(target, &editor.document, origin, dirty)?;
        if let Some(gesture) = editor
            .gesture
            .as_ref()
            .filter(|gesture| gesture.damage.intersect(dirty).is_some())
        {
            self.gesture(target, gesture, origin, dirty);
        }
        Ok(())
    }

    fn document(
        &self,
        target: &ID2D1RenderTarget,
        doc: &Document,
        origin: Point,
        dirty: Rect,
    ) -> windows::core::Result<()> {
        for entry in doc.commands[..doc.applied]
            .iter()
            .filter(|entry| entry.damage.intersect(dirty).is_some())
        {
            match &entry.shape {
                Shape::Stroke(shape, style) => {
                    self.set_ink(style.rgb);
                    let width = style.width;
                    match shape {
                        StrokeShape::Pen(points) => self.pen(target, points, origin, dirty, width),
                        StrokeShape::Arrow(start, end) => {
                            self.arrow(target, *start, *end, origin, width)
                        }
                        StrokeShape::Rectangle(rect) => {
                            self.rectangle(target, *rect, origin, width)
                        }
                        StrokeShape::Ellipse(rect) => self.ellipse(target, *rect, origin, width),
                    }
                }
                Shape::Text { area, units, style } => {
                    let format = self.text_format(style.size_px)?;
                    self.set_ink(style.rgb);
                    // SAFETY: the text format and brush belong to this drawing thread.
                    unsafe {
                        let mode = target.GetTextAntialiasMode();
                        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                        target.DrawText(
                            units,
                            &format,
                            &area_at(*area, origin),
                            &self.ink,
                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                            DWRITE_MEASURING_MODE_NATURAL,
                        );
                        target.SetTextAntialiasMode(mode);
                    }
                }
                Shape::Mosaic(mosaic) => self.mosaic(target, mosaic, origin, dirty),
            }
        }
        Ok(())
    }

    fn text_format(&self, size_px: u16) -> windows::core::Result<IDWriteTextFormat> {
        let mut formats = self.text_formats.borrow_mut();
        if let Some(format) = formats.get(&size_px) {
            return Ok(format.clone());
        }
        let format = text_format(&self.dwrite, size_px)?;
        formats.insert(size_px, format.clone());
        Ok(format)
    }

    fn gesture(&self, target: &ID2D1RenderTarget, gesture: &Gesture, origin: Point, dirty: Rect) {
        let rect = pixel_rect(gesture.start, gesture.end);
        let style = if gesture.tool == Tool::Mosaic {
            StrokeStyle::default()
        } else {
            gesture.style
        };
        self.set_ink(style.rgb);
        let width = style.width;
        match gesture.tool {
            Tool::Pen => self.pen(target, &gesture.points, origin, dirty, width),
            Tool::Arrow => self.arrow(target, gesture.start, gesture.end, origin, width),
            Tool::Rectangle | Tool::Mosaic => self.rectangle(target, rect, origin, width),
            Tool::Ellipse => self.ellipse(target, rect, origin, width),
            Tool::Text => {}
        }
    }

    fn set_ink(&self, rgb: [u8; 3]) {
        if self.ink_color.replace(rgb) == rgb {
            return;
        }
        // SAFETY: the brush belongs to the current drawing target and thread.
        unsafe {
            self.ink.SetColor(&rgb_color(rgb));
        }
    }

    fn pen(
        &self,
        target: &ID2D1RenderTarget,
        points: &[Point],
        origin: Point,
        dirty: Rect,
        width: StrokeWidth,
    ) {
        let pixels = width.pixels() as f32;
        let margin = width.paint_margin();
        // SAFETY: the current target and the pen resources are live between BeginDraw and EndDraw.
        unsafe {
            if let [point] = points {
                target.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: at(*point, origin),
                        radiusX: pixels / 2.0,
                        radiusY: pixels / 2.0,
                    },
                    &self.ink,
                );
            }
            for pair in points.windows(2) {
                if stroke_damage(pixel_rect(pair[0], pair[1]), margin)
                    .intersect(dirty)
                    .is_some()
                {
                    target.DrawLine(
                        at(pair[0], origin),
                        at(pair[1], origin),
                        &self.ink,
                        pixels,
                        &self.round,
                    );
                }
            }
        }
    }

    fn arrow(
        &self,
        target: &ID2D1RenderTarget,
        start: Point,
        end: Point,
        origin: Point,
        width: StrokeWidth,
    ) {
        let start = at(start, origin);
        let end = at(end, origin);
        let (dx, dy) = (end.X - start.X, end.Y - start.Y);
        let length = dx.hypot(dy);
        if length < 1.0 {
            return;
        }
        let head = (length * 0.36).min(width.arrow_head_limit() as f32);
        let pixels = width.pixels() as f32;
        let (ux, uy) = (dx / length, dy / length);
        let back = Vector2 {
            X: end.X - ux * head,
            Y: end.Y - uy * head,
        };
        // SAFETY: the current target and the pen resources are live during drawing.
        unsafe {
            target.DrawLine(start, end, &self.ink, pixels, &self.round);
            for side in [-1.0, 1.0] {
                let wing = Vector2 {
                    X: back.X - uy * head * 0.48 * side,
                    Y: back.Y + ux * head * 0.48 * side,
                };
                target.DrawLine(end, wing, &self.ink, pixels, &self.round);
            }
        }
    }

    fn rectangle(&self, target: &ID2D1RenderTarget, rect: Rect, origin: Point, width: StrokeWidth) {
        // SAFETY: the current target and pen resources are live during drawing.
        unsafe {
            target.DrawRectangle(
                &area_at(rect, origin),
                &self.ink,
                width.pixels() as f32,
                &self.round,
            )
        }
    }

    fn ellipse(&self, target: &ID2D1RenderTarget, rect: Rect, origin: Point, width: StrokeWidth) {
        let area = area_at(rect, origin);
        // SAFETY: the current target and pen resources are live during drawing.
        unsafe {
            target.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: Vector2 {
                        X: (area.left + area.right) / 2.0,
                        Y: (area.top + area.bottom) / 2.0,
                    },
                    radiusX: (area.right - area.left) / 2.0,
                    radiusY: (area.bottom - area.top) / 2.0,
                },
                &self.ink,
                width.pixels() as f32,
                &self.round,
            );
        }
    }

    fn mosaic(&self, target: &ID2D1RenderTarget, mosaic: &Mosaic, origin: Point, dirty: Rect) {
        let Some(visible) = mosaic.rect.intersect(dirty) else {
            return;
        };
        let tile = mosaic.tile;
        let left = mosaic.rect.left;
        let top = mosaic.rect.top;
        let columns = (visible.left - left) / tile..=(visible.right - 1 - left) / tile;
        let rows = (visible.top - top) / tile..=(visible.bottom - 1 - top) / tile;
        // SAFETY: the shared brush belongs to the current target. Individual
        // rectangles are in integer pixels and never extend past the mosaic.
        unsafe {
            for y in rows {
                for x in columns.clone() {
                    let [b, g, r, _] = mosaic.colors[y as usize * mosaic.columns + x as usize];
                    self.set_ink([r, g, b]);
                    let (x, y) = (left + x * tile, top + y * tile);
                    let cell = Rect::new(x, y, x + tile, y + tile).clamp(mosaic.rect);
                    target.FillRectangle(&area_at(cell, origin), &self.ink);
                }
            }
        }
    }
}

pub(crate) fn text_format(
    factory: &IDWriteFactory,
    size_px: u16,
) -> windows::core::Result<IDWriteTextFormat> {
    // SAFETY: the factory and format are used on their owning thread.
    unsafe {
        let format = factory.CreateTextFormat(
            w!("Segoe UI"),
            None::<&IDWriteFontCollection>,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            f32::from(size_px),
            w!("zh-CN"),
        )?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(format)
    }
}

fn at(point: Point, origin: Point) -> Vector2 {
    Vector2 {
        X: (point.x + origin.x) as f32 + 0.5,
        Y: (point.y + origin.y) as f32 + 0.5,
    }
}

pub(crate) fn rgb_color([r, g, b]: [u8; 3]) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
}

fn area_at(rect: Rect, origin: Point) -> D2D_RECT_F {
    let rect = rect.offset(origin.x, origin.y);
    D2D_RECT_F {
        left: rect.left as f32,
        top: rect.top as f32,
        right: rect.right as f32,
        bottom: rect.bottom as f32,
    }
}

pub(crate) fn flatten(
    factory: &ID2D1Factory,
    frame: Frame,
    doc: &Document,
) -> anyhow::Result<Frame> {
    if doc.is_empty() {
        return Ok(frame);
    }
    let (width, height) = (frame.width, frame.height);
    let bounds = Rect::new(0, 0, width as i32, height as i32);
    let draw = |dc| {
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: PIXELS,
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        // SAFETY: the memory DC remains bound to its output DIB for this closure.
        // The uploaded base bitmap is initialized and owned by the input frame.
        unsafe {
            let target = factory.CreateDCRenderTarget(&properties)?;
            target.BindDC(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: bounds.right,
                    bottom: bounds.bottom,
                },
            )?;
            let bitmap = target.CreateBitmap(
                D2D_SIZE_U { width, height },
                Some(frame.pixels().as_ptr().cast()),
                frame.stride,
                &D2D1_BITMAP_PROPERTIES {
                    pixelFormat: PIXELS,
                    dpiX: 96.0,
                    dpiY: 96.0,
                },
            )?;
            let painter = Painter::new(factory, &target)?;
            target.BeginDraw();
            target.DrawBitmap(
                &bitmap,
                Some(&area_at(bounds, Point::default())),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
            );
            let annotations = painter.document(&target, doc, Point::default(), bounds);
            target.EndDraw(None, None).and(annotations)?;
        }
        Ok(())
    };
    // SAFETY: DrawBitmap covers the entire output with the opaque base image
    // before any annotation is painted, and EndDraw must succeed.
    unsafe { gdi::render_to_frame(bounds, draw) }
}

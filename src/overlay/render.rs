use std::cell::OnceCell;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    D2D1_BITMAP_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_IMMEDIATELY, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS, D2D1_RENDER_TARGET_PROPERTIES, D2D1_ROUNDED_RECT,
    ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory,
    IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::{PCWSTR, w};

use crate::annotation::{Editor, Painter, rgb_color};
use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};
use crate::config::UiConfig;

use super::scene::{Label, Magnifier, Scene, Toolbar, ToolbarButton, handles, scaled};

thread_local! {
    static DWRITE: OnceCell<IDWriteFactory> = const { OnceCell::new() };
}

const BLACK: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};
const WHITE: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};
const BUTTON_HOVER: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.22,
    g: 0.26,
    b: 0.32,
    a: 1.0,
};
const BUTTON_PRESSED: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.16,
    g: 0.36,
    b: 0.58,
    a: 1.0,
};
const PIXELS: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT {
    format: DXGI_FORMAT_B8G8R8A8_UNORM,
    alphaMode: D2D1_ALPHA_MODE_IGNORE,
};

pub(super) struct Renderer {
    target: ID2D1HwndRenderTarget,
    bitmaps: Vec<ID2D1Bitmap>,
    black: ID2D1SolidColorBrush,
    white: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    button_hover: ID2D1SolidColorBrush,
    button_pressed: ID2D1SolidColorBrush,
    text: Vec<TextFormats>,
    bounds: Rect,
    retained: bool,
    annotation: Option<Painter>,
}

struct TextFormats {
    dpi: u32,
    label: Option<IDWriteTextFormat>,
    button: IDWriteTextFormat,
}

impl Renderer {
    pub(super) fn new(
        factory: &ID2D1Factory,
        hwnd: HWND,
        bounds: Rect,
        frames: &[CapturedMonitor],
        ui: &UiConfig,
    ) -> anyhow::Result<Self> {
        let properties = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: PIXELS,
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let mut window = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U {
                width: bounds.width() as u32,
                height: bounds.height() as u32,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS | D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };
        // SAFETY: hwnd and the property structures are valid for both calls.
        let (target, retained) = unsafe {
            match factory.CreateHwndRenderTarget(&properties, &window) {
                Ok(target) => (target, true),
                Err(err) => {
                    crate::logging::debug(format!("retained Direct2D target unavailable: {err}"));
                    window.presentOptions = D2D1_PRESENT_OPTIONS_NONE;
                    (factory.CreateHwndRenderTarget(&properties, &window)?, false)
                }
            }
        };
        let bitmap_properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: PIXELS,
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let mut bitmaps = Vec::with_capacity(frames.len());
        // SAFETY: each Frame owns a BGRA buffer matching its dimensions and stride.
        unsafe {
            let limit = target.GetMaximumBitmapSize();
            for monitor in frames {
                let frame = &monitor.frame;
                anyhow::ensure!(
                    frame.width <= limit && frame.height <= limit,
                    "monitor exceeds Direct2D bitmap limit ({limit}px)"
                );
                bitmaps.push(target.CreateBitmap(
                    D2D_SIZE_U {
                        width: frame.width,
                        height: frame.height,
                    },
                    Some(frame.pixels().as_ptr().cast()),
                    frame.stride,
                    &bitmap_properties,
                )?);
            }
            let black = target.CreateSolidColorBrush(&BLACK, None)?;
            let white = target.CreateSolidColorBrush(&WHITE, None)?;
            let dim = target.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    a: ui.dim_alpha,
                    ..BLACK
                },
                None,
            )?;
            let text = text_formats(frames, ui.show_size || (ui.magnifier && ui.show_rgb))?;
            let button_hover = target.CreateSolidColorBrush(&BUTTON_HOVER, None)?;
            let button_pressed = target.CreateSolidColorBrush(&BUTTON_PRESSED, None)?;
            Ok(Self {
                target,
                bitmaps,
                black,
                white,
                dim,
                button_hover,
                button_pressed,
                text,
                bounds,
                retained,
                annotation: None,
            })
        }
    }

    pub(super) fn paint(
        &mut self,
        dirty: Rect,
        frames: &[CapturedMonitor],
        scene: &Scene,
        editing: Option<(&ID2D1Factory, &Editor)>,
    ) -> windows::core::Result<()> {
        let dirty = if self.retained { dirty } else { self.bounds };
        if dirty.is_empty() {
            return Ok(());
        }
        let editing = editing.filter(|(_, editor)| editor.has_marks());
        if let Some((factory, _)) = editing {
            if self.annotation.is_none() {
                self.annotation = Some(Painter::new(factory, &self.target)?);
            }
        }
        // SAFETY: paint is called on the owning UI thread with live target
        // resources. Each pushed clip is popped before EndDraw.
        unsafe {
            let mut annotations = Ok(());
            self.target.BeginDraw();
            self.target
                .PushAxisAlignedClip(&self.client(dirty), D2D1_ANTIALIAS_MODE_ALIASED);
            self.target.FillRectangle(&self.client(dirty), &self.black);
            // Annotation gestures usually invalidate a small area inside the
            // selection. In that case draw the undimmed desktop only once.
            let highlighted = scene.highlighted_rect();
            if !highlighted.is_some_and(|rect| rect.intersect(dirty) == Some(dirty)) {
                self.draw_desktop(frames, dirty);
                self.target.FillRectangle(&self.client(dirty), &self.dim);
            }
            if let Some(rect) = highlighted {
                if let Some(visible) = dirty.intersect(rect) {
                    self.target
                        .PushAxisAlignedClip(&self.client(visible), D2D1_ANTIALIAS_MODE_ALIASED);
                    self.draw_desktop(frames, visible);
                    if let (Some(painter), Some((_, editor))) = (&self.annotation, editing) {
                        annotations = painter.preview(
                            &self.target,
                            editor,
                            Point::new(rect.left - self.bounds.left, rect.top - self.bounds.top),
                            visible.offset(-rect.left, -rect.top),
                        );
                    }
                    self.target.PopAxisAlignedClip();
                }
                self.outline(rect);
                if !scene.editing && scene.selection.is_some() {
                    for (_, _, handle) in handles(frames, rect) {
                        self.target.FillRectangle(&self.client(handle), &self.white);
                        self.target.DrawRectangle(
                            &self.client(handle),
                            &self.black,
                            1.0,
                            None::<&ID2D1StrokeStyle>,
                        );
                    }
                }
                if let Some(label) = scene.label {
                    self.draw_label(label, &format!("{} × {}", rect.width(), rect.height()));
                }
            }
            if let Some(zoom) = scene.magnifier {
                self.draw_magnifier(zoom, &frames[zoom.monitor]);
            }
            if let Some(grip) = scene.text_grip {
                let rect = self.client(grip);
                self.target.FillRectangle(&rect, &self.black);
                let center = (rect.left + rect.right) / 2.0;
                if rect.bottom - rect.top > 6.0 {
                    for x in [center - 1.5, center + 0.5] {
                        self.target.FillRectangle(
                            &D2D_RECT_F {
                                left: x,
                                top: rect.top + 3.0,
                                right: x + 1.0,
                                bottom: rect.bottom - 3.0,
                            },
                            &self.white,
                        );
                    }
                }
            }
            if let Some(toolbar) = scene
                .toolbar
                .filter(|bar| dirty.intersect(bar.rect).is_some())
            {
                self.draw_toolbar(toolbar);
            }
            self.target.PopAxisAlignedClip();
            self.target.EndDraw(None, None).and(annotations)
        }
    }

    unsafe fn draw_desktop(&self, frames: &[CapturedMonitor], clip: Rect) {
        // SAFETY: frame and bitmap indices match, and paint has begun.
        unsafe {
            for (monitor, bitmap) in frames.iter().zip(&self.bitmaps) {
                if monitor.info.rect.intersect(clip).is_some() {
                    self.target.DrawBitmap(
                        bitmap,
                        Some(&self.client(monitor.info.rect)),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                        None,
                    );
                }
            }
        }
    }

    unsafe fn outline(&self, rect: Rect) {
        // SAFETY: both brushes belong to the current target.
        unsafe {
            let area = self.client(rect);
            self.target
                .DrawRectangle(&area, &self.black, 3.0, None::<&ID2D1StrokeStyle>);
            self.target
                .DrawRectangle(&area, &self.white, 1.0, None::<&ID2D1StrokeStyle>);
        }
    }

    unsafe fn draw_magnifier(&self, zoom: Magnifier, monitor: &CapturedMonitor) {
        let target = self.client(zoom.image);
        let x = target.left
            + (zoom.pixel.x as f32 + 0.5 - zoom.sample.left as f32) * zoom.image.width() as f32
                / zoom.sample.width() as f32;
        let y = target.top
            + (zoom.pixel.y as f32 + 0.5 - zoom.sample.top as f32) * zoom.image.height() as f32
                / zoom.sample.height() as f32;
        // SAFETY: the sample lies in the bitmap and pixel lies in the CPU frame.
        unsafe {
            self.target.DrawBitmap(
                &self.bitmaps[zoom.monitor],
                Some(&target),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                Some(&d2d_rect(zoom.sample)),
            );
            self.outline(zoom.image);
            let vertical = D2D_RECT_F {
                left: x.floor(),
                right: x.floor() + 1.0,
                ..target
            };
            let horizontal = D2D_RECT_F {
                top: y.floor(),
                bottom: y.floor() + 1.0,
                ..target
            };
            self.target.FillRectangle(&vertical, &self.black);
            self.target.FillRectangle(&horizontal, &self.black);
            let point = D2D_RECT_F {
                left: x.floor() - 1.0,
                top: y.floor() - 1.0,
                right: x.floor() + 2.0,
                bottom: y.floor() + 2.0,
            };
            self.target.FillRectangle(&point, &self.white);
            if let Some(label) = zoom.label {
                let row = monitor.frame.row(zoom.pixel.y as u32);
                let offset = zoom.pixel.x as usize * 4;
                let (b, g, r) = (row[offset], row[offset + 1], row[offset + 2]);
                self.draw_label(label, &format!("#{r:02X}{g:02X}{b:02X} ({r},{g},{b})"));
            }
        }
    }

    unsafe fn draw_label(&self, label: Label, text: &str) {
        let Some(format) = self
            .text
            .iter()
            .find(|formats| formats.dpi == label.dpi)
            .and_then(|formats| formats.label.as_ref())
        else {
            return;
        };
        let area = self.client(label.rect);
        let inset = D2D_RECT_F {
            left: area.left + scaled(4, label.dpi) as f32,
            ..area
        };
        // SAFETY: the format and brush belong to the current draw.
        unsafe {
            self.target.FillRectangle(&area, &self.black);
            self.draw_text(text, format, &inset);
        }
    }

    unsafe fn draw_toolbar(&self, toolbar: Toolbar) {
        let Some(formats) = self.text.iter().find(|formats| formats.dpi == toolbar.dpi) else {
            return;
        };
        let radius = scaled(5, toolbar.dpi) as f32;
        let rounded = |rect| D2D1_ROUNDED_RECT {
            rect: self.client(rect),
            radiusX: radius,
            radiusY: radius,
        };
        // SAFETY: this is part of the active Direct2D draw; brushes belong to the target.
        unsafe {
            self.target
                .FillRoundedRectangle(&rounded(toolbar.rect), &self.black);
            for (button, rect) in toolbar.buttons() {
                let active = matches!(button, ToolbarButton::Tool(tool) if toolbar.state.active_tool == Some(tool));
                if active || toolbar.hovered == Some(button) {
                    let brush = if active || toolbar.state.pressed == Some(button) {
                        &self.button_pressed
                    } else {
                        &self.button_hover
                    };
                    self.target.FillRoundedRectangle(&rounded(rect), brush);
                }
                let enabled = toolbar.enabled(button);
                if !enabled {
                    self.white.SetOpacity(0.42);
                }
                match button {
                    ToolbarButton::TextSize => {
                        let size = toolbar.state.text_style.size_px;
                        let label = if toolbar.compact {
                            size.to_string()
                        } else {
                            format!("{size} px")
                        };
                        self.draw_text(&label, &formats.button, &self.client(rect));
                    }
                    ToolbarButton::StrokeWidth => {
                        let label = if toolbar.compact {
                            toolbar.state.stroke_style.width.label()
                        } else {
                            match toolbar.state.stroke_style.width {
                                crate::annotation::StrokeWidth::Thin => "线宽 细",
                                crate::annotation::StrokeWidth::Medium => "线宽 中",
                                crate::annotation::StrokeWidth::Thick => "线宽 粗",
                            }
                        };
                        self.draw_text(label, &formats.button, &self.client(rect));
                    }
                    ToolbarButton::Color => {
                        self.draw_color_option(toolbar, rect, &formats.button);
                    }
                    _ => self.draw_text(
                        button.label(toolbar.compact),
                        &formats.button,
                        &self.client(rect),
                    ),
                }
                if !enabled {
                    self.white.SetOpacity(1.0);
                }
            }
        }
    }

    unsafe fn draw_color_option(&self, toolbar: Toolbar, rect: Rect, format: &IDWriteTextFormat) {
        let size = scaled(9, toolbar.dpi)
            .min(rect.width())
            .min(rect.height())
            .max(1);
        let x = if toolbar.compact {
            rect.left + (rect.width() - size) / 2
        } else {
            rect.right - size - scaled(4, toolbar.dpi)
        };
        let y = rect.top + (rect.height() - size) / 2;
        let swatch = self.client(Rect::new(x, y, x + size, y + size));
        // SAFETY: this is part of the toolbar draw. Restore the shared brush after filling.
        unsafe {
            if !toolbar.compact {
                self.draw_text(
                    "颜色",
                    format,
                    &self.client(Rect::new(rect.left, rect.top, x, rect.bottom)),
                );
            }
            self.white
                .SetColor(&rgb_color(toolbar.state.active_color()));
            self.target.FillRectangle(&swatch, &self.white);
            self.white.SetColor(&WHITE);
            self.target
                .DrawRectangle(&swatch, &self.white, 1.0, None::<&ID2D1StrokeStyle>);
        }
    }

    unsafe fn draw_text(&self, text: &str, format: &IDWriteTextFormat, area: &D2D_RECT_F) {
        let mut wide = [0; 64];
        let mut len = 0;
        for (slot, unit) in wide.iter_mut().zip(text.encode_utf16()) {
            *slot = unit;
            len += 1;
        }
        // SAFETY: the format, brush and text buffer remain valid during DrawText.
        unsafe {
            self.target.DrawText(
                &wide[..len],
                format,
                area,
                &self.white,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn client(&self, rect: Rect) -> D2D_RECT_F {
        d2d_rect(rect.offset(-self.bounds.left, -self.bounds.top))
    }
}

fn d2d_rect(rect: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left as f32,
        top: rect.top as f32,
        right: rect.right as f32,
        bottom: rect.bottom as f32,
    }
}

fn text_formats(
    frames: &[CapturedMonitor],
    show_labels: bool,
) -> windows::core::Result<Vec<TextFormats>> {
    let factory = DWRITE.with(|cell| {
        if let Some(factory) = cell.get() {
            return Ok(factory.clone());
        }
        // SAFETY: created lazily on the UI thread.
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let _ = cell.set(factory.clone());
        Ok::<_, windows::core::Error>(factory)
    })?;
    let mut formats: Vec<TextFormats> = Vec::new();
    for monitor in frames {
        let dpi = monitor.info.dpi;
        if !formats.iter().any(|format| format.dpi == dpi) {
            let label = show_labels
                .then(|| text_format(&factory, w!("Consolas"), w!("en-us"), scaled(12, dpi)))
                .transpose()?;
            let button = text_format(&factory, w!("Segoe UI"), w!("zh-CN"), scaled(14, dpi))?;
            // SAFETY: the format is live on this UI thread.
            unsafe {
                button.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            }
            formats.push(TextFormats { dpi, label, button });
        }
    }
    Ok(formats)
}

fn text_format(
    factory: &IDWriteFactory,
    family: PCWSTR,
    locale: PCWSTR,
    size: i32,
) -> windows::core::Result<IDWriteTextFormat> {
    // SAFETY: family and locale are static null-terminated strings.
    unsafe {
        let format = factory.CreateTextFormat(
            family,
            None::<&IDWriteFontCollection>,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size as f32,
            locale,
        )?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        Ok(format)
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Gdi::HMONITOR;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };

    use crate::capture::{
        MonitorInfo,
        frame::{Frame, Point},
    };
    use crate::overlay::input::Selection;
    use crate::platform::window::hinstance;

    use super::*;

    #[test]
    #[ignore = "requires a Windows session that supports native window rendering"]
    fn paints_mixed_dpi_frames_to_hidden_window() -> anyhow::Result<()> {
        let bounds = Rect::new(-256, 0, 256, 256);
        let frames = [(-256, 120), (0, 144)].map(|(x, dpi)| {
            let rect = Rect::new(x, 0, x + 256, 256);
            CapturedMonitor {
                info: MonitorInfo {
                    handle: HMONITOR::default(),
                    rect,
                    work: rect,
                    dpi,
                    primary: x == 0,
                },
                frame: Frame::new(256, 256, vec![128; 256 * 256 * 4]).expect("valid test frame"),
            }
        });
        let ui = UiConfig::default();
        let mut selection = Selection::default();
        selection.rect = Some(Rect::new(-180, 10, 200, 84));
        let scene = Scene::new(
            &frames,
            &ui,
            &selection,
            None,
            Point::new(-230, 225),
            Default::default(),
        );
        assert!(scene.label.is_some());
        assert!(scene.magnifier.is_some());
        assert!(scene.toolbar.is_some());
        let preview = Scene::new(
            &frames,
            &ui,
            &Selection::default(),
            selection.saved_rect(),
            Point::new(-230, 225),
            Default::default(),
        );
        // SAFETY: STATIC is a predefined class, and the test leaves the window hidden.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("RPin render test"),
                WS_POPUP,
                0,
                0,
                bounds.width(),
                bounds.height(),
                None,
                None,
                Some(hinstance()?),
                None,
            )?
        };
        let paint = (|| {
            let mut cache = crate::overlay::Direct2dCache::new(hwnd);
            let mut renderer = Renderer::new(&cache.factory()?, hwnd, bounds, &frames, &ui)?;
            renderer.paint(bounds, &frames, &preview, None)?;
            let damage = scene
                .damage_from(&preview, &frames)
                .expect("selection controls need repaint");
            renderer.paint(damage, &frames, &scene, None)?;
            renderer.paint(Rect::new(0, 90, 220, 150), &frames, &scene, None)?;
            Ok(())
        })();
        // SAFETY: the render target has been dropped, and the test owns this HWND.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        paint
    }
}

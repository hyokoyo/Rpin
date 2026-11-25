use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_TEXT_METRICS, DWriteCreateFactory, IDWriteFactory,
    IDWriteTextFormat,
};
use windows::Win32::Graphics::Gdi::{
    CLIP_DEFAULT_PRECIS, CreateFontW, DC_BRUSH, DEFAULT_CHARSET, DeleteObject, FW_SEMIBOLD,
    GetStockObject, HDC, HFONT, HGDIOBJ, InvalidateRect, NONANTIALIASED_QUALITY, OPAQUE,
    OUT_DEFAULT_PRECIS, SetBkColor, SetBkMode, SetDCBrushColor, SetTextColor,
};
use windows::Win32::UI::Input::Ime::{
    GCS_COMPSTR, ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_WANTRETURN,
    GetPropW, GetWindowTextW, IsWindow, LWA_COLORKEY, PostMessageW, RemovePropW, SW_SHOWNA,
    SWP_NOACTIVATE, SWP_NOZORDER, SendMessageW, SetLayeredWindowAttributes, SetPropW, SetWindowPos,
    ShowWindow, WINDOW_STYLE, WM_APP, WM_CHAR, WM_KEYDOWN, WM_NCDESTROY, WM_SETFONT, WS_BORDER,
    WS_CHILD, WS_EX_LAYERED,
};
use windows::core::{PCWSTR, w};

use crate::annotation::{MAX_TEXT_UNITS, TextStyle, text_format};
use crate::capture::frame::{Point, Rect};
use crate::platform::window::hinstance;

use super::style::color_ref;

pub(super) const WM_TEXT_FINISHED: u32 = WM_APP + 1;
pub(super) const WM_TEXT_CHANGED: u32 = WM_APP + 2;
pub(super) const EN_CHANGE: usize = 0x0300;
const EDIT_SUBCLASS: usize = 1;
const EM_SETLIMITTEXT: u32 = 0x00C5;
const EM_LINESCROLL: u32 = 0x00B6;
const EM_SCROLLCARET: u32 = 0x00B7;
const AUTO_WIDTH: i32 = 96;
const MIN_WIDTH: i32 = 40;
const GRIP_WIDTH: i32 = 10;
const TEXT_PADDING: i32 = 14;
const EDIT_COLOR: PCWSTR = w!("RPin.AnnotationEditColor");

pub(super) struct TextInput {
    pub(super) hwnd: HWND,
    style: Option<TextStyle>,
    canvas: Rect,
    toolbar: Option<Rect>,
    field: Rect,
    anchor: Point,
    font: Option<EditFont>,
    dwrite: IDWriteFactory,
    format: Option<IDWriteTextFormat>,
    min_width: Option<i32>,
    content: TextExtent,
}

#[derive(Clone, Copy, Default)]
struct TextExtent {
    width: i32,
    height: i32,
    line_height: i32,
}

impl TextInput {
    pub(super) fn new(
        owner: HWND,
        canvas: Rect,
        anchor: Point,
        style: TextStyle,
        toolbar: Option<Rect>,
    ) -> anyhow::Result<Self> {
        // SAFETY: the shared factory is used on the edit control's UI thread.
        let dwrite = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        // SAFETY: use the system EDIT class with the live overlay as its parent.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED,
                w!("EDIT"),
                w!(""),
                WS_CHILD
                    | WS_BORDER
                    | WINDOW_STYLE(
                        (ES_MULTILINE | ES_AUTOHSCROLL | ES_AUTOVSCROLL | ES_WANTRETURN) as u32,
                    ),
                canvas.left,
                canvas.top,
                1,
                1,
                Some(owner),
                None,
                Some(hinstance()?),
                None,
            )?
        };
        let mut input = Self {
            hwnd,
            canvas,
            toolbar,
            anchor,
            field: Rect::default(),
            style: None,
            font: None,
            dwrite,
            format: None,
            min_width: None,
            content: TextExtent::default(),
        };
        // SAFETY: the owner remains live until the child is destroyed on this thread.
        unsafe {
            if !SetWindowSubclass(hwnd, Some(edit_proc), EDIT_SUBCLASS, owner.0 as usize).as_bool()
            {
                anyhow::bail!("could not install annotation text controls");
            }
            SendMessageW(hwnd, EM_SETLIMITTEXT, Some(WPARAM(MAX_TEXT_UNITS)), None);
        }
        input.set_style(style)?;
        // SAFETY: the edit window is fully styled and owned by this UI thread.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }
        input.focus();
        Ok(input)
    }

    pub(super) fn set_style(&mut self, style: TextStyle) -> anyhow::Result<()> {
        if self.style == Some(style) {
            return Ok(());
        }
        let size_changed = self.style.is_none_or(|old| old.size_px != style.size_px);
        let font = if size_changed {
            Some(EditFont::new(style.size_px)?)
        } else {
            None
        };
        let format = if size_changed {
            Some(text_format(&self.dwrite, style.size_px)?)
        } else {
            None
        };
        let content = if let Some(format) = &format {
            self.measure(format, style.size_px)?
        } else {
            self.content
        };
        let previous_color = self.style.map(|current| color_ref(current.rgb));
        let color = color_ref(style.rgb);
        // SAFETY: the native edit window and any font belong to this thread.
        // The property stores a nonzero integer cookie, never a dereferenced pointer.
        unsafe {
            if previous_color != Some(color) {
                let old_key = previous_color.map(key_color);
                let key = key_color(color);
                SetPropW(self.hwnd, EDIT_COLOR, Some(color_cookie(color)))?;
                if old_key != Some(key) {
                    if let Err(err) = SetLayeredWindowAttributes(self.hwnd, key, 255, LWA_COLORKEY)
                    {
                        if let Some(previous) = previous_color {
                            let _ = SetPropW(self.hwnd, EDIT_COLOR, Some(color_cookie(previous)));
                        }
                        return Err(err.into());
                    }
                }
            }
        }
        self.apply_extent(content)?;
        // SAFETY: the edit control and font are live on this UI thread.
        unsafe {
            if let Some(font) = &font {
                SendMessageW(
                    self.hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font.0.0 as usize)),
                    Some(LPARAM(0)),
                );
            }
            let _ = InvalidateRect(Some(self.hwnd), None, true);
        }
        if font.is_some() {
            self.font = font;
        }
        if format.is_some() {
            self.format = format;
        }
        self.style = Some(style);
        Ok(())
    }

    fn measure(&self, format: &IDWriteTextFormat, size: u16) -> windows::core::Result<TextExtent> {
        let mut buffer = [0; MAX_TEXT_UNITS + 1];
        let units = self.read_units(&mut buffer);
        let mut metrics = DWRITE_TEXT_METRICS::default();
        // SAFETY: the text buffer and format live through layout and measurement.
        unsafe {
            self.dwrite
                .CreateTextLayout(units, format, 65_536.0, 65_536.0)?
                .GetMetrics(&mut metrics)?;
        }
        // Derive height from explicit lines. The extra leading also
        // accommodates the native EDIT control's font metrics.
        let line_height = (f32::from(size) * 1.38).ceil() as i32;
        Ok(TextExtent {
            width: metrics.widthIncludingTrailingWhitespace.ceil() as i32 + TEXT_PADDING,
            height: line_height * (metrics.lineCount.max(1) as i32) + TEXT_PADDING,
            line_height: line_height + TEXT_PADDING,
        })
    }

    fn apply_extent(&mut self, content: TextExtent) -> windows::core::Result<()> {
        let field = layout(
            self.canvas,
            self.anchor,
            content,
            self.min_width.unwrap_or(AUTO_WIDTH),
            self.toolbar,
        );
        if self.field != field {
            // SAFETY: the edit control is live on this UI thread.
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    None,
                    field.left,
                    field.top,
                    field.width(),
                    field.height(),
                    SWP_NOACTIVATE | SWP_NOZORDER,
                )?;
                if field.width() >= content.width {
                    SendMessageW(
                        self.hwnd,
                        EM_LINESCROLL,
                        Some(WPARAM((-(MAX_TEXT_UNITS as isize)) as usize)),
                        None,
                    );
                }
                SendMessageW(self.hwnd, EM_SCROLLCARET, None, None);
            }
            self.field = field;
        }
        self.content = content;
        Ok(())
    }

    pub(super) fn text_changed(&mut self) -> windows::core::Result<()> {
        if let (Some(format), Some(style)) = (&self.format, self.style) {
            let content = self.measure(format, style.size_px)?;
            self.apply_extent(content)?;
        }
        Ok(())
    }

    pub(super) fn resize(&mut self, width: Option<i32>) -> windows::core::Result<()> {
        let width = width.map(|width| width.max(MIN_WIDTH));
        if width == self.min_width
            || (self.min_width.is_none() && width == Some(self.field.width()))
        {
            return Ok(());
        }
        let previous = std::mem::replace(&mut self.min_width, width);
        if let Err(err) = self.apply_extent(self.content) {
            self.min_width = previous;
            return Err(err);
        }
        Ok(())
    }

    pub(super) fn left(&self) -> i32 {
        self.field.left
    }

    pub(super) fn grip(&self) -> Option<Rect> {
        let width = grip_width(self.canvas);
        if width == 0 {
            return None;
        }
        let height = self.field.height().min(18);
        let top = self.field.top + (self.field.height() - height) / 2;
        Some(Rect::new(
            self.field.right,
            top,
            self.field.right + width,
            top + height,
        ))
    }

    pub(super) fn area(&self) -> Rect {
        self.field.offset(-self.canvas.left, -self.canvas.top)
    }

    pub(super) fn focus(&self) {
        // SAFETY: the edit control is owned by this UI thread.
        unsafe {
            let _ = SetFocus(Some(self.hwnd));
        }
    }

    pub(super) fn forward_press(&self, point: Point, msg: u32, wp: WPARAM) -> bool {
        if !self.contains(point) {
            return false;
        }
        // Color-keyed pixels let mouse messages reach the overlay. Forward
        // these clicks to the edit control so blank space can position the caret.
        let x = (point.x - self.field.left) as u16;
        let y = (point.y - self.field.top) as u16;
        let lp = LPARAM((u32::from(x) | (u32::from(y) << 16)) as isize);
        // SAFETY: the point is inside the edit window and all HWNDs are on the UI thread.
        unsafe {
            SendMessageW(self.hwnd, msg, Some(wp), Some(lp));
        }
        true
    }

    pub(super) fn contains(&self, point: Point) -> bool {
        self.field.contains(point)
    }

    pub(super) fn units(&self) -> Vec<u16> {
        let mut buffer = [0; MAX_TEXT_UNITS + 1];
        self.read_units(&mut buffer).to_vec()
    }

    fn read_units<'a>(&self, buffer: &'a mut [u16; MAX_TEXT_UNITS + 1]) -> &'a [u16] {
        // SAFETY: the live edit control writes at most buffer.len() UTF-16 units.
        let read = unsafe { GetWindowTextW(self.hwnd, buffer) }.max(0) as usize;
        &buffer[..read]
    }
}

fn grip_width(canvas: Rect) -> i32 {
    if canvas.width() >= MIN_WIDTH + GRIP_WIDTH {
        GRIP_WIDTH
    } else {
        0
    }
}

fn layout(
    canvas: Rect,
    anchor: Point,
    content: TextExtent,
    min_width: i32,
    toolbar: Option<Rect>,
) -> Rect {
    let usable_width = canvas.width() - grip_width(canvas);
    let left = anchor.x.min((usable_width - AUTO_WIDTH).max(0));
    let width = content.width.max(min_width).min(usable_width - left);
    let mut bottom = canvas.height();
    let top = anchor.y.min((bottom - content.line_height).max(0));
    // Cap growth above any overlapping toolbar; starting below it is allowed.
    let corridor = Rect::new(left, top, usable_width + grip_width(canvas), bottom)
        .offset(canvas.left, canvas.top);
    if let Some(bar) =
        toolbar.filter(|bar| bar.top > canvas.top && bar.intersect(corridor).is_some())
    {
        bottom = bar.top - canvas.top;
    }
    let top = top.min((bottom - content.line_height).max(0));
    let height = content.height.min(bottom - top);
    Rect::new(left, top, left + width, top + height).offset(canvas.left, canvas.top)
}

struct EditFont(HFONT);

impl EditFont {
    fn new(size: u16) -> windows::core::Result<Self> {
        // SAFETY: the face name is null-terminated. The new font is owned here.
        let font = unsafe {
            CreateFontW(
                -i32::from(size),
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                NONANTIALIASED_QUALITY,
                0,
                w!("Segoe UI"),
            )
        };
        if font.is_invalid() {
            Err(windows::core::Error::from_thread())
        } else {
            Ok(Self(font))
        }
    }
}

impl Drop for EditFont {
    fn drop(&mut self) {
        // SAFETY: the control has either been destroyed or switched to a new font.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0.0));
        }
    }
}

fn color_cookie(color: COLORREF) -> HANDLE {
    HANDLE((color.0 as usize + 1) as *mut _)
}

fn key_color(color: COLORREF) -> COLORREF {
    let key = COLORREF(0x0003_0201);
    if color == key {
        COLORREF(0x0006_0504)
    } else {
        key
    }
}

pub(super) fn edit_colors(edit: HWND, dc: HDC) -> Option<LRESULT> {
    // SAFETY: WM_CTLCOLOREDIT supplies the live child window and its display DC.
    let cookie = unsafe { GetPropW(edit, EDIT_COLOR) }.0 as usize;
    let color = COLORREF(cookie.checked_sub(1)? as u32);
    let key = key_color(color);
    // SAFETY: both stock brush and DC belong to this interaction. The edit
    // paints an opaque key color which Windows removes during composition.
    unsafe {
        SetTextColor(dc, color);
        SetBkColor(dc, key);
        SetBkMode(dc, OPAQUE);
        SetDCBrushColor(dc, key);
        Some(LRESULT(GetStockObject(DC_BRUSH).0 as isize))
    }
}

impl Drop for TextInput {
    fn drop(&mut self) {
        // SAFETY: the child may already have been destroyed with its parent.
        unsafe {
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    parent: usize,
) -> LRESULT {
    // SAFETY: these HWNDs are the child and parent on the same UI thread.
    unsafe {
        if msg == WM_NCDESTROY {
            let _ = RemovePropW(hwnd, EDIT_COLOR);
            let _ = RemoveWindowSubclass(hwnd, Some(edit_proc), id);
        } else if msg == WM_KEYDOWN
            && (wp.0 == VK_ESCAPE.0 as usize
                || (wp.0 == VK_RETURN.0 as usize && GetKeyState(VK_CONTROL.0 as i32) < 0))
        {
            let context = ImmGetContext(hwnd);
            let composing = if context.is_invalid() {
                false
            } else {
                let result = ImmGetCompositionStringW(context, GCS_COMPSTR, None, 0);
                let _ = ImmReleaseContext(hwnd, context);
                result > 0
            };
            if !composing
                && PostMessageW(
                    Some(HWND(parent as _)),
                    WM_TEXT_FINISHED,
                    WPARAM(hwnd.0 as usize),
                    LPARAM(isize::from(wp.0 == VK_RETURN.0 as usize)),
                )
                .is_ok()
            {
                return LRESULT(0);
            }
        } else if msg == WM_CHAR
            && (wp.0 == 27 || wp.0 == 10 || (wp.0 == 13 && GetKeyState(VK_CONTROL.0 as i32) < 0))
        {
            return LRESULT(0);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Gdi::{BLACKNESS, PatBlt};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, GetLayeredWindowAttributes, LAYERED_WINDOW_ATTRIBUTES_FLAGS, MSG,
        PM_REMOVE, PRF_CLIENT, PRF_ERASEBKGND, PeekMessageW, RegisterClassW, SetWindowTextW,
        WINDOW_EX_STYLE, WM_CTLCOLOREDIT, WM_GETFONT, WM_PRINT, WNDCLASSW, WS_POPUP,
    };

    use super::*;

    unsafe extern "system" fn color_host(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_CTLCOLOREDIT {
            if let Some(result) = edit_colors(HWND(lp.0 as *mut _), HDC(wp.0 as *mut _)) {
                return result;
            }
        }
        // SAFETY: forward unhandled messages unchanged.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    #[test]
    fn native_edit_paints_its_blank_area_in_the_transparent_key_color() -> anyhow::Result<()> {
        let instance = hinstance()?;
        let class = w!("RPin.Test.TransparentEdit");
        // SAFETY: the hidden test window uses a registered class on this thread.
        let owner = unsafe {
            let atom = RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(color_host),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            });
            anyhow::ensure!(atom != 0, "could not register test window");
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WS_POPUP,
                0,
                0,
                180,
                80,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        let result = (|| {
            let style = TextStyle::default();
            let area = Rect::new(0, 0, 160, 70);
            let input = TextInput::new(owner, area, Point::default(), style, None)?;
            let [r, g, b, _] = key_color(color_ref(style.rgb)).0.to_le_bytes();
            // SAFETY: PatBlt initializes the full DIB; the edit writes into the same live DC.
            let output = unsafe {
                crate::capture::gdi::render_to_frame(area, |dc| {
                    PatBlt(dc, 0, 0, area.width(), area.height(), BLACKNESS).ok()?;
                    SendMessageW(
                        input.hwnd,
                        WM_PRINT,
                        Some(WPARAM(dc.0 as usize)),
                        Some(LPARAM((PRF_CLIENT | PRF_ERASEBKGND) as isize)),
                    );
                    Ok(())
                })?
            };
            assert_eq!(output.row(15)[70 * 4..70 * 4 + 3], [b, g, r]);
            Ok(())
        })();
        // SAFETY: the test owns the window, and the child is already gone.
        unsafe {
            DestroyWindow(owner)?;
        }
        result
    }

    #[test]
    fn native_edit_reads_unicode_and_routes_ctrl_enter_to_the_overlay() -> anyhow::Result<()> {
        let owner = crate::platform::window::create_hidden_window()?;
        let result = (|| {
            let area = Rect::new(0, 0, 140, 34);
            let mut input =
                TextInput::new(owner, area, Point::default(), TextStyle::default(), None)?;
            let mut message = MSG::default();
            // SAFETY: the test owns the hidden parent and its edit child.
            unsafe {
                SetWindowTextW(input.hwnd, w!("中文 RPin"))?;
                let old_font = SendMessageW(input.hwnd, WM_GETFONT, None, None);
                input.set_style(TextStyle {
                    size_px: 48,
                    rgb: [1, 2, 3],
                })?;
                assert_ne!(SendMessageW(input.hwnd, WM_GETFONT, None, None), old_font);
                let mut key = COLORREF::default();
                let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
                GetLayeredWindowAttributes(input.hwnd, Some(&mut key), None, Some(&mut flags))?;
                assert_eq!(flags, LWA_COLORKEY);
                assert_eq!(key, key_color(color_ref([1, 2, 3])));
                assert_ne!(key, color_ref([1, 2, 3]));
                let mut original = [0; 256];
                GetKeyboardState(&mut original)?;
                let mut ctrl = original;
                ctrl[VK_CONTROL.0 as usize] |= 0x80;
                SetKeyboardState(&ctrl)?;
                SendMessageW(
                    input.hwnd,
                    WM_KEYDOWN,
                    Some(WPARAM(VK_RETURN.0 as usize)),
                    None,
                );
                SetKeyboardState(&original)?;
                assert!(
                    PeekMessageW(
                        &mut message,
                        Some(owner),
                        WM_TEXT_FINISHED,
                        WM_TEXT_FINISHED,
                        PM_REMOVE
                    )
                    .as_bool()
                );
            }
            assert_eq!(String::from_utf16(&input.units())?, "中文 RPin");
            assert_eq!(message.wParam.0, input.hwnd.0 as usize);
            assert_eq!(message.lParam.0, 1);
            Ok(())
        })();
        // SAFETY: the child is gone and the test still owns the parent.
        unsafe {
            DestroyWindow(owner)?;
        }
        result
    }

    #[test]
    fn text_box_grows_by_longest_line_and_respects_manual_width() -> anyhow::Result<()> {
        const EM_GETLINECOUNT: u32 = 0x00BA;
        const EM_SETSEL: u32 = 0x00B1;
        let owner = crate::platform::window::create_hidden_window()?;
        let result = (|| {
            let area = Rect::new(0, 0, 720, 300);
            let mut input =
                TextInput::new(owner, area, Point::new(12, 16), TextStyle::default(), None)?;
            let empty = input.field;
            // SAFETY: the test owns the edit control on this thread.
            unsafe {
                SetWindowTextW(input.hwnd, w!("一行中文字 abcdefghijklmnopqrstuv"))?;
                assert_eq!(SendMessageW(input.hwnd, EM_GETLINECOUNT, None, None).0, 1);
            }
            input.text_changed()?;
            let long = input.field;
            assert!(long.width() > empty.width());
            assert_eq!(long.height(), empty.height());
            assert_eq!((long.left, long.top), (empty.left, empty.top));
            let minimum = long.width() + 32;
            input.resize(Some(minimum))?;
            assert_eq!(input.field.width(), minimum);
            // SAFETY: place the caret at the end, then send a native Enter character.
            unsafe {
                let end = input.units().len();
                SendMessageW(
                    input.hwnd,
                    EM_SETSEL,
                    Some(WPARAM(end)),
                    Some(LPARAM(end as isize)),
                );
                SendMessageW(input.hwnd, WM_CHAR, Some(WPARAM(13)), None);
                assert_eq!(SendMessageW(input.hwnd, EM_GETLINECOUNT, None, None).0, 2);
            }
            input.text_changed()?;
            assert_eq!(input.field.width(), minimum);
            assert!(input.field.height() > long.height());
            assert_eq!(input.field.top, long.top);
            assert!(String::from_utf16(&input.units())?.ends_with("\r\n"));
            input.resize(None)?;
            assert_eq!(input.field.width(), long.width());
            Ok(())
        })();
        // SAFETY: the edit child is gone and the test still owns the parent.
        unsafe {
            DestroyWindow(owner)?;
        }
        result
    }
}

use std::cell::{OnceCell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    COLORREF, D2DERR_RECREATE_TARGET, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct2D::ID2D1Factory;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, HDC, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetKeyState, ReleaseCapture, SetCapture, SetFocus, VIRTUAL_KEY, VK_CONTROL,
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_S, VK_SHIFT, VK_T, VK_UP, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_IBEAM,
    IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IsWindow, KillTimer,
    LoadCursorW, MA_ACTIVATE, PostMessageW, RegisterClassW, SW_SHOWNOACTIVATE, SetCursor,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow, WM_ACTIVATE, WM_APP,
    WM_CANCELMODE, WM_CAPTURECHANGED, WM_CLOSE, WM_COMMAND, WM_CTLCOLOREDIT, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WM_TIMER,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::annotation::{Document, Tool};
use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};
use crate::config::UiConfig;
use crate::output::{OutputAction, SelectionOutcome};
use crate::platform::window::{default_proc, hinstance};

use super::input::{Handle, Hit};
use super::interaction::{Change, Interaction};
use super::render::Renderer;
use super::scene::{Scene, ToolbarButton, hit_test};
use super::style::{choose_color, choose_size, choose_width};
use super::text::{EN_CHANGE, TextInput, WM_TEXT_CHANGED, WM_TEXT_FINISHED, edit_colors};

pub const WM_SELECTION_FINISHED: u32 = WM_APP + 3;
const CLASS: PCWSTR = w!("RPin.SelectionOverlay");
const ACTIVATION_TIMER: usize = 1;
const ACTIVATION_GRACE: Duration = Duration::from_millis(100);

thread_local! {
    static CLASS_INSTANCE: OnceCell<HINSTANCE> = const { OnceCell::new() };
}

pub struct Overlay {
    hwnd: HWND,
    session: Rc<RefCell<Session>>,
}

pub(crate) struct SelectionResult {
    pub outcome: SelectionOutcome,
    pub frames: Vec<CapturedMonitor>,
    pub annotations: Document,
}

struct Session {
    owner: HWND,
    frames: Vec<CapturedMonitor>,
    bounds: Rect,
    ui: UiConfig,
    interaction: Interaction,
    text: Option<TextInput>,
    custom_colors: [COLORREF; 16],
    scene: Scene,
    renderer: Option<Renderer>,
    factory: ID2D1Factory,
    recovering: bool,
    outcome: Option<anyhow::Result<SelectionOutcome>>,
    shown: Instant,
    first_paint: Option<Instant>,
    drag_frames: u32,
    drag_paint_time: Duration,
    slowest_drag_paint: Duration,
}

impl Overlay {
    pub fn show(
        owner: HWND,
        frames: Vec<CapturedMonitor>,
        ui: UiConfig,
        default_action: OutputAction,
        started: Instant,
        factory: ID2D1Factory,
    ) -> anyhow::Result<Self> {
        let bounds = frames
            .iter()
            .map(|m| m.info.rect)
            .reduce(|a, b| a.union(b))
            .filter(|r| !r.is_empty())
            .ok_or_else(|| anyhow::anyhow!("no monitors to select"))?;
        anyhow::ensure!(
            bounds.width() <= 16_384 && bounds.height() <= 16_384,
            "virtual desktop exceeds the overlay limit (16384px)"
        );
        let cursor = cursor_position().unwrap_or(Point::new(bounds.left, bounds.top));
        let interaction = Interaction::new(cursor, default_action);
        let scene = interaction.scene(&frames, &ui);
        let instance = register_class()?;
        // SAFETY: the class is registered and the top-level window uses
        // virtual-desktop physical pixels under the Per-Monitor V2 manifest.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS,
                w!("RPin"),
                WS_POPUP,
                bounds.left,
                bounds.top,
                bounds.width(),
                bounds.height(),
                None,
                None,
                Some(instance),
                None,
            )?
        };
        let renderer = match Renderer::new(&factory, hwnd, bounds, &frames, &ui) {
            Ok(renderer) => renderer,
            Err(err) => {
                // SAFETY: the window was created above and has no session attached.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(err);
            }
        };
        let session = Rc::new(RefCell::new(Session {
            owner,
            frames,
            bounds,
            ui,
            scene,
            interaction,
            text: None,
            custom_colors: [COLORREF(0x00ff_ffff); 16],
            renderer: Some(renderer),
            factory,
            recovering: false,
            outcome: None,
            shown: Instant::now(),
            first_paint: Some(started),
            drag_frames: 0,
            drag_paint_time: Duration::ZERO,
            slowest_drag_paint: Duration::ZERO,
        }));
        let raw = Rc::into_raw(Rc::clone(&session));
        // SAFETY: the window keeps a strong reference until it detaches.
        // No session borrow is held while showing or activating the window.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != raw as isize {
                let err = windows::core::Error::from_thread();
                drop(Rc::from_raw(raw));
                let _ = DestroyWindow(hwnd);
                return Err(err.into());
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        activate(hwnd);
        Ok(Self { hwnd, session })
    }

    pub fn take_outcome(&mut self) -> Option<anyhow::Result<SelectionResult>> {
        let mut session = self.session.try_borrow_mut().ok()?;
        let outcome = session.outcome.take()?;
        Some(outcome.map(|outcome| {
            let frames = if matches!(outcome, SelectionOutcome::Confirmed { .. }) {
                std::mem::take(&mut session.frames)
            } else {
                Vec::new()
            };
            SelectionResult {
                outcome,
                frames,
                annotations: session.interaction.take_document(),
            }
        }))
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: this HWND is used only on the owning UI thread. Detach before
        // APIs that can synchronously dispatch more messages.
        unsafe {
            let attached = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA);
            let session = Rc::as_ptr(&self.session) as isize;
            if attached == session {
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                drop(Rc::from_raw(attached as *const RefCell<Session>));
                if let Ok(mut state) = self.session.try_borrow_mut() {
                    state.text.take();
                    state.renderer.take();
                    if state.drag_frames > 0 {
                        crate::logging::debug(format!(
                            "selection drag paint: frames={}, mean_ms={:.2}, max_ms={:.2}",
                            state.drag_frames,
                            state.drag_paint_time.as_secs_f64() * 1000.0
                                / f64::from(state.drag_frames),
                            state.slowest_drag_paint.as_secs_f64() * 1000.0,
                        ));
                    }
                }
                if GetCapture() == self.hwnd {
                    let _ = ReleaseCapture();
                }
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Session {
    fn finish(&mut self, result: anyhow::Result<SelectionOutcome>) {
        if self.outcome.is_none() {
            self.outcome = Some(result);
            // SAFETY: queue a notification without borrowing the owner or
            // passing a pointer across the message queue.
            if let Err(err) = unsafe {
                PostMessageW(
                    Some(self.owner),
                    WM_SELECTION_FINISHED,
                    WPARAM(0),
                    LPARAM(0),
                )
            } {
                crate::logging::error(format!("could not notify selection owner: {err}"));
            }
        }
    }

    fn refresh(&mut self, hwnd: HWND) {
        let mut next = self.interaction.scene(&self.frames, &self.ui);
        next.text_grip = self
            .text
            .as_ref()
            .and_then(TextInput::grip)
            .map(|grip| grip.offset(self.bounds.left, self.bounds.top));
        if let Some(damage) = next.damage_from(&self.scene, &self.frames) {
            self.invalidate(hwnd, damage);
        }
        self.scene = next;
    }

    fn invalidate(&self, hwnd: HWND, damage: Rect) {
        if let Some(damage) = damage.intersect(self.bounds) {
            let area = damage.offset(-self.bounds.left, -self.bounds.top);
            let area = RECT {
                left: area.left,
                top: area.top,
                right: area.right,
                bottom: area.bottom,
            };
            // SAFETY: invalidate only the local client rect that changed.
            unsafe {
                let _ = InvalidateRect(Some(hwnd), Some(&area), false);
            }
        }
    }

    fn invalidate_annotation(&self, hwnd: HWND, local: Option<Rect>) {
        if let (Some(local), Some(selection)) = (local, self.interaction.selection()) {
            if let Some(rect) = local
                .offset(selection.left, selection.top)
                .intersect(selection)
            {
                self.invalidate(hwnd, rect);
            }
        }
    }

    fn paint(&mut self, hwnd: HWND) {
        let started = (self.interaction.dragging_canvas()
            && crate::logging::current_level() >= crate::logging::Level::Debug)
            .then(Instant::now);
        let paint = match Paint::begin(hwnd) {
            Ok(paint) => paint,
            Err(err) => return self.finish(Err(err.into())),
        };
        let area = paint.info.rcPaint;
        let result = self.draw(hwnd, area);
        drop(paint);
        match result {
            Ok(()) => {
                self.recovering = false;
                if area.left < area.right && area.top < area.bottom {
                    if let Some(started) = started {
                        let duration = started.elapsed();
                        self.drag_frames += 1;
                        self.drag_paint_time += duration;
                        self.slowest_drag_paint = self.slowest_drag_paint.max(duration);
                    }
                    if let Some(started) = self.first_paint.take() {
                        crate::logging::info(format!(
                            "selection first paint: {:.2} ms",
                            started.elapsed().as_secs_f64() * 1000.0
                        ));
                    }
                }
            }
            Err(err)
                if !self.recovering
                    && err
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|err| err.code() == D2DERR_RECREATE_TARGET) =>
            {
                self.renderer = None;
                self.recovering = true;
                // SAFETY: a replacement target needs the entire window redrawn.
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            Err(err) => self.finish(Err(err)),
        }
    }

    fn draw(&mut self, hwnd: HWND, dirty: RECT) -> anyhow::Result<()> {
        let dirty = if self.renderer.is_none() {
            self.renderer = Some(Renderer::new(
                &self.factory,
                hwnd,
                self.bounds,
                &self.frames,
                &self.ui,
            )?);
            self.bounds
        } else {
            Rect::new(dirty.left, dirty.top, dirty.right, dirty.bottom)
                .offset(self.bounds.left, self.bounds.top)
        };
        if let Some(renderer) = &mut self.renderer {
            renderer.paint(
                dirty,
                &self.frames,
                &self.scene,
                Some((&self.factory, self.interaction.editor())),
            )?;
        }
        Ok(())
    }

    fn mouse_point(&self, value: LPARAM) -> Point {
        // Sign-extend each coordinate (GET_X/Y_LPARAM) for captured moves
        // outside the client area, then restore the virtual origin.
        Point::new(
            self.bounds.left + value.0 as i16 as i32,
            self.bounds.top + (value.0 >> 16) as i16 as i32,
        )
    }

    fn key(&mut self, hwnd: HWND, key: VIRTUAL_KEY) {
        // SAFETY: GetKeyState returns the modifier state for this UI queue.
        let (ctrl, shift) = unsafe {
            (
                GetKeyState(VK_CONTROL.0 as i32) < 0,
                GetKeyState(VK_SHIFT.0 as i32) < 0,
            )
        };
        let step = if shift { 10 } else { 1 };
        let change = match key {
            VK_ESCAPE => {
                let (change, release) = self.interaction.escape();
                if release {
                    self.release_mouse(hwnd);
                }
                change
            }
            VK_RETURN => self.interaction.confirm_default(),
            VK_S if ctrl => self.interaction.confirm(OutputAction::SaveAs),
            VK_T if ctrl => self.interaction.confirm(OutputAction::PinAndCopy),
            VK_T => self.interaction.confirm(OutputAction::Pin),
            VK_Z if ctrl => return self.history(hwnd, shift),
            VK_Y if ctrl => return self.history(hwnd, true),
            VK_LEFT => self
                .interaction
                .nudge(self.bounds, Point::new(-step, 0), ctrl),
            VK_RIGHT => self
                .interaction
                .nudge(self.bounds, Point::new(step, 0), ctrl),
            VK_UP => self
                .interaction
                .nudge(self.bounds, Point::new(0, -step), ctrl),
            VK_DOWN => self
                .interaction
                .nudge(self.bounds, Point::new(0, step), ctrl),
            _ => Change::None,
        };
        self.apply_change(hwnd, change);
    }

    fn history(&mut self, hwnd: HWND, redo: bool) {
        self.finish_text(hwnd, true);
        let change = self.interaction.history(redo);
        self.apply_change(hwnd, change);
    }

    fn release_mouse(&self, hwnd: HWND) {
        // SAFETY: release only the capture owned by this window on this thread.
        unsafe {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
        }
    }

    fn open_text(&mut self, hwnd: HWND, selection: Rect, local: Point) {
        let canvas = selection.offset(-self.bounds.left, -self.bounds.top);
        let toolbar = self
            .scene
            .toolbar
            .map(|bar| bar.rect.offset(-self.bounds.left, -self.bounds.top));
        let style = self.interaction.editor().text_style;
        match TextInput::new(hwnd, canvas, local, style, toolbar) {
            Ok(input) => self.text = Some(input),
            Err(err) => {
                crate::logging::warn(format!("could not open annotation text input: {err:#}"))
            }
        }
    }

    fn finish_text(&mut self, hwnd: HWND, commit: bool) {
        let Some(mut text) = self.text.take() else {
            return;
        };
        if self.interaction.end_text_resize() {
            self.release_mouse(hwnd);
        }
        let damage = if commit {
            if let Err(err) = text.text_changed() {
                crate::logging::warn(format!("could not measure final annotation text: {err:#}"));
            }
            self.interaction.add_text(text.area(), text.units())
        } else {
            None
        };
        drop(text);
        // SAFETY: focus returns to the live parent after closing its edit child.
        unsafe {
            let _ = SetFocus(Some(hwnd));
        }
        self.invalidate_annotation(hwnd, damage);
        self.refresh(hwnd);
    }

    fn press_text_grip(&mut self, hwnd: HWND, point: Point, reset: bool) -> bool {
        let Some(grip) = self.scene.text_grip.filter(|grip| grip.contains(point)) else {
            return false;
        };
        let Some(input) = &mut self.text else {
            return false;
        };
        if reset {
            if let Err(err) = input.resize(None) {
                crate::logging::warn(format!("could not reset annotation text width: {err:#}"));
            }
            self.refresh(hwnd);
        } else {
            if capture_mouse(hwnd) {
                self.interaction.begin_text_resize(
                    point.x,
                    grip.left,
                    self.bounds.left + input.left(),
                );
            }
        }
        true
    }

    fn style_option(&mut self, hwnd: HWND, button: ToolbarButton, point: Point) {
        let editor = self.interaction.editor();
        let mut text_style = editor.text_style;
        let mut stroke_style = editor.stroke_style;
        let editing_text = editor.tool == Some(Tool::Text);
        match button {
            ToolbarButton::TextSize => match choose_size(hwnd, point, text_style.size_px) {
                Ok(Some(size)) => text_style.size_px = size,
                Ok(None) => {}
                Err(err) => {
                    crate::logging::warn(format!("could not open annotation font sizes: {err:#}"))
                }
            },
            ToolbarButton::StrokeWidth => match choose_width(hwnd, point, stroke_style.width) {
                Ok(Some(width)) => stroke_style.width = width,
                Ok(None) => {}
                Err(err) => crate::logging::warn(format!(
                    "could not open annotation stroke widths: {err:#}"
                )),
            },
            ToolbarButton::Color => {
                let current = if editing_text {
                    &mut text_style.rgb
                } else {
                    &mut stroke_style.rgb
                };
                if let Some(rgb) = choose_color(hwnd, *current, &mut self.custom_colors) {
                    *current = rgb;
                }
            }
            _ => return,
        }
        // SAFETY: the native menus run a nested message loop; validate the owner afterward.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return;
        }
        if let Some(input) = &mut self.text {
            if let Err(err) = input.set_style(text_style) {
                crate::logging::warn(format!("could not apply annotation text style: {err:#}"));
                text_style = self.interaction.editor().text_style;
            }
            input.focus();
        }
        self.interaction.set_styles(text_style, stroke_style);
        self.refresh(hwnd);
    }

    fn forward_text_press(&self, point: Point, msg: u32, wp: WPARAM) -> bool {
        self.text.as_ref().is_some_and(|input| {
            input.forward_press(
                Point::new(point.x - self.bounds.left, point.y - self.bounds.top),
                msg,
                wp,
            )
        })
    }

    fn press(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, point: Point) {
        if !self.interaction.idle() {
            return;
        }
        let double_click = msg == WM_LBUTTONDBLCLK;
        let change = if let Some(bar) = self.scene.toolbar.filter(|bar| bar.rect.contains(point)) {
            // The toolbar consumes presses on its padding and disabled buttons.
            self.interaction
                .press_toolbar(point, bar.button_at(point), || capture_mouse(hwnd))
        } else if self.press_text_grip(hwnd, point, double_click)
            || self.forward_text_press(point, msg, wp)
        {
            return;
        } else {
            self.finish_text(hwnd, true);
            self.interaction
                .press_canvas(point, double_click, self.bounds, &self.frames, || {
                    capture_mouse(hwnd)
                })
        };
        self.apply_change(hwnd, change);
    }

    fn click_toolbar(&mut self, hwnd: HWND, button: ToolbarButton) {
        if !button.is_style_option() {
            self.finish_text(hwnd, true);
        }
        let change = match button {
            ToolbarButton::Copy => self.interaction.confirm(OutputAction::Copy),
            ToolbarButton::Save => self.interaction.confirm(OutputAction::SaveAs),
            ToolbarButton::Pin => self.interaction.confirm(OutputAction::Pin),
            ToolbarButton::Undo => return self.history(hwnd, false),
            ToolbarButton::Redo => return self.history(hwnd, true),
            ToolbarButton::TextSize | ToolbarButton::StrokeWidth | ToolbarButton::Color => {
                return self.style_option(hwnd, button, self.interaction.cursor());
            }
            ToolbarButton::Tool(tool) => {
                self.interaction.toggle_tool(tool);
                Change::Refresh
            }
        };
        self.apply_change(hwnd, change);
    }

    fn apply_change(&mut self, hwnd: HWND, change: Change) {
        match change {
            Change::None => return,
            Change::Refresh => {}
            Change::Annotation(damage) => self.invalidate_annotation(hwnd, damage),
            Change::OpenText { selection, local } => self.open_text(hwnd, selection, local),
            Change::ResizeText { width, focus } => {
                if let Some(input) = &mut self.text {
                    if let Err(err) = input.resize(Some(width)) {
                        crate::logging::warn(format!("could not resize annotation text: {err:#}"));
                    }
                    if focus {
                        input.focus();
                    }
                }
            }
            Change::ClickToolbar(button) => return self.click_toolbar(hwnd, button),
            Change::Finish(outcome) => return self.finish(Ok(outcome)),
        }
        self.refresh(hwnd);
    }

    fn message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if self.outcome.is_some() && msg != WM_PAINT {
            return Some(LRESULT(0));
        }
        match msg {
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_ACTIVATE as isize)),
            WM_PAINT => self.paint(hwnd),
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                self.press(hwnd, msg, wp, self.mouse_point(lp));
            }
            WM_MOUSEMOVE => {
                let change = self
                    .interaction
                    .move_pointer(self.mouse_point(lp), self.bounds);
                self.apply_change(hwnd, change);
            }
            WM_LBUTTONUP => {
                let change = self.interaction.release_pointer(
                    self.mouse_point(lp),
                    self.bounds,
                    &self.frames,
                    &self.scene,
                )?;
                self.release_mouse(hwnd);
                self.apply_change(hwnd, change);
            }
            WM_SETCURSOR => {
                let cursor = cursor_position().unwrap_or(self.interaction.cursor());
                let system_cursor = if self.interaction.resizing_text()
                    || self
                        .scene
                        .text_grip
                        .is_some_and(|grip| grip.contains(cursor))
                {
                    IDC_SIZEWE
                } else if let Some(toolbar) =
                    self.scene.toolbar.filter(|bar| bar.rect.contains(cursor))
                {
                    if toolbar.button_at(cursor).is_some() {
                        IDC_HAND
                    } else {
                        IDC_ARROW
                    }
                } else if self.text.as_ref().is_some_and(|input| {
                    input.contains(Point::new(
                        cursor.x - self.bounds.left,
                        cursor.y - self.bounds.top,
                    ))
                }) {
                    IDC_IBEAM
                } else if self.interaction.editor().locks_selection() {
                    if self.interaction.editor().tool.is_some()
                        && self
                            .interaction
                            .selection()
                            .is_some_and(|rect| rect.contains(cursor))
                    {
                        IDC_CROSS
                    } else {
                        IDC_ARROW
                    }
                } else {
                    cursor_for(hit_test(&self.frames, self.interaction.selection(), cursor))
                };
                // SAFETY: LoadCursorW loads a shared system cursor.
                unsafe {
                    if let Ok(cursor) = LoadCursorW(None, system_cursor) {
                        SetCursor(Some(cursor));
                    }
                }
                return Some(LRESULT(1));
            }
            WM_KEYDOWN => self.key(hwnd, VIRTUAL_KEY(wp.0 as u16)),
            WM_TEXT_CHANGED => {
                if let Some(input) = self
                    .text
                    .as_mut()
                    .filter(|input| input.hwnd.0 as usize == wp.0)
                {
                    if let Err(err) = input.text_changed() {
                        crate::logging::warn(format!("could not grow annotation text: {err:#}"));
                    }
                    self.refresh(hwnd);
                }
            }
            WM_TEXT_FINISHED => {
                if self
                    .text
                    .as_ref()
                    .is_some_and(|text| text.hwnd.0 as usize == wp.0)
                {
                    self.finish_text(hwnd, lp.0 != 0);
                }
            }
            WM_RBUTTONDOWN | WM_DISPLAYCHANGE | WM_CLOSE | WM_CANCELMODE => {
                let change = self.interaction.cancel();
                self.apply_change(hwnd, change);
            }
            WM_CAPTURECHANGED => {
                let change = self.interaction.lose_capture()?;
                self.apply_change(hwnd, change);
            }
            WM_DPICHANGED => {
                crate::logging::debug("overlay retained physical coordinates after WM_DPICHANGED")
            }
            WM_ACTIVATE if wp.0 as u16 == 0 => {
                if self.shown.elapsed() < ACTIVATION_GRACE {
                    // SAFETY: window timer is scoped to the overlay HWND.
                    if unsafe {
                        SetTimer(
                            Some(hwnd),
                            ACTIVATION_TIMER,
                            ACTIVATION_GRACE.as_millis() as u32,
                            None,
                        )
                    } == 0
                    {
                        self.finish(Ok(SelectionOutcome::Cancelled));
                    }
                } else {
                    self.finish(Ok(SelectionOutcome::Cancelled));
                }
            }
            WM_TIMER if wp.0 == ACTIVATION_TIMER => {
                // SAFETY: cancel our timer and check actual activation.
                unsafe {
                    let _ = KillTimer(Some(hwnd), ACTIVATION_TIMER);
                    if GetForegroundWindow() != hwnd {
                        self.finish(Ok(SelectionOutcome::Cancelled));
                    }
                }
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }
}

struct Paint {
    hwnd: HWND,
    info: PAINTSTRUCT,
}

impl Paint {
    fn begin(hwnd: HWND) -> windows::core::Result<Self> {
        let mut info = PAINTSTRUCT::default();
        // SAFETY: called from WM_PAINT with a live window and writable info.
        if unsafe { BeginPaint(hwnd, &mut info) }.is_invalid() {
            return Err(windows::core::Error::from_thread());
        }
        Ok(Self { hwnd, info })
    }
}

impl Drop for Paint {
    fn drop(&mut self) {
        // SAFETY: this guard pairs a successful BeginPaint on the same thread.
        let _ = unsafe { EndPaint(self.hwnd, &self.info) };
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // An edit can ask for colors synchronously while its session is borrowed.
    if msg == WM_CTLCOLOREDIT {
        if let Some(result) = edit_colors(HWND(lp.0 as *mut _), HDC(wp.0 as *mut _)) {
            return result;
        }
    }
    if msg == WM_COMMAND && ((wp.0 >> 16) & 0xffff) == EN_CHANGE && lp.0 != 0 {
        // SAFETY: defer sizing until the native edit finishes processing the input.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_TEXT_CHANGED,
                WPARAM(lp.0 as usize),
                LPARAM(0),
            )
        };
        return LRESULT(0);
    }
    // SAFETY: the pointer, if attached, belongs to the live Overlay.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Session>;
    if raw.is_null() {
        return default_proc(hwnd, msg, wp, lp);
    }
    // SAFETY: the attached pointer came from Rc::into_raw on this UI thread.
    // Hold a strong reference across callbacks that might close the overlay.
    let session = unsafe {
        Rc::increment_strong_count(raw);
        Rc::from_raw(raw)
    };
    if msg == WM_NCDESTROY {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Rc::from_raw(raw));
        }
        if let Ok(mut state) = session.try_borrow_mut() {
            state.finish(Ok(SelectionOutcome::Cancelled));
        }
        return default_proc(hwnd, msg, wp, lp);
    }
    match catch_unwind(AssertUnwindSafe(|| {
        session
            .try_borrow_mut()
            .ok()
            .and_then(|mut state| state.message(hwnd, msg, wp, lp))
    })) {
        Ok(Some(result)) => result,
        Ok(None) => default_proc(hwnd, msg, wp, lp),
        Err(_) => {
            if let Ok(mut state) = session.try_borrow_mut() {
                state.finish(Err(anyhow::anyhow!("selection window callback failed")));
            }
            LRESULT(0)
        }
    }
}

fn register_class() -> anyhow::Result<HINSTANCE> {
    CLASS_INSTANCE.with(|cell| {
        if let Some(instance) = cell.get() {
            return Ok(*instance);
        }
        let instance = hinstance()?;
        let wc = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: window class name and procedure have static lifetimes.
        if unsafe { RegisterClassW(&wc) } == 0 {
            return Err(windows::core::Error::from_thread().into());
        }
        let _ = cell.set(instance);
        Ok(instance)
    })
}

fn cursor_position() -> Option<Point> {
    let mut cursor = POINT::default();
    // SAFETY: cursor points to writable stack storage.
    unsafe { GetCursorPos(&mut cursor) }.ok()?;
    Some(Point::new(cursor.x, cursor.y))
}

fn capture_mouse(hwnd: HWND) -> bool {
    // SAFETY: the overlay requests capture on its owning UI thread.
    unsafe {
        SetCapture(hwnd);
        GetCapture() == hwnd
    }
}

fn cursor_for(hit: Hit) -> PCWSTR {
    use Handle::*;
    match hit {
        Hit::Outside => IDC_CROSS,
        Hit::Inside => IDC_SIZEALL,
        Hit::Handle(North | South) => IDC_SIZENS,
        Hit::Handle(East | West) => IDC_SIZEWE,
        Hit::Handle(NorthWest | SouthEast) => IDC_SIZENWSE,
        Hit::Handle(NorthEast | SouthWest) => IDC_SIZENESW,
    }
}

fn activate(hwnd: HWND) {
    // SAFETY: the overlay is a live window on the current thread. Detach any
    // temporary input queue association before returning to the message pump.
    unsafe {
        if !SetForegroundWindow(hwnd).as_bool() {
            let foreground = GetForegroundWindow();
            let other = GetWindowThreadProcessId(foreground, None);
            let current = GetCurrentThreadId();
            if other != 0 && current != other && AttachThreadInput(current, other, true).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(Some(hwnd));
                let _ = AttachThreadInput(current, other, false);
            }
        }
        if GetForegroundWindow() == hwnd {
            let _ = SetFocus(Some(hwnd));
        } else {
            crate::logging::debug("overlay will activate on mouse click");
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::WindowsAndMessaging::{
        MSG, PM_REMOVE, PeekMessageW, SendMessageW, WINDOW_EX_STYLE, WM_CHAR,
    };

    use super::*;
    use crate::annotation::TextStyle;

    #[test]
    fn native_edit_notifies_overlay_to_resize_after_typing() -> anyhow::Result<()> {
        let instance = register_class()?;
        // SAFETY: the test owns this hidden window on its UI thread.
        let owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!(""),
                WS_POPUP,
                0,
                0,
                420,
                120,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        let result = (|| {
            let input = TextInput::new(
                owner,
                Rect::new(0, 0, 400, 100),
                Point::default(),
                TextStyle::default(),
                None,
            )?;
            let mut message = MSG::default();
            // SAFETY: dispatch a typed character directly to the live child edit.
            unsafe {
                SendMessageW(input.hwnd, WM_CHAR, Some(WPARAM(b'M' as usize)), None);
                assert!(
                    PeekMessageW(
                        &mut message,
                        Some(owner),
                        WM_TEXT_CHANGED,
                        WM_TEXT_CHANGED,
                        PM_REMOVE,
                    )
                    .as_bool()
                );
            }
            assert_eq!(message.wParam.0, input.hwnd.0 as usize);
            assert_eq!(input.units(), [u16::from(b'M')]);
            Ok(())
        })();
        // SAFETY: the edit child is gone and the test still owns the parent.
        unsafe {
            DestroyWindow(owner)?;
        }
        result
    }
}

use std::cell::{OnceCell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    D2DERR_RECREATE_TARGET, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetKeyState, ReleaseCapture, SetCapture, SetFocus, VIRTUAL_KEY, VK_CONTROL,
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_S, VK_SHIFT, VK_T, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_SIZEALL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, KillTimer, LoadCursorW, MA_ACTIVATE,
    PostMessageW, RegisterClassW, SW_SHOWNOACTIVATE, SetCursor, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, ShowWindow, WM_ACTIVATE, WM_APP, WM_CANCELMODE, WM_CAPTURECHANGED, WM_CLOSE,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN,
    WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::capture::{
    CapturedMonitor,
    frame::{Point, Rect},
};
use crate::config::UiConfig;
use crate::output::{OutputAction, SelectionOutcome};
use crate::platform::window::{default_proc, hinstance};

use super::input::{Handle, Hit, Selection};
use super::render::Renderer;
use super::scene::{Scene, ToolbarButton, hit_test};

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

struct Session {
    owner: HWND,
    frames: Vec<CapturedMonitor>,
    bounds: Rect,
    ui: UiConfig,
    selection: Selection,
    default_action: OutputAction,
    pressed_button: Option<ToolbarButton>,
    cursor: Point,
    scene: Scene,
    renderer: Option<Renderer>,
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
        let selection = Selection::default();
        let scene = Scene::new(&frames, &ui, &selection, cursor, None);
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
        let renderer = match Renderer::new(hwnd, bounds, &frames, &ui) {
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
            cursor,
            scene,
            selection,
            default_action,
            pressed_button: None,
            renderer: Some(renderer),
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

    pub fn take_outcome(
        &mut self,
    ) -> Option<anyhow::Result<(SelectionOutcome, Vec<CapturedMonitor>)>> {
        let mut session = self.session.try_borrow_mut().ok()?;
        let outcome = session.outcome.take()?;
        Some(outcome.map(|outcome| {
            let frames = if matches!(outcome, SelectionOutcome::Confirmed { .. }) {
                std::mem::take(&mut session.frames)
            } else {
                Vec::new()
            };
            (outcome, frames)
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

    fn confirm(&mut self, action: OutputAction) {
        if !self.selection.dragging() {
            if let Some(rect) = self.selection.saved_rect() {
                self.finish(Ok(SelectionOutcome::Confirmed { rect, action }));
            }
        }
    }

    fn refresh(&mut self, hwnd: HWND) {
        let next = Scene::new(
            &self.frames,
            &self.ui,
            &self.selection,
            self.cursor,
            self.pressed_button,
        );
        if let Some(damage) = next
            .damage_from(&self.scene, &self.frames)
            .and_then(|rect| rect.intersect(self.bounds))
        {
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
        self.scene = next;
    }

    fn paint(&mut self, hwnd: HWND) {
        let started = (self.selection.dragging()
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
            self.renderer = Some(Renderer::new(hwnd, self.bounds, &self.frames, &self.ui)?);
            self.bounds
        } else {
            Rect::new(dirty.left, dirty.top, dirty.right, dirty.bottom)
                .offset(self.bounds.left, self.bounds.top)
        };
        if let Some(renderer) = &self.renderer {
            renderer.paint(dirty, &self.frames, &self.scene)?;
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
        let delta = match key {
            VK_ESCAPE => return self.finish(Ok(SelectionOutcome::Cancelled)),
            VK_RETURN => return self.confirm(self.default_action),
            VK_S if ctrl => return self.confirm(OutputAction::SaveAs),
            VK_T if ctrl => return self.confirm(OutputAction::PinAndCopy),
            VK_T => return self.confirm(OutputAction::Pin),
            VK_LEFT => Point::new(-step, 0),
            VK_RIGHT => Point::new(step, 0),
            VK_UP => Point::new(0, -step),
            VK_DOWN => Point::new(0, step),
            _ => return,
        };
        self.selection.nudge(self.bounds, delta, ctrl);
        self.refresh(hwnd);
    }

    /// Consume toolbar presses, including its padding, before selection hit testing.
    fn press_toolbar(&mut self, hwnd: HWND, point: Point) -> bool {
        let Some(toolbar) = self.scene.toolbar.filter(|bar| bar.rect.contains(point)) else {
            return false;
        };
        self.cursor = point;
        if let Some(button) = toolbar.button_at(point) {
            // SAFETY: capture mouse-up even if the cursor moves off the button.
            unsafe {
                SetCapture(hwnd);
                if GetCapture() == hwnd {
                    self.pressed_button = Some(button);
                }
            }
        }
        self.refresh(hwnd);
        true
    }

    fn release_toolbar(&mut self, hwnd: HWND, point: Point) {
        let pressed = self.pressed_button.take();
        self.cursor = point;
        let released = self.scene.toolbar.and_then(|bar| bar.button_at(point));
        // SAFETY: clear the pressed state before releasing this window's capture.
        unsafe {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
        }
        match pressed.filter(|button| Some(*button) == released) {
            Some(ToolbarButton::Copy) => self.confirm(OutputAction::Copy),
            Some(ToolbarButton::Save) => self.confirm(OutputAction::SaveAs),
            Some(ToolbarButton::Pin) => self.confirm(OutputAction::Pin),
            None => self.refresh(hwnd),
        }
    }

    fn message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if self.outcome.is_some() && msg != WM_PAINT {
            return Some(LRESULT(0));
        }
        match msg {
            WM_ERASEBKGND => return Some(LRESULT(1)),
            WM_MOUSEACTIVATE => return Some(LRESULT(MA_ACTIVATE as isize)),
            WM_PAINT => self.paint(hwnd),
            WM_LBUTTONDOWN => {
                let point = self.mouse_point(lp);
                if !self.press_toolbar(hwnd, point) {
                    self.cursor = point;
                    let hit = hit_test(&self.frames, self.selection.rect, point);
                    self.selection.begin(point, self.bounds, hit);
                    // SAFETY: acquire mouse capture on this UI thread.
                    unsafe {
                        SetCapture(hwnd);
                    }
                    self.refresh(hwnd);
                }
            }
            WM_MOUSEMOVE => {
                self.cursor = self.mouse_point(lp);
                self.selection.update(self.cursor, self.bounds);
                self.refresh(hwnd);
            }
            WM_LBUTTONUP if self.pressed_button.is_some() => {
                self.release_toolbar(hwnd, self.mouse_point(lp));
            }
            WM_LBUTTONUP if self.selection.dragging() => {
                self.cursor = self.mouse_point(lp);
                self.selection.update(self.cursor, self.bounds);
                let keep = self.selection.end();
                // SAFETY: release capture after ending the drag.
                unsafe {
                    let _ = ReleaseCapture();
                }
                if keep {
                    self.refresh(hwnd);
                } else {
                    self.finish(Ok(SelectionOutcome::Cancelled));
                }
            }
            WM_LBUTTONDBLCLK => {
                let cursor = self.mouse_point(lp);
                if !self.press_toolbar(hwnd, cursor)
                    && self
                        .selection
                        .rect
                        .is_some_and(|rect| rect.contains(cursor))
                {
                    self.confirm(self.default_action);
                }
            }
            WM_SETCURSOR => {
                let cursor = cursor_position().unwrap_or(self.cursor);
                let system_cursor = if let Some(toolbar) =
                    self.scene.toolbar.filter(|bar| bar.rect.contains(cursor))
                {
                    if toolbar.button_at(cursor).is_some() {
                        IDC_HAND
                    } else {
                        IDC_ARROW
                    }
                } else {
                    cursor_for(hit_test(&self.frames, self.selection.rect, cursor))
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
            WM_RBUTTONDOWN | WM_DISPLAYCHANGE | WM_CLOSE | WM_CANCELMODE => {
                self.finish(Ok(SelectionOutcome::Cancelled));
            }
            WM_CAPTURECHANGED if self.selection.dragging() => {
                self.finish(Ok(SelectionOutcome::Cancelled))
            }
            WM_CAPTURECHANGED if self.pressed_button.take().is_some() => self.refresh(hwnd),
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

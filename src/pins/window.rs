use std::{
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};

use anyhow::{Context, ensure};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GWLP_USERDATA, GetCursorPos, GetWindowLongPtrW, GetWindowRect,
    HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IDC_ARROW, IsWindowVisible, LoadCursorW,
    MENU_ITEM_FLAGS, MF_CHECKED, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, WM_CANCELMODE, WM_CAPTURECHANGED, WM_CLOSE, WM_CONTEXTMENU, WM_DPICHANGED,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::{
    capture::frame::{Frame, Point, Rect},
    config::PinsConfig,
    platform::{menu::PopupMenu, window},
};

use super::{PinId, PinSummary, Reservation, layout::Size, surface};

const PIN_CLASS: PCWSTR = w!("RPin.PinWindow");
const RESET_ZOOM: usize = 1;
const TOGGLE_TOPMOST: usize = 2;
const HIDE: usize = 3;
const CLOSE: usize = 4;
const OPACITY_ITEMS: [(usize, u8); 4] = [(10, 100), (11, 75), (12, 50), (13, 25)];

pub(super) struct PinWindow {
    pub id: PinId,
    hwnd: HWND,
    alive: Rc<Cell<bool>>,
    image_width: u32,
    image_height: u32,
    hidden_for_capture: bool,
}

struct WindowData {
    alive: Rc<Cell<bool>>,
    state: RefCell<State>,
}

struct State {
    frame: Frame,
    budget: Reservation,
    displayed: Size,
    opacity: u8,
    topmost: bool,
    drag: Option<Point>,
}

impl PinWindow {
    pub fn create(
        owner: HWND,
        id: PinId,
        frame: Frame,
        at: Point,
        config: &PinsConfig,
        budget: &Rc<Cell<usize>>,
    ) -> anyhow::Result<Self> {
        let native = image_size(&frame)?;
        let work = work_area(at)?;
        let displayed = native.scaled(1.0, work);
        let at = displayed.position(at, work);
        let opacity = (config.default_opacity * 100.0).round().clamp(10.0, 100.0) as u8;
        let reservation =
            Reservation::reserve(budget, frame.pixels().len() + displayed.pixels() * 4)?;
        let instance = window::hinstance()?;
        // SAFETY: the callback and class name are static. Windows copies the class descriptor.
        let hwnd = unsafe {
            let descriptor = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: PIN_CLASS,
                ..Default::default()
            };
            if RegisterClassW(&descriptor) == 0 {
                let err = windows::core::Error::from_thread();
                if err.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
                    return Err(err).context("registering the pin window class");
                }
            }
            let mut style = WS_EX_LAYERED | WS_EX_TOOLWINDOW;
            if config.always_on_top {
                style |= WS_EX_TOPMOST;
            }
            CreateWindowExW(
                style,
                PIN_CLASS,
                w!("RPin 贴图"),
                WS_POPUP,
                at.x,
                at.y,
                displayed.width,
                displayed.height,
                Some(owner),
                None,
                Some(instance),
                None,
            )?
        };
        let pin = Self {
            id,
            hwnd,
            alive: Rc::new(Cell::new(true)),
            image_width: frame.width,
            image_height: frame.height,
            hidden_for_capture: false,
        };
        surface::present(hwnd, &frame, displayed, at, layer_alpha(opacity))?;
        let state = Rc::new(WindowData {
            alive: Rc::clone(&pin.alive),
            state: RefCell::new(State {
                frame,
                budget: reservation,
                displayed,
                opacity,
                topmost: config.always_on_top,
                drag: None,
            }),
        });
        let raw = Rc::into_raw(state);
        // SAFETY: the HWND owns this strong reference until WM_NCDESTROY clears it.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != raw as isize {
                drop(Rc::from_raw(raw));
                anyhow::bail!("attaching pin window state failed");
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(pin)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.get()
    }

    pub fn summary(&self) -> Option<PinSummary> {
        if !self.is_alive() {
            return None;
        }
        // SAFETY: the marker identifies the original live UI-thread window.
        let visible = unsafe { IsWindowVisible(self.hwnd) }.as_bool();
        Some(PinSummary {
            id: self.id,
            image_width: self.image_width,
            image_height: self.image_height,
            visible,
        })
    }

    pub fn hide_for_capture(&mut self) -> bool {
        // SAFETY: only our own visible window needs hiding.
        self.hidden_for_capture =
            self.is_alive() && unsafe { IsWindowVisible(self.hwnd) }.as_bool();
        if self.hidden_for_capture {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.hidden_for_capture
    }

    pub fn restore_after_capture(&mut self) {
        if std::mem::take(&mut self.hidden_for_capture) && self.is_alive() {
            // SAFETY: never activate a pin while returning from a capture.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }

    pub fn reveal(&self) -> anyhow::Result<()> {
        let bounds = window_rect(self.hwnd)?;
        let at = Point::new(bounds.left, bounds.top);
        let size = Size {
            width: bounds.width(),
            height: bounds.height(),
        };
        let at = size.position(at, work_area(at)?);
        // SAFETY: the manager only reveals its own live window.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                at.x,
                at.y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE,
            )?;
            let _ = SetForegroundWindow(self.hwnd);
        }
        Ok(())
    }
}

impl Drop for PinWindow {
    fn drop(&mut self) {
        if self.is_alive() {
            // SAFETY: this guard only destroys the original window it created.
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl State {
    fn present(&mut self, hwnd: HWND, size: Size, at: Point, opacity: u8) -> anyhow::Result<()> {
        self.budget
            .present(self.frame.pixels().len() + size.pixels() * 4, || {
                surface::present(hwnd, &self.frame, size, at, layer_alpha(opacity))
            })?;
        self.displayed = size;
        self.opacity = opacity;
        Ok(())
    }

    fn zoom(&mut self, hwnd: HWND, multiplier: Option<f64>, anchor: Point) -> anyhow::Result<()> {
        let old = window_rect(hwnd)?;
        let native = image_size(&self.frame)?;
        let current = (f64::from(self.displayed.width) / f64::from(native.width))
            .max(f64::from(self.displayed.height) / f64::from(native.height));
        let requested = multiplier.map_or(1.0, |factor| current * factor);
        let work = work_area(anchor)?;
        let size = native.scaled(requested, work);
        if self.displayed != size {
            self.present(
                hwnd,
                size,
                size.zoom_position(old, anchor, work),
                self.opacity,
            )?;
        }
        Ok(())
    }

    fn set_opacity(&mut self, hwnd: HWND, opacity: u8) -> anyhow::Result<()> {
        if self.opacity != opacity {
            let rect = window_rect(hwnd)?;
            self.present(
                hwnd,
                self.displayed,
                Point::new(rect.left, rect.top),
                opacity,
            )?;
        }
        Ok(())
    }

    fn input(&mut self, hwnd: HWND, msg: u32, wp: WPARAM) -> anyhow::Result<bool> {
        match msg {
            WM_LBUTTONDOWN => {
                let at = cursor()?;
                let rect = window_rect(hwnd)?;
                // SAFETY: the pin receives pointer motion until button-up or cancellation.
                unsafe {
                    let _ = SetCapture(hwnd);
                }
                self.drag = Some(Point::new(at.x - rect.left, at.y - rect.top));
            }
            WM_MOUSEMOVE => {
                if let Some(offset) = self.drag {
                    let at = cursor()?;
                    // SAFETY: moving a layered window leaves its existing pixels in place.
                    unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            at.x - offset.x,
                            at.y - offset.y,
                            0,
                            0,
                            SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
                        )?;
                    }
                }
            }
            WM_LBUTTONUP | WM_CANCELMODE => {
                self.drag = None;
                // SAFETY: do not release capture held by a different window.
                unsafe {
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                }
            }
            WM_CAPTURECHANGED => self.drag = None,
            WM_MOUSEWHEEL => {
                let delta = (wp.0 >> 16) as i16;
                if delta != 0 {
                    // SAFETY: reads the modifier state for the UI thread.
                    if unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 {
                        let steps = (f64::from(delta) / 120.0 * 5.0).round() as i16;
                        self.set_opacity(
                            hwnd,
                            (i16::from(self.opacity) + steps).clamp(10, 100) as u8,
                        )?;
                    } else {
                        self.zoom(
                            hwnd,
                            Some(1.15_f64.powf(f64::from(delta) / 120.0)),
                            cursor()?,
                        )?;
                    }
                }
            }
            // Pins use physical pixels even when dragged across a DPI boundary.
            WM_DPICHANGED => {}
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn menu_command(&mut self, hwnd: HWND, command: usize, point: Point) -> anyhow::Result<()> {
        match command {
            RESET_ZOOM => self.zoom(hwnd, None, point)?,
            TOGGLE_TOPMOST => {
                let order = if self.topmost {
                    HWND_NOTOPMOST
                } else {
                    HWND_TOPMOST
                };
                // SAFETY: only the pin's Z order changes.
                unsafe {
                    SetWindowPos(
                        hwnd,
                        Some(order),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE,
                    )?;
                }
                self.topmost = !self.topmost;
            }
            HIDE => unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            },
            choice => {
                if let Some((_, opacity)) = OPACITY_ITEMS.iter().find(|(id, _)| *id == choice) {
                    self.set_opacity(hwnd, *opacity)?;
                }
            }
        }
        Ok(())
    }
}

fn image_size(frame: &Frame) -> anyhow::Result<Size> {
    let size = Size {
        width: i32::try_from(frame.width)?,
        height: i32::try_from(frame.height)?,
    };
    ensure!(size.width > 0 && size.height > 0, "empty pin image");
    Ok(size)
}

fn layer_alpha(opacity: u8) -> u8 {
    ((u16::from(opacity) * 255 + 50) / 100) as u8
}

fn work_area(point: Point) -> anyhow::Result<Rect> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: the point is in screen coordinates. info is writable for the Win32 call.
    unsafe {
        let handle = MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        if !GetMonitorInfoW(handle, &mut info).as_bool() {
            return Err(windows::core::Error::from_thread()).context("getting the pin work area");
        }
    }
    let r = info.rcWork;
    let work = Rect::new(r.left, r.top, r.right, r.bottom);
    ensure!(!work.is_empty(), "empty monitor work area");
    Ok(work)
}

fn window_rect(hwnd: HWND) -> anyhow::Result<Rect> {
    let mut r = RECT::default();
    // SAFETY: r is writable and the HWND is the live pin window.
    unsafe {
        GetWindowRect(hwnd, &mut r)?;
    }
    Ok(Rect::new(r.left, r.top, r.right, r.bottom))
}

fn cursor() -> anyhow::Result<Point> {
    let mut p = POINT::default();
    // SAFETY: p is writable for GetCursorPos.
    unsafe {
        GetCursorPos(&mut p)?;
    }
    Ok(Point::new(p.x, p.y))
}

fn context_menu(hwnd: HWND, data: &WindowData, lp: LPARAM) -> anyhow::Result<()> {
    let snapshot = data.state.try_borrow()?;
    let opacity = snapshot.opacity;
    let topmost = snapshot.topmost;
    drop(snapshot);

    let menu = PopupMenu::new()?;
    menu.item(RESET_ZOOM, "恢复原始大小", MENU_ITEM_FLAGS(0))?;
    menu.item(TOGGLE_TOPMOST, "保持置顶", checked(topmost))?;
    let levels = PopupMenu::new()?;
    for (id, value) in OPACITY_ITEMS {
        levels.item(id, &format!("{value}%"), checked(value == opacity))?;
    }
    menu.submenu(levels, "不透明度")?;
    menu.separator()?;
    menu.item(HIDE, "隐藏贴图", MENU_ITEM_FLAGS(0))?;
    menu.item(CLOSE, "关闭贴图", MENU_ITEM_FLAGS(0))?;
    let point = if lp.0 == -1 {
        let rect = window_rect(hwnd)?;
        Point::new(rect.left + rect.width() / 2, rect.top + rect.height() / 2)
    } else {
        Point::new(lp.0 as i16 as i32, (lp.0 >> 16) as i16 as i32)
    };
    let command = menu.track(
        hwnd,
        POINT {
            x: point.x,
            y: point.y,
        },
    );
    if data.alive.get() {
        if command == CLOSE {
            // SAFETY: a nested menu may have closed this window; the marker was checked.
            unsafe {
                DestroyWindow(hwnd)?;
            }
        } else {
            data.state
                .try_borrow_mut()?
                .menu_command(hwnd, command, point)?;
        }
    }
    Ok(())
}

fn checked(value: bool) -> MENU_ITEM_FLAGS {
    if value {
        MF_CHECKED
    } else {
        MENU_ITEM_FLAGS(0)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let result = catch_unwind(AssertUnwindSafe(|| handle_message(hwnd, msg, wp, lp)));
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => {
            crate::logging::warn(format!("pin window message 0x{msg:04X}: {err:#}"));
            LRESULT(0)
        }
        Err(_) => {
            crate::logging::error(format!("panic processing pin window message 0x{msg:04X}"));
            LRESULT(0)
        }
    }
}

fn handle_message(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> anyhow::Result<LRESULT> {
    // SAFETY: only this thread accesses the HWND and attaches a WindowData pointer.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowData;
    if raw.is_null() {
        return Ok(window::default_proc(hwnd, msg, wp, lp));
    }
    if msg == WM_NCDESTROY {
        // SAFETY: clear the pointer before reclaiming the HWND's unique strong reference.
        let data = unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            Rc::from_raw(raw)
        };
        data.alive.set(false);
        return Ok(window::default_proc(hwnd, msg, wp, lp));
    }
    // SAFETY: a temporary strong reference survives nested destruction (e.g. a popup menu).
    let data = unsafe {
        Rc::increment_strong_count(raw);
        Rc::from_raw(raw)
    };
    if msg == WM_CLOSE || (msg == WM_KEYDOWN && wp.0 == usize::from(VK_ESCAPE.0)) {
        // SAFETY: this destroys the pin while the temporary reference still keeps its data alive.
        unsafe {
            DestroyWindow(hwnd)?;
        }
        return Ok(LRESULT(0));
    }
    if msg == WM_CONTEXTMENU {
        context_menu(hwnd, &data, lp)?;
        return Ok(LRESULT(0));
    }
    let Ok(mut state) = data.state.try_borrow_mut() else {
        return Ok(window::default_proc(hwnd, msg, wp, lp));
    };
    if state.input(hwnd, msg, wp)? {
        Ok(LRESULT(0))
    } else {
        drop(state);
        Ok(window::default_proc(hwnd, msg, wp, lp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pins::NativePinManager;
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

    #[test]
    #[ignore = "requires native layered-window support"]
    fn native_pins_resize_restore_and_release_their_budget() -> anyhow::Result<()> {
        struct Owner(HWND);
        impl Drop for Owner {
            fn drop(&mut self) {
                // SAFETY: this test owns the hidden main window.
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }

        let owner = Owner(window::create_hidden_window()?);
        let mut manager = NativePinManager::new(owner.0);
        let config = PinsConfig::default();
        let image = || Frame::new(50, 30, vec![100; 50 * 30 * 4]).unwrap();
        let first = manager.show(image(), Point::new(30, 30), &config)?;
        let second = manager.show(image(), Point::new(100, 30), &config)?;
        let first_window = manager.windows[0].hwnd;
        let second_window = manager.windows[1].hwnd;
        assert!(manager.summaries().iter().all(|pin| pin.visible));

        // SAFETY: obtain the first live window's data on its owner thread.
        let raw = unsafe { GetWindowLongPtrW(first_window, GWLP_USERDATA) } as *const WindowData;
        assert!(!raw.is_null());
        {
            // SAFETY: no destruction is requested while resizing this live window.
            let mut state = unsafe { &*raw }.state.borrow_mut();
            state.present(
                first_window,
                Size {
                    width: 100,
                    height: 60,
                },
                Point::new(30, 30),
                50,
            )?;
            assert_eq!(state.opacity, 50);
        }
        assert_eq!(window_rect(first_window)?.width(), 100);

        // SAFETY: the second pin is intentionally hidden before a capture.
        unsafe {
            let _ = ShowWindow(second_window, SW_HIDE);
        }
        manager.hide_all_for_capture();
        assert!(manager.summaries().iter().all(|pin| !pin.visible));
        manager.restore_after_capture();
        let list = manager.summaries();
        assert_eq!(list[0].id, first);
        assert!(list[0].visible);
        assert!(!list[1].visible);
        manager.show_existing(second)?;
        assert!(manager.summaries()[1].visible);

        // SAFETY: send the same close message as a user closing the first pin.
        unsafe {
            SendMessageW(first_window, WM_CLOSE, None, None);
        }
        assert_eq!(manager.summaries().len(), 1);
        assert_eq!(manager.pixels.get(), 2 * 50 * 30 * 4);
        manager.close_all();
        assert!(manager.summaries().is_empty());
        assert_eq!(manager.pixels.get(), 0);
        Ok(())
    }
}

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::NIN_SELECT;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, IsWindow, PBT_APMRESUMEAUTOMATIC,
    PostMessageW, PostQuitMessage, SendMessageW, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_COPYDATA,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONUP, WM_NCDESTROY,
    WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER,
};

use crate::logging;
use crate::output::save::WM_SAVE_DONE;
use crate::overlay::{DIRECT2D_IDLE_TIMER, WM_SELECTION_FINISHED};
use crate::platform::{single_instance::read_copy_data, tray::Tray, window::default_proc};

use super::{
    App, Command, HOTKEY_FULLSCREEN, HOTKEY_FULLSCREEN_SAVE, HOTKEY_PIN, HOTKEY_REGION,
    HOTKEY_WINDOW,
};

pub(crate) const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_DISPATCH: u32 = WM_APP + 2;

enum Event {
    Command(Command),
    SelectionFinished,
}

pub(crate) struct WindowState {
    app: RefCell<App>,
    events: RefCell<VecDeque<Event>>,
    dispatching: Cell<bool>,
    exit_code: Cell<i32>,
}

impl WindowState {
    pub(crate) fn new(app: App) -> Self {
        Self {
            app: RefCell::new(app),
            events: RefCell::new(VecDeque::new()),
            dispatching: Cell::new(false),
            exit_code: Cell::new(0),
        }
    }

    pub(crate) fn run_startup(&self, hwnd: HWND, command: Command) {
        if command != Command::Show {
            logging::info(format!(
                "startup command: {}",
                command.ipc_name().unwrap_or("unknown")
            ));
            self.enqueue(Event::Command(command));
            // SAFETY: the main thread has attached this state to hwnd. Dispatch
            // through wndproc so startup uses the same callback and panic handling.
            unsafe { SendMessageW(hwnd, WM_APP_DISPATCH, None, None) };
        }
    }

    fn enqueue(&self, event: Event) {
        self.events.borrow_mut().push_back(event);
    }

    fn wake(hwnd: HWND) -> windows::core::Result<()> {
        // SAFETY: the wake message carries no pointers and targets the main thread.
        unsafe { PostMessageW(Some(hwnd), WM_APP_DISPATCH, WPARAM(0), LPARAM(0)) }
    }

    unsafe fn receive_command(&self, hwnd: HWND, lparam: LPARAM) -> LRESULT {
        // SAFETY: called only for WM_COPYDATA and the payload is read synchronously.
        let Some(command) = (unsafe { read_copy_data(lparam) }) else {
            return LRESULT(0);
        };
        // This callback may run inside a modal loop. Acknowledge the copied
        // command without waiting to borrow App or execute any application work.
        if Self::wake(hwnd).is_err() {
            return LRESULT(0);
        }
        self.enqueue(Event::Command(command));
        LRESULT(1)
    }

    fn handle_message(&self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        let result = match msg {
            WM_COPYDATA => {
                // SAFETY: Windows provides the COPYDATASTRUCT for this callback.
                return unsafe { self.receive_command(hwnd, lp) };
            }
            WM_HOTKEY => {
                if let Some(command) = hotkey_command(wp.0 as i32) {
                    self.enqueue(Event::Command(command));
                }
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let event = lp.0 as u32 & 0xffff;
                let command = match event {
                    NIN_SELECT | WM_LBUTTONUP => Some(Command::Region),
                    WM_RBUTTONUP | WM_CONTEXTMENU if !self.dispatching.get() => {
                        self.tray_command(hwnd)
                    }
                    _ => None,
                };
                if let Some(command) = command {
                    self.enqueue(Event::Command(command));
                }
                LRESULT(0)
            }
            WM_SELECTION_FINISHED => {
                self.enqueue(Event::SelectionFinished);
                LRESULT(0)
            }
            WM_QUERYENDSESSION => return LRESULT(1),
            WM_CLOSE => {
                self.enqueue(Event::Command(Command::Exit));
                LRESULT(0)
            }
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    logging::info("Windows session ending; cleaning up");
                    if let Ok(mut app) = self.app.try_borrow_mut() {
                        app.end_session();
                    } else {
                        self.events
                            .borrow_mut()
                            .push_front(Event::Command(Command::Exit));
                    }
                }
                LRESULT(0)
            }
            WM_APP_DISPATCH | WM_SAVE_DONE => LRESULT(0),
            WM_NCDESTROY => return default_proc(hwnd, msg, wp, lp),
            WM_DESTROY => {
                if let Ok(mut app) = self.app.try_borrow_mut() {
                    app.shutdown();
                }
                logging::info("main window destroyed; exiting");
                // SAFETY: ends the message loop on the owning UI thread.
                unsafe { PostQuitMessage(self.exit_code.get()) };
                return LRESULT(0);
            }
            _ => {
                if self.handle_window_event(msg, wp) {
                    LRESULT(0)
                } else {
                    default_proc(hwnd, msg, wp, lp)
                }
            }
        };
        self.dispatch(hwnd);
        result
    }

    fn tray_command(&self, hwnd: HWND) -> Option<Command> {
        let menu = self.app.try_borrow().ok()?.menu_state();
        let command = Tray::show_menu(hwnd, &menu);
        // SAFETY: the nested menu loop can dispatch an exit and destroy hwnd.
        command.filter(|_| unsafe { IsWindow(Some(hwnd)) }.as_bool())
    }

    fn handle_window_event(&self, msg: u32, wp: WPARAM) -> bool {
        let Ok(mut app) = self.app.try_borrow_mut() else {
            return false;
        };
        if app.taskbar_created_message != 0 && msg == app.taskbar_created_message {
            logging::info("Explorer TaskbarCreated; re-adding tray icon");
            if let Err(err) = app.tray.add() {
                logging::error(format!("re-adding tray icon failed: {err}"));
            }
            return true;
        }
        match msg {
            WM_TIMER if wp.0 == DIRECT2D_IDLE_TIMER => {
                let seconds = app.config.ui.direct2d_idle_seconds;
                app.direct2d.update_timer(seconds);
            }
            WM_POWERBROADCAST if wp.0 == PBT_APMRESUMEAUTOMATIC as usize => {
                app.refresh_after_resume();
            }
            WM_DISPLAYCHANGE => {
                app.close_overlay();
                logging::info("WM_DISPLAYCHANGE received");
            }
            _ => return false,
        }
        true
    }

    fn dispatch(&self, hwnd: HWND) {
        let Some(_dispatch) = DispatchGuard::enter(&self.dispatching) else {
            return;
        };
        // SAFETY: the nested tray menu can have destroyed the main window.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return;
        }
        loop {
            let prompt = {
                let Ok(mut app) = self.app.try_borrow_mut() else {
                    return;
                };
                app.finish_save();
                if app.exit_requested {
                    self.events.borrow_mut().clear();
                    let can_exit = !app.saver.busy();
                    drop(app);
                    if can_exit {
                        // SAFETY: all App borrows ended before native destruction.
                        if let Err(err) = unsafe { DestroyWindow(hwnd) } {
                            logging::error(format!("could not destroy main window: {err}"));
                            unsafe { PostQuitMessage(1) };
                        }
                    }
                    return;
                }
                let event = self.events.borrow_mut().pop_front();
                match event {
                    Some(Event::Command(command)) => app.handle_command(command),
                    Some(Event::SelectionFinished) => app.finish_selection(),
                    None => return,
                }
            };
            if let Some(prompt) = prompt {
                // The dialog can dispatch Win32 messages and receive IPC. Keep the
                // dispatch guard active, but let the modal loop run without an App borrow.
                let path = prompt.choose_path(hwnd);
                // SAFETY: native callbacks may have destroyed the owner during the dialog.
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    return;
                }
                let mut app = self.app.borrow_mut();
                if !app.exit_requested {
                    app.complete_save_prompt(prompt, path);
                }
            }
        }
    }
}

struct DispatchGuard<'a>(&'a Cell<bool>);

impl<'a> DispatchGuard<'a> {
    fn enter(active: &'a Cell<bool>) -> Option<Self> {
        (!active.replace(true)).then(|| Self(active))
    }
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn hotkey_command(id: i32) -> Option<Command> {
    logging::info(format!("WM_HOTKEY id={id}"));
    Some(match id {
        HOTKEY_REGION => Command::Region,
        HOTKEY_FULLSCREEN => Command::Fullscreen,
        HOTKEY_FULLSCREEN_SAVE => Command::FullscreenSave,
        HOTKEY_PIN => Command::NewPin,
        HOTKEY_WINDOW => {
            logging::info("window hotkey reserved for v1.4; ignored");
            return None;
        }
        _ => {
            logging::warn(format!("unknown hotkey id={id}"));
            return None;
        }
    })
}

pub(crate) unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    // SAFETY: main keeps the state alive through the loop and detaches before dropping it.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    let Some(state) = (unsafe { raw.as_ref() }) else {
        return default_proc(hwnd, msg, wp, lp);
    };
    catch_unwind(AssertUnwindSafe(|| state.handle_message(hwnd, msg, wp, lp))).unwrap_or_else(
        |_| {
            logging::error(format!("panic processing message 0x{msg:04X}"));
            state.exit_code.set(1);
            if msg == WM_DESTROY {
                // SAFETY: destruction cannot be retried. End the UI loop with an error.
                unsafe { PostQuitMessage(1) };
            } else {
                state
                    .events
                    .borrow_mut()
                    .push_front(Event::Command(Command::Exit));
                let _ = WindowState::wake(hwnd);
            }
            LRESULT(0)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::platform::window::{attach_app, create_hidden_window, detach_app};
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, WM_QUIT,
    };

    struct TestWindow {
        hwnd: HWND,
        state: Box<WindowState>,
    }

    impl TestWindow {
        fn new() -> anyhow::Result<Self> {
            let hwnd = create_hidden_window()?;
            let state = Box::new(WindowState::new(App::with_tray(
                hwnd,
                Config::default(),
                Tray::inactive(hwnd),
                None,
            )));
            // SAFETY: TestWindow owns the box and detaches it before dropping it.
            unsafe { attach_app(hwnd, &raw const *state) };
            Ok(Self { hwnd, state })
        }

        fn send(&self, msg: u32) -> LRESULT {
            // SAFETY: the test owns the hidden window on this thread.
            unsafe { SendMessageW(self.hwnd, msg, None, None) }
        }

        fn send_command(&self, name: &str) -> LRESULT {
            let data = COPYDATASTRUCT {
                dwData: 1,
                cbData: name.len() as u32,
                lpData: name.as_ptr() as *mut _,
            };
            // SAFETY: the payload and its bytes live through the synchronous send.
            unsafe {
                SendMessageW(
                    self.hwnd,
                    WM_COPYDATA,
                    None,
                    Some(LPARAM((&raw const data) as isize)),
                )
            }
        }

        fn is_live(&self) -> bool {
            // SAFETY: IsWindow accepts stale HWNDs.
            unsafe { IsWindow(Some(self.hwnd)) }.as_bool()
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            if self.is_live() {
                detach_app(self.hwnd);
                // SAFETY: the test owns this hidden window and has detached its state.
                let _ = unsafe { DestroyWindow(self.hwnd) };
            }
            let mut quit = MSG::default();
            // SAFETY: discard WM_QUIT posted to this test thread during destruction.
            let _ = unsafe { PeekMessageW(&mut quit, None, WM_QUIT, WM_QUIT, PM_REMOVE) };
        }
    }

    #[test]
    fn ipc_is_acknowledged_and_drained_after_nested_dispatch() -> anyhow::Result<()> {
        let window = TestWindow::new()?;
        let state = &window.state;
        let modal = DispatchGuard::enter(&state.dispatching).unwrap();
        let app = state.app.borrow_mut();
        assert_eq!(window.send_command("capture-window"), LRESULT(0));
        assert_eq!(window.send_command("show"), LRESULT(1));
        assert_eq!(window.send_command("exit"), LRESULT(1));
        drop(app);

        // A nested native loop can consume both wakes before the dialog returns.
        let mut msg = MSG::default();
        for _ in 0..2 {
            // SAFETY: these are real posted messages for the test-owned window.
            unsafe {
                assert!(
                    PeekMessageW(
                        &mut msg,
                        Some(window.hwnd),
                        WM_APP_DISPATCH,
                        WM_APP_DISPATCH,
                        PM_REMOVE,
                    )
                    .as_bool()
                );
                DispatchMessageW(&msg);
            }
        }
        assert!(window.is_live());
        assert!(matches!(
            state.events.borrow().front(),
            Some(Event::Command(Command::Show))
        ));
        assert!(matches!(
            state.events.borrow().back(),
            Some(Event::Command(Command::Exit))
        ));
        drop(modal);

        state.dispatch(window.hwnd);
        assert!(!window.is_live());
        assert!(state.events.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn exit_keeps_dispatching_until_pending_save_finishes() -> anyhow::Result<()> {
        for result in [
            Ok("screenshot.png".into()),
            Err(anyhow::anyhow!("simulated save failure")),
        ] {
            let window = TestWindow::new()?;
            let sender = window.state.app.borrow_mut().saver.simulate_pending();
            window.send(WM_CLOSE);
            assert!(window.is_live());
            assert!(window.state.app.borrow().exit_requested);
            assert_eq!(window.send(WM_QUERYENDSESSION), LRESULT(1));

            sender.send(result)?;
            window.send(WM_SAVE_DONE);
            assert!(!window.is_live());
            assert!(!window.state.app.borrow().saver.busy());
        }
        Ok(())
    }
}

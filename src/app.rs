use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;
use windows::Win32::UI::Shell::NIN_SELECT;
use windows::Win32::UI::WindowsAndMessaging::PBT_APMRESUMEAUTOMATIC;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW, IsWindow, PostQuitMessage,
    RegisterWindowMessageW, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONUP, WM_POWERBROADCAST,
    WM_QUERYENDSESSION, WM_RBUTTONUP, WM_TIMER,
};
use windows::core::w;

use crate::capture::{
    CaptureBackend,
    frame::{Frame, Point},
    gdi::GdiCapture,
    monitor_at_cursor,
};
use crate::config::Config;
use crate::logging::Level;
use crate::output::png::Compression;
use crate::output::save::{SaveWorker, WM_SAVE_DONE};
use crate::output::{ImageFormat, OutputAction, OutputError, SelectionOutcome, crop, filename};
use crate::overlay::{DIRECT2D_IDLE_TIMER, Direct2dCache, Overlay, WM_SELECTION_FINISHED};
use crate::pins::NativePinManager;
use crate::platform::hotkey::Hotkey;
use crate::platform::single_instance::{SingleInstance, read_copy_data};
use crate::platform::tray::{Tray, TrayCommand, TrayMenuState};
use crate::platform::{autostart, clipboard, hotkey, process::log_memory, save_dialog, window};

pub const WM_APP_TRAY: u32 = WM_APP + 1;

const WM_APP_COMMAND: u32 = WM_APP + 2;
const IPC_COMMANDS: [&str; 6] = [
    "show",
    "capture-region",
    "capture-fullscreen",
    "capture-fullscreen-save",
    "reload-config",
    "exit",
];

const HOTKEY_REGION: i32 = 1;
const HOTKEY_FULLSCREEN: i32 = 2;
const HOTKEY_PIN: i32 = 3;
const HOTKEY_WINDOW: i32 = 4;
const HOTKEY_FULLSCREEN_SAVE: i32 = 5;

struct HotkeyBinding {
    id: i32,
}

pub struct App {
    hwnd: HWND,
    config: Config,
    tray: Tray,
    hotkeys: Vec<HotkeyBinding>,
    region_hotkey: Option<Hotkey>,
    fullscreen_hotkey: Option<Hotkey>,
    fullscreen_save_hotkey: Option<Hotkey>,
    pin_hotkey: Option<Hotkey>,
    autostart_enabled: bool,
    taskbar_created_message: u32,
    exit_requested: bool,
    exit_code: i32,
    overlay: Option<Overlay>,
    direct2d: Direct2dCache,
    saver: SaveWorker,
    pins: NativePinManager,
    /// Keeps the named mutex alive for the process lifetime.
    _single_instance: Option<SingleInstance>,
}

impl App {
    pub fn new(
        hwnd: HWND,
        config: Config,
        single_instance: Option<SingleInstance>,
    ) -> anyhow::Result<Self> {
        let tray = Tray::new(hwnd)?;
        let autostart_enabled = autostart::is_enabled();

        // SAFETY: RegisterWindowMessageW is safe to call; 0 means failure.
        let taskbar_created_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

        let mut app = Self {
            hwnd,
            config,
            tray,
            hotkeys: Vec::new(),
            region_hotkey: None,
            fullscreen_hotkey: None,
            fullscreen_save_hotkey: None,
            pin_hotkey: None,
            autostart_enabled,
            taskbar_created_message,
            exit_requested: false,
            exit_code: 0,
            overlay: None,
            direct2d: Direct2dCache::new(hwnd),
            saver: SaveWorker::default(),
            pins: NativePinManager::new(hwnd),
            _single_instance: single_instance,
        };
        app.register_hotkeys();
        Ok(app)
    }

    fn register_hotkeys(&mut self) {
        for binding in self.hotkeys.drain(..) {
            hotkey::unregister(self.hwnd, binding.id);
        }
        let mut failed = false;
        let specs = &self.config.hotkeys;
        for (id, spec, fallback, label, active) in [
            (
                HOTKEY_REGION,
                &specs.region,
                Some("Ctrl+Alt+A"),
                "region",
                &mut self.region_hotkey,
            ),
            (
                HOTKEY_FULLSCREEN,
                &specs.fullscreen,
                Some("Ctrl+Alt+F"),
                "fullscreen",
                &mut self.fullscreen_hotkey,
            ),
            (
                HOTKEY_FULLSCREEN_SAVE,
                &specs.fullscreen_save,
                None,
                "fullscreen save",
                &mut self.fullscreen_save_hotkey,
            ),
            (HOTKEY_PIN, &specs.pin, None, "pin", &mut self.pin_hotkey),
        ] {
            *active = None;
            if spec.trim().is_empty() {
                continue;
            }
            match Self::register_one(self.hwnd, id, spec, fallback, label) {
                Ok(hotkey) => {
                    *active = Some(hotkey);
                    self.hotkeys.push(HotkeyBinding { id });
                    crate::logging::info(format!("{label} hotkey: {}", hotkey.spec()));
                }
                Err(err) => {
                    failed = true;
                    crate::logging::error(err);
                }
            }
        }

        if failed {
            self.tray.notify(
                "热键注册失败",
                "请修改配置文件中的 hotkeys，然后重新加载配置。",
                true,
            );
        }
    }

    fn register_one(
        hwnd: HWND,
        id: i32,
        spec: &str,
        fallback: Option<&str>,
        label: &str,
    ) -> Result<Hotkey, String> {
        let parsed =
            hotkey::parse(spec).map_err(|err| format!("{label} hotkey '{spec}': {err}"))?;
        if let Some(warning) = parsed.warning {
            crate::logging::warn(warning);
        }

        match hotkey::register(hwnd, id, parsed.hotkey) {
            Ok(()) => Ok(parsed.hotkey),
            Err(err) => {
                let reason = format!(
                    "{label} hotkey '{}' registration failed: {err}",
                    parsed.hotkey.spec()
                );
                let Some(fallback) = fallback else {
                    return Err(reason);
                };
                crate::logging::warn(format!("{reason}; trying fallback '{fallback}'"));
                let fallback_parsed = hotkey::parse(fallback)
                    .map_err(|fallback_err| format!("fallback '{fallback}': {fallback_err}"))?;
                hotkey::register(hwnd, id, fallback_parsed.hotkey).map_err(|fallback_err| {
                    format!("{label} hotkey fallback '{fallback}' failed: {fallback_err}")
                })?;
                Ok(fallback_parsed.hotkey)
            }
        }
    }

    fn handle_hotkey(&mut self, id: i32) {
        crate::logging::info(format!("WM_HOTKEY id={id}"));
        match id {
            HOTKEY_REGION => self.request_region(),
            HOTKEY_FULLSCREEN => self.request_fullscreen(OutputAction::Copy),
            HOTKEY_FULLSCREEN_SAVE => self.request_fullscreen(OutputAction::SaveAsOnly),
            HOTKEY_PIN => self.request_selection(OutputAction::Pin),
            HOTKEY_WINDOW => crate::logging::info("window hotkey reserved for v1.4; ignored"),
            _ => crate::logging::warn(format!("unknown hotkey id={id}")),
        }
    }

    fn request_region(&mut self) {
        self.request_selection(OutputAction::Copy);
    }

    fn request_selection(&mut self, default_action: OutputAction) {
        if !self.ready_to_capture() {
            return;
        }
        let started = Instant::now();
        self.pins.hide_all_for_capture();
        let frames = GdiCapture::new().capture_all();
        self.pins.restore_after_capture();
        match frames.map_err(anyhow::Error::from).and_then(|frames| {
            let factory = self.direct2d.factory()?;
            Overlay::show(
                self.hwnd,
                frames,
                self.config.ui.clone(),
                default_action,
                started,
                factory,
            )
        }) {
            Ok(overlay) => self.overlay = Some(overlay),
            Err(err) => {
                self.direct2d
                    .start_idle(self.config.ui.direct2d_idle_seconds);
                self.report_capture_error(err);
            }
        }
    }

    fn request_fullscreen(&mut self, action: OutputAction) {
        if !self.ready_to_capture() {
            return;
        }
        let started = Instant::now();
        let monitor = match monitor_at_cursor() {
            Ok(monitor) => monitor,
            Err(err) => return self.report_capture_error(err.into()),
        };
        self.pins.hide_all_for_capture();
        let frame = GdiCapture::new().capture_monitor(monitor);
        self.pins.restore_after_capture();
        match frame {
            Ok(captured) => self.execute_output(
                captured.frame,
                action,
                Point::new(captured.info.rect.left, captured.info.rect.top),
                started,
            ),
            Err(err) => self.report_capture_error(err.into()),
        }
    }

    fn ready_to_capture(&mut self) -> bool {
        if self.overlay.is_some() || self.exit_requested {
            return false;
        }
        self.finish_save();
        if self.saver.busy() {
            self.notify_capture("RPin", "正在保存上一张截图。", false);
            return false;
        }
        true
    }

    fn finish_selection(&mut self) {
        let started = Instant::now();
        let Some(result) = self.overlay.as_mut().and_then(Overlay::take_outcome) else {
            return;
        };
        self.overlay.take();
        let result = result.and_then(|result| match result.outcome {
            SelectionOutcome::Confirmed { rect, action } => {
                crate::logging::info(format!("selection confirmed: {rect:?}, action={action:?}"));
                let frame = crop::compose(result.frames, rect)?;
                let frame = if result.annotations.is_empty() {
                    frame
                } else {
                    let factory = self.direct2d.factory()?;
                    crate::annotation::flatten(&factory, frame, &result.annotations)?
                };
                Ok(Some((frame, action, Point::new(rect.left, rect.top))))
            }
            SelectionOutcome::Cancelled => Ok(None),
        });
        self.direct2d
            .start_idle(self.config.ui.direct2d_idle_seconds);
        match result {
            Ok(Some((frame, action, at))) => self.execute_output(frame, action, at, started),
            Ok(None) => crate::logging::debug("selection cancelled"),
            Err(err) => self.report_capture_error(err),
        }
    }

    fn close_overlay(&mut self) {
        let was_open = self.overlay.take().is_some();
        if was_open {
            self.direct2d
                .start_idle(self.config.ui.direct2d_idle_seconds);
        }
    }

    fn report_capture_error(&self, err: anyhow::Error) {
        crate::logging::error(format!("capture failed: {err:#}"));
        self.notify_capture("截图失败", "请查看日志了解失败原因。", true);
    }

    fn notify_capture(&self, title: &str, message: &str, error: bool) {
        if self.config.capture.notifications {
            self.tray.notify(title, message, error);
        }
    }

    fn execute_output(&mut self, frame: Frame, action: OutputAction, at: Point, started: Instant) {
        let config = &self.config.capture;
        let (explicit_save, copy_to_clipboard) = match action {
            OutputAction::Copy => (false, config.copy_to_clipboard),
            OutputAction::SaveAs => (true, config.copy_to_clipboard),
            OutputAction::SaveAsOnly => (true, false),
            OutputAction::Pin | OutputAction::PinAndCopy => {
                if action == OutputAction::PinAndCopy {
                    self.copy_output(&frame, started);
                }
                if let Err(err) = self.pins.show(frame, at, &self.config.pins) {
                    crate::logging::error(format!("creating pin failed: {err:#}"));
                    self.notify_capture("贴图失败", &err.to_string(), true);
                }
                return;
            }
        };
        let mut target = None;
        if explicit_save {
            // A cancelled dialog is silent and leaves the clipboard untouched.
            match self.choose_output_path(action == OutputAction::SaveAs) {
                Ok(Some(path)) => target = Some(path),
                Ok(None) => return,
                Err(err) => return self.report_save_error(err),
            }
        }
        log_memory("with output frame", Level::Debug);
        let copied = copy_to_clipboard && self.copy_output(&frame, started);
        if !explicit_save && config.auto_save {
            // Copy is the primary action. Cancelling the optional save
            // dialog after copying leaves the clipboard image available.
            match self.choose_output_path(false) {
                Ok(path) => target = path,
                Err(err) => return self.report_save_error(err),
            }
        }
        if let Some(target) = target {
            let compression = if config.png_compression == "small" {
                Compression::Small
            } else {
                Compression::Fast
            };
            if let Err(err) = self.saver.start(self.hwnd, frame, target, compression) {
                self.report_save_error(err);
            }
        } else {
            drop(frame);
            log_memory("after output frame released", Level::Debug);
            if copied {
                self.notify_capture("已复制截图", "可粘贴到其他应用。", false);
            } else if !copy_to_clipboard && !config.auto_save {
                self.notify_capture("截图未输出", "请启用剪贴板复制或保存提示。", false);
            }
        }
    }

    fn copy_output(&self, frame: &Frame, started: Instant) -> bool {
        match clipboard::copy_image(self.hwnd, frame) {
            Ok(()) => {
                crate::logging::info(format!(
                    "clipboard ready in {:.2} ms ({} × {})",
                    started.elapsed().as_secs_f64() * 1000.0,
                    frame.width,
                    frame.height
                ));
                true
            }
            Err(err) => {
                crate::logging::error(format!("clipboard failed: {err:#}"));
                let detail = if err
                    .downcast_ref::<OutputError>()
                    .is_some_and(|err| matches!(err, OutputError::ClipboardBusy))
                {
                    "剪贴板被其他程序占用，请重试。"
                } else {
                    "请查看日志了解失败原因。"
                };
                self.notify_capture("复制失败", detail, true);
                false
            }
        }
    }

    fn choose_output_path(&self, allow_jpeg: bool) -> anyhow::Result<Option<std::path::PathBuf>> {
        let config = &self.config.capture;
        let directory = config.save_dir.trim();
        let directory = (!directory.is_empty()).then(|| std::path::Path::new(directory));
        let name = filename::stem(&config.filename);
        save_dialog::choose_image(self.hwnd, directory, &name, allow_jpeg)
    }

    fn finish_save(&mut self) {
        let Some(result) = self.saver.take_result() else {
            return;
        };
        match result {
            Ok(path) => {
                let format = ImageFormat::from_path(&path).map_or("image", ImageFormat::label);
                log_memory(
                    &format!("after {format} worker released frame"),
                    Level::Debug,
                );
                crate::logging::info(format!("saved {}", path.display()));
                self.notify_capture("截图已保存", &path.to_string_lossy(), false);
            }
            Err(err) => self.report_save_error(err),
        }
    }

    fn report_save_error(&self, err: anyhow::Error) {
        crate::logging::error(format!("save failed: {err:#}"));
        self.notify_capture("保存失败", "请查看日志了解失败原因。", true);
    }

    fn menu_state(&self) -> TrayMenuState {
        let region_hint = self
            .region_hotkey
            .map(|h| h.spec())
            .unwrap_or_else(|| self.config.hotkeys.region.clone());
        let fullscreen_hint = self
            .fullscreen_hotkey
            .map(|h| h.spec())
            .unwrap_or_else(|| self.config.hotkeys.fullscreen.clone());
        let fullscreen_save_hint = self
            .fullscreen_save_hotkey
            .map(|h| h.spec())
            .unwrap_or_default();

        TrayMenuState {
            autostart: self.autostart_enabled,
            region_hint,
            fullscreen_hint,
            fullscreen_save_hint,
            pin_hint: self.pin_hotkey.map(|key| key.spec()).unwrap_or_default(),
            pins: self.pins.summaries(),
        }
    }

    fn handle_command(&mut self, command: TrayCommand) {
        match command {
            TrayCommand::Region => self.request_region(),
            TrayCommand::Fullscreen => self.request_fullscreen(OutputAction::Copy),
            TrayCommand::FullscreenSave => self.request_fullscreen(OutputAction::SaveAsOnly),
            TrayCommand::NewPin => self.request_selection(OutputAction::Pin),
            TrayCommand::ShowPin(id) => {
                if let Err(err) = self.pins.show_existing(id) {
                    crate::logging::warn(format!("showing pin failed: {err:#}"));
                }
            }
            TrayCommand::ClosePins => self.pins.close_all(),
            TrayCommand::OpenConfig => self.open_config(),
            TrayCommand::ReloadConfig => self.reload_config(),
            TrayCommand::ToggleAutostart => self.toggle_autostart(),
            TrayCommand::Exit => self.request_exit(),
        }
    }

    fn open_config(&self) {
        let path = crate::config::config_path();
        match std::process::Command::new("notepad.exe").arg(&path).spawn() {
            Ok(_) => crate::logging::info(format!("opened config: {}", path.display())),
            Err(err) => {
                crate::logging::error(format!("failed to open config: {err}"));
                self.tray.notify(
                    "无法打开配置",
                    "未找到 notepad.exe，请手动编辑配置文件。",
                    true,
                );
            }
        }
    }

    fn reload_config(&mut self) {
        let (config, warnings) = crate::config::load_or_default();
        for warning in warnings {
            crate::logging::warn(warning);
        }
        crate::logging::set_level(&config.app.log_level);
        self.config = config;
        self.direct2d
            .update_timer(self.config.ui.direct2d_idle_seconds);
        self.autostart_enabled = autostart::is_enabled();
        self.register_hotkeys();
        crate::logging::info("config reloaded");
        self.tray.notify("RPin", "配置已重新加载。", false);
    }

    fn toggle_autostart(&mut self) {
        let target = !self.autostart_enabled;
        match autostart::set_enabled(target) {
            Ok(()) => {
                self.autostart_enabled = target;
                crate::logging::info(format!("autostart set to {target}"));
                self.tray.notify(
                    "RPin",
                    if target {
                        "已启用开机自启。"
                    } else {
                        "已关闭开机自启。"
                    },
                    false,
                );
            }
            Err(err) => {
                crate::logging::error(format!("autostart update failed: {err}"));
                self.tray
                    .notify("开机自启设置失败", "请查看日志了解详情。", true);
            }
        }
    }

    fn request_exit(&mut self) {
        // Do not call DestroyWindow here: WM_DESTROY would re-enter wndproc
        // while `&mut App` is still borrowed. The window procedure destroys
        // the window after this message handler returns.
        self.exit_requested = true;
    }

    pub fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    fn handle_copy_data(&mut self, lparam: LPARAM) -> LRESULT {
        if let Some(command) = read_copy_data(lparam) {
            crate::logging::info(format!("WM_COPYDATA command: {command}"));
            if let Some(id) = IPC_COMMANDS.iter().position(|&item| item == command) {
                // SAFETY: only a small command index is queued on our UI thread.
                let queued = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(self.hwnd),
                        WM_APP_COMMAND,
                        WPARAM(id),
                        LPARAM(0),
                    )
                };
                return LRESULT(queued.is_ok() as isize);
            }
            LRESULT(0)
        } else {
            LRESULT(0)
        }
    }

    pub fn apply_startup_command(&mut self, command: &str) {
        if command != "show" {
            crate::logging::info(format!("startup command: {command}"));
            self.handle_external_command(command);
        }
    }

    fn handle_external_command(&mut self, command: &str) {
        match command {
            "capture-region" => self.request_region(),
            "capture-fullscreen" => self.request_fullscreen(OutputAction::Copy),
            "capture-fullscreen-save" => self.request_fullscreen(OutputAction::SaveAsOnly),
            "reload-config" => self.reload_config(),
            "show" => self.tray.notify("RPin", "已在运行。", false),
            "exit" => self.request_exit(),
            _ => crate::logging::warn(format!("unknown command: {command}")),
        }
    }

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if self.taskbar_created_message != 0 && msg == self.taskbar_created_message {
            crate::logging::info("Explorer TaskbarCreated; re-adding tray icon");
            if let Err(err) = self.tray.add() {
                crate::logging::error(format!("re-adding tray icon failed: {err}"));
            }
            return LRESULT(0);
        }

        match msg {
            WM_HOTKEY => {
                self.handle_hotkey(wparam.0 as i32);
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let event = (lparam.0 as u32) & 0xFFFF;
                match event {
                    NIN_SELECT | WM_LBUTTONUP => self.request_region(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_COMMAND => {
                if let Some(command) = IPC_COMMANDS.get(wparam.0) {
                    self.handle_external_command(command);
                }
                LRESULT(0)
            }
            WM_COPYDATA => self.handle_copy_data(lparam),
            WM_SELECTION_FINISHED => {
                self.finish_selection();
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == DIRECT2D_IDLE_TIMER => {
                self.direct2d
                    .update_timer(self.config.ui.direct2d_idle_seconds);
                LRESULT(0)
            }
            WM_SAVE_DONE => {
                self.finish_save();
                LRESULT(0)
            }
            WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
                self.tray.remove();
                self.register_hotkeys();
                crate::logging::info("power broadcast; refreshing tray icon");
                if let Err(err) = self.tray.add() {
                    crate::logging::warn(format!("tray refresh failed: {err}"));
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                self.close_overlay();
                crate::logging::info("WM_DISPLAYCHANGE received");
                LRESULT(0)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    crate::logging::info("Windows session ending; cleaning up");
                    self.request_exit();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                self.request_exit();
                LRESULT(0)
            }
            WM_DESTROY => {
                self.overlay.take();
                self.direct2d.clear();
                self.pins.close_all();
                self.saver.shutdown();
                crate::logging::info("main window destroyed; exiting");
                for binding in self.hotkeys.drain(..) {
                    hotkey::unregister(self.hwnd, binding.id);
                }
                self.tray.remove();
                // SAFETY: standard shutdown of the UI thread message loop.
                unsafe { PostQuitMessage(self.exit_code) };
                LRESULT(0)
            }
            _ => window::default_proc(self.hwnd, msg, wparam, lparam),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.overlay.take();
        self.direct2d.clear();
        self.saver.shutdown();
        for binding in self.hotkeys.drain(..) {
            hotkey::unregister(self.hwnd, binding.id);
        }
        self.pins.close_all();
        self.tray.remove();
    }
}

pub unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: reads the pointer attached after App construction.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
    if raw.is_null() {
        return window::default_proc(hwnd, msg, wparam, lparam);
    }

    // SAFETY: main owns the box until the message loop and window teardown finish.
    let state = unsafe { &*raw };
    let result = catch_unwind(AssertUnwindSafe(|| {
        // TrackPopupMenu dispatches messages synchronously, including IPC and exit.
        // Snapshot its labels so the nested loop can borrow the application again.
        let event = lparam.0 as u32 & 0xffff;
        if msg == WM_APP_TRAY && matches!(event, WM_RBUTTONUP | WM_CONTEXTMENU) {
            let Ok(app) = state.try_borrow() else {
                return LRESULT(0);
            };
            let menu_state = app.menu_state();
            drop(app);
            let command = Tray::show_menu(hwnd, &menu_state);
            // SAFETY: the nested menu loop may have dispatched an exit.
            let live = unsafe { IsWindow(Some(hwnd)) }.as_bool();
            if let Some(command) = command.filter(|_| live) {
                state.borrow_mut().handle_command(command);
            }
            return LRESULT(0);
        }
        match state.try_borrow_mut() {
            Ok(mut app) => app.handle_message(msg, wparam, lparam),
            Err(_) => window::default_proc(hwnd, msg, wparam, lparam),
        }
    }))
    .unwrap_or_else(|_| {
        crate::logging::error(format!("panic processing message 0x{msg:04X}"));
        if msg == WM_DESTROY {
            // SAFETY: destruction cannot be retried. End the UI loop with an error.
            unsafe {
                PostQuitMessage(1);
            }
        } else if let Ok(mut app) = state.try_borrow_mut() {
            app.exit_code = 1;
            app.request_exit();
        }
        LRESULT(0)
    });

    let should_exit = state
        .try_borrow_mut()
        .is_ok_and(|mut app| std::mem::take(&mut app.exit_requested));
    if should_exit {
        // SAFETY: all App borrows ended, and the exit request was consumed.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
    result
}

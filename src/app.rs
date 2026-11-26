mod command;
mod window_state;

use std::path::PathBuf;
use std::time::Instant;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
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
use crate::output::save::SaveWorker;
use crate::output::{ImageFormat, OutputAction, OutputError, SelectionOutcome, crop, filename};
use crate::overlay::{Direct2dCache, Overlay};
use crate::pins::NativePinManager;
use crate::platform::capture_windows::WindowTargets;
use crate::platform::hotkey::Hotkey;
use crate::platform::single_instance::SingleInstance;
use crate::platform::tray::{Tray, TrayMenuState};
use crate::platform::{autostart, clipboard, hotkey, process::log_memory, save_dialog};

pub(crate) use command::Command;
pub(crate) use window_state::{WM_APP_TRAY, WindowState, wndproc};

const HOTKEY_REGION: i32 = 1;
const HOTKEY_FULLSCREEN: i32 = 2;
const HOTKEY_PIN: i32 = 3;
const HOTKEY_WINDOW: i32 = 4;
const HOTKEY_FULLSCREEN_SAVE: i32 = 5;

struct HotkeyBinding {
    id: i32,
}

struct SavePrompt {
    frame: Frame,
    directory: Option<PathBuf>,
    name: String,
    clipboard: SaveClipboard,
    compression: Compression,
    started: Instant,
}

enum SaveClipboard {
    AfterConfirmation { enabled: bool },
    AlreadyHandled { copied: bool },
}

impl SavePrompt {
    fn choose_path(&self, owner: HWND) -> anyhow::Result<Option<PathBuf>> {
        save_dialog::choose_image(owner, self.directory.as_deref(), &self.name)
    }
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
        let mut app = Self::with_tray(hwnd, config, tray, single_instance);
        app.autostart_enabled = autostart::is_enabled();
        // SAFETY: RegisterWindowMessageW is safe to call; 0 means failure.
        app.taskbar_created_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        app.register_hotkeys();
        Ok(app)
    }

    fn with_tray(
        hwnd: HWND,
        config: Config,
        tray: Tray,
        single_instance: Option<SingleInstance>,
    ) -> Self {
        Self {
            hwnd,
            config,
            tray,
            hotkeys: Vec::new(),
            region_hotkey: None,
            fullscreen_hotkey: None,
            fullscreen_save_hotkey: None,
            pin_hotkey: None,
            autostart_enabled: false,
            taskbar_created_message: 0,
            exit_requested: false,
            overlay: None,
            direct2d: Direct2dCache::new(hwnd),
            saver: SaveWorker::default(),
            pins: NativePinManager::new(hwnd),
            _single_instance: single_instance,
        }
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

    fn request_selection(&mut self, default_action: OutputAction) {
        if !self.ready_to_capture() {
            return;
        }
        let started = Instant::now();
        self.pins.hide_all_for_capture();
        let window_targets = WindowTargets::snapshot();
        let frames = GdiCapture::new().capture_all();
        self.pins.restore_after_capture();
        match frames.map_err(anyhow::Error::from).and_then(|frames| {
            let factory = self.direct2d.factory()?;
            Overlay::show(
                self.hwnd,
                frames,
                window_targets,
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

    fn request_fullscreen(&mut self, action: OutputAction) -> Option<SavePrompt> {
        if !self.ready_to_capture() {
            return None;
        }
        let started = Instant::now();
        let monitor = match monitor_at_cursor() {
            Ok(monitor) => monitor,
            Err(err) => {
                self.report_capture_error(err.into());
                return None;
            }
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
            Err(err) => {
                self.report_capture_error(err.into());
                None
            }
        }
    }

    fn ready_to_capture(&self) -> bool {
        if self.overlay.is_some() || self.exit_requested {
            return false;
        }
        if self.saver.busy() {
            self.notify_capture("RPin", "正在保存上一张截图。", false);
            return false;
        }
        true
    }

    fn finish_selection(&mut self) -> Option<SavePrompt> {
        let started = Instant::now();
        let result = self.overlay.as_mut().and_then(Overlay::take_outcome)?;
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
            Ok(None) => {
                crate::logging::debug("selection cancelled");
                None
            }
            Err(err) => {
                self.report_capture_error(err);
                None
            }
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

    fn execute_output(
        &mut self,
        frame: Frame,
        action: OutputAction,
        at: Point,
        started: Instant,
    ) -> Option<SavePrompt> {
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
                return None;
            }
        };
        let copied = if explicit_save {
            false
        } else {
            log_memory("with output frame", Level::Debug);
            copy_to_clipboard && self.copy_output(&frame, started)
        };
        if explicit_save || config.auto_save {
            let directory = config.save_dir.trim();
            let compression = if config.png_compression == "small" {
                Compression::Small
            } else {
                Compression::Fast
            };
            return Some(SavePrompt {
                frame,
                directory: (!directory.is_empty()).then(|| PathBuf::from(directory)),
                name: filename::stem(&config.filename),
                clipboard: if explicit_save {
                    SaveClipboard::AfterConfirmation {
                        enabled: copy_to_clipboard,
                    }
                } else {
                    SaveClipboard::AlreadyHandled { copied }
                },
                compression,
                started,
            });
        }
        self.release_output(frame, copied);
        if !copy_to_clipboard {
            self.notify_capture("截图未输出", "请启用剪贴板复制或保存提示。", false);
        }
        None
    }

    fn complete_save_prompt(
        &mut self,
        prompt: SavePrompt,
        target: anyhow::Result<Option<PathBuf>>,
    ) {
        let target = match target {
            Ok(Some(target)) => target,
            Ok(None) => {
                if let SaveClipboard::AlreadyHandled { copied } = prompt.clipboard {
                    self.release_output(prompt.frame, copied);
                }
                return;
            }
            Err(err) => return self.report_save_error(err),
        };
        if let SaveClipboard::AfterConfirmation { enabled } = prompt.clipboard {
            log_memory("with output frame", Level::Debug);
            if enabled {
                self.copy_output(&prompt.frame, prompt.started);
            }
        }
        if let Err(err) = self
            .saver
            .start(self.hwnd, prompt.frame, target, prompt.compression)
        {
            self.report_save_error(err);
        }
    }

    fn release_output(&self, frame: Frame, copied: bool) {
        drop(frame);
        log_memory("after output frame released", Level::Debug);
        if copied {
            self.notify_capture("已复制截图", "可粘贴到其他应用。", false);
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

    fn finish_save(&mut self) {
        if let Some(result) = self.saver.take_result() {
            self.report_save_result(result);
        }
    }

    fn report_save_result(&self, result: anyhow::Result<PathBuf>) {
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

    fn handle_command(&mut self, command: Command) -> Option<SavePrompt> {
        match command {
            Command::Show => self.tray.notify("RPin", "已在运行。", false),
            Command::Region => self.request_selection(OutputAction::Copy),
            Command::Fullscreen => return self.request_fullscreen(OutputAction::Copy),
            Command::FullscreenSave => return self.request_fullscreen(OutputAction::SaveAsOnly),
            Command::NewPin => self.request_selection(OutputAction::Pin),
            Command::ShowPin(id) => {
                if let Err(err) = self.pins.show_existing(id) {
                    crate::logging::warn(format!("showing pin failed: {err:#}"));
                }
            }
            Command::ClosePins => self.pins.close_all(),
            Command::OpenConfig => self.open_config(),
            Command::ReloadConfig => self.reload_config(),
            Command::ToggleAutostart => self.toggle_autostart(),
            Command::Exit => self.request_exit(),
        }
        None
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
        self.exit_requested = true;
        self.close_overlay();
    }

    fn end_session(&mut self) {
        self.request_exit();
        if let Some(result) = self.saver.wait_for_session_end() {
            self.report_save_result(result);
        }
    }

    fn refresh_after_resume(&mut self) {
        self.tray.remove();
        self.register_hotkeys();
        crate::logging::info("power broadcast; refreshing tray icon");
        if let Err(err) = self.tray.add() {
            crate::logging::warn(format!("tray refresh failed: {err}"));
        }
    }

    fn shutdown(&mut self) {
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

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown();
    }
}

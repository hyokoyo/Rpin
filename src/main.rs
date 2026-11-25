#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod annotation;
mod app;
mod capture;
mod capture_test;
mod config;
mod logging;
mod output;
mod overlay;
mod pins;
mod platform;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, MB_ICONERROR, MB_OK, MessageBoxW};
use windows::core::PCWSTR;

fn main() {
    let code = std::panic::catch_unwind(run).unwrap_or_else(|_| {
        fatal("RPin 崩溃", "请查看日志了解失败原因。");
        1
    });
    std::process::exit(code);
}

fn run() -> i32 {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.next_if(|arg| arg == "--capture-bench").is_some() {
        let Some(options) = capture_test::BenchOptions::parse(args) else {
            eprintln!(
                "Usage: rpin --capture-bench [runs] [--monitor] [--method direct|copy|compatible]"
            );
            return 2;
        };
        return match capture_test::bench(options) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("RPin: {err:#}");
                1
            }
        };
    }
    if args.next_if(|arg| arg == "--capture-test").is_some() {
        let root = args
            .next()
            .map_or_else(|| ".".into(), std::path::PathBuf::from);
        if args.next().is_some() {
            eprintln!("Usage: rpin --capture-test [output-directory]");
            return 2;
        }
        return match capture_test::run(&root) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("RPin: {err:#}");
                1
            }
        };
    }
    let command = command_from_args(args);
    if command == "version" {
        println!("RPin {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    // Only the primary process creates/repairs configuration and opens the log.
    let instance = match platform::single_instance::acquire(&command) {
        Ok(platform::single_instance::AcquireResult::Primary(instance)) => instance,
        Ok(platform::single_instance::AcquireResult::Secondary) => return 0,
        Err(err) => {
            eprintln!("RPin: {err}");
            return 1;
        }
    };
    let (config, warnings) = config::load_or_default();
    if let Err(err) = logging::init(&config.app.log_level) {
        eprintln!("RPin: failed to initialize logging: {err}");
    }
    for warning in warnings {
        logging::warn(warning);
    }
    install_panic_hook();

    logging::info(format!("RPin {} starting", env!("CARGO_PKG_VERSION")));
    if let Some(path) = logging::log_file_path() {
        logging::debug(format!("log file: {}", path.display()));
    }

    let _com = platform::com::ComGuard::init();

    let hwnd = match platform::window::create_hidden_window() {
        Ok(hwnd) => hwnd,
        Err(err) => {
            logging::error(format!("failed to create main window: {err}"));
            fatal("RPin 启动失败", &format!("无法创建主窗口：{err}"));
            return 1;
        }
    };

    let mut app = match app::App::new(hwnd, config, Some(instance)) {
        Ok(app) => app,
        Err(err) => {
            logging::error(format!("failed to initialize app: {err}"));
            // SAFETY: hwnd was created above and is no longer needed.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            }
            fatal("RPin 启动失败", &format!("初始化失败：{err}"));
            return 1;
        }
    };
    app.apply_startup_command(&command);
    if app.exit_requested() {
        logging::info("exit requested by command line");
        // SAFETY: no messages have been dispatched yet; the App is still local.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
        return 0;
    }

    let raw = Box::into_raw(Box::new(std::cell::RefCell::new(app)));
    platform::window::attach_app(hwnd, raw);

    logging::info(format!("message loop ready pid={}", std::process::id()));
    let exit_code = platform::window::run_message_loop();

    // SAFETY: the message loop has ended. If the window still exists, detach
    // the App pointer before destroying it, then reclaim the Box exactly once.
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            platform::window::detach_app(hwnd);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
        drop(Box::from_raw(raw));
    }

    logging::info(format!("RPin exited with code {exit_code}"));
    exit_code
}

fn command_from_args(args: impl Iterator<Item = std::ffi::OsString>) -> String {
    for arg in args {
        match arg.to_str() {
            Some("--capture-region") => return "capture-region".to_string(),
            Some("--capture-fullscreen") => return "capture-fullscreen".to_string(),
            Some("--capture-fullscreen-save") => return "capture-fullscreen-save".to_string(),
            Some("--reload-config") => return "reload-config".to_string(),
            Some("--exit") => return "exit".to_string(),
            Some("--version" | "-V") => return "version".to_string(),
            _ => {}
        }
    }
    "show".to_string()
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let message = format!("panic: {info}");
        if std::thread::current().name() == Some(output::save::THREAD_NAME) {
            logging::error(&message);
            return;
        }
        let marker: String = message.chars().take(2048).collect();
        let _ = std::fs::write(config::logs_dir().join("crash.marker"), marker);
        logging::error(&message);
    }));
}

fn fatal(title: &str, message: &str) {
    let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let message_wide: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both strings are NUL-terminated and live for the call.
    unsafe {
        let _ = MessageBoxW(
            None::<HWND>,
            PCWSTR(message_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

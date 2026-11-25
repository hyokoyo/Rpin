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
    let Some(command) = command_from_args(args) else {
        println!("RPin {}", env!("CARGO_PKG_VERSION"));
        return 0;
    };
    // Only the primary process creates/repairs configuration and opens the log.
    let instance = match platform::single_instance::acquire(
        command.ipc_name().expect("CLI commands have an IPC name"),
    ) {
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

    let app = match app::App::new(hwnd, config, Some(instance)) {
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
    let raw = Box::into_raw(Box::new(app::WindowState::new(app)));
    // SAFETY: main owns raw until the window and message loop have finished.
    unsafe {
        platform::window::attach_app(hwnd, raw);
        (*raw).run_startup(hwnd, command);
    }

    logging::info(format!("message loop ready pid={}", std::process::id()));
    let exit_code = platform::window::run_message_loop();

    // SAFETY: the message loop has ended. If the window still exists, detach
    // the state pointer before destroying it, then reclaim the Box exactly once.
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

fn command_from_args(args: impl Iterator<Item = std::ffi::OsString>) -> Option<app::Command> {
    for arg in args {
        let Some(arg) = arg.to_str() else { continue };
        if matches!(arg, "--version" | "-V") {
            return None;
        }
        if let Some(command) = arg
            .strip_prefix("--")
            .and_then(app::Command::from_ipc)
            .filter(|&command| command != app::Command::Show)
        {
            return Some(command);
        }
    }
    Some(app::Command::Show)
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

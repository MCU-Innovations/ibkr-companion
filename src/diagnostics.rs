use chrono::Local;
use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU8, Ordering};

const OFF: u8 = 0;
const ERROR: u8 = 1;
const WARN: u8 = 2;
const INFO: u8 = 3;
const DEBUG: u8 = 4;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

/// Attach a release GUI build to its launching terminal, if there is one.
/// Explorer launches stay console-free. The early CRT hook in main.rs calls
/// this before Rust initializes stdio; main calls it again as a fallback.
pub fn attach_parent_console() -> bool {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        ) != 0
    }

    #[cfg(not(windows))]
    {
        false
    }
}

pub fn init() {
    let value = std::env::var("IBKR_COMPANION_LOG")
        .unwrap_or_else(|_| "info".into())
        .to_ascii_lowercase();
    let level = match value.as_str() {
        "off" => OFF,
        "error" => ERROR,
        "warn" => WARN,
        "info" => INFO,
        "debug" => DEBUG,
        _ => {
            warn(format_args!(
                "Unknown IBKR_COMPANION_LOG={value}; using info"
            ));
            INFO
        }
    };
    LEVEL.store(level, Ordering::Relaxed);
}

fn write(level: u8, label: &str, message: fmt::Arguments<'_>) {
    if LEVEL.load(Ordering::Relaxed) < level {
        return;
    }
    let timestamp = Local::now().format("%H:%M:%S%.3f");
    // A GUI launch may have no console. Ignore that write error so logging
    // never affects the Slint event loop or background network tasks.
    let _ = writeln!(io::stderr().lock(), "{timestamp} {label} {message}");
}

pub fn error(message: fmt::Arguments<'_>) {
    write(ERROR, "ERROR", message);
}

pub fn warn(message: fmt::Arguments<'_>) {
    write(WARN, "WARN ", message);
}

pub fn info(message: fmt::Arguments<'_>) {
    write(INFO, "INFO ", message);
}

pub fn debug(message: fmt::Arguments<'_>) {
    write(DEBUG, "DEBUG", message);
}

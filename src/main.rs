#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    if let Err(error) = caret_mode::platform::run() {
        eprintln!("Caret Mode: {error}");
        if !std::env::args().any(|arg| arg == "--smoke-test" || arg == "--diagnose") {
            caret_mode::platform::report_error(&error.to_string());
        }
        std::process::exit(1);
    }

    #[cfg(not(windows))]
    {
        eprintln!("caret-mode supports Windows 11 only.");
        std::process::exit(1);
    }
}

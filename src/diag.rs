//! The debug channel: `SWARMY_DEBUG=1 swarmy … 2>log`. Failures that a front end does not show
//! (a file that could not be read where a fallback is harmless, a best-effort cleanup) are
//! written here instead of vanishing, so a wrong number can be traced back to its cause.

/// Whether `SWARMY_DEBUG` is set (read once).
pub fn enabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("SWARMY_DEBUG").is_some())
}

/// One line on stderr when the debug channel is on: `swarmy[<area>]: <message>`.
pub fn log(area: &str, message: &str) {
    if enabled() {
        eprintln!("swarmy[{area}]: {message}");
    }
}

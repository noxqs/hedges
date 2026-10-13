//! A log file in the user's own directory, for working out why Hedge stopped.
//!
//! Hedge normally runs from autostart with no terminal attached, so anything it
//! printed to stderr went nowhere. Three sources feed the file:
//!
//! * our own messages (the [`log!`](crate::log) macro), which also still go to
//!   stderr so running it from a terminal is unchanged;
//! * **Rust panics**, with location and backtrace, via a panic hook;
//! * **GTK/GLib warnings and criticals**, via the GLib log handler — those are
//!   what usually precede a GTK crash, and they are invisible otherwise.
//!
//! Startup and shutdown are both logged. That pairing is the point: a session
//! whose last line is `--- session ended ---` exited cleanly, while one that
//! stops mid-stream died hard (a segfault inside C code kills the process
//! outright, so no Rust hook can report it — but whatever was logged last says
//! what it was doing).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Rotate once the file passes this, keeping one previous session's worth.
const MAX_BYTES: u64 = 1_000_000;

/// Serialises writers (the GLib handler and panic hook can fire off-thread).
static LOCK: Mutex<()> = Mutex::new(());

fn state_dir() -> PathBuf {
    // XDG puts logs in the state dir, not cache: they should survive a cleanup.
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local/state")
        });
    base.join("hedge")
}

/// `~/.local/state/hedge/hedge.log`
pub fn path() -> PathBuf {
    state_dir().join("hedge.log")
}

/// The previous log, kept across one rotation.
pub fn previous_path() -> PathBuf {
    state_dir().join("hedge.log.1")
}

/// Local time, `2026-08-09 14:32:07`. Local rather than UTC because the person
/// reading this is comparing it against when they saw the crash.
fn stamp() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "--------- --:--:--".to_string())
}

/// Append one line. Never panics and never fails loudly — a logger that can
/// take the app down with it is worse than no logger.
pub fn write_line(level: &str, msg: &str) {
    let Ok(_guard) = LOCK.lock() else { return };
    let dir = state_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = path();
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = std::fs::rename(&path, previous_path());
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {level:<5} {msg}", stamp());
    }
}

/// Log a line and mirror it to stderr, so a terminal run looks as it always did.
///
/// ```ignore
/// log!("saved {n} hedges");
/// log!(WARN, "could not trash {}: {e}", path.display());
/// ```
#[macro_export]
macro_rules! log {
    (WARN, $($arg:tt)*) => {{
        let m = format!($($arg)*);
        eprintln!("hedge: {m}");
        $crate::log::write_line("WARN", &m);
    }};
    ($($arg:tt)*) => {{
        let m = format!($($arg)*);
        eprintln!("hedge: {m}");
        $crate::log::write_line("INFO", &m);
    }};
}

/// Start logging: open the session, and route panics and GLib messages here.
///
/// Called first thing in `main`, before GTK, so a failure during startup is
/// still captured.
pub fn init() {
    write_line("INFO", "--- session started ---");
    write_line(
        "INFO",
        &format!(
            "hedge {} pid {} exe {}",
            crate::version::current().long_label(),
            std::process::id(),
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into()),
        ),
    );
    write_line(
        "INFO",
        &format!(
            "session DISPLAY={} XDG_CURRENT_DESKTOP={}",
            std::env::var("DISPLAY").unwrap_or_else(|_| "?".into()),
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "?".into()),
        ),
    );

    // Panics: keep the default stderr report, then record it with a backtrace.
    // force_capture means we get one whether or not RUST_BACKTRACE is set —
    // a crash the user has already hit is not one you want to ask them to
    // reproduce with a different environment variable.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".into());
        write_line("PANIC", &format!("{info} at {where_}"));
        write_line(
            "PANIC",
            &format!("backtrace:\n{}", std::backtrace::Backtrace::force_capture()),
        );
        previous(info);
    }));

    // GTK/GLib's own diagnostics. A CRITICAL is nearly always the last thing
    // said before the widget layer misbehaves, so it belongs in the file.
    //
    // Installing a handler means we receive *everything*, including the DEBUG
    // chatter GLib normally suppresses (dbus auth, gio module loading). That
    // would bury the one line that matters, so debug and info are dropped
    // unless HEDGE_DEBUG asks for them.
    let verbose = std::env::var_os("HEDGE_DEBUG").is_some();
    glib::log_set_default_handler(move |domain, level, msg| {
        let level_name = match level {
            glib::LogLevel::Error => "ERROR",
            glib::LogLevel::Critical => "CRIT",
            glib::LogLevel::Warning => "WARN",
            glib::LogLevel::Message => "INFO",
            glib::LogLevel::Info | glib::LogLevel::Debug if !verbose => return,
            glib::LogLevel::Info => "INFO",
            glib::LogLevel::Debug => "DEBUG",
        };
        write_line(level_name, &format!("[{}] {msg}", domain.unwrap_or("glib")));
    });
}

/// Log the signals that end a session, and quit through the normal path.
///
/// Without this, logging out looks exactly like a crash: the session manager
/// sends SIGTERM, the process dies where it stands, and the log just stops —
/// the same shape as a segfault. Handling it turns the most common "it
/// disappeared" into a line that says so.
pub fn watch_exit_signals(quit: impl Fn() + Clone + 'static) {
    for (num, name) in [(15, "SIGTERM"), (2, "SIGINT"), (1, "SIGHUP")] {
        let quit = quit.clone();
        glib::unix_signal_add_local(num, move || {
            write_line("INFO", &format!("received {name} — shutting down"));
            quit();
            glib::ControlFlow::Break
        });
    }
}

/// Close the session cleanly. The presence of this line is what tells you the
/// process exited rather than died.
pub fn shutdown(reason: &str) {
    write_line("INFO", &format!("--- session ended ({reason}) ---"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not three: it drives `XDG_STATE_HOME`, which is process-wide,
    /// so splitting it up would let the cases race each other.
    #[test]
    fn records_lines_rotates_and_catches_a_panic() {
        let dir = std::env::temp_dir().join("hedge-log-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_STATE_HOME", &dir);
        assert_eq!(path(), dir.join("hedge/hedge.log"));

        write_line("INFO", "hello");
        let text = std::fs::read_to_string(path()).unwrap();
        assert!(text.contains("INFO  hello"), "got: {text}");
        assert!(text.starts_with("20"), "line should start with a date: {text}");

        // over the size limit, the current file is kept as .1 and a new one starts
        std::fs::write(path(), vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        write_line("INFO", "after rotation");
        assert!(previous_path().exists(), "old log should be kept as .1");
        let text = std::fs::read_to_string(path()).unwrap();
        assert!(text.contains("after rotation"));
        assert!(!text.contains("xxxx"), "rotated file should be fresh");

        // the panic hook is the reason this module exists: a crash must leave
        // the reason, the source location and a backtrace behind
        init();
        let caught = std::panic::catch_unwind(|| panic!("boom for the test"));
        assert!(caught.is_err());
        let text = std::fs::read_to_string(path()).unwrap();
        assert!(text.contains("PANIC"), "no panic line in: {text}");
        assert!(text.contains("boom for the test"));
        assert!(text.contains("src/log.rs:"), "panic should carry its location");
        assert!(text.contains("backtrace:"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

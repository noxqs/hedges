//! Hedge — a zen, drag-friendly app launcher for the Linux desktop.
//!
//! Rust/GTK3 port of the Python `hedge_app` package. Module layout mirrors the
//! original one-to-one.

// First, out of alphabetical order on purpose: `#[macro_use]` only exports the
// `log!` macro to modules declared *after* it.
#[macro_use]
mod log;

mod app;
mod claude_mon;
mod clock;
mod colors;
mod config;
mod desktop;
mod desktop_apps;
mod desktop_click;
mod hedge_window;
mod install;
mod lock;
mod memfence;
mod ping;
mod settings_window;
mod snapshot;
mod sysmon;
mod sysmon_view;
mod version;

fn main() {
    std::process::exit(app::main());
}

//! The application: owns the config and every Hedge widget, plus the
//! single-instance command-line actions (`--settings`, `--add`).
//!
//! Port of `app.py`. Because GTK signal callbacks are `'static`, the shared
//! mutable state (config + live windows) lives behind an `Rc<RefCell<_>>`;
//! [`HedgeApp`] is a cheap clonable handle to it that the windows hold.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::config::{self, Config};
use crate::hedge_window::HedgeWindow;
use crate::settings_window::SettingsWindow;

pub const APP_ID: &str = "org.hedge.Hedge";

/// How a live hedge should refresh after a config change.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// CSS only — no layout churn.
    Style,
    /// Animated tile reflow.
    Layout,
    /// Full rebuild.
    Render,
}

/// A tile drag in flight from one hedge to another (or a reorder within one).
/// `(src_hedge, index, dst_hedge, insert_at)`.
pub type PendingMove = (String, usize, String, Option<usize>);

struct State {
    app: gtk::Application,
    cfg: Config,
    hedges: HashMap<String, HedgeWindow>,
    clock: Option<crate::clock::ClockWindow>,
    /// Host monitoring. Always exists; idle until the config switches it on.
    ping: crate::ping::Monitor,
    /// CPU/GPU/memory/network sampling, likewise idle until asked for.
    sysmon: crate::sysmon::Monitor,
    /// Claude Code sessions, plan usage and service status, likewise idle.
    claude: crate::claude_mon::Monitor,
    settings: Option<SettingsWindow>,
    pending_move: Option<PendingMove>,
    _indicator: Option<libappindicator::AppIndicator>,
    /// Single-instance lock, held for as long as this process runs.
    _lock: Option<crate::lock::Lock>,
    _hold: Option<gio::ApplicationHoldGuard>,
    _css: Option<gtk::CssProvider>,
    /// Peek mode: hedges and desktop icons temporarily out of the way.
    /// Runtime only — never written to the config, and always undone on quit.
    peeking: bool,
}

/// Clonable handle to the shared application state.
#[derive(Clone)]
pub struct HedgeApp(Rc<RefCell<State>>);

impl HedgeApp {
    pub fn new() -> Self {
        // HEDGE_APP_ID lets a dev instance register under a different D-Bus
        // name so it can run alongside another Hedge (e.g. the Python daemon).
        let app_id = std::env::var("HEDGE_APP_ID").unwrap_or_else(|_| APP_ID.to_string());
        let app = gtk::Application::builder()
            .application_id(&app_id)
            .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
            .build();

        let this = HedgeApp(Rc::new(RefCell::new(State {
            app: app.clone(),
            cfg: Config::default(),
            hedges: HashMap::new(),
            clock: None,
            ping: crate::ping::Monitor::new(),
            sysmon: crate::sysmon::Monitor::new(),
            claude: crate::claude_mon::Monitor::new(),
            settings: None,
            pending_move: None,
            _indicator: None,
            _lock: None,
            _hold: None,
            _css: None,
            peeking: false,
        })));

        // every finished round repaints the desklet panel
        let s = this.clone();
        this.0.borrow().ping.connect_update(move || s.refresh_clock_ping());
        let s = this.clone();
        this.0
            .borrow()
            .sysmon
            .connect_update(move || s.on_sysmon_sample());
        let s = this.clone();
        this.0
            .borrow()
            .sysmon
            .connect_mem_warning(move |eta| s.on_mem_warning(eta));
        let s = this.clone();
        this.0
            .borrow()
            .claude
            .connect_update(move || s.refresh_clock_claude());

        let s = this.clone();
        app.connect_startup(move |_| s.on_startup());
        let s = this.clone();
        app.connect_activate(move |_| s.on_activate());
        let s = this.clone();
        app.connect_command_line(move |_, cmdline| s.on_command_line(cmdline));
        // quitting mid-peek must not leave the desktop icons switched off
        let s = this.clone();
        app.connect_shutdown(move |_| s.set_peeking(false));

        this
    }

    pub fn run(&self) -> i32 {
        // gtk::Application::run reads std::env::args itself.
        let app = self.0.borrow().app.clone();
        let app_id = app.application_id().map(|s| s.to_string()).unwrap_or_default();

        // The lock comes first, and is the authority on "is one already
        // running". A second copy that slips past the D-Bus name — a different
        // session bus, a stale name, a hand-started binary — would otherwise
        // run as a second daemon over the same config.json, and the last one
        // to save wins.
        // A copy that has just asked the previous instance to quit is racing
        // that instance's shutdown, so it waits rather than refusing.
        let attempts = if std::env::var_os("HEDGE_TOOK_OVER").is_some() {
            24
        } else {
            1
        };
        let held = match crate::lock::acquire_retrying(&app_id, attempts) {
            Ok(lock) => {
                self.0.borrow_mut()._lock = Some(lock);
                None
            }
            Err(holder) => {
                log!(
                    "another Hedge ({}) already holds the lock for {app_id}",
                    holder
                        .pid
                        .map(|p| format!("pid {p}"))
                        .unwrap_or_else(|| "unknown pid".into())
                );
                Some(holder)
            }
        };

        // Ask *before* running whether someone else already owns our D-Bus
        // name: run() unregisters on the way out, so is_remote() is only
        // answerable while registered. register() is what run() would do
        // anyway, and is idempotent.
        let reg = app.register(gio::Cancellable::NONE);
        let handed_over = reg.is_ok() && app.is_remote();

        // Lock held but D-Bus says we're the only one: the two disagree, and
        // the lock is the one that can't be fooled. Refuse rather than start a
        // second daemon whose saves would fight the first one's.
        if let (Some(holder), false) = (&held, handed_over) {
            let who = holder
                .pid
                .map(|p| format!("(pid {p}) "))
                .unwrap_or_default();
            let msg = format!(
                "Another Hedge {who}is already running.\n\n\
                 Quit it from its tray menu (Hedges → Quit) first. To run this build \
                 alongside it instead, give it an identity of its own:\n  \
                 HEDGE_APP_ID=org.hedge.HedgeDev XDG_CONFIG_HOME=/tmp/hedge-dev {}",
                std::env::args().next().unwrap_or_else(|| "hedge".into()),
            );
            log!(WARN, "refusing to start: the lock for {app_id} is held");
            if gtk::init().is_ok() {
                self.tell(None, gtk::MessageType::Warning, "Hedge is already running", &msg);
            } else {
                println!("hedge: {msg}");
            }
            return 1;
        }
        if std::env::var_os("HEDGE_DEBUG").is_some() {
            log!("startup: register={:?} is_remote={} args={} handed_over={handed_over}",
                 reg.is_ok(), app.is_remote(), std::env::args().len());
        }

        // A bare second launch otherwise does nothing visible: GApplication
        // hands the command line to whichever process already owns the name and
        // this one exits 0 having shown nothing. Ask what the user wanted.
        // Flagged launches (`--settings`, `--add`, …) are *meant* to be
        // forwarded, so they fall straight through.
        if handed_over && std::env::args().len() <= 1 && self.offer_take_over(&app) {
            return 0; // taking over: a fresh copy is already on its way up
        }

        let code = app.run().into();

        if handed_over && std::env::args().len() <= 1 {
            println!(
                "hedge: a Hedge is already running, so this build handed over to it and exited.\n\
                 Quit that one first, or give this build an identity of its own:\n  \
                 HEDGE_APP_ID=org.hedge.HedgeDev XDG_CONFIG_HOME=/tmp/hedge-dev {}",
                std::env::args().next().unwrap_or_else(|| "hedge".into()),
            );
        }
        code
    }

    /// Another Hedge owns our D-Bus name. Offer to replace it.
    ///
    /// Returns true when the caller should just exit: the running instance has
    /// been asked to quit and a fresh copy of *this* binary is scheduled to
    /// start once the name is free. We can't simply carry on in this process —
    /// GApplication has already registered us as the remote end and there is no
    /// way back from that — so the takeover is done by re-exec, the same trick
    /// the post-upgrade restart uses.
    fn offer_take_over(&self, app: &gtk::Application) -> bool {
        // Second time around: the re-exec still found someone holding the name,
        // so stop rather than bounce between copies forever.
        if std::env::var_os("HEDGE_TOOK_OVER").is_some() {
            let msg = "The running Hedge did not stop, so this copy did not start.\n\n\
                       Quit it from its tray menu (Hedges → Quit) and try again. An older \
                       build won't understand the request to stand down.";
            log!(WARN, "take-over failed — the previous instance still owns the name");
            if gtk::init().is_ok() {
                self.tell(None, gtk::MessageType::Warning, "Hedge is still running", msg);
            } else {
                println!("hedge: {msg}");
            }
            return true;
        }

        // No display: keep the old console behaviour rather than hanging.
        if gtk::init().is_err() {
            return false;
        }

        let body = format!(
            "A Hedge is already running, and only one can manage your hedges at a time.\n\n\
             This copy:  {}\n{}\n\n\
             Closing the running one and starting this copy takes a couple of seconds. \
             Your hedges are saved either way.",
            crate::version::current().label(),
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        if !self.ask(
            None,
            "Hedge is already running",
            &body,
            "Close it and start this one",
        ) {
            log!("second instance: kept the running Hedge");
            return false;
        }

        log!("second instance: asking the running Hedge to quit, then restarting");
        app.activate_action("quit", None);
        spawn_delayed_self();
        true
    }

    // -- shared accessors ---------------------------------------------------
    /// Read a copy of a hedge block (fields are cheap; apps clone too).
    pub fn hedge(&self, name: &str) -> Option<config::Hedge> {
        self.0.borrow().cfg.hedges.get(name).cloned()
    }

    /// Run `f` with a mutable reference to a hedge block, if it exists.
    pub fn with_hedge_mut<R>(&self, name: &str, f: impl FnOnce(&mut config::Hedge) -> R) -> Option<R> {
        let mut st = self.0.borrow_mut();
        st.cfg.hedges.get_mut(name).map(f)
    }

    /// Run `f` with a mutable reference to the whole config.
    pub fn with_cfg_mut<R>(&self, f: impl FnOnce(&mut Config) -> R) -> R {
        let mut st = self.0.borrow_mut();
        f(&mut st.cfg)
    }

    /// A cloned snapshot of the whole config (for read-only iteration).
    pub fn cfg_snapshot(&self) -> Config {
        self.0.borrow().cfg.clone()
    }

    pub fn gtk_app(&self) -> gtk::Application {
        self.0.borrow().app.clone()
    }

    pub fn take_pending_move(&self) -> Option<PendingMove> {
        self.0.borrow_mut().pending_move.take()
    }

    pub fn set_pending_move(&self, m: Option<PendingMove>) {
        self.0.borrow_mut().pending_move = m;
    }

    // -- lifecycle ----------------------------------------------------------
    fn on_startup(&self) {
        {
            let mut st = self.0.borrow_mut();
            st.cfg = config::load();
        }
        let _ = config::make_backup(); // snapshot the config we started from
        self.repair_positions();
        self.load_base_css();
        self.sync_clock();
        self.sync_ping();
        self.sync_sysmon();
        self.sync_claude();
        self.build_tray();
        // double-click empty desktop -> toggle the desktop icons
        if !crate::desktop_click::spawn(self) {
            log!(WARN, "desktop double-click unavailable");
        }
        // A "quit" action, exported over D-Bus by GApplication for free: it is
        // how a second copy of Hedge asks this one to stand down (see
        // `offer_take_over`). Nothing else uses it.
        let act = gio::SimpleAction::new("quit", None);
        let this = self.clone();
        act.connect_activate(move |_, _| {
            log!("another instance asked us to quit");
            this.gtk_app().quit();
        });
        self.0.borrow().app.add_action(&act);

        // stay alive as a desktop daemon even with no visible top-level
        let guard = self.0.borrow().app.hold();
        self.0.borrow_mut()._hold = Some(guard);
        // log-out sends SIGTERM; go through quit() so the log records it and
        // shutdown handlers (peek restore) still run
        let this = self.clone();
        crate::log::watch_exit_signals(move || this.gtk_app().quit());
        // ask about installing/upgrading once the desktop has settled — at
        // login this runs while the panel is still filling up
        let this = self.clone();
        glib::timeout_add_seconds_local_once(3, move || this.offer_install_if_useful());
    }

    fn on_activate(&self) {
        // NB: read the flag into a local first — a borrow() temporary in an
        // `if` condition lives through the block, and sync_hedges borrows_mut.
        let empty = self.0.borrow().hedges.is_empty();
        if empty {
            self.sync_hedges();
            // once the windows exist and have real sizes, pull apart anything
            // that is already overlapping
            let this = self.clone();
            glib::idle_add_local_once(move || this.separate_hedges());
        }
    }

    fn on_command_line(&self, cmdline: &gio::ApplicationCommandLine) -> i32 {
        // clone the handle out before activate(), which re-enters on_activate
        let app = self.gtk_app();
        app.activate();
        let args = cmdline.arguments();
        let has = |flag: &str| args.iter().any(|a| a.to_string_lossy() == flag);
        if has("--add") {
            self.add_hedge(true);
        }
        if has("--settings") {
            self.open_settings();
        }
        // save only: restoring moves real desktop icons, so it stays behind
        // the confirmation dialog in the settings window
        if has("--save-desktop") {
            self.save_desktop_snapshot();
        }
        if has("--recall") {
            self.recall_all_hedges();
        }
        if has("--toggle-desktop") {
            self.toggle_desktop();
        }
        if has("--hide-desktop") {
            self.hide_desktop();
        }
        if has("--show-desktop") {
            self.show_desktop();
        }

        0
    }

    fn load_base_css(&self) {
        let provider = gtk::CssProvider::new();
        if let Err(e) = provider.load_from_data(include_bytes!("style.css")) {
            log!(WARN, "css load failed: {e}");
            return;
        }
        if let Some(screen) = gdk::Screen::default() {
            gtk::StyleContext::add_provider_for_screen(
                &screen,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        self.0.borrow_mut()._css = Some(provider);
    }

    // -- system tray icon ---------------------------------------------------
    fn build_tray(&self) {
        use libappindicator::{AppIndicator, AppIndicatorStatus};

        let menu = gtk::Menu::new();

        let title = gtk::MenuItem::with_label("Hedges");
        title.set_sensitive(false);
        menu.append(&title);
        menu.append(&gtk::SeparatorMenuItem::new());

        let add_item = |label: &str, menu: &gtk::Menu, cb: Box<dyn Fn()>| {
            let mi = gtk::MenuItem::with_label(label);
            mi.connect_activate(move |_| cb());
            menu.append(&mi);
        };

        // Config first: AppIndicator gives us no click/double-click hook (the
        // StatusNotifierItem protocol is menu-only), so the fastest possible
        // route to settings is having it at the top of the menu.
        let s = self.clone();
        add_item("Config…", &menu, Box::new(move || s.open_settings()));
        let s = self.clone();
        add_item("Add a Hedge", &menu, Box::new(move || {
            s.add_hedge(false);
        }));
        menu.append(&gtk::SeparatorMenuItem::new());
        let s = self.clone();
        add_item("Toggle desktop icons", &menu, Box::new(move || s.toggle_desktop()));
        menu.append(&gtk::SeparatorMenuItem::new());
        let s = self.clone();
        add_item("Back up config now", &menu, Box::new(move || s.backup_now()));
        let s = self.clone();
        add_item("Restore last backup", &menu, Box::new(move || s.restore_last()));
        menu.append(&gtk::SeparatorMenuItem::new());
        let s = self.clone();
        add_item("About…", &menu, Box::new(move || s.show_about()));
        menu.append(&gtk::SeparatorMenuItem::new());
        let s = self.clone();
        add_item("Quit", &menu, Box::new(move || s.0.borrow().app.quit()));
        menu.show_all();

        // NB: a tray double-click is not detectable, and we no longer pretend
        // otherwise. StatusNotifierItem has no click concept at all, and the
        // menu below is exported over DBusMenu as a *model* — the panel renders
        // its own menu from it, so this GtkMenu is never mapped and gives us
        // nothing to observe. Watching the X RECORD tap for a double-click on
        // the panel strut was tried and removed: nothing at the X level can
        // tell our icon from any other, so it fired on every panel
        // double-click. Use the menu, or `hedge --settings` on a keybinding.

        // ship the bundled icon into a cache dir so the indicator can theme it
        let icon_dir = config::config_dir().join("icons");
        let _ = std::fs::create_dir_all(&icon_dir);
        let _ = std::fs::write(icon_dir.join("hedge.svg"), include_bytes!("../assets/icons/hedge.svg"));

        let mut indicator = AppIndicator::new("hedge", "hedge");
        indicator.set_icon_theme_path(icon_dir.to_str().unwrap_or("."));
        indicator.set_status(AppIndicatorStatus::Active);
        indicator.set_icon_full("hedge", "Hedges");
        indicator.set_menu(&mut menu.clone());

        self.0.borrow_mut()._indicator = Some(indicator);
    }

    // -- hedge management ---------------------------------------------------
    /// Make the live Hedge windows match the config.
    pub fn sync_hedges(&self) {
        let names: Vec<String> = self.0.borrow().cfg.hedges.keys().cloned().collect();
        let nameset: std::collections::HashSet<&String> = names.iter().collect();

        let gone: Vec<String> = self
            .0
            .borrow()
            .hedges
            .keys()
            .filter(|n| !nameset.contains(*n))
            .cloned()
            .collect();
        for name in gone {
            if let Some(w) = self.0.borrow_mut().hedges.remove(&name) {
                w.destroy();
            }
        }

        for name in names {
            let existing = self.0.borrow().hedges.get(&name).cloned();
            match existing {
                Some(w) => w.render(),
                None => {
                    let w = HedgeWindow::new(self, name.clone());
                    self.0.borrow_mut().hedges.insert(name, w);
                }
            }
        }
    }

    pub fn refresh_hedge(&self, name: &str, mode: Refresh) {
        let w = self.0.borrow().hedges.get(name).cloned();
        if let Some(w) = w {
            match mode {
                Refresh::Style => w.restyle(),
                Refresh::Layout => w.relayout(),
                Refresh::Render => w.render(),
            }
        }
    }

    /// Rename a hedge from anywhere, keeping the live window and settings in
    /// sync. Returns true on success.
    pub fn rename_hedge_live(&self, old: &str, new: &str) -> bool {
        let new = new.trim();
        if new.is_empty() || new == old {
            return false;
        }
        {
            let mut st = self.0.borrow_mut();
            if !config::rename_hedge(&mut st.cfg, old, new) {
                return false;
            }
        }
        self.save();

        let w = self.0.borrow_mut().hedges.remove(old);
        if let Some(w) = w {
            w.set_name(new);
            self.0.borrow_mut().hedges.insert(new.to_string(), w.clone());
            w.render();
        }

        let settings = self.0.borrow().settings.clone();
        if let Some(settings) = settings {
            if settings.is_visible() {
                settings.on_hedges_changed(Some(new));
            }
        }
        true
    }

    /// Complete a tile drag from hedge `src` to `dst` (same hedge = reorder),
    /// inserting at slot `insert_at`.
    pub fn finish_move(&self, src: &str, index: usize, dst: &str, insert_at: Option<usize>) {
        // pop the entry from the source
        let entry = self.with_hedge_mut(src, |f| {
            if index < f.apps.len() {
                Some(f.apps.remove(index))
            } else {
                None
            }
        });
        let entry = match entry.flatten() {
            Some(e) => e,
            None => return,
        };

        if src == dst {
            self.with_hedge_mut(src, |f| {
                let mut pos = insert_at.unwrap_or(f.apps.len());
                if pos > index {
                    pos -= 1; // account for the popped entry
                }
                let pos = pos.min(f.apps.len());
                f.apps.insert(pos, entry);
            });
            self.save();
            self.refresh_hedge(src, Refresh::Render);
            return;
        }

        let inserted = self.with_hedge_mut(dst, |f| {
            let pos = insert_at.map(|p| p.min(f.apps.len())).unwrap_or(f.apps.len());
            f.apps.insert(pos, entry.clone());
        });
        if inserted.is_none() {
            // destination vanished; revert
            self.with_hedge_mut(src, |f| {
                let pos = index.min(f.apps.len());
                f.apps.insert(pos, entry);
            });
            return;
        }
        self.save();
        self.refresh_hedge(src, Refresh::Render);
        self.refresh_hedge(dst, Refresh::Render);
    }

    /// Rects every hedge except `exclude` occupies on screen. Docked hedges
    /// are included via their visible sliver so a floating hedge can't be
    /// dropped on top of a dock handle.
    pub fn other_hedge_rects(&self, exclude: &str) -> Vec<crate::hedge_window::Rect> {
        let hedges: Vec<(String, HedgeWindow)> = self
            .0
            .borrow()
            .hedges
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        hedges
            .into_iter()
            .filter(|(n, _)| n != exclude)
            .map(|(_, w)| w.collision_rect())
            .collect()
    }

    /// Reveal (or re-hide) every hide-until-hover hedge, so a drag isn't
    /// fought against invisible neighbours.
    pub fn reveal_hidden_hedges(&self, reveal: bool) {
        let hedges: Vec<HedgeWindow> = self.0.borrow().hedges.values().cloned().collect();
        if std::env::var("HEDGE_DEBUG").is_ok() {
            log!("reveal_hidden_hedges({reveal}) over {} hedge(s)", hedges.len());
        }
        for w in hedges {
            w.reveal_for_drag(reveal);
        }
    }

    /// Pull apart any hedges that already overlap (config edited by hand, a
    /// monitor disappeared, a hedge grew). Runs once after the windows exist.
    pub fn separate_hedges(&self) {
        let hedges: Vec<(String, HedgeWindow)> = self
            .0
            .borrow()
            .hedges
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Repeat until nothing moves: one pass is not enough, because pushing
        // a hedge clear of one neighbour can drop it onto a hedge that was
        // already processed. Bounded so a layout that cannot be satisfied
        // (more hedges than fit) settles instead of looping.
        let mut changed_any = false;
        for _ in 0..6 {
            let mut moved_this_pass = false;
            for (name, w) in &hedges {
                if w.is_collapsed() {
                    continue;
                }
                let (x, y, fw, fh) = w.rect();
                // docked hedges act as obstacles but are never themselves
                // moved — shoving one off its edge would silently undock it
                let others: Vec<crate::hedge_window::Rect> = hedges
                    .iter()
                    .filter(|(n, _)| n != name)
                    .map(|(_, o)| o.collision_rect())
                    .collect();
                let (nx, ny) = crate::hedge_window::resolve_overlap(x, y, fw, fh, &others);
                if (nx, ny) != (x, y) {
                    w.move_window(nx, ny);
                    self.with_hedge_mut(name, |f| {
                        f.x = nx as i64;
                        f.y = ny as i64;
                    });
                    moved_this_pass = true;
                }
            }
            changed_any |= moved_this_pass;
            if !moved_this_pass {
                break;
            }
        }
        if changed_any {
            self.save();
        }
    }

    /// Bring every hedge back onto the primary monitor, undocked and unrolled,
    /// tiled left-to-right from the top-left of the work area.
    ///
    /// The blunt recovery for "where did my hedges go" — an unplugged monitor,
    /// a hand-edited config, or a hedge docked somewhere unreachable.
    pub fn recall_all_hedges(&self) {
        let area = gdk::Display::default()
            .and_then(|d| d.primary_monitor().or_else(|| d.monitor(0)))
            .map(|m| m.workarea())
            .unwrap_or_else(|| gdk::Rectangle::new(0, 0, 1920, 1080));
        let (margin, gap) = (40, 24);
        let (left, top) = (area.x() + margin, area.y() + margin);
        let right = area.x() + area.width() - margin;

        let hedges: Vec<(String, HedgeWindow)> = self
            .0
            .borrow()
            .hedges
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let (mut x, mut y, mut row_h) = (left, top, 0);
        for (name, w) in hedges {
            // undock/unroll first, so the size we measure is the floating one
            w.place_at(x, y);
            let (_, _, mut fw, mut fh) = w.rect();
            if x > left && x + fw > right {
                // wrap to the next row and re-measure: undocking can change size
                x = left;
                y += row_h + gap;
                row_h = 0;
                w.place_at(x, y);
                let r = w.rect();
                fw = r.2;
                fh = r.3;
            }
            self.with_hedge_mut(&name, |f| {
                f.x = x as i64;
                f.y = y as i64;
                f.edge = None;
                f.rolled = false;
            });
            row_h = row_h.max(fh);
            x += fw + gap;
        }
        self.save();
        self.separate_hedges();
    }

    /// Put a hedge back in the middle of the primary monitor, undocked — the
    /// way out when one has ended up off-screen and can't be dragged back.
    pub fn center_hedge(&self, name: &str) {
        let w = self.0.borrow().hedges.get(name).cloned();
        let (fw, fh) = w
            .as_ref()
            .map(|w| {
                let r = w.rect();
                (r.2, r.3)
            })
            .unwrap_or((320, 200));
        let area = gdk::Display::default()
            .and_then(|d| d.primary_monitor().or_else(|| d.monitor(0)))
            .map(|m| m.workarea())
            .unwrap_or_else(|| gdk::Rectangle::new(0, 0, 1920, 1080));
        let x = area.x() + (area.width() - fw).max(0) / 2;
        let y = area.y() + (area.height() - fh).max(0) / 2;
        self.with_hedge_mut(name, |f| {
            f.x = x as i64;
            f.y = y as i64;
            f.edge = None;
            f.rolled = false;
        });
        self.save();
        if let Some(w) = w {
            w.place_at(x, y);
        }
        self.separate_hedges();
    }

    pub fn add_hedge(&self, at_pointer: bool) -> String {
        let name = {
            let mut st = self.0.borrow_mut();
            // no starter apps: a hedge you just made should be an empty
            // frame to drag things into, not something to clear out first
            config::add_hedge(&mut st.cfg, None, false)
        };
        let (mut x, mut y) = self.default_spot();
        if at_pointer {
            if let Some((px, py)) = self.pointer() {
                x = px - 12;
                y = py - 12;
            }
        }
        let (x, y) = self.clamp_to_monitors(x, y);
        self.with_hedge_mut(&name, |f| {
            f.x = x;
            f.y = y;
        });
        self.save();
        self.sync_hedges();
        let settings = self.0.borrow().settings.clone();
        if let Some(settings) = settings {
            settings.refresh_hedge_list();
        }
        name
    }

    // -- monitor-aware positioning -----------------------------------------
    fn monitor_rects(&self) -> Vec<gdk::Rectangle> {
        let display = match gdk::Display::default() {
            Some(d) => d,
            None => return Vec::new(),
        };
        (0..display.n_monitors())
            .filter_map(|i| display.monitor(i))
            .map(|m| m.geometry())
            .collect()
    }

    fn default_spot(&self) -> (i64, i64) {
        if let Some(display) = gdk::Display::default() {
            let mon = display.primary_monitor().or_else(|| display.monitor(0));
            if let Some(mon) = mon {
                let g = mon.geometry();
                return (g.x() as i64 + 80, g.y() as i64 + 80);
            }
        }
        (80, 80)
    }

    fn clamp_to_monitors(&self, x: i64, y: i64) -> (i64, i64) {
        let rects = self.monitor_rects();
        if rects.is_empty() {
            return (x.max(0), y.max(0));
        }
        let inside = rects.iter().any(|r| {
            let (rx, ry, rw, rh) = (r.x() as i64, r.y() as i64, r.width() as i64, r.height() as i64);
            rx <= x && x < rx + rw && ry <= y && y < ry + rh
        });
        if inside {
            (x, y)
        } else {
            self.default_spot()
        }
    }

    /// Nudge floating hedges that ended up outside every monitor back into
    /// view (clamped, never teleported). Docked hedges are left alone.
    fn repair_positions(&self) {
        let rects = self.monitor_rects();
        if rects.is_empty() {
            return;
        }
        let min_x = rects.iter().map(|r| r.x() as i64).min().unwrap();
        let max_x = rects.iter().map(|r| (r.x() + r.width()) as i64).max().unwrap();
        let min_y = rects.iter().map(|r| r.y() as i64).min().unwrap();
        let max_y = rects.iter().map(|r| (r.y() + r.height()) as i64).max().unwrap();

        let mut changed = false;
        {
            let mut st = self.0.borrow_mut();
            for hedge in st.cfg.hedges.values_mut() {
                if hedge.edge.is_some() {
                    continue;
                }
                let nx = hedge.x.clamp(min_x, max_x - 60);
                let ny = hedge.y.clamp(min_y, max_y - 60);
                if (nx, ny) != (hedge.x, hedge.y) {
                    hedge.x = nx;
                    hedge.y = ny;
                    changed = true;
                }
            }
        }
        if changed {
            self.save();
        }
    }

    fn pointer(&self) -> Option<(i64, i64)> {
        let display = gdk::Display::default()?;
        let seat = display.default_seat()?;
        let device = seat.pointer()?;
        let (_screen, x, y) = device.position();
        Some((x as i64, y as i64))
    }

    // -- shared services for windows ---------------------------------------
    pub fn save(&self) {
        let st = self.0.borrow();
        if let Err(e) = config::save(&st.cfg) {
            log!(WARN, "save failed: {e}");
        }
    }

    pub fn launch(&self, command: &str) {
        if command.is_empty() {
            return;
        }
        // each app in its own scope, out of the session whose memory is
        // reserved for the desktop
        if !self.0.borrow().cfg.launch_unscoped {
            if let Some(scoped) = crate::memfence::scoped_command(command) {
                if glib::spawn_command_line_async(&scoped).is_ok() {
                    return;
                }
                log!(WARN, "scoped launch failed, launching directly: {command}");
            }
        }
        if glib::spawn_command_line_async(command).is_err() {
            // fallback: run it through a shell
            if let Err(e) = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .spawn()
            {
                log!(WARN, "launch failed: {command} ({e})");
            }
        }
    }

    // -- desktop icon hiding ------------------------------------------------
    pub fn toggle_desktop(&self) {
        let hidden = self.0.borrow().cfg.desktop_hidden;
        if hidden {
            self.show_desktop();
        } else {
            self.hide_desktop();
        }
    }

    pub fn hide_desktop(&self) {
        match crate::desktop::hide() {
            Ok(n) => {
                self.0.borrow_mut().cfg.desktop_hidden = true;
                self.save();
                self.refresh_settings_desktop();
                log!("hid {n} desktop item(s)");
            }
            Err(e) => log!(WARN, "hide desktop failed: {e}"),
        }
    }

    pub fn show_desktop(&self) {
        match crate::desktop::show() {
            Ok(n) => {
                self.0.borrow_mut().cfg.desktop_hidden = false;
                self.save();
                self.refresh_settings_desktop();
                log!("restored {n} desktop item(s)");
            }
            Err(e) => log!(WARN, "show desktop failed: {e}"),
        }
    }

    /// Keep the Desktop settings page honest when the tray, the hotkey or a
    /// double-click on the desktop is what changed things.
    fn refresh_settings_desktop(&self) {
        let settings = self.0.borrow().settings.clone();
        if let Some(s) = settings {
            s.refresh_desktop_page();
        }
    }

    pub fn backup_now(&self) {
        match config::make_backup() {
            Ok(Some(p)) => log!("backed up config -> {}", p.display()),
            Ok(None) => log!("nothing to back up yet"),
            Err(e) => log!(WARN, "backup failed: {e}"),
        }
    }

    pub fn restore_last(&self) {
        match config::restore_latest() {
            Ok(true) => {
                let cfg = config::load();
                self.0.borrow_mut().cfg = cfg;
                self.sync_hedges();
                let settings = self.0.borrow().settings.clone();
                if let Some(settings) = settings {
                    settings.on_hedges_changed(None);
                }
                log!("restored most recent backup");
            }
            Ok(false) => log!("no backup to restore"),
            Err(e) => log!(WARN, "restore failed: {e}"),
        }
    }

    // -- peek (double-click the desktop) ------------------------------------
    /// Nemo's own switch for desktop icons. Unlike our stash, this hides them
    /// instantly and moves nothing, which is what a temporary peek wants.
    fn nemo_desktop_settings() -> Option<gio::Settings> {
        let src = gio::SettingsSchemaSource::default()?;
        // lookup first: gio::Settings::new aborts on a missing schema
        src.lookup("org.nemo.desktop", true)?;
        Some(gio::Settings::new("org.nemo.desktop"))
    }

    pub fn is_peeking(&self) -> bool {
        self.0.borrow().peeking
    }

    /// Show/hide every hedge and the desktop icons together, so a double-click
    /// on the desktop clears the screen and a second one puts it all back.
    pub fn set_peeking(&self, peeking: bool) {
        if self.0.borrow().peeking == peeking {
            return;
        }
        self.0.borrow_mut().peeking = peeking;

        let hedges: Vec<HedgeWindow> = self.0.borrow().hedges.values().cloned().collect();
        for w in hedges {
            w.set_visible(!peeking);
        }
        // the clock is desktop furniture too — leaving it floating over a
        // "show me the desktop" peek would look like a bug
        let clock = self.0.borrow().clock.clone();
        if let Some(c) = clock {
            c.set_visible(!peeking);
        }
        if let Some(settings) = Self::nemo_desktop_settings() {
            let _ = settings.set_boolean("show-desktop-icons", !peeking);
        }
        eprintln!(
            "hedge: {} desktop",
            if peeking { "peeking at" } else { "restored" }
        );
    }

    /// Make the live clock desklet match the config: create it, drop it, or
    /// refresh it. Safe to call after any change to the `clock` block.
    pub fn sync_clock(&self) {
        let want = self.0.borrow().cfg.clock.enabled;
        let have = self.0.borrow().clock.clone();
        match (want, have) {
            (true, Some(c)) => c.refresh(),
            (true, None) => {
                let c = crate::clock::ClockWindow::new(self);
                self.0.borrow_mut().clock = Some(c);
            }
            (false, Some(c)) => {
                self.0.borrow_mut().clock = None;
                c.close();
            }
            (false, None) => {}
        }
    }

    // -- host monitoring ----------------------------------------------------
    pub fn ping_monitor(&self) -> crate::ping::Monitor {
        self.0.borrow().ping.clone()
    }

    /// Start/stop/re-time the poller to match the config and repaint the panel.
    /// Safe to call after any change to the `ping` block.
    pub fn sync_ping(&self) {
        let (cfg, monitor) = {
            let st = self.0.borrow();
            (st.cfg.ping.clone(), st.ping.clone())
        };
        monitor.sync(&cfg);
        self.refresh_clock_ping();
    }

    fn refresh_clock_ping(&self) {
        let clock = self.0.borrow().clock.clone();
        if let Some(c) = clock {
            c.refresh_panel();
        }
    }

    pub fn sysmon_monitor(&self) -> crate::sysmon::Monitor {
        self.0.borrow().sysmon.clone()
    }

    /// Start/stop/re-time the sampler to match the config and rebuild the
    /// cards, which is what a section being switched on or off changes.
    pub fn sync_sysmon(&self) {
        let (cfg, monitor) = {
            let st = self.0.borrow();
            (st.cfg.sysmon.clone(), st.sysmon.clone())
        };
        monitor.sync(&cfg);
        let clock = self.0.borrow().clock.clone();
        if let Some(c) = clock {
            c.refresh_panel();
        }
    }

    /// The memory forecast crossed into (`Some`) or out of (`None`) the
    /// warning horizon. A desktop notification rather than anything on the
    /// desklet: the desklet sits under every window, and by the time anyone
    /// looks at it the machine may already be past saving.
    fn on_mem_warning(&self, eta: Option<f64>) {
        const ID: &str = "memory-running-out";
        let app = self.gtk_app();
        let Some(secs) = eta else {
            app.withdraw_notification(ID);
            return;
        };
        let mut body = "At the current rate of climb.".to_string();
        if let Some((name, kb)) = crate::sysmon::biggest_process() {
            body = format!(
                "{body} Largest process: {name} ({}).",
                crate::sysmon::human_kb(kb)
            );
        }
        let title = format!("Memory runs out in ~{}", crate::sysmon::human_eta(secs));
        log!(WARN, "{title}. {body}");
        let n = gio::Notification::new(&title);
        n.set_body(Some(&body));
        n.set_priority(gio::NotificationPriority::Urgent);
        n.set_icon(&gio::ThemedIcon::new("dialog-warning"));
        app.send_notification(Some(ID), &n);
    }

    fn on_sysmon_sample(&self) {
        let clock = self.0.borrow().clock.clone();
        if let Some(c) = clock {
            c.on_sysmon_sample();
        }
    }

    pub fn claude_monitor(&self) -> crate::claude_mon::Monitor {
        self.0.borrow().claude.clone()
    }

    /// Start/stop/re-scope the Claude pollers to match the config and rebuild
    /// the section. Safe to call after any change to the `claude` block.
    pub fn sync_claude(&self) {
        let (cfg, monitor) = {
            let st = self.0.borrow();
            (st.cfg.claude.clone(), st.claude.clone())
        };
        monitor.sync(&cfg);
        self.refresh_clock_ping();
    }

    /// A Claude poll landed: redraw just that section, not the whole panel.
    fn refresh_clock_claude(&self) {
        let clock = self.0.borrow().clock.clone();
        if let Some(c) = clock {
            c.refresh_claude();
        }
    }

    pub fn toggle_peek(&self) {
        self.set_peeking(!self.is_peeking());
    }

    // -- desktop snapshot ---------------------------------------------------
    /// Capture every hedge plus the desktop icon layout.
    pub fn save_desktop_snapshot(&self) -> Option<crate::snapshot::Snapshot> {
        let cfg = self.cfg_snapshot();
        match crate::snapshot::save(&cfg) {
            Ok(s) => {
                eprintln!(
                    "hedge: saved snapshot — {} hedge(s), {} desktop icon(s)",
                    s.config.hedges.len(),
                    s.desktop.len()
                );
                Some(s)
            }
            Err(e) => {
                log!(WARN, "snapshot failed: {e}");
                None
            }
        }
    }

    /// Put the hedges and the desktop icons back the way the snapshot had them.
    /// The current config is backed up first, so this is itself undoable via
    /// "Restore last backup".
    pub fn restore_desktop_snapshot(&self) -> bool {
        let snap = match crate::snapshot::load() {
            Ok(s) => s,
            Err(e) => {
                log!(WARN, "no snapshot to restore ({e})");
                return false;
            }
        };
        let _ = config::make_backup();
        let icons = crate::snapshot::apply_desktop(&snap.desktop);
        self.0.borrow_mut().cfg = snap.config;
        self.save();
        self.sync_hedges();
        let settings = self.0.borrow().settings.clone();
        if let Some(settings) = settings {
            settings.on_hedges_changed(None);
        }
        log!("restored snapshot — {icons} desktop icon(s) repositioned");
        true
    }

    pub fn open_settings(&self) {
        let existing = self.0.borrow().settings.clone();
        let settings = match existing {
            Some(s) => s,
            None => {
                let s = SettingsWindow::new(self);
                self.0.borrow_mut().settings = Some(s.clone());
                s
            }
        };
        settings.present();
    }

    /// Settings, opened on the Desklet page. The desklet's gear icon is a
    /// shortcut to its *own* settings, so landing on whichever page was last
    /// looked at would miss the point.
    pub fn open_desklet_settings(&self) {
        self.open_settings();
        let settings = self.0.borrow().settings.clone();
        if let Some(s) = settings {
            s.show_desklet_page();
        }
    }

    // -- install / upgrade / uninstall --------------------------------------
    /// Startup nudge, a few seconds after login so it doesn't race the desktop.
    ///
    /// Only ever fires for a binary that *isn't* the installed one — a dev
    /// build or a freshly unpacked tarball — so the installed daemon never
    /// nags. "Never" is remembered; Settings keeps offering both regardless.
    fn offer_install_if_useful(&self) {
        // A dev instance running under its own app id is a scratch copy by
        // definition — it must not nag about installing itself over the real
        // one every time it is launched.
        if std::env::var_os("HEDGE_APP_ID").is_some() {
            return;
        }
        let st = crate::install::install_state_if_prompt_worthwhile();
        let Some((st, kind)) = st else { return };
        // NB: read the flag out before opening a dialog that re-enters us.
        let dismissed = self.0.borrow().cfg.install_prompt_dismissed;
        if dismissed {
            return;
        }

        let exe = std::env::current_exe().unwrap_or_default();
        // The decline button names the alternative rather than deferring:
        // running uninstalled is a supported way to use Hedge, not a "not yet".
        let (heading, detail, accept, decline) = match kind {
            crate::install::Offer::Install => (
                "Install Hedge, or run it standalone?",
                format!(
                    "You're running Hedge {} from\n{}\n\nInstalling copies it to {}, adds the \
                     autostart entry, the “Hedge Settings” menu item and the desktop right-click \
                     “Add a Hedge” action, and restarts Hedge from the installed copy. Everything \
                     goes in your home directory — no root, no package manager.\n\nStandalone \
                     works exactly the same, but only while this process runs: no autostart, no \
                     menu entry, no right-click actions. Your hedges are saved either way.",
                    st.running.label(),
                    exe.display(),
                    crate::install::binary_path().display(),
                ),
                "Install & restart",
                "Run standalone",
            ),
            crate::install::Offer::Upgrade => (
                "Upgrade the installed Hedge?",
                format!(
                    "This build:  {}\nInstalled:   {}\n\nThe build you're running is newer. \
                     Upgrading replaces the installed copy, refreshes its desktop entries and \
                     restarts Hedge on the new build.\n\n\
                     Or carry on with this one as it is — the installed copy is left alone.",
                    st.running.label(),
                    st.installed.as_ref().map(|m| m.build.label()).unwrap_or_default(),
                ),
                "Upgrade & restart",
                "Not now",
            ),
        };

        let dlg = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            heading,
        );
        dlg.set_secondary_text(Some(&detail));
        dlg.add_button("Don't ask again", gtk::ResponseType::Reject);
        dlg.add_button(decline, gtk::ResponseType::Close);
        dlg.add_button(accept, gtk::ResponseType::Accept);
        dlg.set_keep_above(true);
        let resp = dlg.run();
        dlg.close();

        match resp {
            gtk::ResponseType::Accept => self.install_and_restart(),
            gtk::ResponseType::Reject => {
                self.with_cfg_mut(|c| c.install_prompt_dismissed = true);
                self.save();
            }
            _ => {}
        }
    }

    /// Install this build and re-exec into it. The one approval already
    /// happened — the startup offer's button says "& restart" — so a second
    /// dialog here would just replay the question; only failure is reported.
    fn install_and_restart(&self) {
        match crate::install::install() {
            Err(e) => self.tell(None, gtk::MessageType::Error, "Install failed", &e),
            Ok((m, report)) => {
                log!("installed {} — restarting into it", m.build.label());
                for n in &report.notes {
                    log!("install note: {n}");
                }
                crate::install::spawn_delayed_restart();
                self.gtk_app().quit();
            }
        }
    }

    /// Install (or upgrade) this build, report the outcome, and offer to
    /// restart into the freshly installed copy.
    pub fn run_install(&self, parent: Option<&gtk::Window>) {
        let was_installed_copy = crate::install::running_is_installed();
        match crate::install::install() {
            Err(e) => self.tell(parent, gtk::MessageType::Error, "Install failed", &e),
            Ok((m, report)) => {
                let mut body = format!(
                    "Hedge {} is installed at\n{}\n\nAutostart, the app-menu launcher and the \
                     desktop right-click actions are registered.",
                    m.build.label(),
                    m.binary.display(),
                );
                for n in &report.notes {
                    body.push_str(&format!("\n\n{n}"));
                }
                // A fresh binary is on disk but this process is still the old
                // one, so the offer to restart belongs in the dialog that
                // reports the install rather than after it. An in-place upgrade
                // used to get a plain message ending in "Restart Hedge to run
                // the new build" — the one thing it could have done for you.
                body.push_str(
                    "\n\nRestarting runs the new build. This copy keeps running until you do.",
                );
                let title = match was_installed_copy {
                    true => "Upgraded — restart Hedge?",
                    false => "Installed — restart Hedge?",
                };
                if self.ask(parent, title, &body, "Restart Hedge now") {
                    crate::install::spawn_delayed_restart();
                    self.gtk_app().quit();
                }
            }
        }
    }

    /// Remove the install, with a confirmation that spells out what goes.
    pub fn run_uninstall(&self, parent: Option<&gtk::Window>, purge: bool) {
        let quitting = crate::install::running_is_installed();
        let mut warn = format!(
            "This removes {}, the autostart entry, the “Hedge Settings” launcher and the \
             desktop right-click actions.\n\nAny hidden desktop icons are put back first.",
            crate::install::binary_path().display(),
        );
        if purge {
            warn.push_str(&format!(
                "\n\nIt will also DELETE {} — your hedges, settings and backups.",
                config::config_dir().display()
            ));
        } else {
            warn.push_str(&format!(
                "\n\nYour config and backups in {} are kept.",
                config::config_dir().display()
            ));
        }
        if quitting {
            warn.push_str("\n\nHedge is running from the installed copy, so it will quit.");
        }
        if !self.ask(parent, "Uninstall Hedge?", &warn, "Uninstall") {
            return;
        }

        match crate::install::uninstall(purge) {
            Err(e) => self.tell(parent, gtk::MessageType::Error, "Uninstall failed", &e),
            Ok(report) => {
                if quitting {
                    self.gtk_app().quit();
                    return;
                }
                let mut body = format!("Removed {} file(s).", report.touched.len());
                for n in &report.notes {
                    body.push_str(&format!("\n\n{n}"));
                }
                self.tell(parent, gtk::MessageType::Info, "Uninstalled", &body);
            }
        }
    }

    fn tell(&self, parent: Option<&gtk::Window>, kind: gtk::MessageType, heading: &str, body: &str) {
        let dlg = gtk::MessageDialog::new(
            parent,
            gtk::DialogFlags::MODAL,
            kind,
            gtk::ButtonsType::Ok,
            heading,
        );
        dlg.set_secondary_text(Some(body));
        dlg.set_keep_above(true);
        dlg.run();
        dlg.close();
    }

    fn ask(&self, parent: Option<&gtk::Window>, heading: &str, body: &str, accept: &str) -> bool {
        let dlg = gtk::MessageDialog::new(
            parent,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            heading,
        );
        dlg.set_secondary_text(Some(body));
        dlg.add_button("Cancel", gtk::ResponseType::Cancel);
        dlg.add_button(accept, gtk::ResponseType::Accept);
        dlg.set_keep_above(true);
        let resp = dlg.run();
        dlg.close();
        resp == gtk::ResponseType::Accept
    }

    pub fn show_about(&self) {
        let dlg = gtk::AboutDialog::new();
        dlg.set_program_name("Hedges");
        dlg.set_version(Some(&crate::version::current().long_label()));
        dlg.set_comments(Some(
            "A zen, drag-friendly app launcher for your desktop.\n\
             Drag apps in, drag them out, hedge them off.",
        ));
        dlg.set_logo_icon_name(Some("preferences-desktop"));
        dlg.set_keep_above(true);
        dlg.connect_response(|d, _| d.close());
        dlg.show();
    }
}

pub fn main() -> i32 {
    // Install-related flags are handled *before* GTK: they must work with no
    // display, and single-instance forwarding would hand them to an already
    // running daemon instead of doing them here.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--version") || has("-V") {
        println!("hedge {}", crate::version::current().long_label());
        return 0;
    }
    if has("--help") || has("-h") {
        print_help();
        return 0;
    }
    if has("--status") {
        print_status();
        return 0;
    }
    if has("--install") {
        return cli_install();
    }
    if has("--uninstall") {
        return cli_uninstall(has("--purge"));
    }
    if has("--log") {
        return print_log();
    }

    // before GTK: a failure while starting up should still reach the file
    crate::log::init();
    crate::config::migrate_from_fence();
    glib::set_prgname(Some("hedge"));
    let app = HedgeApp::new();
    let code = app.run();
    crate::log::shutdown(&format!("exit {code}"));
    code
}

/// Start this same binary again in a moment, detached.
///
/// The delay is what lets the outgoing instance drop the D-Bus name before the
/// new one tries to claim it. `HEDGE_TOOK_OVER` marks the new process so that,
/// if the name is *still* held when it starts, it says so instead of putting up
/// the same dialog again.
fn spawn_delayed_self() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(quoted) = shlex::try_quote(&exe.to_string_lossy()).map(|s| s.into_owned()) else {
        return;
    };
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("sleep 2; exec {quoted}"))
        .env("HEDGE_TOOK_OVER", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Print the log, so a user with a terminal never has to hunt for the path.
fn print_log() -> i32 {
    let path = crate::log::path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            println!("{}", path.display());
            println!("{}", "-".repeat(path.display().to_string().len()));
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("hedge: no log at {} ({e})", path.display());
            1
        }
    }
}

fn print_help() {
    println!(
        "hedge {ver}\n\
         A zen, drag-friendly app launcher for the Linux desktop.\n\
         \n\
         Usage: hedge [OPTION]\n\
         \n\
         Running Hedge:\n\
         \x20 (no option)      start the desktop daemon\n\
         \x20 --settings       open the settings window\n\
         \x20 --add            add a new Hedge\n\
         \x20 --recall         bring every Hedge back onto this screen\n\
         \x20 --toggle-desktop hide/show the desktop icons\n\
         \x20 --hide-desktop, --show-desktop\n\
         \x20 --save-desktop   snapshot Hedges and desktop icon positions\n\
         \n\
         Installing (all in your home directory, no root):\n\
         \x20 --install        install/upgrade this build to {bin}\n\
         \x20 --uninstall      remove it, and the autostart, menu and right-click entries\n\
         \x20 --uninstall --purge  also delete ~/.config/hedge (config and backups)\n\
         \x20 --status         print installed vs. running version\n\
         \x20 --version, --help\n\
         \n\
         Diagnosing a crash:\n\
         \x20 --log            print {log}\n\
         \x20                  (panics, GTK criticals and lifecycle are recorded there;\n\
         \x20                   a session with no \"session ended\" line died hard)\n",
        ver = crate::version::current().long_label(),
        bin = crate::install::binary_path().display(),
        log = crate::log::path().display(),
    );
}

fn print_status() {
    let st = crate::install::state();
    println!("running:   {}", st.running.long_label());
    match &st.installed {
        Some(m) => {
            println!("installed: {}", m.build.long_label());
            println!("location:  {}", m.binary.display());
            if st.is_installed_copy {
                println!("this process is the installed copy");
            } else if st.upgrade_available() {
                println!("upgrade:   this build is newer — run `hedge --install`");
            } else {
                println!("upgrade:   none, the installed copy is up to date or newer");
            }
        }
        None => println!("installed: no — run `hedge --install`"),
    }
    let log = crate::log::path();
    match std::fs::metadata(&log) {
        Ok(m) => println!("log:       {} ({} bytes)", log.display(), m.len()),
        Err(_) => println!("log:       {} (nothing logged yet)", log.display()),
    }
}

fn cli_install() -> i32 {
    match crate::install::install() {
        Ok((m, report)) => {
            for p in &report.touched {
                println!("installed  {}", p.display());
            }
            for n in &report.notes {
                println!("note: {n}");
            }
            println!("\nHedge {} is installed.", m.build.label());
            println!("Start it with: {}", m.binary.display());
            0
        }
        Err(e) => {
            eprintln!("hedge: install failed: {e}");
            1
        }
    }
}

fn cli_uninstall(purge: bool) -> i32 {
    match crate::install::uninstall(purge) {
        Ok(report) => {
            for p in &report.touched {
                println!("removed  {}", p.display());
            }
            for n in &report.notes {
                println!("note: {n}");
            }
            println!("\nHedge uninstalled.");
            0
        }
        Err(e) => {
            eprintln!("hedge: uninstall failed: {e}");
            1
        }
    }
}

//! Master-detail settings: a sidebar of Hedges on the left, the selected
//! Hedge's appearance + apps on the right. Edits save immediately and refresh
//! the live Hedge widgets.
//!
//! Port of `settings_window.py`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

use crate::app::{HedgeApp, Refresh};
use crate::config::{self, AppEntry};
use crate::desktop_apps;

fn hex_to_rgba(h: &str) -> gdk::RGBA {
    h.parse::<gdk::RGBA>()
        .unwrap_or_else(|_| gdk::RGBA::new(0.0, 0.0, 0.0, 1.0))
}

fn rgba_to_hex(rgba: &gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.red() * 255.0 + 0.5) as u8,
        (rgba.green() * 255.0 + 0.5) as u8,
        (rgba.blue() * 255.0 + 0.5) as u8,
    )
}

/// Labels of the two pages that aren't a hedge, in sidebar order.
/// Hops a "fill from traceroute" will take. Past this the route is mostly the
/// destination's own network, and every hop costs a ping every round.
const HOP_LIMIT: usize = 15;

const NAV_GENERAL: &str = "General";
const NAV_DESKLET: &str = "Desklet";

// which refresh a key needs on the live hedge
fn refresh_for(key: &str) -> Refresh {
    match key {
        "accent" | "bgColor" | "bgOpacity" | "cornerRadius" | "padding" | "labelColor" | "titleColor"
        | "showBackground" | "expandOnHover" | "openOnHover" | "titleFontSize" | "showBorder" | "hideUntilHover" => Refresh::Style,
        "columns" | "tileSpacing" => Refresh::Layout,
        _ => Refresh::Render,
    }
}

struct Inner {
    app: HedgeApp,
    win: gtk::Window,
    /// The hedges.
    list: gtk::ListBox,
    /// The pages that aren't a hedge: General, Desklet. A second list rather
    /// than more rows in the first, so "the selected hedge" stays unambiguous
    /// — the add/remove/rename buttons act on `list` and nothing else.
    nav: gtk::ListBox,
    detail_scroll: gtk::ScrolledWindow,
    names: RefCell<Vec<String>>,
    current: RefCell<Option<String>>,
    loading: Cell<bool>,
    apps_list: RefCell<Option<gtk::ListBox>>,
    // -- Desktop page. Built once, refreshed in place, because the tray menu
    // and Super+Shift+D can change the same state behind its back.
    icons_switch: RefCell<Option<gtk::Switch>>,
    icons_status: RefCell<Option<gtk::Label>>,
    snapshot_status: RefCell<Option<gtk::Label>>,
    /// Container the host table is rebuilt into after every edit.
    hosts_box: RefCell<Option<gtk::Box>>,
    /// The traceroute button, so a trace in flight can say so on itself.
    trace_button: RefCell<Option<gtk::Button>>,
    /// Set while the page is being written *to*, so echoing a value back into
    /// a switch doesn't read as the user having flipped it.
    desktop_syncing: Cell<bool>,
    /// Set while one list is clearing the other's selection, so the resulting
    /// "nothing is selected" signal doesn't wipe the page just chosen.
    nav_syncing: Cell<bool>,
}

#[derive(Clone)]
pub struct SettingsWindow(Rc<Inner>);

impl SettingsWindow {
    pub fn new(app: &HedgeApp) -> Self {
        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        win.set_title("Hedge Settings");
        win.set_default_size(760, 540);
        win.style_context().add_class("hedge-settings");

        // One navigation, down the left: the hedges, then the pages that
        // aren't a hedge. A stack switcher across the top as well would have
        // been two places to look for the same kind of thing.
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        win.add(&root);

        let list = gtk::ListBox::new();
        let nav = gtk::ListBox::new();
        let detail_scroll = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);

        let this = SettingsWindow(Rc::new(Inner {
            app: app.clone(),
            win: win.clone(),
            list: list.clone(),
            nav: nav.clone(),
            detail_scroll: detail_scroll.clone(),
            names: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            loading: Cell::new(false),
            apps_list: RefCell::new(None),
            icons_switch: RefCell::new(None),
            icons_status: RefCell::new(None),
            snapshot_status: RefCell::new(None),
            hosts_box: RefCell::new(None),
            trace_button: RefCell::new(None),
            desktop_syncing: Cell::new(false),
            nav_syncing: Cell::new(false),
        }));

        root.pack_start(&this.build_sidebar(), false, false, 0);
        root.pack_start(
            &gtk::Separator::new(gtk::Orientation::Vertical),
            false,
            false,
            0,
        );
        detail_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        detail_scroll.set_hexpand(true);
        root.pack_start(&detail_scroll, true, true, 0);

        // keep the app alive; just hide the settings window on close
        win.connect_delete_event(|w, _| {
            w.hide();
            glib::Propagation::Stop
        });

        this.reload_sidebar(true, None);
        // no hedges yet: land on General rather than an empty detail pane
        if this.0.names.borrow().is_empty() {
            this.select_nav(NAV_GENERAL);
        }
        this
    }

    /// Everything that isn't a hedge and isn't drawn on the desktop: getting
    /// hedges back, the desktop's own icons, snapshots, and the build.
    fn build_general_page(&self) -> gtk::Box {
        self.clear_page_handles();
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
        box_.set_halign(gtk::Align::Center);
        box_.set_margin_top(4);
        box_.set_margin_bottom(16);

        let title = gtk::Label::new(Some("HEDGES"));
        title.style_context().add_class("hedge-section-title");
        box_.pack_start(&title, false, false, 0);

        let recall = gtk::Button::with_label("Recall all Hedges to this screen");
        recall.set_tooltip_text(Some(
            "Undock, unroll and tile every Hedge onto the primary monitor",
        ));
        let this = self.clone();
        recall.connect_clicked(move |_| this.0.app.recall_all_hedges());
        box_.pack_start(&recall, false, false, 0);

        let hint = gtk::Label::new(Some(
            "Use this if a Hedge has ended up off-screen or on a monitor that is gone.",
        ));
        hint.style_context().add_class("hedge-dim");
        box_.pack_start(&hint, false, false, 0);

        box_.pack_start(&self.build_desktop_icons_section(), false, false, 0);
        box_.pack_start(&self.build_snapshot_section(), false, false, 0);
        box_.pack_start(&self.build_version_section(), false, false, 0);
        box_
    }

    // -- Desklet page -------------------------------------------------------
    /// Everything drawn on the desktop itself: the clock, the host table under
    /// it, and the charts under that — in the order they appear on screen.
    fn build_desklet_page(&self) -> gtk::Box {
        self.clear_page_handles();
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
        box_.set_halign(gtk::Align::Center);
        box_.set_margin_top(4);
        box_.set_margin_bottom(16);
        box_.pack_start(&self.build_clock_section(), false, false, 0);
        box_.pack_start(&self.build_hosts_section(), false, false, 0);
        box_.pack_start(&self.build_sysmon_section(), false, false, 0);
        box_.pack_start(&self.build_memfence_section(), false, false, 0);
        box_.pack_start(&self.build_claude_section(), false, false, 0);
        box_
    }

    /// The Claude section: local Claude Code sessions, plan usage, and the
    /// service status page, at the bottom of the desklet.
    fn build_claude_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("CLAUDE"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        grid.set_halign(gtk::Align::Center);
        let row = Rc::new(Cell::new(0));
        let add_row = |grid: &gtk::Grid, row: &Rc<Cell<i32>>, label: &str, w: &gtk::Widget| {
            let l = gtk::Label::new(Some(label));
            l.set_xalign(1.0);
            grid.attach(&l, 0, row.get(), 1, 1);
            w.set_halign(gtk::Align::Start);
            grid.attach(w, 1, row.get(), 1, 1);
            row.set(row.get() + 1);
        };

        let c = self.0.app.cfg_snapshot().claude;
        let opts: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));

        let enabled = gtk::Switch::new();
        enabled.set_active(c.enabled);
        let this = self.clone();
        let o = opts.clone();
        enabled.connect_state_set(move |_, on| {
            this.0.app.with_cfg_mut(|cfg| cfg.claude.enabled = on);
            this.0.app.save();
            this.0.app.sync_claude();
            for w in o.borrow().iter() {
                w.set_sensitive(on);
            }
            glib::Propagation::Proceed
        });
        add_row(&grid, &row, "Show Claude activity", enabled.upcast_ref());

        let toggle = |label: &str, tip: &str, on: bool, set: fn(&mut config::Claude, bool)| {
            let sw = gtk::Switch::new();
            sw.set_active(on);
            if !tip.is_empty() {
                sw.set_tooltip_text(Some(tip));
            }
            let this = self.clone();
            sw.connect_state_set(move |_, v| {
                this.0.app.with_cfg_mut(|cfg| set(&mut cfg.claude, v));
                this.0.app.save();
                this.0.app.sync_claude();
                glib::Propagation::Proceed
            });
            opts.borrow_mut().push(sw.clone().upcast());
            add_row(&grid, &row, label, sw.upcast_ref());
        };
        toggle(
            "Sessions",
            "Every Claude Code session on this machine, coloured by what it's doing: \
             green working, orange idle, red waiting on you.",
            c.show_sessions,
            |s, v| s.show_sessions = v,
        );
        toggle(
            "Plan usage",
            "The 5-hour, weekly and per-model usage bars. Read with your Claude Code \
             sign-in; asks api.anthropic.com every five minutes and never changes anything.",
            c.show_usage,
            |s, v| s.show_usage = v,
        );
        toggle(
            "Service status",
            "status.anthropic.com's verdict, polled every 30 seconds — the dot strip \
             is the last half hour.",
            c.show_server_status,
            |s, v| s.show_server_status = v,
        );

        for w in opts.borrow().iter() {
            w.set_sensitive(c.enabled);
        }
        box_.pack_start(&grid, false, false, 0);
        box_
    }

    /// Widget handles the pages hand out live only as long as the page that
    /// built them: `set_detail` destroys the previous page, and a refresh must
    /// not go poking at its corpse.
    fn clear_page_handles(&self) {
        *self.0.icons_switch.borrow_mut() = None;
        *self.0.icons_status.borrow_mut() = None;
        *self.0.snapshot_status.borrow_mut() = None;
        *self.0.hosts_box.borrow_mut() = None;
        *self.0.trace_button.borrow_mut() = None;
    }

    /// The mini charts: what to draw, how often, and which hardware to read.
    fn build_sysmon_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("SYSTEM CHARTS"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        grid.set_halign(gtk::Align::Center);
        let row = Rc::new(Cell::new(0));
        let add_row = |grid: &gtk::Grid, row: &Rc<Cell<i32>>, label: &str, w: &gtk::Widget| {
            let l = gtk::Label::new(Some(label));
            l.set_xalign(1.0);
            grid.attach(&l, 0, row.get(), 1, 1);
            w.set_halign(gtk::Align::Start);
            grid.attach(w, 1, row.get(), 1, 1);
            row.set(row.get() + 1);
        };

        let c = self.0.app.cfg_snapshot().sysmon;
        // everything under the master switch greys out with it, the way the
        // clock section does
        let opts: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));

        let enabled = gtk::Switch::new();
        enabled.set_active(c.enabled);
        let this = self.clone();
        let o = opts.clone();
        enabled.connect_state_set(move |_, on| {
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.enabled = on);
            this.0.app.save();
            this.0.app.sync_sysmon();
            for w in o.borrow().iter() {
                w.set_sensitive(on);
            }
            glib::Propagation::Proceed
        });
        add_row(&grid, &row, "Show system charts", enabled.upcast_ref());

        // one switch per chart: each is its own section of the desklet
        let toggle = |label: &str, on: bool, set: fn(&mut config::Sysmon, bool)| {
            let sw = gtk::Switch::new();
            sw.set_active(on);
            let this = self.clone();
            sw.connect_state_set(move |_, v| {
                this.0.app.with_cfg_mut(|cfg| set(&mut cfg.sysmon, v));
                this.0.app.save();
                this.0.app.sync_sysmon();
                glib::Propagation::Proceed
            });
            opts.borrow_mut().push(sw.clone().upcast());
            add_row(&grid, &row, label, sw.upcast_ref());
        };
        toggle("CPU", c.show_cpu, |s, v| s.show_cpu = v);
        toggle("GPU", c.show_gpu, |s, v| s.show_gpu = v);
        toggle("Memory", c.show_mem, |s, v| s.show_mem = v);
        toggle("Network", c.show_net, |s, v| s.show_net = v);
        toggle("Busiest processes", c.show_processes, |s, v| s.show_processes = v);
        toggle("Clock speed (CPU & GPU)", c.show_clock_speed, |s, v| s.show_clock_speed = v);

        let interval = gtk::SpinButton::with_range(1.0, 60.0, 1.0);
        interval.set_value(c.interval() as f64);
        interval.set_tooltip_text(Some(
            "Seconds between readings. Every point on a chart is a rate over this long.",
        ));
        let this = self.clone();
        interval.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.interval_sec = v);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        opts.borrow_mut().push(interval.clone().upcast());
        add_row(&grid, &row, "Sample every (s)", interval.upcast_ref());

        // Only GPUs this machine actually exposes are offered — a dropdown
        // listing "NVIDIA" on a machine with no nvidia-smi is a bug report
        // waiting to happen.
        let gpu = gtk::ComboBoxText::new();
        gpu.append(Some("auto"), "Automatic");
        for g in crate::sysmon::list_gpus() {
            gpu.append(Some(&g.id), &g.label);
        }
        gpu.append(Some("off"), "None");
        if !gpu.set_active_id(Some(&c.gpu)) {
            gpu.set_active_id(Some("auto"));
        }
        let this = self.clone();
        gpu.connect_changed(move |w| {
            let id = w.active_id().map(|s| s.to_string()).unwrap_or_default();
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.gpu = id);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        opts.borrow_mut().push(gpu.clone().upcast());
        add_row(&grid, &row, "GPU", gpu.upcast_ref());

        let bg = gtk::ColorButton::with_rgba(&hex_to_rgba(&c.chart_bg_color));
        let this = self.clone();
        bg.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.chart_bg_color = hex.clone());
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        opts.borrow_mut().push(bg.clone().upcast());
        add_row(&grid, &row, "Chart background", bg.upcast_ref());

        let bg_opacity = gtk::SpinButton::with_range(0.0, 100.0, 5.0);
        bg_opacity.set_value((c.chart_bg_opacity * 100.0).round());
        bg_opacity.set_tooltip_text(Some(
            "How solid the card behind each chart is. 0 draws the curve straight on the wallpaper.",
        ));
        let this = self.clone();
        bg_opacity.connect_value_changed(move |w| {
            let v = w.value() / 100.0;
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.chart_bg_opacity = v);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        opts.borrow_mut().push(bg_opacity.clone().upcast());
        add_row(&grid, &row, "Background opacity (%)", bg_opacity.upcast_ref());

        let corners = gtk::SpinButton::with_range(0.0, 24.0, 1.0);
        corners.set_value(c.chart_corner_radius as f64);
        corners.set_tooltip_text(Some("Corner rounding of each chart card; 0 is square."));
        let this = self.clone();
        corners.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.chart_corner_radius = v);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        opts.borrow_mut().push(corners.clone().upcast());
        add_row(&grid, &row, "Chart corners", corners.upcast_ref());

        let net = gtk::ComboBoxText::new();
        net.append(Some("auto"), "Automatic (default route)");
        for iface in crate::sysmon::list_interfaces() {
            net.append(Some(&iface), &iface);
        }
        if !net.set_active_id(Some(&c.net_interface)) {
            net.set_active_id(Some("auto"));
        }
        let this = self.clone();
        net.connect_changed(move |w| {
            let id = w.active_id().map(|s| s.to_string()).unwrap_or_default();
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.net_interface = id);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        opts.borrow_mut().push(net.clone().upcast());
        add_row(&grid, &row, "Network interface", net.upcast_ref());

        let task = gtk::Entry::new();
        task.set_text(&c.task_manager);
        task.set_width_chars(22);
        task.set_placeholder_text(Some(
            crate::sysmon::task_manager_command("")
                .as_deref()
                .unwrap_or("none found"),
        ));
        task.set_tooltip_text(Some(
            "Run this when the CPU chart is clicked. Empty uses whichever system monitor is installed.",
        ));
        let this = self.clone();
        task.connect_changed(move |w| {
            let v = w.text().to_string();
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.task_manager = v);
            this.0.app.save();
        });
        opts.borrow_mut().push(task.clone().upcast());
        add_row(&grid, &row, "Click CPU runs", task.upcast_ref());

        // Not under the master switch: the warning is worth having with the
        // charts off, and keeps the sampler running on its own.
        let horizon = gtk::SpinButton::with_range(1.0, 60.0, 1.0);
        horizon.set_value(c.mem_warning_minutes.clamp(1, 60) as f64);
        horizon.set_sensitive(c.mem_warning);
        horizon.set_tooltip_text(Some(
            "Warn when memory, climbing at its current rate, would run out within this many minutes.",
        ));
        let mem_warn = gtk::Switch::new();
        mem_warn.set_active(c.mem_warning);
        mem_warn.set_tooltip_text(Some(
            "A desktop notification when memory use is on course to run out, \
             naming the largest process. Works with the charts switched off.",
        ));
        let this = self.clone();
        let h = horizon.clone();
        let fl: Rc<RefCell<Option<gtk::SpinButton>>> = Rc::new(RefCell::new(None));
        let fl2 = fl.clone();
        mem_warn.connect_state_set(move |_, on| {
            if let Some(f) = fl2.borrow().as_ref() {
                f.set_sensitive(on);
            }
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.mem_warning = on);
            this.0.app.save();
            this.0.app.sync_sysmon();
            h.set_sensitive(on);
            glib::Propagation::Proceed
        });
        add_row(&grid, &row, "Warn before memory runs out", mem_warn.upcast_ref());
        let this = self.clone();
        horizon.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.mem_warning_minutes = v);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        add_row(&grid, &row, "Warn this far ahead (min)", horizon.upcast_ref());

        let floor = gtk::SpinButton::with_range(0.0, 95.0, 5.0);
        floor.set_value(c.mem_warning_min_percent.clamp(0, 95) as f64);
        floor.set_sensitive(c.mem_warning);
        floor.set_tooltip_text(Some(
            "No warning while less than this much memory is in use, however fast it climbs.",
        ));
        let this = self.clone();
        floor.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.sysmon.mem_warning_min_percent = v);
            this.0.app.save();
            this.0.app.sync_sysmon();
        });
        add_row(&grid, &row, "Only warn above (% used)", floor.upcast_ref());
        *fl.borrow_mut() = Some(floor);

        for w in opts.borrow().iter() {
            w.set_sensitive(c.enabled);
        }
        box_.pack_start(&grid, false, false, 0);

        let hint = gtk::Label::new(Some(
            "The charts live under the clock, behind the same chevron as the host table — \
             switch the clock on in General to see them.",
        ));
        hint.style_context().add_class("hedge-dim");
        hint.set_line_wrap(true);
        hint.set_max_width_chars(54);
        hint.set_justify(gtk::Justification::Center);
        box_.pack_start(&hint, false, false, 0);
        box_
    }

    /// Memory reserved for the desktop, and apps kept out of it. See
    /// `memfence` for why both halves are needed.
    fn build_memfence_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("MEMORY FENCE"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let state = gtk::Label::new(None);
        state.set_line_wrap(true);
        state.set_max_width_chars(54);
        state.set_justify(gtk::Justification::Center);
        box_.pack_start(&state, false, false, 0);

        let button = gtk::Button::new();
        button.set_halign(gtk::Align::Center);
        box_.pack_start(&button, false, false, 0);

        let show = {
            let state = state.clone();
            let button = button.clone();
            move || {
                let st = crate::memfence::status();
                let text = if st.session_min > 0 {
                    let mut t = format!(
                        "Protected: {} reserved for Cinnamon and Hedge",
                        crate::memfence::human_bytes(st.session_min)
                    );
                    if st.display_min > 0 {
                        t += &format!(
                            ", {} for the display server",
                            crate::memfence::human_bytes(st.display_min)
                        );
                    }
                    t + "."
                } else if st.installed {
                    "Protection is installed and takes effect at the next login.".to_string()
                } else {
                    "Not protected: when memory runs out, the desktop freezes along with \
                     the program that caused it."
                        .to_string()
                };
                state.set_text(&text);
                button.set_label(if st.installed {
                    "Remove protection…"
                } else {
                    "Protect the desktop…"
                });
                button.set_sensitive(true);
            }
        };
        show();
        button.set_tooltip_text(Some(
            "Asks for your password once, then writes systemd settings that keep memory \
             reserved for the display server, Cinnamon, Hedge and the system services. \
             Takes effect immediately and survives a reboot.",
        ));
        button.connect_clicked(move |b| {
            let script = if crate::memfence::status().installed {
                crate::memfence::remove_script()
            } else {
                crate::memfence::install_script()
            };
            let argv: [&std::ffi::OsStr; 4] = [
                "pkexec".as_ref(),
                "/bin/sh".as_ref(),
                "-c".as_ref(),
                script.as_ref(),
            ];
            let proc_ = match gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
                Ok(p) => p,
                Err(e) => {
                    log!(WARN, "memory fence: pkexec failed to start ({e})");
                    return;
                }
            };
            b.set_sensitive(false);
            let show = show.clone();
            proc_.wait_check_async(None::<&gio::Cancellable>, move |res| {
                if let Err(e) = res {
                    // includes the user cancelling the password prompt
                    log!(WARN, "memory fence: not applied ({e})");
                }
                show();
            });
        });

        let scoped = gtk::Switch::new();
        scoped.set_active(!self.0.app.cfg_snapshot().launch_unscoped);
        scoped.set_halign(gtk::Align::Start);
        scoped.set_tooltip_text(Some(
            "Start each app from a tile in its own memory group, outside the desktop \
             session. Otherwise the app shares — and can use up — the memory reserved \
             for the desktop.",
        ));
        let this = self.clone();
        scoped.connect_state_set(move |_, on| {
            this.0.app.with_cfg_mut(|cfg| cfg.launch_unscoped = !on);
            this.0.app.save();
            glib::Propagation::Proceed
        });
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_halign(gtk::Align::Center);
        row.pack_start(&gtk::Label::new(Some("Launch apps in their own memory group")), false, false, 0);
        row.pack_start(&scoped, false, false, 0);
        box_.pack_start(&row, false, false, 0);

        let hint = gtk::Label::new(Some(
            "Apps started from the Cinnamon menu or a terminal may still share the \
             desktop's reserve; apps started from a Hedge tile never do.",
        ));
        hint.style_context().add_class("hedge-dim");
        hint.set_line_wrap(true);
        hint.set_max_width_chars(54);
        hint.set_justify(gtk::Justification::Center);
        box_.pack_start(&hint, false, false, 0);
        box_
    }

    /// Stash the desktop's contents out of sight, and put them back.
    fn build_desktop_icons_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("DESKTOP ICONS"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.set_halign(gtk::Align::Center);
        let label = gtk::Label::new(Some("Hide desktop icons"));
        row.pack_start(&label, false, false, 0);

        let sw = gtk::Switch::new();
        sw.set_valign(gtk::Align::Center);
        sw.set_active(self.0.app.cfg_snapshot().desktop_hidden);
        let this = self.clone();
        sw.connect_state_set(move |_, on| {
            // set_active fires this too; only a real click should act
            if !this.0.desktop_syncing.get() {
                if on {
                    this.0.app.hide_desktop();
                } else {
                    this.0.app.show_desktop();
                }
                this.refresh_desktop_page();
            }
            glib::Propagation::Proceed
        });
        row.pack_start(&sw, false, false, 0);
        box_.pack_start(&row, false, false, 0);

        let status = gtk::Label::new(None);
        status.style_context().add_class("hedge-dim");
        box_.pack_start(&status, false, false, 0);

        let hint = gtk::Label::new(Some(
            "Nothing is deleted: items move to a stash folder and come straight back, \
             and they survive a logout. Super+Shift+D does the same thing.",
        ));
        hint.style_context().add_class("hedge-dim");
        hint.set_line_wrap(true);
        hint.set_max_width_chars(54);
        hint.set_justify(gtk::Justification::Center);
        box_.pack_start(&hint, false, false, 0);

        *self.0.icons_switch.borrow_mut() = Some(sw);
        *self.0.icons_status.borrow_mut() = Some(status);
        self.refresh_desktop_page();
        box_
    }

    fn build_snapshot_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("DESKTOP SNAPSHOT"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let status = gtk::Label::new(None);
        status.style_context().add_class("hedge-dim");
        status.set_text(&snapshot_status());

        let save = gtk::Button::with_label("Save desktop");
        save.set_tooltip_text(Some(
            "Remember every Hedge (position, size, settings, contents) and where each desktop icon sits",
        ));
        let this = self.clone();
        let st = status.clone();
        save.connect_clicked(move |_| {
            this.0.app.save_desktop_snapshot();
            st.set_text(&snapshot_status());
        });
        box_.pack_start(&save, false, false, 0);

        let restore = gtk::Button::with_label("Restore desktop");
        restore.set_tooltip_text(Some(
            "Put the Hedges and desktop icons back as they were when you last saved",
        ));
        let this = self.clone();
        restore.connect_clicked(move |b| {
            // moves real desktop icons and replaces the current hedge layout,
            // so make the user say yes first
            let win = b.toplevel().and_then(|w| w.downcast::<gtk::Window>().ok());
            let dlg = gtk::MessageDialog::new(
                win.as_ref(),
                gtk::DialogFlags::MODAL,
                gtk::MessageType::Question,
                gtk::ButtonsType::OkCancel,
                "Restore the saved desktop?\n\nThis replaces your current Hedges and moves \
                 desktop icons back to their saved positions. Your current setup is backed \
                 up first, so you can undo it with “Restore last backup”.",
            );
            let resp = dlg.run();
            dlg.close();
            if resp == gtk::ResponseType::Ok {
                this.0.app.restore_desktop_snapshot();
            }
        });
        box_.pack_start(&restore, false, false, 0);

        box_.pack_start(&status, false, false, 0);
        let note = gtk::Label::new(Some(
            "Icon positions are stored as file metadata; nemo may need a moment to redraw.",
        ));
        note.style_context().add_class("hedge-dim");
        note.set_line_wrap(true);
        note.set_max_width_chars(54);
        note.set_justify(gtk::Justification::Center);
        box_.pack_start(&note, false, false, 0);

        *self.0.snapshot_status.borrow_mut() = Some(status);
        box_
    }

    /// The host monitor: a tree of hosts and how often to ping them.
    ///
    /// The table is a tree because reachability is: a host behind a router
    /// that is down hasn't been proven down, so it is drawn grey rather than
    /// red and isn't pinged at all until its parent answers again.
    fn build_hosts_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("HOST MONITOR"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let cfg = self.0.app.cfg_snapshot().ping;

        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        grid.set_halign(gtk::Align::Center);

        let l = gtk::Label::new(Some("Monitor hosts"));
        l.set_xalign(1.0);
        grid.attach(&l, 0, 0, 1, 1);
        let enabled = gtk::Switch::new();
        enabled.set_halign(gtk::Align::Start);
        enabled.set_active(cfg.enabled);
        let this = self.clone();
        enabled.connect_state_set(move |_, on| {
            if !this.0.desktop_syncing.get() {
                this.0.app.with_cfg_mut(|c| c.ping.enabled = on);
                this.0.app.save();
                this.0.app.sync_ping();
                this.rebuild_hosts_table();
            }
            glib::Propagation::Proceed
        });
        grid.attach(&enabled, 1, 0, 1, 1);

        let l = gtk::Label::new(Some("Ping every"));
        l.set_xalign(1.0);
        grid.attach(&l, 0, 1, 1, 1);
        let interval = gtk::SpinButton::with_range(5.0, 3600.0, 5.0);
        interval.set_halign(gtk::Align::Start);
        interval.set_value(cfg.interval() as f64);
        interval.set_tooltip_text(Some(
            "Seconds between rounds. One round pings every host, parents first.",
        ));
        let this = self.clone();
        interval.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|c| c.ping.interval_sec = v);
            this.0.app.save();
            this.0.app.sync_ping();
            this.rebuild_hosts_table();
        });
        let unit = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        unit.pack_start(&interval, false, false, 0);
        unit.pack_start(&gtk::Label::new(Some("seconds")), false, false, 0);
        grid.attach(&unit, 1, 1, 1, 1);

        let l = gtk::Label::new(Some("History badges"));
        l.set_xalign(1.0);
        grid.attach(&l, 0, 2, 1, 1);
        let segments = gtk::SpinButton::with_range(4.0, 48.0, 1.0);
        segments.set_halign(gtk::Align::Start);
        segments.set_value(cfg.segment_count() as f64);
        segments.set_tooltip_text(Some(
            "Badges per host. The row always covers the last three hours, so more badges \
             means finer periods, not a longer history.",
        ));
        let this = self.clone();
        segments.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|c| c.ping.segments = v);
            this.0.app.save();
            this.0.app.sync_ping();
        });
        grid.attach(&segments, 1, 2, 1, 1);
        box_.pack_start(&grid, false, false, 0);

        let hosts = gtk::Box::new(gtk::Orientation::Vertical, 0);
        box_.pack_start(&hosts, false, false, 0);
        *self.0.hosts_box.borrow_mut() = Some(hosts);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_halign(gtk::Align::Center);
        let add = gtk::Button::with_label("Add host…");
        let this = self.clone();
        add.connect_clicked(move |_| this.host_dialog(None));
        actions.pack_start(&add, false, false, 0);

        // Fill the table from the route to somewhere: each hop becomes a host
        // behind the one before it, which is exactly the shape that makes a
        // slow or dead hop legible — everything past it goes grey, not red.
        let target = gtk::Entry::new();
        target.set_text(&cfg.trace_target);
        target.set_width_chars(14);
        target.set_placeholder_text(Some("google.com"));
        let this = self.clone();
        target.connect_changed(move |w| {
            let v = w.text().to_string();
            this.0.app.with_cfg_mut(|c| c.ping.trace_target = v);
            this.0.app.save();
        });
        actions.pack_start(&target, false, false, 0);

        let clear = gtk::Button::with_label("Delete all");
        clear.set_tooltip_text(Some("Remove every host from the table"));
        let this = self.clone();
        clear.connect_clicked(move |_| this.delete_all_hosts());
        actions.pack_start(&clear, false, false, 0);

        let trace = gtk::Button::with_label("Fill from traceroute");
        trace.set_tooltip_text(Some(
            "Trace the route to that address and put the first hops in the table",
        ));
        let this = self.clone();
        let entry = target.clone();
        trace.connect_clicked(move |_| this.fill_from_traceroute(&entry.text()));
        actions.pack_start(&trace, false, false, 0);
        *self.0.trace_button.borrow_mut() = Some(trace);
        box_.pack_start(&actions, false, false, 0);

        let hint = gtk::Label::new(Some(
            "Put a host behind the thing it depends on and it is drawn indented under it. \
             The live badges — three hours in fifteen-minute periods — are on the desklet, \
             under the time.",
        ));
        hint.style_context().add_class("hedge-dim");
        hint.set_line_wrap(true);
        hint.set_max_width_chars(54);
        hint.set_justify(gtk::Justification::Center);
        box_.pack_start(&hint, false, false, 0);

        self.rebuild_hosts_table();
        box_
    }

    /// Redraw the host rows from config + the monitor's current view.
    fn rebuild_hosts_table(&self) {
        let Some(hosts) = self.0.hosts_box.borrow().clone() else {
            return;
        };
        for child in hosts.children() {
            hosts.remove(&child);
        }

        let cfg = self.0.app.cfg_snapshot().ping;
        if cfg.hosts.is_empty() {
            let empty = gtk::Label::new(Some("No hosts yet."));
            empty.style_context().add_class("hedge-dim");
            empty.set_margin_top(8);
            empty.set_margin_bottom(8);
            hosts.pack_start(&empty, false, false, 0);
            hosts.show_all();
            return;
        }

        let grid = gtk::Grid::new();
        grid.set_row_spacing(4);
        grid.set_column_spacing(10);
        grid.set_margin_top(8);
        grid.set_margin_bottom(8);

        // Config only — no badges, no latency. This table is the editor; the
        // live picture belongs on the desklet, under the time, where you can
        // see it without opening a settings window.
        for (row, (index, depth)) in cfg.tree_order().into_iter().enumerate() {
            let r = row as i32;
            let entry = &cfg.hosts[index];

            let name = gtk::Label::new(Some(&entry.name));
            name.set_xalign(0.0);
            // the indent is the tree: a child sits under what it depends on,
            // capped so a fifteen-hop route stays inside the pane
            name.set_margin_start(depth.min(6) as i32 * 12);
            grid.attach(&name, 0, r, 1, 1);

            let host = gtk::Label::new(Some(&entry.host));
            host.set_xalign(0.0);
            host.style_context().add_class("ping-host");
            grid.attach(&host, 1, r, 1, 1);

            let edit = gtk::Button::with_label("Edit");
            let this = self.clone();
            edit.connect_clicked(move |_| this.host_dialog(Some(index)));
            grid.attach(&edit, 2, r, 1, 1);

            let del = gtk::Button::with_label("Delete");
            let this = self.clone();
            let name = entry.name.clone();
            del.connect_clicked(move |_| this.delete_host(&name));
            grid.attach(&del, 3, r, 1, 1);
        }
        hosts.pack_start(&grid, false, false, 0);
        hosts.show_all();
    }

    /// Add (`None`) or edit an existing host.
    fn host_dialog(&self, index: Option<usize>) {
        let cfg = self.0.app.cfg_snapshot().ping;
        let existing = index.and_then(|i| cfg.hosts.get(i)).cloned();
        let old_name = existing.as_ref().map(|h| h.name.clone());

        let dlg = gtk::Dialog::with_buttons(
            Some(if existing.is_some() { "Edit host" } else { "Add host" }),
            Some(&self.0.win),
            gtk::DialogFlags::MODAL,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Save", gtk::ResponseType::Ok),
            ],
        );
        dlg.set_default_response(gtk::ResponseType::Ok);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(8);
        grid.set_column_spacing(10);
        grid.set_margin_top(12);
        grid.set_margin_bottom(12);
        grid.set_margin_start(12);
        grid.set_margin_end(12);

        let name = gtk::Entry::new();
        name.set_activates_default(true);
        name.set_placeholder_text(Some("Router"));
        let host = gtk::Entry::new();
        host.set_activates_default(true);
        host.set_placeholder_text(Some("192.168.1.1 or example.com"));
        if let Some(h) = &existing {
            name.set_text(&h.name);
            host.set_text(&h.host);
        }

        // A host may not sit under itself or under anything that already sits
        // under it — that would be a cycle, and a cycle has no root to ping
        // first. Descendants are simply not offered.
        let parent = gtk::ComboBoxText::new();
        parent.append(Some(""), "— none (top level) —");
        for h in &cfg.hosts {
            if let Some(me) = &old_name {
                if &h.name == me || is_descendant(&cfg, &h.name, me) {
                    continue;
                }
            }
            parent.append(Some(&h.name), &h.name);
        }
        parent.set_active_id(Some(existing.as_ref().map_or("", |h| h.parent.as_str())));

        let error = gtk::Label::new(None);
        error.style_context().add_class("hedge-dim");
        error.set_line_wrap(true);
        error.set_max_width_chars(38);

        for (row, (label, widget)) in [
            ("Name", name.clone().upcast::<gtk::Widget>()),
            ("Host or IP", host.clone().upcast()),
            ("Sits behind", parent.clone().upcast()),
        ]
        .into_iter()
        .enumerate()
        {
            let l = gtk::Label::new(Some(label));
            l.set_xalign(1.0);
            grid.attach(&l, 0, row as i32, 1, 1);
            grid.attach(&widget, 1, row as i32, 1, 1);
        }
        grid.attach(&error, 0, 3, 2, 1);
        dlg.content_area().add(&grid);
        dlg.show_all();

        loop {
            if dlg.run() != gtk::ResponseType::Ok {
                break;
            }
            let new_name = name.text().trim().to_string();
            let new_host = host.text().trim().to_string();
            let new_parent = parent.active_id().map(|s| s.to_string()).unwrap_or_default();

            let cfg = self.0.app.cfg_snapshot().ping;
            let clash = cfg
                .hosts
                .iter()
                .enumerate()
                .any(|(i, h)| h.name == new_name && Some(i) != index);
            let problem = if new_name.is_empty() {
                Some("Give the host a name.")
            } else if new_host.is_empty() {
                Some("Give the host an address to ping.")
            } else if clash {
                Some("Another host already has that name.")
            } else {
                None
            };
            if let Some(msg) = problem {
                error.set_text(msg);
                continue;
            }

            self.0.app.with_cfg_mut(|c| {
                match index {
                    Some(i) => {
                        // children point at the parent by name, so a rename has
                        // to drag them along or the tree falls apart
                        if let Some(old) = &old_name {
                            if old != &new_name {
                                for h in c.ping.hosts.iter_mut() {
                                    if &h.parent == old {
                                        h.parent = new_name.clone();
                                    }
                                }
                            }
                        }
                        let h = &mut c.ping.hosts[i];
                        h.name = new_name.clone();
                        h.host = new_host.clone();
                        h.parent = new_parent.clone();
                    }
                    None => c.ping.hosts.push(config::PingHost {
                        name: new_name.clone(),
                        host: new_host.clone(),
                        parent: new_parent.clone(),
                        ..Default::default()
                    }),
                }
            });
            self.0.app.save();
            self.0.app.sync_ping();
            self.rebuild_hosts_table();
            break;
        }
        dlg.close();
    }

    /// Replace the host table with the route to `target`.
    ///
    /// The trace runs on a worker thread behind a modal notice: it takes
    /// seconds, and a settings window frozen mid-click looks like a crash.
    fn fill_from_traceroute(&self, target: &str) {
        let target = target.trim().to_string();
        if target.is_empty() {
            self.tell("Give it something to trace to first.");
            return;
        }
        let existing = self.0.app.cfg_snapshot().ping.hosts.len();
        if existing > 0 {
            // Named buttons, not OK/Cancel: "OK" to a question about throwing
            // away fifteen hosts is not a word anyone should have to squint at.
            let dlg = gtk::Dialog::with_buttons(
                Some("Replace hosts"),
                Some(&self.0.win),
                gtk::DialogFlags::MODAL,
                &[
                    ("Cancel", gtk::ResponseType::Cancel),
                    ("Replace", gtk::ResponseType::Ok),
                ],
            );
            dlg.set_default_response(gtk::ResponseType::Ok);
            let message = gtk::Label::new(Some(&format!(
                "Replace the {existing} host(s) in the table with the route to {target}?"
            )));
            message.set_line_wrap(true);
            message.set_max_width_chars(44);
            message.set_margin_top(16);
            message.set_margin_bottom(16);
            message.set_margin_start(16);
            message.set_margin_end(16);
            dlg.content_area().add(&message);
            dlg.show_all();
            let resp = dlg.run();
            dlg.close();
            log!("traceroute fill: replace {existing} host(s)? -> {resp:?}");
            if resp != gtk::ResponseType::Ok {
                return;
            }
        }

        // Progress lives on the button, not in a modal. A modal with no
        // buttons is indistinguishable from a hung application, and if
        // anything goes wrong there is no way to dismiss it.
        let button = self.0.trace_button.borrow().clone();
        if let Some(b) = &button {
            b.set_label("Tracing…");
            b.set_sensitive(false);
        }

        #[allow(deprecated)] // see the note in ping::Monitor::new
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let traced = target.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ping::traceroute(&traced, HOP_LIMIT));
        });

        let this = self.clone();
        rx.attach(None, move |result| {
            if let Some(b) = &button {
                b.set_label("Fill from traceroute");
                b.set_sensitive(true);
            }
            match result {
                Ok(hops) => this.apply_traceroute(hops, &target),
                Err(e) => this.tell(&format!("Could not trace the route: {e}")),
            }
            glib::ControlFlow::Break
        });
    }

    /// Turn traced hops into hosts, in path order.
    ///
    /// Deliberately flat, not chained. Parenting them would mean a dead hop
    /// greys out everything past it, which reads well for a router-and-its-
    /// clients tree but badly for a route: routes are full of hops that never
    /// answer ICMP while forwarding perfectly, and one of those would blank
    /// the rest of the path. Set a parent by hand where it genuinely holds.
    fn apply_traceroute(&self, hops: Vec<crate::ping::Hop>, target: &str) {
        let last = hops.len().saturating_sub(1);
        let mut names: Vec<String> = Vec::new();
        for (i, hop) in hops.iter().enumerate() {
            let mut name = crate::ping::hop_label(hop, i, i == last, target);
            // consecutive hops through one network share a label, and a name
            // is what children point at, so it has to be unique
            if names.contains(&name) {
                let base = name.clone();
                for n in 2.. {
                    name = format!("{base} {n}");
                    if !names.contains(&name) {
                        break;
                    }
                }
            }
            names.push(name);
        }

        self.0.app.with_cfg_mut(|c| {
            c.ping.hosts = hops
                .iter()
                .enumerate()
                .map(|(i, hop)| config::PingHost {
                    name: names[i].clone(),
                    host: hop.address.clone(),
                    parent: String::new(),
                    ..Default::default()
                })
                .collect();
            // a route worth watching is a route being watched
            c.ping.enabled = true;
        });
        self.0.app.save();
        self.0.app.sync_ping();
        self.rebuild_hosts_table();
        log!("filled {} hosts from the route to {target}", hops.len());
    }

    /// A plain modal notice.
    fn tell(&self, message: &str) {
        let dlg = gtk::MessageDialog::new(
            Some(&self.0.win),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Info,
            gtk::ButtonsType::Ok,
            message,
        );
        dlg.run();
        dlg.close();
    }

    /// Empty the host table. Confirmed, because a filled route is a minute of
    /// tracing and several minutes of history to throw away.
    fn delete_all_hosts(&self) {
        let count = self.0.app.cfg_snapshot().ping.hosts.len();
        if count == 0 {
            return;
        }
        let dlg = gtk::MessageDialog::new(
            Some(&self.0.win),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Question,
            gtk::ButtonsType::OkCancel,
            &format!("Delete all {count} host(s)?"),
        );
        let resp = dlg.run();
        dlg.close();
        if resp != gtk::ResponseType::Ok {
            return;
        }
        self.0.app.with_cfg_mut(|c| c.ping.hosts.clear());
        self.0.app.save();
        self.0.app.sync_ping();
        self.rebuild_hosts_table();
    }

    /// Remove a host, promoting anything that sat behind it rather than
    /// stranding it under a parent that no longer exists.
    fn delete_host(&self, name: &str) {
        self.0.app.with_cfg_mut(|c| {
            let promote_to = c
                .ping
                .hosts
                .iter()
                .find(|h| h.name == name)
                .map(|h| h.parent.clone())
                .unwrap_or_default();
            for h in c.ping.hosts.iter_mut() {
                if h.parent == name {
                    h.parent = promote_to.clone();
                }
            }
            c.ping.hosts.retain(|h| h.name != name);
        });
        self.0.app.save();
        self.0.app.sync_ping();
        self.rebuild_hosts_table();
    }

    /// Re-read desktop state into the page: called when it is shown, and
    /// whenever the tray or the hotkey changes something behind its back.
    pub fn refresh_desktop_page(&self) {
        let hidden = self.0.app.cfg_snapshot().desktop_hidden;
        self.0.desktop_syncing.set(true);
        if let Some(sw) = self.0.icons_switch.borrow().as_ref() {
            sw.set_active(hidden);
        }
        self.0.desktop_syncing.set(false);
        if let Some(l) = self.0.icons_status.borrow().as_ref() {
            l.set_text(&desktop_icons_status(hidden));
        }
        if let Some(l) = self.0.snapshot_status.borrow().as_ref() {
            l.set_text(&snapshot_status());
        }
        self.rebuild_hosts_table();
    }

    /// The clock desklet: one switch to show it, and its options underneath.
    ///
    /// The options stay visible but go insensitive when the clock is off, so
    /// it's clear what turning it on will give you.
    fn build_clock_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("CLOCK"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        grid.set_halign(gtk::Align::Center);
        let row = Rc::new(Cell::new(0));
        let add_row = |grid: &gtk::Grid, row: &Rc<Cell<i32>>, label: &str, w: &gtk::Widget| {
            let l = gtk::Label::new(Some(label));
            l.set_xalign(1.0);
            grid.attach(&l, 0, row.get(), 1, 1);
            w.set_halign(gtk::Align::Start);
            grid.attach(w, 1, row.get(), 1, 1);
            row.set(row.get() + 1);
        };

        let c = self.0.app.cfg_snapshot().clock;

        // everything below the master switch, so it can be greyed out together
        let opts: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));

        let enabled = gtk::Switch::new();
        enabled.set_active(c.enabled);
        let this = self.clone();
        let o = opts.clone();
        enabled.connect_state_set(move |_, on| {
            this.0.app.with_cfg_mut(|cfg| cfg.clock.enabled = on);
            this.0.app.save();
            this.0.app.sync_clock();
            for w in o.borrow().iter() {
                w.set_sensitive(on);
            }
            glib::Propagation::Proceed
        });
        add_row(&grid, &row, "Show clock on the desktop", enabled.upcast_ref());

        // each option writes its field, saves, and re-renders the live clock
        let toggle = |label: &str, on: bool, set: fn(&mut config::Clock, bool)| {
            let sw = gtk::Switch::new();
            sw.set_active(on);
            let this = self.clone();
            sw.connect_state_set(move |_, v| {
                this.0.app.with_cfg_mut(|cfg| set(&mut cfg.clock, v));
                this.0.app.save();
                this.0.app.sync_clock();
                glib::Propagation::Proceed
            });
            opts.borrow_mut().push(sw.clone().upcast());
            add_row(&grid, &row, label, sw.upcast_ref());
        };
        toggle("24-hour time", c.twenty_four_hour, |c, v| c.twenty_four_hour = v);
        toggle("Show seconds", c.show_seconds, |c, v| c.show_seconds = v);
        toggle("Show the date", c.show_date, |c, v| c.show_date = v);
        toggle("Panel behind it", c.show_background, |c, v| c.show_background = v);
        toggle("Panel border", c.show_border, |c, v| c.show_border = v);

        let size = gtk::SpinButton::with_range(10.0, 300.0, 2.0);
        size.set_value(c.font_size as f64);
        let this = self.clone();
        size.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.clock.font_size = v);
            this.0.app.save();
            this.0.app.sync_clock();
        });
        opts.borrow_mut().push(size.clone().upcast());
        add_row(&grid, &row, "Text size", size.upcast_ref());

        let panel_bg = gtk::ColorButton::with_rgba(&hex_to_rgba(&c.bg_color));
        panel_bg.set_tooltip_text(Some("Colour of the panel behind the desklet."));
        let this = self.clone();
        panel_bg.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_cfg_mut(|cfg| cfg.clock.bg_color = hex.clone());
            this.0.app.save();
            this.0.app.sync_clock();
        });
        opts.borrow_mut().push(panel_bg.clone().upcast());
        add_row(&grid, &row, "Panel colour", panel_bg.upcast_ref());

        let panel_opacity = gtk::SpinButton::with_range(0.0, 100.0, 5.0);
        panel_opacity.set_value((c.bg_opacity * 100.0).round());
        panel_opacity.set_tooltip_text(Some(
            "How solid the panel is. 0 leaves the border, if it is switched on, with nothing behind it.",
        ));
        let this = self.clone();
        panel_opacity.connect_value_changed(move |w| {
            let v = w.value() / 100.0;
            this.0.app.with_cfg_mut(|cfg| cfg.clock.bg_opacity = v);
            this.0.app.save();
            this.0.app.sync_clock();
        });
        opts.borrow_mut().push(panel_opacity.clone().upcast());
        add_row(&grid, &row, "Panel opacity (%)", panel_opacity.upcast_ref());

        let panel_corners = gtk::SpinButton::with_range(0.0, 60.0, 2.0);
        panel_corners.set_value(c.corner_radius as f64);
        panel_corners.set_tooltip_text(Some(
            "Corner rounding of the desklet panel; 0 is square. Needs the panel switched on.",
        ));
        let this = self.clone();
        panel_corners.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.clock.corner_radius = v);
            this.0.app.save();
            this.0.app.sync_clock();
        });
        opts.borrow_mut().push(panel_corners.clone().upcast());
        add_row(&grid, &row, "Panel corners", panel_corners.upcast_ref());

        let padding = gtk::SpinButton::with_range(0.0, 80.0, 2.0);
        padding.set_value(c.padding as f64);
        padding.set_tooltip_text(Some(
            "Space between the panel edge and its contents. Needs the panel switched on.",
        ));
        let this = self.clone();
        padding.connect_value_changed(move |w| {
            let v = w.value() as i64;
            this.0.app.with_cfg_mut(|cfg| cfg.clock.padding = v);
            this.0.app.save();
            this.0.app.sync_clock();
        });
        opts.borrow_mut().push(padding.clone().upcast());
        add_row(&grid, &row, "Panel margin", padding.upcast_ref());

        let col = gtk::ColorButton::with_rgba(&hex_to_rgba(&c.color));
        let this = self.clone();
        col.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_cfg_mut(|cfg| cfg.clock.color = hex.clone());
            this.0.app.save();
            this.0.app.sync_clock();
        });
        opts.borrow_mut().push(col.clone().upcast());
        add_row(&grid, &row, "Text colour", col.upcast_ref());

        for w in opts.borrow().iter() {
            w.set_sensitive(c.enabled);
        }
        box_.pack_start(&grid, false, false, 0);

        let hint = gtk::Label::new(Some("Drag the clock to move it; its place is remembered."));
        hint.style_context().add_class("hedge-dim");
        box_.pack_start(&hint, false, false, 0);
        box_
    }

    /// Version + install state, with the install/upgrade/uninstall buttons.
    ///
    /// The labels are rebuilt by `refresh` after every action rather than read
    /// once, because installing changes what all three widgets should say.
    fn build_version_section(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);

        let sect = gtk::Label::new(Some("VERSION"));
        sect.style_context().add_class("hedge-section-title");
        box_.pack_start(&sect, false, false, 0);

        let running = gtk::Label::new(None);
        running.style_context().add_class("hedge-dim");
        let installed = gtk::Label::new(None);
        installed.style_context().add_class("hedge-dim");
        box_.pack_start(&running, false, false, 0);
        box_.pack_start(&installed, false, false, 0);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_halign(gtk::Align::Center);
        let action = gtk::Button::with_label("Install");
        let uninstall = gtk::Button::with_label("Uninstall");
        row.pack_start(&action, false, false, 0);
        row.pack_start(&uninstall, false, false, 0);
        box_.pack_start(&row, false, false, 0);

        let purge = gtk::CheckButton::with_label("Uninstall also deletes my hedges and settings");
        purge.set_halign(gtk::Align::Center);
        box_.pack_start(&purge, false, false, 0);

        let refresh: Rc<dyn Fn()> = {
            let (running, installed) = (running.clone(), installed.clone());
            let (action, uninstall) = (action.clone(), uninstall.clone());
            Rc::new(move || {
                let st = crate::install::state();
                running.set_text(&format!("This build: {}", st.running.label()));
                // A packaged build (the .deb) is upgraded and removed by the
                // package manager; copying it into $HOME on top would just
                // leave two Hedges racing at login. Uninstall stays live so a
                // leftover home install can still be cleaned out from here.
                if crate::install::packaged() {
                    installed.set_text("Installed: system package — your package manager owns it");
                    action.set_sensitive(false);
                    action.set_tooltip_text(Some(
                        "This build runs from /usr; upgrades come from the package",
                    ));
                    uninstall.set_sensitive(st.installed.is_some());
                    return;
                }
                match &st.installed {
                    None => {
                        installed.set_text("Installed: not installed");
                        action.set_label("Install Hedge");
                        action.set_tooltip_text(Some(
                            "Copy this build to your home directory and register autostart, the \
                             app-menu launcher and the desktop right-click actions",
                        ));
                        action.set_sensitive(true);
                        uninstall.set_sensitive(false);
                    }
                    Some(m) => {
                        let mut txt = format!("Installed: {}", m.build.label());
                        if st.is_installed_copy {
                            txt.push_str("  — you're running it");
                        }
                        installed.set_text(&txt);
                        let upgradable = st.upgrade_available();
                        action.set_label(if upgradable {
                            "Upgrade to this build"
                        } else {
                            "Reinstall this build"
                        });
                        action.set_tooltip_text(Some(if upgradable {
                            "Replace the installed copy with the newer build you're running"
                        } else {
                            "Write the binary and every desktop entry again"
                        }));
                        action.set_sensitive(true);
                        uninstall.set_sensitive(true);
                    }
                }
            })
        };
        refresh();

        let parent = |b: &gtk::Button| {
            b.toplevel().and_then(|w| w.downcast::<gtk::Window>().ok())
        };

        let this = self.clone();
        let r = refresh.clone();
        action.connect_clicked(move |b| {
            this.0.app.run_install(parent(b).as_ref());
            r();
        });

        let this = self.clone();
        let r = refresh.clone();
        let purge_cb = purge.clone();
        uninstall.connect_clicked(move |b| {
            this.0.app.run_uninstall(parent(b).as_ref(), purge_cb.is_active());
            r();
        });

        box_
    }

    // -- public API used by HedgeApp ---------------------------------------
    pub fn present(&self) {
        self.0.win.show_all();
        // the desktop can have been changed by the tray or the hotkey since
        // this window was last looked at, and so can the list of hedges
        self.refresh_desktop_page();
        self.refresh_hedge_list();
        self.0.win.present();
    }

    pub fn is_visible(&self) -> bool {
        self.0.win.is_visible()
    }

    /// Switch to the Desklet page — what the desklet's own gear icon asks for.
    /// Selecting the row is what builds the page, the same as a click would.
    pub fn show_desklet_page(&self) {
        if let Some(row) = self.0.nav.row_at_index(1) {
            self.0.nav.select_row(Some(&row));
        }
    }

    pub fn on_hedges_changed(&self, select: Option<&str>) {
        self.reload_sidebar(select.is_none(), select);
    }

    /// Re-read the hedge list after one was added from outside this window
    /// (the tray, `--add`), keeping whichever hedge is open selected.
    pub fn refresh_hedge_list(&self) {
        let current = self
            .0
            .list
            .selected_row()
            .and_then(|r| self.name_at_row(&r));
        if *self.0.names.borrow() == config::hedge_names(&snapshot_cfg(&self.0.app)) {
            return; // nothing changed: don't rebuild the open page under the user
        }
        self.reload_sidebar(false, current.as_deref());
    }

    // -- sidebar ------------------------------------------------------------
    fn build_sidebar(&self) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
        box_.style_context().add_class("hedge-sidebar");
        box_.set_size_request(200, -1);

        // The pages that aren't a hedge come first: they are a fixed, short
        // list, so putting them above a hedge list that grows keeps them in
        // the same place no matter how many hedges there are.
        self.0.nav.set_selection_mode(gtk::SelectionMode::Single);
        for label in [NAV_GENERAL, NAV_DESKLET] {
            let row = gtk::ListBoxRow::new();
            let lbl = gtk::Label::new(Some(label));
            lbl.set_xalign(0.0);
            row.add(&lbl);
            self.0.nav.add(&row);
        }
        let this = self.clone();
        self.0.nav.connect_row_selected(move |_, row| this.on_nav_selected(row));
        box_.pack_start(&self.0.nav, false, false, 0);

        box_.pack_start(
            &gtk::Separator::new(gtk::Orientation::Horizontal),
            false,
            false,
            0,
        );

        let title = gtk::Label::new(Some("HEDGES"));
        title.set_xalign(0.0);
        title.style_context().add_class("hedge-section-title");
        box_.pack_start(&title, false, false, 0);

        self.0.list.set_selection_mode(gtk::SelectionMode::Single);
        self.0.list.set_activate_on_single_click(false);
        let this = self.clone();
        self.0.list.connect_row_selected(move |_, row| this.on_row_selected(row));
        let this = self.clone();
        self.0.list.connect_row_activated(move |_, _| this.on_rename());

        let scroll = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.add(&self.0.list);
        box_.pack_start(&scroll, true, true, 0);

        let btns = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        btns.set_homogeneous(true);
        let add_btn = |icon: &str, tip: &str| {
            let b = gtk::Button::from_icon_name(Some(icon), gtk::IconSize::Button);
            b.set_tooltip_text(Some(tip));
            b
        };
        let b_add = add_btn("list-add-symbolic", "Add Hedge");
        let this = self.clone();
        b_add.connect_clicked(move |_| this.on_add());
        let b_rem = add_btn("list-remove-symbolic", "Remove Hedge");
        let this = self.clone();
        b_rem.connect_clicked(move |_| this.on_remove());
        let b_ren = add_btn("document-edit-symbolic", "Rename Hedge");
        let this = self.clone();
        b_ren.connect_clicked(move |_| this.on_rename());
        btns.pack_start(&b_add, true, true, 0);
        btns.pack_start(&b_rem, true, true, 0);
        btns.pack_start(&b_ren, true, true, 0);
        box_.pack_start(&btns, false, false, 6);
        box_
    }

    /// Select a page by name, as if it had been clicked.
    fn select_nav(&self, label: &str) {
        let index = match label {
            NAV_GENERAL => 0,
            _ => 1,
        };
        if let Some(row) = self.0.nav.row_at_index(index) {
            self.0.nav.select_row(Some(&row));
        }
    }

    fn on_nav_selected(&self, row: Option<&gtk::ListBoxRow>) {
        // the selection we are being told about may be the one we just cleared
        if self.0.nav_syncing.get() {
            return;
        }
        let Some(row) = row else { return };
        // one selection across both lists, so it is always clear what the
        // detail pane is showing
        self.0.nav_syncing.set(true);
        self.0.list.unselect_all();
        self.0.nav_syncing.set(false);
        *self.0.current.borrow_mut() = None;

        let page = if row.index() == 0 {
            self.build_general_page()
        } else {
            self.build_desklet_page()
        };
        self.set_detail(&page.upcast());
    }

    fn reload_sidebar(&self, select_first: bool, select_name: Option<&str>) {
        for child in self.0.list.children() {
            self.0.list.remove(&child);
        }
        let names = config::hedge_names(&snapshot_cfg(&self.0.app));
        *self.0.names.borrow_mut() = names.clone();
        for name in &names {
            let row = gtk::ListBoxRow::new();
            let lbl = gtk::Label::new(Some(name));
            lbl.set_xalign(0.0);
            row.add(&lbl);
            self.0.list.add(&row);
        }
        self.0.list.show_all();

        let target = select_name
            .map(str::to_string)
            .or_else(|| if select_first { names.first().cloned() } else { None });
        if let Some(target) = target {
            if let Some(idx) = names.iter().position(|n| *n == target) {
                if let Some(row) = self.0.list.row_at_index(idx as i32) {
                    self.0.list.select_row(Some(&row));
                }
            }
        }
    }

    fn name_at_row(&self, row: &gtk::ListBoxRow) -> Option<String> {
        let idx = row.index();
        if idx < 0 {
            return None;
        }
        self.0.names.borrow().get(idx as usize).cloned()
    }

    fn on_row_selected(&self, row: Option<&gtk::ListBoxRow>) {
        // being deselected because a page was picked on the other list is not
        // the same as the user selecting nothing
        if self.0.nav_syncing.get() {
            return;
        }
        match row.and_then(|r| self.name_at_row(r)) {
            None => {
                *self.0.current.borrow_mut() = None;
                self.set_detail(&gtk::Label::new(Some("No Hedge selected")).upcast());
            }
            Some(name) => {
                self.0.nav_syncing.set(true);
                self.0.nav.unselect_all();
                self.0.nav_syncing.set(false);
                *self.0.current.borrow_mut() = Some(name.clone());
                self.build_detail(&name);
            }
        }
    }

    // -- sidebar actions ----------------------------------------------------
    fn on_add(&self) {
        let name = {
            let app = self.0.app.clone();
            app.with_cfg_mut(|cfg| config::add_hedge(cfg, None, false))
        };
        self.0.app.save();
        self.0.app.sync_hedges();
        self.reload_sidebar(false, Some(&name));
    }

    fn on_remove(&self) {
        let current = self.0.current.borrow().clone();
        let Some(current) = current else { return };
        let dlg = gtk::MessageDialog::new(
            Some(&self.0.win),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Question,
            gtk::ButtonsType::OkCancel,
            &format!("Remove Hedge “{current}”?"),
        );
        let resp = dlg.run();
        dlg.close();
        if resp == gtk::ResponseType::Ok {
            self.0.app.with_cfg_mut(|cfg| config::remove_hedge(cfg, &current));
            self.0.app.save();
            self.0.app.sync_hedges();
            *self.0.current.borrow_mut() = None;
            self.reload_sidebar(true, None);
        }
    }

    fn on_rename(&self) {
        let current = self.0.current.borrow().clone();
        let Some(current) = current else { return };
        if let Some(new) = self.prompt_text("Rename Hedge", "New name:", &current) {
            self.0.app.rename_hedge_live(&current, &new);
        }
    }

    fn prompt_text(&self, title: &str, label: &str, initial: &str) -> Option<String> {
        let dlg = gtk::Dialog::with_buttons(
            Some(title),
            Some(&self.0.win),
            gtk::DialogFlags::MODAL,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("OK", gtk::ResponseType::Ok),
            ],
        );
        let area = dlg.content_area();
        area.set_spacing(8);
        area.set_border_width(12);
        let lbl = gtk::Label::new(Some(label));
        lbl.set_xalign(0.0);
        area.add(&lbl);
        let entry = gtk::Entry::new();
        entry.set_text(initial);
        entry.set_activates_default(true);
        dlg.set_default_response(gtk::ResponseType::Ok);
        area.add(&entry);
        dlg.show_all();
        let result = if dlg.run() == gtk::ResponseType::Ok {
            let t = entry.text().trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        } else {
            None
        };
        dlg.close();
        result
    }

    // -- detail panel -------------------------------------------------------
    fn set_detail(&self, widget: &gtk::Widget) {
        if let Some(old) = self.0.detail_scroll.child() {
            self.0.detail_scroll.remove(&old);
        }
        self.0.detail_scroll.add(widget);
        self.0.detail_scroll.show_all();
    }

    fn changed(&self, key: &str) {
        if self.0.loading.get() {
            return;
        }
        let current = self.0.current.borrow().clone();
        let Some(current) = current else { return };
        self.0.app.save();
        self.0.app.refresh_hedge(&current, refresh_for(key));
    }

    fn build_detail(&self, name: &str) {
        self.clear_page_handles();
        self.0.loading.set(true);
        let b = self.0.app.hedge(name).unwrap_or_default();

        let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
        outer.style_context().add_class("hedge-detail");

        let grid = gtk::Grid::new();
        grid.set_column_spacing(12);
        grid.set_row_spacing(10);
        let row = Rc::new(Cell::new(0i32));
        let add_row = |grid: &gtk::Grid, row: &Rc<Cell<i32>>, label: &str, widget: &gtk::Widget| {
            let lab = gtk::Label::new(Some(label));
            lab.set_xalign(0.0);
            let r = row.get();
            grid.attach(&lab, 0, r, 1, 1);
            grid.attach(widget, 1, r, 1, 1);
            row.set(r + 1);
        };

        // colours
        let name_s = name.to_string();
        let accent = gtk::ColorButton::with_rgba(&hex_to_rgba(&b.accent));
        let this = self.clone();
        let n = name_s.clone();
        accent.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_hedge_mut(&n, |f| f.accent = hex.clone());
            this.changed("accent");
        });
        add_row(&grid, &row, "Accent colour", accent.upcast_ref());

        let bgc = gtk::ColorButton::with_rgba(&hex_to_rgba(&b.bg_color));
        let this = self.clone();
        let n = name_s.clone();
        bgc.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_hedge_mut(&n, |f| f.bg_color = hex.clone());
            this.changed("bgColor");
        });
        add_row(&grid, &row, "Background colour", bgc.upcast_ref());

        // Tile label text (the app name under each icon).
        let lblc = gtk::ColorButton::with_rgba(&hex_to_rgba(&b.label_color));
        let this = self.clone();
        let n = name_s.clone();
        lblc.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_hedge_mut(&n, |f| f.label_color = hex.clone());
            this.changed("labelColor");
        });
        lblc.set_tooltip_text(Some("Colour of the app name shown under each icon"));
        add_row(&grid, &row, "Label colour", lblc.upcast_ref());

        // Title bar text. An empty stored value means "choose automatically";
        // show what is actually being drawn so the button isn't blank.
        let title_now = if b.title_color.trim().is_empty() {
            crate::colors::contrast_text(&b.accent)
        } else {
            b.title_color.clone()
        };
        let ttlc = gtk::ColorButton::with_rgba(&hex_to_rgba(&title_now));
        let this = self.clone();
        let n = name_s.clone();
        ttlc.connect_color_set(move |w| {
            let hex = rgba_to_hex(&w.rgba());
            this.0.app.with_hedge_mut(&n, |f| f.title_color = hex.clone());
            this.changed("titleColor");
        });
        ttlc.set_tooltip_text(Some(
            "Colour of the hedge's own name in the title bar. Starts out chosen \
             automatically to contrast with the accent; setting it here pins it.",
        ));
        add_row(&grid, &row, "Title colour", ttlc.upcast_ref());

        add_row(&grid, &row, "Transparency",
            self.slider(&name_s, "bgOpacity", 0.0, 1.0, 0.05, true).upcast_ref());
        add_row(&grid, &row, "Corner radius (0 = square)",
            self.slider(&name_s, "cornerRadius", 0.0, 60.0, 1.0, false).upcast_ref());
        add_row(&grid, &row, "Columns",
            self.slider(&name_s, "columns", 1.0, 12.0, 1.0, false).upcast_ref());
        add_row(&grid, &row, "Icon size",
            self.slider(&name_s, "iconSize", 16.0, 128.0, 2.0, false).upcast_ref());
        add_row(&grid, &row, "Tile spacing",
            self.slider(&name_s, "tileSpacing", 0.0, 40.0, 1.0, false).upcast_ref());
        add_row(&grid, &row, "Padding",
            self.slider(&name_s, "padding", 0.0, 60.0, 1.0, false).upcast_ref());
        // 11 is the largest size the fixed-height bar can hold; above that the
        // bar grows with the font, which is why the label says so.
        add_row(&grid, &row, "Title font size (>11 grows the bar)",
            self.slider(&name_s, "titleFontSize", 6.0, 20.0, 1.0, false).upcast_ref());
        // the tile's width follows the font too: a line wraps at 12
        // characters of whatever size the label is
        add_row(&grid, &row, "Label font size",
            self.slider(&name_s, "labelFontSize", 6.0, 24.0, 1.0, false).upcast_ref());
        add_row(&grid, &row, "Show labels",
            self.switch(&name_s, "showLabels").upcast_ref());
        add_row(&grid, &row, "Show title",
            self.switch(&name_s, "showTitle").upcast_ref());
        add_row(&grid, &row, "Show background",
            self.switch(&name_s, "showBackground").upcast_ref());
        add_row(&grid, &row, "Open on hover (when collapsed)",
            self.switch(&name_s, "expandOnHover").upcast_ref());
        add_row(&grid, &row, "Open on title-bar hover (when rolled up)",
            self.switch(&name_s, "openOnHover").upcast_ref());
        add_row(&grid, &row, "Always show border",
            self.switch(&name_s, "showBorder").upcast_ref());
        add_row(&grid, &row, "Hide until mouse-over",
            self.switch(&name_s, "hideUntilHover").upcast_ref());

        outer.pack_start(&grid, false, false, 0);

        // Rescue for a hedge that has ended up off-screen (monitor unplugged,
        // hand-edited config) where there is nothing left to grab and drag.
        let recentre = gtk::Button::with_label("Centre on screen");
        recentre.set_tooltip_text(Some(
            "Move this Hedge back to the middle of the primary monitor, undocked",
        ));
        recentre.set_halign(gtk::Align::Start);
        let this = self.clone();
        let n = name_s.clone();
        recentre.connect_clicked(move |_| this.0.app.center_hedge(&n));
        outer.pack_start(&recentre, false, false, 0);

        let sect = gtk::Label::new(Some("APPS"));
        sect.set_xalign(0.0);
        sect.style_context().add_class("hedge-section-title");
        outer.pack_start(&sect, false, false, 0);
        outer.pack_start(&self.build_apps(&name_s), true, true, 0);

        self.set_detail(outer.upcast_ref());
        self.0.loading.set(false);
    }

    fn slider(&self, name: &str, key: &str, lo: f64, hi: f64, step: f64, pct: bool) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let cur = self.0.app.hedge(name).map(|f| hedge_get(&f, key)).unwrap_or(lo);
        let adj = gtk::Adjustment::new(cur, lo, hi, step, step, 0.0);
        let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adj));
        scale.set_hexpand(true);
        scale.set_size_request(160, -1);
        scale.set_draw_value(false);
        let entry = gtk::Entry::new();
        entry.set_width_chars(4);
        entry.set_max_width_chars(4);
        entry.set_alignment(1.0);

        let fmt = move |v: f64| -> String {
            if pct {
                format!("{}", (v * 100.0).round() as i64)
            } else {
                format!("{}", v.round() as i64)
            }
        };
        entry.set_text(&fmt(adj.value()));

        let guard = Rc::new(Cell::new(false));
        let key_s = key.to_string();
        let name_s = name.to_string();

        // scale -> entry + config
        {
            let (this, entry, adj, guard, fmt) =
                (self.clone(), entry.clone(), adj.clone(), guard.clone(), fmt.clone());
            let key_s = key_s.clone();
            let name_s = name_s.clone();
            adj.clone().connect_value_changed(move |adj| {
                if guard.get() {
                    return;
                }
                guard.set(true);
                entry.set_text(&fmt(adj.value()));
                guard.set(false);
                let v = adj.value();
                this.0.app.with_hedge_mut(&name_s, |f| hedge_set(f, &key_s, v, pct));
                this.changed(&key_s);
            });
        }

        // entry -> scale + config
        {
            let (this, adj, guard, fmt) =
                (self.clone(), adj.clone(), guard.clone(), fmt.clone());
            let key_s = key_s.clone();
            let name_s = name_s.clone();
            let commit = move |entry: &gtk::Entry| {
                let raw: f64 = match entry.text().parse() {
                    Ok(v) => v,
                    Err(_) => {
                        entry.set_text(&fmt(adj.value()));
                        return;
                    }
                };
                let v = if pct { raw / 100.0 } else { raw };
                let v = v.clamp(lo, hi);
                guard.set(true);
                adj.set_value(v);
                guard.set(false);
                entry.set_text(&fmt(v));
                this.0.app.with_hedge_mut(&name_s, |f| hedge_set(f, &key_s, v, pct));
                this.changed(&key_s);
            };
            let c1 = commit.clone();
            entry.connect_activate(move |e| c1(e));
            entry.connect_focus_out_event(move |e, _| {
                commit(e);
                glib::Propagation::Proceed
            });
        }

        box_.pack_start(&scale, true, true, 0);
        box_.pack_start(&entry, false, false, 0);
        box_
    }

    fn switch(&self, name: &str, key: &str) -> gtk::Switch {
        let sw = gtk::Switch::new();
        sw.set_halign(gtk::Align::Start);
        let cur = self
            .0
            .app
            .hedge(name)
            .map(|f| hedge_get_bool(&f, key))
            .unwrap_or(true);
        sw.set_active(cur);
        let this = self.clone();
        let key_s = key.to_string();
        let name_s = name.to_string();
        sw.connect_state_set(move |_, state| {
            this.0.app.with_hedge_mut(&name_s, |f| hedge_set_bool(f, &key_s, state));
            this.changed(&key_s);
            glib::Propagation::Proceed
        });
        sw
    }

    // -- apps editor --------------------------------------------------------
    fn build_apps(&self, name: &str) -> gtk::Box {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let apps_list = gtk::ListBox::new();
        apps_list.set_selection_mode(gtk::SelectionMode::Single);
        *self.0.apps_list.borrow_mut() = Some(apps_list.clone());
        let frame = gtk::Frame::new(None);
        frame.add(&apps_list);
        box_.pack_start(&frame, true, true, 0);
        self.reload_apps(name);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let mkbtn = |icon: &str, tip: &str| {
            let b = gtk::Button::from_icon_name(Some(icon), gtk::IconSize::Button);
            b.set_tooltip_text(Some(tip));
            b
        };
        let n = name.to_string();
        let b_add = mkbtn("list-add-symbolic", "Add app");
        let this = self.clone();
        let nn = n.clone();
        b_add.connect_clicked(move |_| this.on_app_add(&nn));
        let b_rem = mkbtn("list-remove-symbolic", "Remove app");
        let this = self.clone();
        let nn = n.clone();
        b_rem.connect_clicked(move |_| this.on_app_remove(&nn));
        let b_up = mkbtn("go-up-symbolic", "Move up");
        let this = self.clone();
        let nn = n.clone();
        b_up.connect_clicked(move |_| this.move_app(&nn, -1));
        let b_dn = mkbtn("go-down-symbolic", "Move down");
        let this = self.clone();
        let nn = n.clone();
        b_dn.connect_clicked(move |_| this.move_app(&nn, 1));
        bar.pack_start(&b_add, false, false, 0);
        bar.pack_start(&b_rem, false, false, 0);
        bar.pack_start(&b_up, false, false, 0);
        bar.pack_start(&b_dn, false, false, 0);

        let hint = gtk::Label::new(Some("  …or drag apps from the menu/desktop onto a Hedge"));
        hint.set_xalign(0.0);
        hint.style_context().add_class("hedge-dim");
        bar.pack_start(&hint, false, false, 0);
        box_.pack_start(&bar, false, false, 0);
        box_
    }

    fn reload_apps(&self, name: &str) {
        let Some(list) = self.0.apps_list.borrow().clone() else {
            return;
        };
        for child in list.children() {
            list.remove(&child);
        }
        let b = self.0.app.hedge(name).unwrap_or_default();
        for app in &b.apps {
            let row = gtk::ListBoxRow::new();
            let rb = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            rb.style_context().add_class("hedge-app-row");
            let icon = if app.icon.is_empty() {
                "application-x-executable"
            } else {
                &app.icon
            };
            rb.pack_start(
                &gtk::Image::from_icon_name(Some(icon), gtk::IconSize::LargeToolbar),
                false,
                false,
                0,
            );
            let txt = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let display = if app.name.is_empty() { &app.command } else { &app.name };
            let nm = gtk::Label::new(Some(display));
            nm.set_xalign(0.0);
            let cmd = gtk::Label::new(Some(&app.command));
            cmd.set_xalign(0.0);
            cmd.style_context().add_class("hedge-dim");
            txt.pack_start(&nm, false, false, 0);
            txt.pack_start(&cmd, false, false, 0);
            rb.pack_start(&txt, true, true, 0);
            row.add(&rb);
            list.add(&row);
        }
        list.show_all();
    }

    fn selected_app_index(&self) -> Option<usize> {
        let list = self.0.apps_list.borrow().clone()?;
        let row = list.selected_row()?;
        let idx = row.index();
        if idx < 0 {
            None
        } else {
            Some(idx as usize)
        }
    }

    fn on_app_add(&self, name: &str) {
        if let Some(entry) = self.app_picker() {
            self.0.app.with_hedge_mut(name, |f| f.apps.push(entry));
            self.changed("apps");
            self.reload_apps(name);
        }
    }

    fn on_app_remove(&self, name: &str) {
        let Some(idx) = self.selected_app_index() else {
            return;
        };
        let gone = self
            .0
            .app
            .with_hedge_mut(name, |f| (idx < f.apps.len()).then(|| f.apps.remove(idx)));
        if let Some(entry) = gone.flatten() {
            crate::desktop::restore_entry_to_view(&entry.command);
        }
        self.changed("apps");
        self.reload_apps(name);
    }

    fn move_app(&self, name: &str, delta: i32) {
        let Some(idx) = self.selected_app_index() else {
            return;
        };
        let j = idx as i32 + delta;
        self.0.app.with_hedge_mut(name, |f| {
            if j >= 0 && (j as usize) < f.apps.len() {
                f.apps.swap(idx, j as usize);
            }
        });
        self.changed("apps");
        self.reload_apps(name);
    }

    fn app_picker(&self) -> Option<AppEntry> {
        let dlg = gtk::Dialog::with_buttons(
            Some("Add an app"),
            Some(&self.0.win),
            gtk::DialogFlags::MODAL,
            &[
                ("Cancel", gtk::ResponseType::Cancel),
                ("Add", gtk::ResponseType::Ok),
            ],
        );
        dlg.set_default_size(420, 460);
        let area = dlg.content_area();
        area.set_border_width(8);

        let search = gtk::SearchEntry::new();
        area.pack_start(&search, false, false, 0);
        let listbox = gtk::ListBox::new();
        let scroll = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        scroll.set_vexpand(true);
        scroll.add(&listbox);
        area.pack_start(&scroll, true, true, 0);

        let apps = Rc::new(desktop_apps::scan_apps());
        // rows filtered in place; store the visible subset's entries by index
        let visible: Rc<RefCell<Vec<desktop_apps::InstalledApp>>> = Rc::new(RefCell::new(Vec::new()));

        let populate = {
            let listbox = listbox.clone();
            let apps = apps.clone();
            let visible = visible.clone();
            move |term: &str| {
                for c in listbox.children() {
                    listbox.remove(&c);
                }
                let term = term.to_lowercase();
                let mut vis = visible.borrow_mut();
                vis.clear();
                for a in apps.iter() {
                    if !term.is_empty() && !a.name.to_lowercase().contains(&term) {
                        continue;
                    }
                    let row = gtk::ListBoxRow::new();
                    let hb = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    hb.pack_start(
                        &gtk::Image::from_icon_name(Some(&a.icon), gtk::IconSize::LargeToolbar),
                        false,
                        false,
                        0,
                    );
                    let lbl = gtk::Label::new(Some(&a.name));
                    lbl.set_xalign(0.0);
                    hb.pack_start(&lbl, true, true, 0);
                    row.add(&hb);
                    listbox.add(&row);
                    vis.push(a.clone());
                }
                listbox.show_all();
            }
        };
        populate("");
        {
            let populate = populate.clone();
            search.connect_search_changed(move |s| populate(&s.text()));
        }
        {
            let dlg = dlg.clone();
            listbox.connect_row_activated(move |_, _| dlg.response(gtk::ResponseType::Ok));
        }

        let result = if dlg.run() == gtk::ResponseType::Ok {
            listbox.selected_row().and_then(|row| {
                let idx = row.index();
                if idx < 0 {
                    None
                } else {
                    visible.borrow().get(idx as usize).map(|e| {
                        AppEntry::new(&e.name, &e.command, &e.icon)
                    })
                }
            })
        } else {
            None
        };
        dlg.close();
        result
    }
}

// -- numeric field get/set bridges (Python treated the block as a dict) -----
fn hedge_get(f: &config::Hedge, key: &str) -> f64 {
    match key {
        "bgOpacity" => f.bg_opacity,
        "cornerRadius" => f.corner_radius as f64,
        "columns" => f.columns as f64,
        "iconSize" => f.icon_size as f64,
        "tileSpacing" => f.tile_spacing as f64,
        "padding" => f.padding as f64,
        "titleFontSize" => f.title_font_size as f64,
        "labelFontSize" => f.label_font_size as f64,
        _ => 0.0,
    }
}

fn hedge_set(f: &mut config::Hedge, key: &str, v: f64, pct: bool) {
    if pct {
        // stored rounded to 3 decimals, matching Python
        let v = (v * 1000.0).round() / 1000.0;
        if key == "bgOpacity" {
            f.bg_opacity = v;
        }
        return;
    }
    let iv = v as i64;
    match key {
        "cornerRadius" => f.corner_radius = iv,
        "columns" => f.columns = iv,
        "iconSize" => f.icon_size = iv,
        "tileSpacing" => f.tile_spacing = iv,
        "padding" => f.padding = iv,
        "titleFontSize" => f.title_font_size = iv,
        "labelFontSize" => f.label_font_size = iv,
        _ => {}
    }
}

fn hedge_get_bool(f: &config::Hedge, key: &str) -> bool {
    match key {
        "showLabels" => f.show_labels,
        "showTitle" => f.show_title,
        "showBackground" => f.show_background,
        "expandOnHover" => f.expand_on_hover,
        "openOnHover" => f.open_on_hover,
        "showBorder" => f.show_border,
        "hideUntilHover" => f.hide_until_hover,
        _ => true,
    }
}

fn hedge_set_bool(f: &mut config::Hedge, key: &str, v: bool) {
    match key {
        "showLabels" => f.show_labels = v,
        "showTitle" => f.show_title = v,
        "showBackground" => f.show_background = v,
        "expandOnHover" => f.expand_on_hover = v,
        "openOnHover" => f.open_on_hover = v,
        "showBorder" => f.show_border = v,
        "hideUntilHover" => f.hide_until_hover = v,
        _ => {}
    }
}

fn snapshot_cfg(app: &HedgeApp) -> config::Config {
    app.cfg_snapshot()
}

/// One-line description of the stored snapshot, for the General page.
/// Whether `name` sits anywhere below `ancestor` in the host tree. Bounded by
/// the host count so a config hand-edited into a cycle can still be opened.
fn is_descendant(cfg: &config::Ping, name: &str, ancestor: &str) -> bool {
    let mut at = name;
    for _ in 0..cfg.hosts.len() {
        let Some(h) = cfg.hosts.iter().find(|h| h.name == at) else {
            return false;
        };
        if h.parent.is_empty() {
            return false;
        }
        if h.parent == ancestor {
            return true;
        }
        at = &h.parent;
    }
    false
}

fn desktop_icons_status(hidden: bool) -> String {
    if !hidden {
        return "Icons are showing.".to_string();
    }
    let stashed = std::fs::read_dir(crate::desktop::stash_dir())
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    format!("Icons are hidden — {stashed} item(s) in the stash.")
}

fn snapshot_status() -> String {
    match crate::snapshot::existing() {
        Some(ts) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(ts);
            let mins = now.saturating_sub(ts) / 60;
            if mins < 1 {
                "Saved just now".to_string()
            } else if mins < 60 {
                format!("Saved {mins} minute(s) ago")
            } else if mins < 60 * 24 {
                format!("Saved {} hour(s) ago", mins / 60)
            } else {
                format!("Saved {} day(s) ago", mins / (60 * 24))
            }
        }
        None => "No desktop snapshot saved yet".to_string(),
    }
}

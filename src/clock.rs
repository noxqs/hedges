//! The clock desklet: a time (and date) readout that sits on the desktop.
//!
//! Deliberately a much smaller thing than [`HedgeWindow`](crate::hedge_window):
//! no grid, no drop target, no edge docking, no resize — a clock is text you
//! place once. What it *does* share is the window recipe that makes a widget
//! behave like part of the desktop rather than an application: undecorated,
//! translucent, kept below, on every workspace, out of the taskbar and
//! alt-tab, and never taking focus.
//!
//! Switched on in Settings → General; off by default, so an existing config
//! gains nothing on upgrade.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::app::HedgeApp;
use crate::colors;

/// Movement before a press counts as a drag rather than a click.
const DRAG_SLOP: f64 = 4.0;
/// Debounce on writing a new position to the config.
const SAVE_DELAY_MS: u64 = 400;
/// The date is drawn at this fraction of the time's size.
const DATE_SCALE: f64 = 0.34;
/// The host table is drawn at this fraction of the time's size, floored so it
/// stays legible under a small clock.
const PING_SCALE: f64 = 0.22;

/// Which part of the info panel a control shows and hides.
#[derive(Clone, Copy)]
enum Section {
    Hosts,
    Charts,
    Procs,
    Claude,
}


struct Drag {
    /// Pointer position when the button went down, in root coordinates.
    root: (f64, f64),
    /// Window position at that moment.
    win: (i32, i32),
    moved: bool,
}

struct Inner {
    app: HedgeApp,
    win: gtk::Window,
    time: gtk::Label,
    date: gtk::Label,
    /// The controls to the right of the time: open Settings, then show/hide the
    /// hosts, the charts and the busiest processes. A toggle is hidden outright
    /// when its feature is switched off, so none is ever a dead control.
    hosts_btn: gtk::Button,
    charts_btn: gtk::Button,
    procs_btn: gtk::Button,
    claude_btn: gtk::Button,
    /// Every control's image, so `apply_css` can scale them with the clock.
    icons: Vec<gtk::Image>,
    /// Container the host table is rebuilt into after every round.
    ping_box: gtk::Box,
    /// The mini charts, below the host table.
    sysmon: crate::sysmon_view::SysmonPanel,
    /// The Claude section, rebuilt whenever one of its polls lands.
    claude_box: gtk::Box,
    css: RefCell<Option<gtk::CssProvider>>,
    drag: RefCell<Option<Drag>>,
    tick: RefCell<Option<glib::SourceId>>,
    save_timer: RefCell<Option<glib::SourceId>>,
    /// Last rendered strings, so a per-second tick only touches the labels
    /// when the text actually changes.
    shown: RefCell<(String, String)>,
    visible: Cell<bool>,
}

/// Clonable handle to the live clock window.
#[derive(Clone)]
pub struct ClockWindow(Rc<Inner>);

impl ClockWindow {
    pub fn new(app: &HedgeApp) -> Self {
        let cfg = app.cfg_snapshot().clock;

        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        win.set_title("Hedge Clock");
        win.set_decorated(false);
        win.set_resizable(false);
        win.set_app_paintable(true);
        // Normal, not Utility: a transient type with no transient-parent gets
        // promoted out of the below-layer by Muffin the moment another window
        // in our group appears. See ARCHITECTURE.md.
        win.set_type_hint(gdk::WindowTypeHint::Normal);
        win.set_keep_below(true);
        win.stick();
        win.set_skip_taskbar_hint(true);
        win.set_skip_pager_hint(true);
        // never steal the keyboard from whatever the user is doing
        win.set_accept_focus(false);
        win.set_focus_on_map(false);
        if let Some(screen) = gdk::Screen::default() {
            if let Some(visual) = screen.rgba_visual() {
                win.set_visual(Some(&visual));
            }
        }

        let ebox = gtk::EventBox::new();
        ebox.set_visible_window(false);
        ebox.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::BUTTON_RELEASE_MASK
                | gdk::EventMask::POINTER_MOTION_MASK,
        );
        win.add(&ebox);

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.style_context().add_class("clock-panel");
        ebox.add(&panel);

        let time = gtk::Label::new(None);
        time.style_context().add_class("clock-time");

        // The time and its three controls travel together: the row is centred
        // as one unit, so the icons sit against the right of the time itself
        // rather than drifting to the far edge when the panel below is wider.
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        head.set_halign(gtk::Align::Center);
        head.pack_start(&time, false, false, 0);

        // Stacked, not in a row: four of them at the scale below come to about
        // the height of the time itself, so the column costs the desklet no
        // extra height and keeps the time the widest thing in the header.
        let icon_row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // centred against the full height of the time, not its baseline
        icon_row.set_valign(gtk::Align::Center);
        icon_row.style_context().add_class("clock-icons");
        // `-symbolic` on purpose: a symbolic icon is recoloured to the CSS
        // colour, so these follow whatever colour the clock is set to.
        let control = |icon: &str, tip: &str| {
            let img = gtk::Image::from_icon_name(Some(icon), gtk::IconSize::Menu);
            let btn = gtk::Button::new();
            btn.add(&img);
            btn.set_relief(gtk::ReliefStyle::None);
            btn.set_focus_on_click(false);
            btn.set_tooltip_text(Some(tip));
            btn.style_context().add_class("clock-icon");
            (btn, img)
        };
        let (cfg_btn, cfg_img) = control("emblem-system-symbolic", "Desklet settings");
        let (hosts_btn, hosts_img) = control("network-wired-symbolic", "");
        let (charts_btn, charts_img) = control("utilities-system-monitor-symbolic", "");
        let (procs_btn, procs_img) = control("view-list-symbolic", "");
        // No stock icon says "Claude"; the starburst glyph is the closest
        // thing to its mark that a plain label can draw, and as text it takes
        // the control strip's colour and scale through CSS like the icons do.
        let claude_glyph = gtk::Label::new(Some("✱"));
        claude_glyph.style_context().add_class("claude-glyph");
        let claude_btn = gtk::Button::new();
        claude_btn.add(&claude_glyph);
        claude_btn.set_relief(gtk::ReliefStyle::None);
        claude_btn.set_focus_on_click(false);
        claude_btn.style_context().add_class("clock-icon");
        // A toggle whose feature is off is hidden rather than greyed, so it is
        // kept clear of `show_all` and revealed by `refresh_panel`. Its image
        // is shown here explicitly — `show_all` skips a no-show-all widget and
        // everything under it, which would leave an empty, invisible button.
        for (btn, img) in [
            (&hosts_btn, &hosts_img),
            (&charts_btn, &charts_img),
            (&procs_btn, &procs_img),
        ] {
            btn.set_no_show_all(true);
            img.show();
        }
        claude_btn.set_no_show_all(true);
        claude_glyph.show();
        for btn in [&cfg_btn, &hosts_btn, &charts_btn, &procs_btn, &claude_btn] {
            icon_row.pack_start(btn, false, false, 0);
        }
        head.pack_start(&icon_row, false, false, 0);
        panel.pack_start(&head, false, false, 0);

        let date = gtk::Label::new(None);
        date.style_context().add_class("clock-date");
        panel.pack_start(&date, false, false, 0);

        let ping_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        ping_box.set_no_show_all(true);
        ping_box.style_context().add_class("ping-panel");
        panel.pack_start(&ping_box, false, false, 0);

        let sysmon = crate::sysmon_view::SysmonPanel::new(app);
        panel.pack_start(&sysmon.widget(), false, false, 0);

        let claude_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        claude_box.set_no_show_all(true);
        claude_box.style_context().add_class("claude-panel");
        panel.pack_start(&claude_box, false, false, 0);

        let this = ClockWindow(Rc::new(Inner {
            app: app.clone(),
            win: win.clone(),
            time,
            date,
            hosts_btn: hosts_btn.clone(),
            charts_btn: charts_btn.clone(),
            procs_btn: procs_btn.clone(),
            claude_btn: claude_btn.clone(),
            icons: vec![cfg_img, hosts_img, charts_img, procs_img],
            ping_box,
            sysmon,
            claude_box,
            css: RefCell::new(None),
            drag: RefCell::new(None),
            tick: RefCell::new(None),
            save_timer: RefCell::new(None),
            shown: RefCell::new((String::new(), String::new())),
            visible: Cell::new(true),
        }));

        let t = this.clone();
        ebox.connect_button_press_event(move |_, ev| t.on_press(ev));
        let t = this.clone();
        ebox.connect_motion_notify_event(move |_, ev| t.on_motion(ev));
        let t = this.clone();
        ebox.connect_button_release_event(move |_, ev| t.on_release(ev));
        let t = this.clone();
        win.connect_destroy(move |_| t.on_destroy());
        let t = this.clone();
        cfg_btn.connect_clicked(move |_| t.0.app.open_desklet_settings());
        let t = this.clone();
        hosts_btn.connect_clicked(move |_| t.toggle_section(Section::Hosts));
        let t = this.clone();
        charts_btn.connect_clicked(move |_| t.toggle_section(Section::Charts));
        let t = this.clone();
        procs_btn.connect_clicked(move |_| t.toggle_section(Section::Procs));
        let t = this.clone();
        claude_btn.connect_clicked(move |_| t.toggle_section(Section::Claude));

        this.apply_css();
        this.refresh_text(true);
        win.move_(cfg.x as i32, cfg.y as i32);
        win.show_all();
        // show_all would reveal the date even when it is switched off
        this.apply_visibility();
        this.refresh_panel();
        win.move_(cfg.x as i32, cfg.y as i32);

        // One-second tick regardless of whether seconds are displayed: the
        // label is only touched when the rendered text changes, so this costs
        // a format and a compare, and it can never drift out of step the way
        // a minute-aligned timer does after a suspend.
        let t = this.clone();
        let id = glib::timeout_add_local(Duration::from_secs(1), move || {
            t.refresh_text(false);
            glib::ControlFlow::Continue
        });
        *this.0.tick.borrow_mut() = Some(id);

        this
    }

    fn block(&self) -> crate::config::Clock {
        self.0.app.cfg_snapshot().clock
    }

    /// Rebuild this clock's CSS. Scoped to the window's own style classes, and
    /// installed screen-wide like the hedges' — hence the distinct names.
    pub fn apply_css(&self) {
        let c = self.block();
        let font = c.font_size.clamp(10, 300);
        let date_font = ((font as f64 * DATE_SCALE).round() as i64).max(8);
        let bg = if c.show_background {
            colors::rgba(&c.bg_color, c.bg_opacity)
        } else {
            "transparent".to_string()
        };
        // Both apply once the panel has an edge — a background, a border, or
        // both. Without one there is nothing to be inside of, and padding a
        // transparent panel would only shift the clock away from where it was
        // dropped.
        let framed = c.show_background || c.show_border;
        let pad = if framed { c.padding.clamp(0, 80) } else { 0 };
        // The panel's inset, equal on every side (see the CSS). Nothing to be
        // inside of means no inset at all, so an unframed clock still sits where
        // it was dropped rather than being nudged by a margin.
        let p = if framed { pad + 6 } else { 0 };
        let radius = if framed { c.corner_radius.clamp(0, 60) } else { 0 };

        // The host table rides on the clock's own scale so it stays in
        // proportion, with floors that keep it readable under a small clock.
        let ping_font = ((font as f64 * PING_SCALE).round() as i64).clamp(9, 20);
        // twelve badges have to fit beside a host name, so they are small
        let seg = ((ping_font as f64 * 0.45).round() as i64).max(4);

        // The border is independent of the background on purpose: an outline
        // with nothing behind it is a perfectly good way to hedge the desklet
        // off from a busy wallpaper without dimming it.
        let border = if c.show_border {
            format!("1px solid {}", colors::rgba(&c.color, 0.28))
        } else {
            "none".to_string()
        };

        let css = format!(
            r#"
        /* Equal on all four sides. It used to carry an extra 6px left and right
           — breathing room for a big time against the panel edge — which read
           as a *thin* bottom border whenever the last thing in the panel was a
           chart card, whose fill makes the gap below it exact and obvious. The
           6px is now simply the floor for every side. */
        .clock-panel {{
            background-color: {bg};
            border: {border};
            border-radius: {radius}px;
            padding: {p}px;
        }}
        .clock-time {{
            color: {col};
            font-size: {font}px;
            font-weight: 300;
        }}
        .clock-date {{
            color: {col};
            font-size: {date_font}px;
            letter-spacing: 1px;
        }}
        /* The controls beside the time: settings, hosts, charts, programs.
           One colour for all four, whichever sections are up — they are a
           control strip, not a readout, and a lit/unlit state here competed with
           the panel below for attention. Which sections are showing is legible
           from the panel itself, and each tooltip says which way its click goes.
           The colour is the clock's own pulled halfway to grey rather than a
           fixed grey: a fixed light grey vanished against a pale panel. */
        .clock-panel .clock-icons {{ margin-left: 8px; }}
        .clock-panel .clock-icon {{
            color: {icon_col};
            padding: 0 2px;
            min-height: 0;
            min-width: 0;
            border: none;
            background: none;
            box-shadow: none;
            opacity: 0.8;
        }}
        .clock-panel .clock-icon:hover {{ opacity: 1; }}
        /* The host table and the charts are two different readings; a little air
           between them stops the strip of badges reading as a chart axis. The
           gap *below* is a separator, not a border, so `refresh_panel` drops it
           when the table is the last thing in the panel — left on, it made the
           bottom border deeper than the sides. */
        .clock-panel .ping-panel {{ margin-top: 2px; }}
        .clock-panel .ping-name, .clock-panel .ping-latency {{
            color: {col};
            font-size: {ping_font}px;
            /* the theme gives labels a little vertical padding; a table of
               fifteen hops does not want it fifteen times over */
            padding: 0;
            margin: 0;
            min-height: 0;
        }}
        .clock-panel .ping-latency {{ opacity: 0.75; }}
        /* a host is only named in a colour when that colour means something */
        .clock-panel .ping-name.ping-down, .clock-panel .ping-latency.ping-down {{ color: #f85149; opacity: 1; }}
        .clock-panel .ping-name.ping-mixed, .clock-panel .ping-latency.ping-mixed {{ color: #d29922; opacity: 1; }}
        .clock-panel .ping-name.ping-blocked, .clock-panel .ping-latency.ping-blocked {{ color: #8b949e; opacity: 1; }}
        .clock-panel .ping-name.ping-unknown, .clock-panel .ping-latency.ping-unknown {{ opacity: 0.45; }}
        .clock-panel .ping-seg {{
            min-width: {seg}px;
            min-height: {seg}px;
            border-radius: 1px;
            /* no horizontal margin: the badges butt together into one strip,
               and a hairline border is what keeps the periods readable */
            margin: 0;
            border: 1px solid alpha({col}, 0.28);
        }}
        .clock-panel .ping-seg.ping-up {{ background-color: #3fb950; }}
        .clock-panel .ping-seg.ping-mixed {{ background-color: #d29922; }}
        .clock-panel .ping-seg.ping-down {{ background-color: #f85149; }}
        .clock-panel .ping-seg.ping-blocked {{ background-color: #6e7681; }}
        .clock-panel .ping-seg.ping-unknown {{ background-color: alpha({col}, 0.15); }}
        /* The Claude section. Same palette as the host table — green working,
           orange idle, red needs-you — so colour means the same thing on every
           row of the desklet. The ✱ toggle is a text glyph, not a symbolic
           icon, so its size rides the same formula as the icons' pixel size. */
        .clock-panel .claude-glyph {{ font-size: {glyph}px; }}
        .clock-panel .claude-panel {{ margin-top: 2px; }}
        .clock-panel .claude-name, .clock-panel .claude-server {{
            color: {col};
            font-size: {ping_font}px;
            padding: 0;
            margin: 0;
            min-height: 0;
        }}
        /* Smaller than the readings on purpose: the muted columns (model
           tags, row labels) are context, not data, and their width is part of
           what decides whether the section fits inside the clock's width. */
        .clock-panel .claude-model {{
            color: {icon_col};
            font-size: {claude_small}px;
            padding: 0;
            margin: 0;
            min-height: 0;
        }}
        .clock-panel .claude-name.claude-busy {{ color: #3fb950; }}
        .clock-panel .claude-name.claude-idle {{ color: #d29922; }}
        .clock-panel .claude-name.claude-waiting {{ color: #f85149; }}
        .clock-panel .claude-name.claude-unknown {{ color: #8b949e; }}
        /* round on purpose, unlike the host badges' squares: a different shape
           for a different reading — and a size of its own, slightly smaller
           than the ping badges, so ten of them plus their labels stay well
           inside the width the clock sets */
        .clock-panel .claude-seg {{
            min-width: {claude_seg}px;
            min-height: {claude_seg}px;
            border-radius: {claude_seg}px;
            margin: 0;
        }}
        .clock-panel .claude-seg.claude-up {{ background-color: #3fb950; }}
        .clock-panel .claude-seg.claude-mixed {{ background-color: #d29922; }}
        .clock-panel .claude-seg.claude-down {{ background-color: #f85149; }}
        .clock-panel .claude-seg.claude-unknown {{ background-color: alpha({col}, 0.15); }}
        .clock-panel .claude-server.claude-up {{ color: #3fb950; }}
        .clock-panel .claude-server.claude-mixed {{ color: #d29922; }}
        .clock-panel .claude-server.claude-down {{ color: #f85149; }}
        .clock-panel .claude-server.claude-unknown {{ color: #8b949e; }}
        "#,
            col = c.color,
            icon_col = colors::muted(&c.color, 0.5),
            glyph = ((font as f64 * 0.22).round() as i64).clamp(10, 24),
            claude_small = ((ping_font as f64 * 0.75).round() as i64).max(8),
            claude_seg = ((ping_font as f64 * 0.4).round() as i64).max(4),
        );

        let provider = gtk::CssProvider::new();
        if let Err(e) = provider.load_from_data(css.as_bytes()) {
            log!(WARN, "clock CSS failed to load: {e}");
            return;
        }
        if let Some(screen) = gdk::Screen::default() {
            if let Some(old) = self.0.css.borrow_mut().take() {
                gtk::StyleContext::remove_provider_for_screen(&screen, &old);
            }
            gtk::StyleContext::add_provider_for_screen(
                &screen,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }
        *self.0.css.borrow_mut() = Some(provider);
        // Icons scale with the clock rather than sitting at a fixed size, which
        // a large time dwarfs. Floored so they stay hittable under a small one.
        let icon_px = ((font as f64 * 0.22).round() as i32).clamp(10, 24);
        for img in &self.0.icons {
            img.set_pixel_size(icon_px);
        }
        // the charts are cairo, not CSS: hand them the same colour and scale
        self.0.sysmon.set_style(&c.color, ping_font as f64);
    }

    /// Re-read the config and repaint: called when Settings changes something.
    pub fn refresh(&self) {
        self.apply_css();
        self.refresh_text(true);
        self.apply_visibility();
        self.refresh_panel();
        let c = self.block();
        self.0.win.move_(c.x as i32, c.y as i32);
    }

    // -- the info panel: hosts, charts, processes ---------------------------
    /// Show or hide one section. The state is config, not runtime, so the
    /// desklet comes back the way it was left.
    fn toggle_section(&self, which: Section) {
        self.0.app.with_cfg_mut(|cfg| match which {
            Section::Hosts => cfg.panel_hosts = !cfg.panel_hosts,
            Section::Charts => cfg.panel_charts = !cfg.panel_charts,
            // The process list has no monitor of its own to switch on — the
            // reading comes along with every system sample — so `showProcesses`
            // *is* its display switch, and this icon is a shortcut to the same
            // setting rather than a second flag layered over it.
            Section::Procs => cfg.sysmon.show_processes = !cfg.sysmon.show_processes,
            Section::Claude => cfg.panel_claude = !cfg.panel_claude,
        });
        self.0.app.save();
        match which {
            // `showProcesses` is a *sampler* setting, not only a display one:
            // the monitor holds its own copy of the config and skips reading
            // processes when that copy says they aren't wanted. Flipping the
            // config here without handing it over left the list switched on and
            // permanently empty — no amount of clicking recovered it, because
            // nothing else ever tells the monitor. Every Settings toggle goes
            // through `sync_sysmon` for exactly this reason; it refreshes the
            // panel itself, so this arm must not do it twice.
            Section::Procs => self.0.app.sync_sysmon(),
            // `panel_claude` is display-only: the pollers keep running so the
            // strip's history is intact when the section comes back
            Section::Hosts | Section::Charts | Section::Claude => self.refresh_panel(),
        }
    }

    /// Rebuild the host table and the charts from the monitors' current view.
    ///
    /// Wholesale rather than in place: it is a handful of labels every round or
    /// two, and rebuilding means the tree can gain, lose or re-parent a host
    /// without any diffing.
    pub fn refresh_panel(&self) {
        let cfg = self.0.app.cfg_snapshot();
        let ping = &cfg.ping;
        let hosts_live = ping.enabled && !ping.hosts.is_empty();
        let charts_live = cfg.sysmon.enabled;
        let hosts_open = cfg.panel_hosts;
        let charts_open = cfg.panel_charts;
        let procs_open = cfg.sysmon.show_processes;
        let claude_live = cfg.claude.enabled;
        let claude_open = cfg.panel_claude;

        // A toggle is only shown when there is something for it to show. The
        // tooltip is what says which way a click goes; the icon itself keeps one
        // colour either way (see the CSS).
        for (btn, live, open, on, off) in [
            (
                &self.0.hosts_btn,
                hosts_live,
                hosts_open,
                "Hide the hosts",
                "Show the hosts",
            ),
            (
                &self.0.charts_btn,
                charts_live,
                charts_open,
                "Hide the charts",
                "Show the charts",
            ),
            (
                &self.0.procs_btn,
                charts_live,
                procs_open,
                "Hide the busiest programs",
                "Show the busiest programs",
            ),
            (
                &self.0.claude_btn,
                claude_live,
                claude_open,
                "Hide Claude",
                "Show Claude",
            ),
        ] {
            btn.set_tooltip_text(Some(if open { on } else { off }));
            btn.set_visible(live);
        }

        // The gap under the host table separates it from whatever comes next, so
        // it only belongs there when something does. As the last thing in the
        // panel the table needs no more than the panel's own inset, or the
        // bottom border ends up deeper than the sides.
        let anything_below = (charts_open && charts_live)
            || (procs_open && charts_live)
            || (claude_open && claude_live);
        self.0.ping_box.set_margin_bottom(if anything_below { 8 } else { 0 });

        for child in self.0.ping_box.children() {
            self.0.ping_box.remove(&child);
        }
        if hosts_open && hosts_live {
            let table = self.build_ping_table(ping);
            self.0.ping_box.add(&table);
            // show_all on the box itself would do nothing — it carries
            // no_show_all so the window's show_all can't unfold it — so the
            // table is shown directly and the box only revealed
            table.show_all();
            self.0.ping_box.show();
        } else {
            self.0.ping_box.hide();
        }

        let on_screen = self.0.visible.get();
        self.0
            .sysmon
            .refresh(charts_open && on_screen, procs_open && on_screen);

        self.refresh_claude();

        // the window is only as big as its content, so it has to be told to
        // shrink back after the panel folds away
        self.0.win.resize(1, 1);
    }

    /// Rebuild the Claude section from the monitor's current view. Wholesale,
    /// like the host table: a handful of labels every few seconds, and
    /// rebuilding means rows can come and go without any diffing.
    pub fn refresh_claude(&self) {
        let cfg = self.0.app.cfg_snapshot();
        for child in self.0.claude_box.children() {
            self.0.claude_box.remove(&child);
        }
        if cfg.claude.enabled && cfg.panel_claude && self.0.visible.get() {
            let section = self.build_claude_section(&cfg.claude, &cfg.clock);
            self.0.claude_box.add(&section);
            // same no-show-all dance as the host table: show the content, then
            // reveal the box — show_all on the box itself is a no-op
            section.show_all();
            self.0.claude_box.show();
        } else {
            self.0.claude_box.hide();
        }
        self.0.win.resize(1, 1);
    }

    /// Two columns: the sessions down the left, coloured by status; the usage
    /// and service rows down the right, every one the same shape — a label, a
    /// strip of dots, a value — so the whole block reads as one instrument.
    fn build_claude_section(
        &self,
        c: &crate::config::Claude,
        _clock: &crate::config::Clock,
    ) -> gtk::Box {
        use crate::claude_mon::{human_age, human_left, UsageWindow, SERVER_DOTS};

        let monitor = self.0.app.claude_monitor();
        let now_ms = glib::real_time() / 1000;

        // Spans the panel like the host table and the chart cards do — but it
        // must never be what *sets* the panel's width. The session names
        // ellipsize rather than push (see below), so the clock and the charts
        // decide how wide the desklet is and this section fits inside that,
        // sessions taking whatever slack there is.
        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        columns.set_hexpand(true);

        if c.show_sessions {
            // A grid, so the names line up in their own column however long
            // each row's model tag is.
            let left = gtk::Grid::new();
            left.set_row_spacing(0);
            left.set_column_spacing(6);
            left.set_valign(gtk::Align::Start);
            let sessions = monitor.sessions();
            if sessions.is_empty() {
                let l = gtk::Label::new(Some("no sessions"));
                l.set_xalign(0.0);
                l.style_context().add_class("claude-name");
                l.style_context().add_class("claude-unknown");
                left.attach(&l, 1, 0, 1, 1);
            }
            for (row, s) in sessions.iter().enumerate() {
                let row = row as i32;
                // the model rides left of the name, in the strip's muted
                // colour so the status colour stays the loudest thing here
                let model = gtk::Label::new(s.model.as_deref());
                model.set_xalign(1.0);
                model.style_context().add_class("claude-model");
                left.attach(&model, 0, row, 1, 1);

                // the project names the row; only a project running two
                // sessions needs Claude's derived name to tell them apart
                let text = if s.shares_project {
                    format!("{} · {}", s.project, s.title)
                } else {
                    s.project.clone()
                };
                let l = gtk::Label::new(Some(&text));
                l.set_xalign(0.0);
                // A long project name trims itself rather than widening the
                // desklet: its natural width is what would otherwise force
                // the whole panel wider than the clock and charts want to be.
                // The full name is in the tooltip either way.
                l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                l.set_hexpand(true);
                l.style_context().add_class("claude-name");
                l.style_context().add_class(s.status.css_class());
                let mut tip = format!("{} — {}", s.project, s.status.label());
                if let Some(why) = &s.waiting_for {
                    tip = format!("{tip}: {why}");
                }
                tip.push_str(&format!(
                    " — for {}",
                    human_age(now_ms - s.status_since)
                ));
                tip.push_str("\nclick to open");
                l.set_tooltip_text(Some(&tip));
                // An event box under the name, so a press raises the IDE or
                // terminal hosting the session instead of starting the
                // desklet drag — Stop keeps it from the clock's own handler.
                // The lookup shells out to wmctrl, so it runs off the loop.
                let ev = gtk::EventBox::new();
                ev.set_visible_window(false);
                ev.add(&l);
                ev.set_hexpand(true);
                let (pid, project) = (s.pid, s.project.clone());
                ev.connect_button_press_event(move |_, e| {
                    if e.button() != 1 {
                        return glib::Propagation::Proceed;
                    }
                    let project = project.clone();
                    std::thread::spawn(move || {
                        if !crate::claude_mon::focus_session(pid, &project) {
                            log!(WARN, "no window found for claude session pid {pid}");
                        }
                    });
                    glib::Propagation::Stop
                });
                left.attach(&ev, 1, row, 1, 1);
            }
            columns.pack_start(&left, true, true, 0);
        }

        if c.show_usage || c.show_server_status {
            // Every row is the same instrument: label, a strip of SERVER_DOTS
            // dots, a value. Usage fills its strip left-to-right like a bar;
            // the API strip is time, oldest left. Same dots, same colours,
            // same width — which is what makes the block read as one thing.
            let right = gtk::Grid::new();
            right.set_row_spacing(2);
            right.set_column_spacing(4);
            right.set_valign(gtk::Align::Start);
            let mut row = 0;
            let mut add = |name: &str,
                           dots: Vec<&'static str>,
                           value: &str,
                           value_cls: Option<&'static str>,
                           tip: Option<String>| {
                let l = gtk::Label::new(Some(name));
                l.set_xalign(1.0);
                l.style_context().add_class("claude-model");
                right.attach(&l, 0, row, 1, 1);
                let strip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                strip.set_valign(gtk::Align::Center);
                for cls in dots {
                    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    dot.style_context().add_class("claude-seg");
                    dot.style_context().add_class(cls);
                    strip.pack_start(&dot, false, false, 0);
                }
                right.attach(&strip, 1, row, 1, 1);
                let v = gtk::Label::new(Some(value));
                // Right-aligned: the grid holds the panel's right edge, so
                // the values share the same right edge as the process
                // percentages and the chart cards above.
                v.set_xalign(1.0);
                // Reserve the widest value this column can show ("100%" and
                // every status label fit in 4 chars) so a status flip or a
                // usage jump cannot change the panel's width — the window
                // sizes to content, and desktop furniture must not move when
                // a reading changes.
                v.set_width_chars(4);
                v.style_context().add_class("claude-server");
                if let Some(cls) = value_cls {
                    v.style_context().add_class(cls);
                }
                right.attach(&v, 2, row, 1, 1);
                // The hint covers the whole row, not just the dots — a strip
                // eight pixels tall is no target for a mouse.
                if let Some(t) = &tip {
                    for w in [l.upcast_ref::<gtk::Widget>(), strip.upcast_ref(), v.upcast_ref()] {
                        w.set_tooltip_text(Some(t));
                    }
                }
                row += 1;
            };

            // a usage strip fills with the window: green, then orange past
            // 70%, red past 90% — the same thresholds as everywhere else
            let usage_dots = |pct: f64| -> Vec<&'static str> {
                let filled = ((pct / 100.0) * SERVER_DOTS as f64).round() as usize;
                let colour = match pct {
                    p if p >= 90.0 => "claude-down",
                    p if p >= 70.0 => "claude-mixed",
                    _ => "claude-up",
                };
                (0..SERVER_DOTS)
                    .map(|i| if i < filled { colour } else { "claude-unknown" })
                    .collect()
            };
            let empty = || vec!["claude-unknown"; SERVER_DOTS];

            if c.show_usage {
                match monitor.usage() {
                    Some(Ok(u)) => {
                        let mut windows: Vec<(String, UsageWindow)> = Vec::new();
                        if let Some(w) = u.five_hour {
                            windows.push(("5 hr".to_string(), w));
                        }
                        if let Some(w) = u.seven_day {
                            windows.push(("week".to_string(), w));
                        }
                        for (name, w) in &u.per_model {
                            windows.push((name.clone(), *w));
                        }
                        for (name, w) in windows {
                            let pct = w.percent.clamp(0.0, 100.0);
                            // "week window — 88% used — 6d 13h left (resets Fri 08:00)"
                            let tip = w.resets_at.map(|reset| {
                                let mut t = format!(
                                    "{name} window — {pct:.0}% used — {}",
                                    human_left(reset - now_ms)
                                );
                                if let Some(at) = glib::DateTime::from_unix_local(reset / 1000)
                                    .ok()
                                    .and_then(|dt| dt.format("%a %H:%M").ok())
                                {
                                    t.push_str(&format!(" (resets {at})"));
                                }
                                t
                            });
                            add(&name, usage_dots(pct), &format!("{pct:.0}%"), None, tip);
                        }
                    }
                    // The value column budgets four characters; an arbitrary
                    // error string ("io: failed to lookup address…") must not
                    // be what sets the desklet's width. The reason rides in
                    // the tooltip instead.
                    Some(Err(why)) => add(
                        "usage",
                        empty(),
                        "n/a",
                        Some("claude-unknown"),
                        Some(format!("usage unavailable — {why}")),
                    ),
                    None => {
                        add("5 hr", empty(), "…", None, None);
                        add("week", empty(), "…", None, None);
                    }
                }
            }

            if c.show_server_status {
                let dots = monitor
                    .server_segments()
                    .iter()
                    .map(|h| h.css_class())
                    .collect();
                let (text, cls) = match monitor.server_now() {
                    Some(h) => (h.label(), Some(h.css_class())),
                    None => ("…", None),
                };
                add(
                    "API",
                    dots,
                    text,
                    cls,
                    Some(
                        "Claude service status (status.anthropic.com) — the last 30 minutes, \
                         oldest left. Green up, orange minor incident, red outage, grey unknown."
                            .to_string(),
                    ),
                );
            }

            // pack_end, so the readings hold the right edge even with the
            // session column switched off
            columns.pack_end(&right, false, false, 0);
        }

        columns
    }

    /// A fresh system sample landed: slide the charts on.
    pub fn on_sysmon_sample(&self) {
        self.0.sysmon.on_sample();
    }

    fn build_ping_table(&self, cfg: &crate::config::Ping) -> gtk::Grid {
        use crate::ping::{segment_span, SPAN_SECS};

        let grid = gtk::Grid::new();
        // no gap between rows: the table reads as one block, the way a
        // traceroute does, rather than as separate lines
        grid.set_row_spacing(0);
        grid.set_column_spacing(10);
        // span the panel, so the rows line up with the chart cards below
        grid.set_hexpand(true);

        let segments = cfg.segment_count();
        let badge_tip = format!(
            "The last {} — {segments} periods of {}, oldest left, newest right.\n\
             Green: up throughout. Orange: up some of the time. \
             Red: down throughout. Grey: unreachable behind something that was down.",
            human_span(SPAN_SECS),
            human_span(segment_span(segments)),
        );

        for (row, st) in self.0.app.ping_monitor().statuses().iter().enumerate() {
            let row = row as i32;
            let health = st.current();

            let name = gtk::Label::new(Some(&st.name));
            name.set_xalign(0.0);
            // The indent shows what sits behind what. Capped, because a route
            // filled from a traceroute is a chain fifteen deep and indenting
            // it faithfully would push the desklet off the screen.
            name.set_margin_start(st.depth.min(4) as i32 * 8);
            name.style_context().add_class("ping-name");
            name.style_context().add_class(health.css_class());
            name.set_tooltip_text(Some(&format!("{} — {}", st.name, st.host)));
            grid.attach(&name, 0, row, 1, 1);

            let badges = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            badges.set_valign(gtk::Align::Center);
            // the slack in the row goes here, between the name and the
            // latency, so both stay pinned to their own edge
            badges.set_hexpand(true);
            badges.set_halign(gtk::Align::Center);
            badges.set_tooltip_text(Some(&badge_tip));
            for &health in &st.segments {
                let seg = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                seg.style_context().add_class("ping-seg");
                seg.style_context().add_class(health.css_class());
                badges.pack_start(&seg, false, false, 0);
            }
            grid.attach(&badges, 1, row, 1, 1);

            let latency = gtk::Label::new(Some(&st.latency_label()));
            latency.set_xalign(1.0);
            latency.set_halign(gtk::Align::End);
            latency.style_context().add_class("ping-latency");
            latency.style_context().add_class(health.css_class());
            grid.attach(&latency, 2, row, 1, 1);
        }
        grid
    }

    fn apply_visibility(&self) {
        let c = self.block();
        self.0.date.set_visible(c.show_date);
        // a window that is only as big as its content has to be told to shrink
        self.0.win.resize(1, 1);
    }

    /// Update the labels if the time they should show has changed.
    fn refresh_text(&self, force: bool) {
        let c = self.block();
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        let time_fmt = match (c.twenty_four_hour, c.show_seconds) {
            (true, false) => "%H:%M",
            (true, true) => "%H:%M:%S",
            (false, false) => "%l:%M %p",
            (false, true) => "%l:%M:%S %p",
        };
        let time = now
            .format(time_fmt)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        // Composed rather than one format string: `%e` pads a single-digit day
        // to two characters, which reads as "Sunday,  9 August", and `%-e`
        // isn't portable. Weekday and month still come from the locale.
        let date = if c.show_date {
            let weekday = now.format("%A").map(|s| s.to_string()).unwrap_or_default();
            let month = now.format("%B").map(|s| s.to_string()).unwrap_or_default();
            format!("{weekday}, {} {month}", now.day_of_month())
        } else {
            String::new()
        };

        let mut shown = self.0.shown.borrow_mut();
        if force || shown.0 != time {
            self.0.time.set_text(&time);
            shown.0 = time;
        }
        if force || shown.1 != date {
            self.0.date.set_text(&date);
            shown.1 = date;
        }
    }

    /// Hidden during a peek, along with the hedges.
    pub fn set_visible(&self, visible: bool) {
        if self.0.visible.replace(visible) == visible {
            return;
        }
        if visible {
            self.0.win.show();
            let c = self.block();
            self.0.win.move_(c.x as i32, c.y as i32);
        } else {
            self.0.win.hide();
        }
        // nothing off screen should be animating
        self.refresh_panel();
    }

    pub fn close(&self) {
        self.0.sysmon.stop_animation();
        if let Some(id) = self.0.tick.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.0.save_timer.borrow_mut().take() {
            id.remove();
        }
        if let Some(screen) = gdk::Screen::default() {
            if let Some(old) = self.0.css.borrow_mut().take() {
                gtk::StyleContext::remove_provider_for_screen(&screen, &old);
            }
        }
        self.0.win.close();
    }

    // -- dragging -----------------------------------------------------------
    fn on_press(&self, ev: &gdk::EventButton) -> glib::Propagation {
        if ev.button() != 1 {
            return glib::Propagation::Proceed;
        }
        *self.0.drag.borrow_mut() = Some(Drag {
            root: ev.root(),
            win: self.0.win.position(),
            moved: false,
        });
        glib::Propagation::Stop
    }

    fn on_motion(&self, ev: &gdk::EventMotion) -> glib::Propagation {
        // read the drag out before touching the window: moving it re-enters
        // here through configure/motion, and holding the borrow would panic
        let Some((root, win, moved)) = self
            .0
            .drag
            .borrow()
            .as_ref()
            .map(|d| (d.root, d.win, d.moved))
        else {
            return glib::Propagation::Proceed;
        };
        let (rx, ry) = ev.root();
        let (dx, dy) = (rx - root.0, ry - root.1);
        if !moved && dx.abs() < DRAG_SLOP && dy.abs() < DRAG_SLOP {
            return glib::Propagation::Proceed;
        }
        if let Some(d) = self.0.drag.borrow_mut().as_mut() {
            d.moved = true;
        }
        self.0
            .win
            .move_(win.0 + dx.round() as i32, win.1 + dy.round() as i32);
        glib::Propagation::Stop
    }

    fn on_release(&self, _ev: &gdk::EventButton) -> glib::Propagation {
        let Some(d) = self.0.drag.borrow_mut().take() else {
            return glib::Propagation::Proceed;
        };
        if d.moved {
            self.queue_save();
        }
        glib::Propagation::Stop
    }

    /// Write the position back, debounced — a drag ends with a flurry of
    /// configure events and the config is rewritten atomically each time.
    fn queue_save(&self) {
        if let Some(id) = self.0.save_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(SAVE_DELAY_MS), move || {
            *this.0.save_timer.borrow_mut() = None;
            let (x, y) = this.0.win.position();
            this.0.app.with_cfg_mut(|cfg| {
                cfg.clock.x = x as i64;
                cfg.clock.y = y as i64;
            });
            this.0.app.save();
            glib::ControlFlow::Break
        });
        *self.0.save_timer.borrow_mut() = Some(id);
    }

    fn on_destroy(&self) {
        self.0.sysmon.stop_animation();
        if let Some(id) = self.0.tick.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.0.save_timer.borrow_mut().take() {
            id.remove();
        }
    }
}

/// Seconds as something to read in a tooltip: "90 seconds" helps nobody.
fn human_span(secs: u64) -> String {
    match secs {
        0..=90 => format!("{secs} seconds"),
        91..=5400 => format!("{} minutes", (secs as f64 / 60.0).round() as u64),
        _ => format!("{} hours", (secs as f64 / 3600.0).round() as u64),
    }
}

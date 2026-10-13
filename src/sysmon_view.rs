//! The desklet's mini charts, drawn with cairo.
//!
//! One [`gtk::DrawingArea`] per series, each a self-contained card: a
//! translucent panel, a grid, its name in the top-left, its current reading in
//! the top-right, and a smooth curve of the last [`MAX_POINTS`] readings.
//!
//! The curve is a Catmull-Rom spline converted to cubic beziers, so it passes
//! through every reading rather than smoothing them away, and the whole series
//! slides left continuously between samples: the animation clock is a plain
//! GLib timer, not the frame clock, because the compositor throttles the paint
//! clock of an obscured keep-below window and the charts would freeze
//! (the same trap documented on the hedge dock slide).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::app::HedgeApp;
use crate::sysmon::{self, Sample, Series, MAX_POINTS};

/// Redraw rate while the panel is on screen.
const FRAME_MS: u64 = 50;
/// Height of one chart card, before the desklet's scale is applied.
const CHART_H: i32 = 54;
/// Width a chart asks for; it stretches to the panel if the table is wider.
const CHART_W: i32 = 210;
/// Seconds between the charts' vertical grid lines.
const GRID_SECS: f64 = 30.0;

/// Accent per series, as cairo components.
fn series_rgb(s: Series) -> (f64, f64, f64) {
    match s {
        Series::Cpu => (0.478, 0.635, 0.968),  // #7aa2f7
        Series::Gpu => (0.733, 0.604, 0.968),  // #bb9af7
        Series::Mem => (0.620, 0.808, 0.416),  // #9ece6a
        Series::Net => (0.878, 0.686, 0.408),  // #e0af68 — download
    }
}

/// Upload, on the network chart. Deliberately across the wheel from the
/// download amber rather than a near neighbour: two curves on one axis in
/// adjacent hues are two curves nobody can tell apart.
const NET_TX_RGB: (f64, f64, f64) = (0.490, 0.812, 1.0); // #7dcfff
/// The memory forecast, when it says the machine is about to run out.
const WARN_RGB: (f64, f64, f64) = (0.969, 0.463, 0.557); // #f7768e

struct Inner {
    app: HedgeApp,
    root: gtk::Box,
    charts: RefCell<Vec<(Series, gtk::DrawingArea)>>,
    procs: gtk::Grid,
    anim: RefCell<Option<glib::SourceId>>,
    /// When the newest sample landed, in monotonic microseconds — the origin
    /// the horizontal scroll is measured from.
    sampled_at: Cell<i64>,
    /// Text colour inherited from the clock.
    color: RefCell<gdk::RGBA>,
    /// Font size for titles and readings.
    font: Cell<f64>,
    /// Corner rounding of a card, in px.
    corner: Cell<f64>,
    /// Card background, premultiplied from the configured colour + opacity.
    /// Cached rather than read per frame: `draw` runs 20 times a second per
    /// card, and cloning the whole config there would be absurd.
    chart_bg: Cell<(f64, f64, f64, f64)>,
    show_clock: Cell<bool>,
    /// Seconds between samples. Cached for the same reason as `chart_bg`: both
    /// the scroll phase and the time grid need it on every frame.
    interval: Cell<f64>,
    /// Samples taken since start-up, counting past the history's own cap.
    ///
    /// The curve gets its motion from the history shifting along by one sample
    /// and `phase` animating the gap, which is enough for a shape that has no
    /// fixed reference point. A time grid does need one: anchoring a line to the
    /// newest sample's position alone leaves it oscillating inside a single
    /// sample's width, so the lines sat still while the curve moved under them.
    samples: Cell<u64>,
}

/// Clonable handle to the charts.
#[derive(Clone)]
pub struct SysmonPanel(Rc<Inner>);

impl SysmonPanel {
    pub fn new(app: &HedgeApp) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_no_show_all(true);

        let procs = gtk::Grid::new();
        procs.set_row_spacing(1);
        procs.set_column_spacing(8);
        procs.set_margin_top(2);

        let this = SysmonPanel(Rc::new(Inner {
            app: app.clone(),
            root,
            charts: RefCell::new(Vec::new()),
            procs,
            anim: RefCell::new(None),
            sampled_at: Cell::new(glib::monotonic_time()),
            color: RefCell::new(gdk::RGBA::WHITE),
            font: Cell::new(10.0),
            corner: Cell::new(6.0),
            chart_bg: Cell::new((0.043, 0.051, 0.078, 0.45)),
            show_clock: Cell::new(true),
            interval: Cell::new(2.0),
            samples: Cell::new(0),
        }));
        this
    }

    pub fn widget(&self) -> gtk::Box {
        self.0.root.clone()
    }

    /// Adopt the clock's colour and scale.
    pub fn set_style(&self, color: &str, font_px: f64) {
        *self.0.color.borrow_mut() = color.parse().unwrap_or(gdk::RGBA::WHITE);
        self.0.font.set(font_px.clamp(8.0, 18.0));
    }

    /// A fresh sample landed: restart the slide and repaint.
    pub fn on_sample(&self) {
        self.0.sampled_at.set(glib::monotonic_time());
        self.0.samples.set(self.0.samples.get().wrapping_add(1));
        self.refresh_procs();
        for (_, area) in self.0.charts.borrow().iter() {
            area.queue_draw();
        }
    }

    /// Build (or rebuild) the cards the config asks for and start or stop the
    /// animation. Called whenever the panel is folded, unfolded or reconfigured.
    ///
    /// The charts and the process list are asked for separately, because the
    /// desklet has an icon for each: either can be on screen without the other.
    pub fn refresh(&self, charts_visible: bool, procs_visible: bool) {
        let cfg = self.0.app.cfg_snapshot().sysmon;
        let want_charts = charts_visible && cfg.enabled;
        let want_procs = procs_visible && cfg.enabled && cfg.show_processes;
        let live = want_charts || want_procs;

        // style the cards read every frame, resolved once here
        let bg = cfg
            .chart_bg_color
            .parse::<gdk::RGBA>()
            .unwrap_or(gdk::RGBA::BLACK);
        self.0.chart_bg.set((
            bg.red() as f64,
            bg.green() as f64,
            bg.blue() as f64,
            cfg.chart_bg_opacity.clamp(0.0, 1.0),
        ));
        self.0.show_clock.set(cfg.show_clock_speed);
        self.0.interval.set(cfg.interval() as f64);
        self.0.corner.set(cfg.chart_corner_radius.clamp(0, 24) as f64);

        for child in self.0.root.children() {
            self.0.root.remove(&child);
        }
        self.0.charts.borrow_mut().clear();
        if !live {
            self.stop_animation();
            self.0.root.hide();
            return;
        }

        let wanted = [
            (Series::Cpu, cfg.show_cpu && want_charts),
            (Series::Gpu, cfg.show_gpu && want_charts),
            (Series::Mem, cfg.show_mem && want_charts),
            (Series::Net, cfg.show_net && want_charts),
        ];
        for (series, on) in wanted {
            if !on {
                continue;
            }
            let area = gtk::DrawingArea::new();
            area.set_size_request(CHART_W, CHART_H);
            area.set_hexpand(true);
            let this = self.clone();
            area.connect_draw(move |w, cr| this.draw_chart(series, w, cr));

            if series == Series::Cpu {
                // the one chart that is also a button; the rest let presses
                // fall through to the clock so the desklet can still be dragged
                let ebox = gtk::EventBox::new();
                ebox.set_visible_window(false);
                ebox.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
                ebox.add(&area);
                ebox.set_tooltip_text(Some("Open the system monitor"));
                let this = self.clone();
                ebox.connect_button_press_event(move |_, ev| {
                    if ev.button() == 1 {
                        this.open_task_manager();
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                });
                self.0.root.pack_start(&ebox, false, false, 0);
            } else {
                self.0.root.pack_start(&area, false, false, 0);
            }
            self.0.charts.borrow_mut().push((series, area));
        }

        if want_procs {
            self.0.root.pack_start(&self.0.procs, false, false, 0);
            self.refresh_procs();
        }

        // the cards are shown individually: `root` carries no_show_all so the
        // clock's own show_all can't unfold the panel, which also makes
        // show_all on `root` itself a no-op
        for child in self.0.root.children() {
            child.show_all();
        }
        self.0.root.show();
        // Only the curves animate. With the charts hidden and just the process
        // list up there is nothing to slide, so the frame clock stays idle.
        match self.0.charts.borrow().is_empty() {
            true => self.stop_animation(),
            false => self.start_animation(),
        }
    }

    fn open_task_manager(&self) {
        let cfg = self.0.app.cfg_snapshot().sysmon;
        match sysmon::task_manager_command(&cfg.task_manager) {
            Some(cmd) => self.0.app.launch(&cmd),
            None => log!(
                WARN,
                "no system monitor found; set sysmon.taskManager in the config"
            ),
        }
    }

    // -- the busiest processes ---------------------------------------------
    fn refresh_procs(&self) {
        for child in self.0.procs.children() {
            self.0.procs.remove(&child);
        }
        if !self.0.app.cfg_snapshot().sysmon.show_processes {
            return;
        }
        // Always TOP_PROCS rows, padded with blanks. An idle machine has
        // fewer processes busy enough to list, and letting the row count
        // follow made the desklet change height under the pointer — a widget
        // you drag must not resize itself while you are looking at it.
        let busiest = self.0.app.sysmon_monitor().top_procs();
        for row in 0..sysmon::TOP_PROCS {
            let entry = busiest.get(row);

            // a space, not an empty string: a blank label is guaranteed to
            // occupy its line height, which is the whole point of the padding
            let name = gtk::Label::new(Some(entry.map_or(" ", |p| p.name.as_str())));
            name.set_xalign(0.0);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            name.set_max_width_chars(18);
            name.style_context().add_class("ping-name");
            self.0.procs.attach(&name, 0, row as i32, 1, 1);

            let text = entry
                .map(|p| format!("{:.1}%", p.cpu))
                .unwrap_or_else(|| " ".to_string());
            let cpu = gtk::Label::new(Some(&text));
            cpu.set_xalign(1.0);
            cpu.set_hexpand(true);
            cpu.style_context().add_class("ping-latency");
            self.0.procs.attach(&cpu, 1, row as i32, 1, 1);
        }
        self.0.procs.show_all();
    }

    // -- animation ----------------------------------------------------------
    fn start_animation(&self) {
        if self.0.anim.borrow().is_some() {
            return;
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(FRAME_MS), move || {
            for (_, area) in this.0.charts.borrow().iter() {
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
        *self.0.anim.borrow_mut() = Some(id);
    }

    pub fn stop_animation(&self) {
        if let Some(id) = self.0.anim.borrow_mut().take() {
            id.remove();
        }
    }

    /// How far the series has travelled towards its next sample, 0..1.
    fn phase(&self) -> f64 {
        // the cached interval, not a config snapshot: this runs every frame for
        // every card, and cloning the whole config there is what the cache above
        // exists to avoid
        let interval = self.0.interval.get() * 1_000_000.0;
        let elapsed = (glib::monotonic_time() - self.0.sampled_at.get()) as f64;
        (elapsed / interval).clamp(0.0, 1.0)
    }

    // -- drawing ------------------------------------------------------------
    fn draw_chart(
        &self,
        series: Series,
        widget: &gtk::DrawingArea,
        cr: &gtk::cairo::Context,
    ) -> glib::Propagation {
        let w = widget.allocated_width() as f64;
        let h = widget.allocated_height() as f64;
        let (r, g, b) = series_rgb(series);
        let fg = *self.0.color.borrow();
        let font = self.0.font.get();

        // card
        let (br, bg_, bb, ba) = self.0.chart_bg.get();
        rounded_rect(cr, 0.5, 0.5, w - 1.0, h - 1.0, self.0.corner.get());
        cr.set_source_rgba(br, bg_, bb, ba);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.12);
        cr.set_line_width(1.0);
        let _ = cr.stroke();

        // plot area, under the caption row
        let top = font + 6.0;
        let (px, py, pw, ph) = (4.0, top, w - 8.0, h - top - 4.0);

        // grid
        cr.set_line_width(1.0);
        cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.08);
        for i in 1..4 {
            let y = (py + ph * i as f64 / 4.0).round() + 0.5;
            cr.move_to(px, y);
            cr.line_to(px + pw, y);
        }
        // Vertical lines every GRID_SECS of history rather than at fractions of
        // the width: a line you can put a number on. They are anchored to the
        // newest reading and so travel left with the curve, which is the only
        // way a line can keep meaning the same moment in time. Skipped when a
        // long sample interval would pack them tighter than they are readable.
        let step = pw / (MAX_POINTS - 1) as f64;
        let spacing = GRID_SECS / self.0.interval.get().max(0.1) * step;
        if spacing >= 6.0 {
            // How far the data has travelled since start-up, wrapped into one
            // gap: that is what makes a line march left with the curve and a new
            // one arrive at the right edge every GRID_SECS.
            let travelled = (self.0.samples.get() as f64 + self.phase()) * step;
            let mut x = px + pw - travelled % spacing;
            while x > px {
                let xr = x.round() + 0.5;
                cr.move_to(xr, py);
                cr.line_to(xr, py + ph);
                x -= spacing;
            }
        }
        let _ = cr.stroke();

        let monitor = self.0.app.sysmon_monitor();
        let history = monitor.history();
        let latest = history.last().copied().unwrap_or_default();

        // the series, scaled to its own ceiling
        let scale = match series {
            Series::Net => monitor.net_scale(),
            _ => 100.0,
        };
        let values: Vec<f64> = history.iter().map(|s| value_of(series, s)).collect();
        self.draw_curve(cr, &values, scale, px, py, pw, ph, (r, g, b), true);
        if series == Series::Net {
            // upload rides on the same axis, as a line without a fill
            let up: Vec<f64> = history.iter().map(|s| s.net_tx).collect();
            self.draw_curve(cr, &up, scale, px, py, pw, ph, NET_TX_RGB, false);
        }

        // caption: name left, reading right
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Bold,
        );
        cr.set_font_size(font);
        cr.set_source_rgba(
            fg.red() as f64,
            fg.green() as f64,
            fg.blue() as f64,
            0.85,
        );
        cr.move_to(6.0, font + 1.0);
        let _ = cr.show_text(series.title());
        // `auto` picks the card for you and a machine can have both, so the
        // GPU chart says which one it is actually reading
        if series == Series::Gpu {
            if let Some(vendor) = latest.gpu_vendor {
                if let Ok(ext) = cr.text_extents(series.title()) {
                    cr.select_font_face(
                        "Sans",
                        gtk::cairo::FontSlant::Normal,
                        gtk::cairo::FontWeight::Normal,
                    );
                    cr.set_font_size(font * 0.85);
                    cr.set_source_rgba(r, g, b, 0.75);
                    cr.move_to(6.0 + ext.x_advance() + 5.0, font + 1.0);
                    let _ = cr.show_text(vendor);
                    cr.set_font_size(font);
                }
            }
        }

        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Normal,
        );
        // Right-aligned, but drawn left to right, so the runs have to be
        // measured before the first one can be placed.
        let mut runs = caption_runs(series, &latest, self.0.show_clock.get(), (r, g, b));
        if series == Series::Mem {
            if let Some(eta) = monitor.mem_eta() {
                // ASCII for the same reason as the network caption
                runs.insert(0, (format!("full in {}  ", sysmon::human_eta(eta)), WARN_RGB));
            }
        }
        let total: f64 = runs
            .iter()
            .filter_map(|(text, _)| cr.text_extents(text).ok().map(|e| e.x_advance()))
            .sum();
        let mut x = w - 6.0 - total;
        for (text, (cr_, cg, cb)) in &runs {
            let Ok(ext) = cr.text_extents(text) else { continue };
            cr.move_to(x, font + 1.0);
            cr.set_source_rgba(*cr_, *cg, *cb, 0.95);
            let _ = cr.show_text(text);
            x += ext.x_advance();
        }

        glib::Propagation::Proceed
    }

    /// A Catmull-Rom spline through `values`, optionally filled underneath.
    ///
    /// The x origin is shifted left by the fraction of the sampling interval
    /// that has elapsed, which is what makes the chart scroll rather than jump
    /// once per sample.
    #[allow(clippy::too_many_arguments)]
    fn draw_curve(
        &self,
        cr: &gtk::cairo::Context,
        values: &[f64],
        scale: f64,
        px: f64,
        py: f64,
        pw: f64,
        ph: f64,
        (r, g, b): (f64, f64, f64),
        fill: bool,
    ) {
        if values.len() < 2 {
            return;
        }
        let step = pw / (MAX_POINTS - 1) as f64;
        let shift = self.phase() * step;
        // right-aligned: the newest reading sits at the right edge and the
        // history trails off to the left
        let x_of = |i: usize| px + pw - (values.len() - 1 - i) as f64 * step - shift;
        let y_of = |v: f64| py + ph - (v / scale).clamp(0.0, 1.0) * ph;
        let pts: Vec<(f64, f64)> = values
            .iter()
            .enumerate()
            .map(|(i, v)| (x_of(i), y_of(*v)))
            .collect();

        cr.save().ok();
        // clip so the scrolled tail can't spill outside the card
        cr.rectangle(px, py, pw, ph);
        cr.clip();

        spline_path(cr, &pts);
        if fill {
            let last = pts[pts.len() - 1];
            let first = pts[0];
            cr.line_to(last.0, py + ph);
            cr.line_to(first.0, py + ph);
            cr.close_path();
            let grad = gtk::cairo::LinearGradient::new(0.0, py, 0.0, py + ph);
            grad.add_color_stop_rgba(0.0, r, g, b, 0.45);
            grad.add_color_stop_rgba(1.0, r, g, b, 0.02);
            if cr.set_source(&grad).is_ok() {
                let _ = cr.fill();
            }
            spline_path(cr, &pts);
        }
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(1.6);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        let _ = cr.stroke();
        cr.restore().ok();
    }
}

fn value_of(series: Series, s: &Sample) -> f64 {
    match series {
        Series::Cpu => s.cpu,
        Series::Gpu => s.gpu.unwrap_or(0.0),
        Series::Mem => s.mem,
        Series::Net => s.net_rx,
    }
}

/// The top-right caption for a chart, as coloured runs.
///
/// Most charts are one run in the series colour. The network chart is two,
/// each painted like the curve it describes — otherwise "in" and "out" are
/// two numbers in one colour above two lines the eye can't assign them to.
///
/// Clock speed sits between load and temperature — the order a reading is
/// usually thought about: how busy, how fast, how hot.
fn caption_runs(
    series: Series,
    s: &Sample,
    show_clock: bool,
    accent: (f64, f64, f64),
) -> Vec<(String, (f64, f64, f64))> {
    let temp = |t: Option<f64>| t.map(|t| format!("  {t:.0}°C")).unwrap_or_default();
    let clock = |c: Option<f64>| match c.filter(|_| show_clock) {
        Some(mhz) => format!("  {}", sysmon::human_clock(mhz)),
        None => String::new(),
    };
    let one = |text: String| vec![(text, accent)];
    match series {
        Series::Cpu => one(format!("{:.0}%{}{}", s.cpu, clock(s.cpu_clock), temp(s.cpu_temp))),
        Series::Gpu => match s.gpu {
            Some(g) => one(format!("{g:.0}%{}{}", clock(s.gpu_clock), temp(s.gpu_temp))),
            None => one("n/a".to_string()),
        },
        Series::Mem => one(format!(
            "{:.0}%  {} / {}",
            s.mem,
            sysmon::human_kb(s.mem_used_kb),
            sysmon::human_kb(s.mem_total_kb)
        )),
        // ASCII on purpose: this is drawn with cairo's toy font API, which
        // does no font fallback, and the arrow glyphs came out as tofu
        Series::Net => vec![
            (format!("in {}", sysmon::human_rate(s.net_rx)), accent),
            ("  ".to_string(), accent),
            (format!("out {}", sysmon::human_rate(s.net_tx)), NET_TX_RGB),
        ],
    }
}

/// Catmull-Rom through every point, emitted as cubic beziers.
///
/// The tangent at each point is the direction between its neighbours, scaled
/// by a sixth — the standard conversion, and the reason the curve passes
/// through the readings instead of near them.
fn spline_path(cr: &gtk::cairo::Context, pts: &[(f64, f64)]) {
    cr.new_path();
    cr.move_to(pts[0].0, pts[0].1);
    for i in 0..pts.len() - 1 {
        let p0 = pts[i.saturating_sub(1)];
        let p1 = pts[i];
        let p2 = pts[i + 1];
        let p3 = pts[(i + 2).min(pts.len() - 1)];
        cr.curve_to(
            p1.0 + (p2.0 - p0.0) / 6.0,
            p1.1 + (p2.1 - p0.1) / 6.0,
            p2.0 - (p3.0 - p1.0) / 6.0,
            p2.1 - (p3.1 - p1.1) / 6.0,
            p2.0,
            p2.1,
        );
    }
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

//! A single Hedge: an undecorated, translucent desktop widget showing a grid of
//! app tiles, accepting apps dragged in, and collapsing to a slim name handle at
//! a screen edge (with a smooth slide in/out).
//!
//! Full port of `hedge_window.py`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;

use crate::app::HedgeApp;
use crate::colors;
use crate::config::Hedge;

/// How close to a monitor edge a dropped hedge must be before it docks.
///
/// Deliberately tiny. At 48px a hedge dropped anywhere near a corner was
/// swallowed into a dock, so there was no way to park one *close to* an edge
/// without it collapsing. Docking is now something you have to mean.
const EDGE_THRESHOLD: i32 = 4;
const LEAVE_GRACE: u64 = 250; // ms before a hovered Hedge rolls back up
/// How far outside a hedge still counts as hovering it. Reaching a hedge at a
/// screen edge means shoving the pointer at that edge, which routinely
/// overshoots past the window — without slack the hedge hides again the
/// instant you arrive, and flickers.
const HOVER_MARGIN: i32 = 24;
const DRAG_SLOP: f64 = 6.0; // px of movement before a press counts as a drag
const ANIM_MS: f64 = 140.0; // tile shift animation duration (ease-out)
const SLIDE_MS: f64 = 160.0; // docked hedge slide in/out duration
const RESIZE_MARGIN: f64 = 8.0; // px grab zone along the sides
/// Corner grab zone. Deliberately much larger than the side margin: a corner
/// resizes both axes at once, so it is the one people aim for, and an 8px
/// square is a hard target on a translucent window with no visible frame.
/// 20px matches the default body padding, so it doesn't reach into the tiles.
const RESIZE_CORNER: f64 = 20.0;
const SNAP_TOL: i32 = 12; // px pull toward a whole-tile size while resizing
const NEIGHBOUR_SNAP: i32 = 12; // px pull toward a neighbouring hedge's edge
const ROLL_MS: f64 = 160.0; // title-bar roll up/down duration
/// How many lines a tile label may wrap to before it finally gives up and
/// ellipsizes. At ~12 characters a line that is ~36 characters of name.
///
/// Every label reserves this many lines whether it needs them or not, so a
/// cell's size is a property of the hedge's settings rather than of which names
/// happen to be in it. Letting it vary made the resize lattice — and the "3 × 3"
/// badge read off it — describe a grid the layout wasn't using.
const TILE_LABEL_LINES: i32 = 3;
const DIM_MS: f64 = 500.0; // fade-out of an idle docked hedge
const DIM_ALPHA: f64 = 0.35; // how far a docked hedge fades
/// How long a hover-opened hedge stays open before rolling itself back up.
pub const OPEN_HOLD_SECS: u64 = 15;

/// Source of the per-hedge style scope (`hedge-css-N`); see `Inner::css_class`.
static CSS_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// custom DnD target for moving tiles between hedges (same process)
const TILE_TARGET: &str = "application/x-hedge-tile";
const TILE_INFO: u32 = 1;

fn tile_target() -> gtk::TargetEntry {
    gtk::TargetEntry::new(TILE_TARGET, gtk::TargetFlags::SAME_APP, TILE_INFO)
}

fn edge_cursor(edge: gdk::WindowEdge) -> Option<&'static str> {
    use gdk::WindowEdge::*;
    Some(match edge {
        NorthWest => "nw-resize",
        North => "n-resize",
        NorthEast => "ne-resize",
        West => "w-resize",
        East => "e-resize",
        SouthWest => "sw-resize",
        South => "s-resize",
        SouthEast => "se-resize",
        _ => return None,
    })
}

struct Press {
    root_x: f64,
    root_y: f64,
    win_x: i32,
    win_y: i32,
    moved: bool,
}

/// An interactive resize in flight, driven by us rather than the WM.
struct Resize {
    edge: gdk::WindowEdge,
    root_x: f64,
    root_y: f64,
    win_x: i32,
    win_y: i32,
    w: i32,
    h: i32,
}

/// `Resize` read out of its `RefCell` before touching the window — never hold
/// the borrow across a call that can re-enter (see ARCHITECTURE.md).
type ResizeState = (gdk::WindowEdge, f64, f64, i32, i32, i32, i32);

// tile tween: (x0, y0, x1, y1, start_us)
type Tween = (f64, f64, f64, f64, Option<i64>);
// slide tween: (cx, cy, tx, ty, start_us)
type Slide = (f64, f64, f64, f64, i64);

struct Inner {
    app: HedgeApp,
    win: gtk::Window,
    ebox: gtk::EventBox,
    name: RefCell<String>,
    css: RefCell<Option<gtk::CssProvider>>,
    /// Unique style class for this hedge. Every selector this window's CSS
    /// emits is scoped under it — see `apply_css`.
    css_class: String,

    collapsed: Cell<bool>,
    edge: RefCell<Option<String>>,
    expanded: Cell<bool>,

    bar: RefCell<Option<gtk::Box>>,
    header: RefCell<Option<gtk::Box>>,
    /// The panel's body (everything under the title bar) — the part that rolls.
    body: RefCell<Option<gtk::Box>>,
    /// Clipping container holding the body; shrinking its height request is
    /// what actually rolls the hedge up (see `set_body_visible_h`).
    roll_clip: RefCell<Option<gtk::ScrolledWindow>>,
    rolled: Cell<bool>,
    /// roll tween: (from_h, to_h, start_us)
    roll: Cell<Option<(f64, f64, i64)>>,
    roll_id: RefCell<Option<glib::SourceId>>,
    /// hold-open countdown for a hover-opened hedge
    hold_timer: RefCell<Option<glib::SourceId>>,
    /// grace period before a hide-until-hover hedge fades back out
    hide_timer: RefCell<Option<glib::SourceId>>,
    dock_layout: RefCell<Option<gtk::Layout>>,
    dock_panel: RefCell<Option<gtk::Box>>,
    panel_size: Cell<(i32, i32)>,
    panel_pos: Cell<(i32, i32)>,
    slide: Cell<Option<Slide>>,
    slide_id: RefCell<Option<glib::SourceId>>,
    last_alloc: Cell<(i32, i32)>,
    watch_id: RefCell<Option<glib::SourceId>>,
    dim_timer: RefCell<Option<glib::SourceId>>,
    /// dim fade tween: (from, to, start_us)
    dim_fade: Cell<Option<(f64, f64, i64)>>,
    dim_fade_id: RefCell<Option<glib::SourceId>>,
    vis_cache: Cell<i32>,
    collapse_timer: RefCell<Option<glib::SourceId>>,

    press: RefCell<Option<Press>>,
    resize: RefCell<Option<Resize>>,

    tiles: RefCell<Vec<gtk::Button>>,
    fixed: RefCell<Option<gtk::Layout>>,
    pos: RefCell<HashMap<usize, (i32, i32)>>,
    anims: RefCell<HashMap<usize, Tween>>,
    anim_running: Cell<bool>,
    anim_id: RefCell<Option<glib::SourceId>>,
    placeholder: RefCell<Option<gtk::Box>>,
    /// "3 × 2" readout shown while resizing (see `show_size_badge`).
    badge: RefCell<Option<gtk::Label>>,
    hover_index: Cell<Option<usize>>,
    hover_timer: RefCell<Option<glib::SourceId>>,
    user_resized: Cell<bool>,
    /// where the window sat before a peek hid it
    pre_hide_pos: Cell<(i32, i32)>,
    layout_cols: Cell<Option<usize>>,
    geom_timer: RefCell<Option<glib::SourceId>>,
    cursor_name: RefCell<Option<String>>,
    /// Snap lattice captured when a resize starts, `(base_w, base_h, inc_w,
    /// inc_h)`. Measured once because the drag itself changes the sizes the
    /// lattice would otherwise be derived from — see `measure_snap_grid`.
    resize_grid: Cell<Option<(i32, i32, i32, i32)>>,
    /// An inline title edit is open. Cleared by whichever of Enter / Escape /
    /// focus-out gets there first, so only one of them commits.
    renaming: Cell<bool>,
    drag_failed: Cell<bool>,
    /// The URI handed to the drop target when a tile is dragged out, kept so
    /// `on_tile_drag_end` can check the copy actually landed before dropping
    /// the tile from the hedge.
    drag_out_uri: RefCell<Option<String>>,
    /// The target reported a *move* (GTK's drag-data-delete), i.e. it took
    /// ownership. Proof enough on its own that the tile can go.
    drag_out_moved: Cell<bool>,
}

/// Clonable handle to a live Hedge window.
#[derive(Clone)]
pub struct HedgeWindow(Rc<Inner>);

impl HedgeWindow {
    pub fn new(app: &HedgeApp, name: String) -> Self {
        let block = app.hedge(&name).unwrap_or_default();
        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        win.set_title(&name);
        win.set_decorated(false);
        win.set_resizable(true);
        win.set_skip_taskbar_hint(true);
        win.set_skip_pager_hint(true);
        win.set_keep_below(true);
        // NORMAL, *not* UTILITY — this is load-bearing. Muffin/Mutter promote a
        // window whose type is one of the "transient types" (dialog, toolbar,
        // menu, utility) and which has no transient-parent to the highest layer
        // used by any window in its WM group. The Settings window is NORMAL and
        // in the same group, so as a UTILITY the hedges were dragged out of the
        // below-layer and pinned on top of everything the moment Settings
        // opened. NORMAL is not a transient type, so keep-below sticks.
        // skip_taskbar/skip_pager (set above) keep NORMAL out of the window
        // list and alt-tab, which is all UTILITY was buying us.
        win.set_type_hint(gdk::WindowTypeHint::Normal);
        win.set_focus_on_map(false);
        win.set_accept_focus(false);
        win.stick();
        win.style_context().add_class("hedge-window");

        // Per-hedge style scope. The CSS provider is installed screen-wide, so
        // without a unique class every hedge's rules would apply to every other
        // hedge and the last one loaded would win — changing one hedge's colour
        // would repaint them all. Counter, not name, so a rename can't collide.
        let css_class = format!(
            "hedge-css-{}",
            CSS_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        win.style_context().add_class(&css_class);

        enable_transparency(&win);

        let ebox = gtk::EventBox::new();
        ebox.set_above_child(false);
        ebox.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
                | gdk::EventMask::BUTTON_RELEASE_MASK
                | gdk::EventMask::POINTER_MOTION_MASK,
        );
        win.add(&ebox);
        win.add_events(gdk::EventMask::ENTER_NOTIFY_MASK | gdk::EventMask::LEAVE_NOTIFY_MASK);

        let fw = HedgeWindow(Rc::new(Inner {
            app: app.clone(),
            win: win.clone(),
            ebox: ebox.clone(),
            name: RefCell::new(name.clone()),
            css: RefCell::new(None),
            css_class,
            collapsed: Cell::new(block.edge.is_some()),
            edge: RefCell::new(block.edge.clone()),
            expanded: Cell::new(false),
            bar: RefCell::new(None),
            header: RefCell::new(None),
            body: RefCell::new(None),
            roll_clip: RefCell::new(None),
            rolled: Cell::new(block.rolled),
            roll: Cell::new(None),
            roll_id: RefCell::new(None),
            hold_timer: RefCell::new(None),
            hide_timer: RefCell::new(None),
            dock_layout: RefCell::new(None),
            dock_panel: RefCell::new(None),
            panel_size: Cell::new((0, 0)),
            panel_pos: Cell::new((0, 0)),
            slide: Cell::new(None),
            slide_id: RefCell::new(None),
            last_alloc: Cell::new((0, 0)),
            watch_id: RefCell::new(None),
            dim_timer: RefCell::new(None),
            dim_fade: Cell::new(None),
            dim_fade_id: RefCell::new(None),
            vis_cache: Cell::new(0),
            collapse_timer: RefCell::new(None),
            press: RefCell::new(None),
            resize: RefCell::new(None),
            tiles: RefCell::new(Vec::new()),
            fixed: RefCell::new(None),
            pos: RefCell::new(HashMap::new()),
            anims: RefCell::new(HashMap::new()),
            anim_running: Cell::new(false),
            anim_id: RefCell::new(None),
            placeholder: RefCell::new(None),
            badge: RefCell::new(None),
            hover_index: Cell::new(None),
            hover_timer: RefCell::new(None),
            user_resized: Cell::new(false),
            pre_hide_pos: Cell::new((block.x as i32, block.y as i32)),
            layout_cols: Cell::new(None),
            geom_timer: RefCell::new(None),
            cursor_name: RefCell::new(None),
            resize_grid: Cell::new(None),
            renaming: Cell::new(false),
            drag_failed: Cell::new(false),
            drag_out_uri: RefCell::new(None),
            drag_out_moved: Cell::new(false),
        }));

        // window drag/resize
        let this = fw.clone();
        ebox.connect_button_press_event(move |_, ev| this.on_press(ev));
        let this = fw.clone();
        ebox.connect_motion_notify_event(move |_, ev| this.on_motion(ev));
        let this = fw.clone();
        ebox.connect_button_release_event(move |_, ev| this.on_release(ev));

        // hover expand when collapsed
        let this = fw.clone();
        win.connect_enter_notify_event(move |_, ev| this.on_enter(ev));
        let this = fw.clone();
        win.connect_leave_notify_event(move |_, ev| this.on_leave(ev));

        // geometry tracking
        let this = fw.clone();
        win.connect_configure_event(move |_, ev| {
            let (w, h) = ev.size();
            this.on_configure(w as i32, h as i32);
            false
        });
        let this = fw.clone();
        win.connect_size_allocate(move |_, alloc| this.on_size_allocate(alloc.width(), alloc.height()));
        let this = fw.clone();
        win.connect_destroy(move |_| this.on_destroy());

        // drag IN
        win.drag_dest_set(
            gtk::DestDefaults::ALL,
            &[tile_target()],
            gdk::DragAction::COPY | gdk::DragAction::MOVE,
        );
        win.drag_dest_add_uri_targets();
        let this = fw.clone();
        win.connect_drag_data_received(move |_, ctx, x, y, data, info, time| {
            this.on_drop(ctx, x, y, data, info, time)
        });
        let this = fw.clone();
        win.connect_drag_motion(move |_, _ctx, x, y, _time| this.on_drag_motion(x, y));
        let this = fw.clone();
        win.connect_drag_leave(move |_, _ctx, _time| this.on_drag_leave());

        fw.render();
        win.move_(block.x as i32, block.y as i32);
        if let (Some(w), Some(h)) = (block.width, block.height) {
            win.resize(w as i32, h as i32);
        }
        win.show_all();
        if fw.0.collapsed.get() {
            fw.show_handle();
        } else {
            // after show_all, which would otherwise re-expand a rolled hedge
            fw.apply_rolled_state();
        }
        fw.apply_resting_opacity();
        fw
    }

    // -- helpers ------------------------------------------------------------
    fn block(&self) -> Hedge {
        self.0.app.hedge(&self.0.name.borrow()).unwrap_or_default()
    }

    fn name(&self) -> String {
        self.0.name.borrow().clone()
    }

    // -- public API used by HedgeApp ---------------------------------------
    pub fn destroy(&self) {
        self.on_destroy();
        unsafe {
            self.0.win.destroy();
        }
    }

    /// Current on-screen rect.
    pub fn rect(&self) -> Rect {
        let (x, y) = self.0.win.position();
        (x, y, self.0.win.allocated_width(), self.0.win.allocated_height())
    }

    /// The rect other hedges must not cover.
    ///
    /// For a docked hedge this is only the visible sliver, not the whole
    /// panel: most of a docked panel is slid off the edge and click-through,
    /// so treating it as solid would reserve a large invisible no-go area.
    /// Same geometry `update_input_shape` uses to decide what is clickable.
    pub fn collision_rect(&self) -> Rect {
        let (x, y) = self.0.win.position();
        if !self.0.collapsed.get() {
            return (
                x,
                y,
                self.0.win.allocated_width(),
                self.0.win.allocated_height(),
            );
        }
        let (pw, ph) = self.0.panel_size.get();
        let (dx, dy) = self.0.panel_pos.get();
        let (x0, y0) = (dx.max(0), dy.max(0));
        let (x1, y1) = ((dx + pw).min(pw), (dy + ph).min(ph));
        (x + x0, y + y0, (x1 - x0).max(1), (y1 - y0).max(1))
    }

    /// Show a hide-until-hover hedge for the duration of someone else's drag,
    /// so the user can see what they are about to push against, then let it
    /// fade back out afterwards.
    pub fn reveal_for_drag(&self, reveal: bool) {
        if !self.block().hide_until_hover {
            return;
        }
        if std::env::var("HEDGE_DEBUG").is_ok() {
            eprintln!("hedge[{}]: reveal_for_drag({reveal})", self.name());
        }
        if reveal {
            if let Some(id) = self.0.hide_timer.borrow_mut().take() {
                id.remove();
            }
            self.fade_to(1.0);
        } else {
            self.schedule_hide();
        }
    }

    /// Temporarily take the hedge off screen (peek mode). Keeps all state.
    ///
    /// The position has to be restored by hand: hiding and re-showing makes
    /// the WM map the window afresh, and GTK does not remember where it was —
    /// so without this every hedge reappears wherever the WM decides to put
    /// it. The move is deferred to an idle because a move issued before the
    /// window is actually mapped gets overridden by that mapping.
    pub fn set_visible(&self, visible: bool) {
        if visible {
            let (x, y) = self.0.pre_hide_pos.get();
            self.0.win.show();
            let this = self.clone();
            glib::idle_add_local_once(move || {
                this.0.win.move_(x, y);
                if this.0.collapsed.get() {
                    this.snap_to_edge();
                }
            });
        } else {
            self.0.pre_hide_pos.set(self.0.win.position());
            self.0.win.hide();
        }
    }

    pub fn is_collapsed(&self) -> bool {
        self.0.collapsed.get()
    }

    pub fn move_window(&self, x: i32, y: i32) {
        self.0.win.move_(x, y);
    }

    /// Undock, un-roll and drop the hedge at `(x, y)` — the rescue path for a
    /// hedge that has ended up somewhere unreachable.
    pub fn place_at(&self, x: i32, y: i32) {
        *self.0.edge.borrow_mut() = None;
        self.0.collapsed.set(false);
        self.0.expanded.set(false);
        self.0.rolled.set(false);
        self.0.vis_cache.set(0);
        self.render();
        self.0.win.move_(x, y);
    }

    pub fn set_name(&self, new: &str) {
        *self.0.name.borrow_mut() = new.to_string();
        self.0.win.set_title(new);
    }

    pub fn restyle(&self) {
        self.apply_css();
        self.sync_window_size();
    }

    pub fn relayout(&self) {
        if self.0.fixed.borrow().is_none() {
            self.render();
            return;
        }
        self.layout_tiles(self.0.hover_index.get(), true);
        self.sync_window_size();
    }

    pub fn render(&self) {
        if std::env::var("HEDGE_DEBUG").is_ok() {
            eprintln!("hedge[{}]: render", self.name());
        }
        self.apply_css();
        if let Some(child) = self.0.ebox.child() {
            self.0.ebox.remove(&child);
        }
        *self.0.bar.borrow_mut() = None;
        *self.0.header.borrow_mut() = None;
        *self.0.body.borrow_mut() = None;
        *self.0.roll_clip.borrow_mut() = None;
        *self.0.dock_layout.borrow_mut() = None;
        *self.0.dock_panel.borrow_mut() = None;

        if self.0.collapsed.get() {
            let panel = self.build_edge_layout();
            // A docked hedge that has been resized keeps that size: the panel's
            // request drives panel_size, which every bit of dock geometry
            // (slide offset, input shape, snap position) is measured from.
            let b = self.block();
            if let (Some(w), Some(h)) = (b.width, b.height) {
                panel.set_size_request(w as i32, h as i32);
            }
            let dock = gtk::Layout::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
            dock.put(&panel, 0, 0);
            self.0.ebox.add(&dock);
            self.0.ebox.show_all();
            self.relayout_measured();
            let pw = panel.preferred_width().1;
            let ph = panel.preferred_height().1;
            self.0.panel_size.set((pw, ph));
            self.0.vis_cache.set(0);
            self.bar_vis(); // cache the sliver while freshly styled
            dock.set_size_request(pw, ph);
            let (dx, dy) = self.dock_offset();
            self.0.panel_pos.set((dx, dy));
            dock.move_(&panel, dx, dy);
            *self.0.dock_panel.borrow_mut() = Some(panel);
            *self.0.dock_layout.borrow_mut() = Some(dock);
            self.sync_window_size();
            let this = self.clone();
            glib::idle_add_local_once(move || this.snap_to_edge());
            if !self.0.expanded.get() {
                self.schedule_dim();
            }
        } else {
            self.undim();
            let panel = self.build_panel();
            self.0.ebox.add(&panel);
            self.0.ebox.show_all();
            // build_body laid the tiles out before they were realized, and an
            // unrealized tile measures differently (a label reports its full
            // natural width only once it exists). Redo it now that the widgets
            // are real, otherwise the provisional cell size survives until the
            // next re-layout and every tile past the first shifts sideways
            // then — which is what a drag was triggering.
            self.relayout_measured();
            self.sync_window_size();
            self.clear_input_shape();
            self.apply_rolled_state();
            self.apply_resting_opacity();
            if std::env::var("HEDGE_DEBUG").is_ok() {
                let this = self.clone();
                glib::idle_add_local_once(move || {
                    if let Some(c) = this.0.roll_clip.borrow().clone() {
                        let a = c.allocation();
                        log!("CLIP alloc=({},{}) {}x{}", a.x(), a.y(), a.width(), a.height());
                    }
                    if let Some(f) = this.0.fixed.borrow().clone() {
                        let a = f.allocation();
                        eprintln!(
                            "hedge: GRID alloc=({},{}) {}x{} size={:?} binw={:?}",
                            a.x(), a.y(), a.width(), a.height(), f.size(),
                            f.bin_window().map(|w| (w.width(), w.height()))
                        );
                    }
                    for (i, t) in this.0.tiles.borrow().iter().enumerate() {
                        let a = t.allocation();
                        eprintln!(
                            "hedge: tile {i} alloc=({},{}) {}x{} realized={} win={}",
                            a.x(),
                            a.y(),
                            a.width(),
                            a.height(),
                            t.is_realized(),
                            t.window().is_some()
                        );
                    }
                });
            }
        }
    }

    // -- CSS ----------------------------------------------------------------
    fn apply_css(&self) {
        let b = self.block();
        let accent = &b.accent;
        let radius = b.corner_radius.max(0);
        let pad = b.padding;
        let bg = colors::rgba(&b.bg_color, if b.show_background { b.bg_opacity } else { 0.0 });
        let hover = colors::rgba(accent, 0.16);
        // soft radial glow for the hovered tile — no hard outline
        let glow_core = colors::rgba(accent, 0.30);
        let glow_mid = colors::rgba(accent, 0.14);
        let ph = colors::rgba(accent, 0.5);
        let bord = colors::rgba(accent, 0.6);
        // resting border: transparent unless the hedge asks for a permanent one
        let resting_border = if b.show_border {
            bord.clone()
        } else {
            "transparent".to_string()
        };
        // Auto by default — black or white, whichever reads on the accent —
        // but an explicit titleColor wins. Empty means "keep choosing for me",
        // which is what a hedge that has never been given one stores.
        let titletxt = match b.title_color.trim() {
            "" => colors::contrast_text(accent),
            explicit => explicit.to_string(),
        };
        // Tile label colour. style.css sets a default; this rule is scoped to
        // the hedge and loaded later, so it wins.
        let labeltxt = {
            let c = b.label_color.trim();
            if c.is_empty() { "#c7cad6".to_string() } else { c.to_string() }
        };
        // Clamped so a hand-edited config can't produce something unreadable.
        // Note 11 is the largest value that leaves the title bar at its fixed
        // 30px — above that the bar grows with the font's line height, which
        // CSS gives us no way to cap without clipping the glyphs.
        let title_font = b.title_font_size.clamp(6, 20);
        let label_font = b.label_font_size.clamp(6, 24);
        // the header now sits flush against the panel edge (no border inset),
        // so its corners take the panel's radius rather than radius - 2
        let hr = radius.max(0);
        // Tiles are square for every hedge, independent of its corner radius:
        // the hover highlight reads as its own element, and inheriting a large
        // hedge radius rounded a ~68x90 tile into an ellipse.
        let tile_radius = 0;

        let edge = self.0.edge.borrow().clone();
        let edge = edge.as_deref();
        // The collapsed handle keeps the hedge's OWN radius on its
        // interior-facing side, flat against the screen edge. No need to force
        // a pill: CSS already clamps border-radius to half the box, so a large
        // radius becomes a true semicircular end by itself while a small one
        // stays nearly square. Forcing 999px made every rounded hedge — even
        // one set to radius 4 — dock as a full pill.
        let pill = radius;
        let bar_radius = match edge {
            Some("left") => format!("0 {pill}px {pill}px 0"),
            Some("right") => format!("{pill}px 0 0 {pill}px"),
            Some("top") => format!("0 0 {pill}px {pill}px"),
            Some("bottom") => format!("{pill}px {pill}px 0 0"),
            _ => format!("{pill}px"),
        };
        let panel_radius = match edge {
            Some("left") => format!("0 {radius}px {radius}px 0"),
            Some("right") => format!("{radius}px 0 0 {radius}px"),
            Some("top") => format!("0 0 {radius}px {radius}px"),
            Some("bottom") => format!("{radius}px {radius}px 0 0"),
            _ => format!("{radius}px"),
        };
        let bar_pad = if matches!(edge, Some("left") | Some("right")) {
            "12px 1px"
        } else {
            "1px 12px"
        };
        // Fixed handle thickness, matching the floating title bar's 30px
        // (28 + 1px padding each side). Without it the handle was sized by the
        // rotated label and varied per hedge.
        let bar_thickness = if matches!(edge, Some("left") | Some("right")) {
            "min-width: 28px;"
        } else {
            "min-height: 28px;"
        };

        // Every selector is scoped under this hedge's unique class: the
        // provider below is installed screen-wide, so unscoped rules would
        // leak onto every other hedge (last provider loaded wins).
        let cls = &self.0.css_class;
        let css = format!(
            r#"
        .{cls} .hedge-panel {{
            background-color: {bg};
            border-radius: {panel_radius};
            padding: 0;
            /* The highlight is an INSET SHADOW, not a border. A border takes
               layout space, so the title bar started 2px inside the hedge's
               real edge and that ring — transparent when unhighlighted — read
               as a black gap between the title bar and the edge. An inset
               shadow occupies no space, so the header reaches the true edge
               and the ring is drawn over the content instead. */
            border: none;
            box-shadow: inset 0 0 0 2px {resting_border};
            transition: box-shadow 150ms ease;
        }}
        .{cls}.hedge-hover .hedge-panel {{ box-shadow: inset 0 0 0 2px {bord}; }}
        .{cls} .hedge-header {{
            background-color: {accent};
            border-radius: {hr}px {hr}px 0 0;
        }}
        .{cls} .hedge-title {{ color: {titletxt}; font-size: {title_font}px; }}
        .{cls} .hedge-label {{ color: {labeltxt}; font-size: {label_font}px; }}
        .{cls} .hedge-rename {{ font-size: {title_font}px; }}
        .{cls} .hedge-body {{ padding: {pad}px; }}
        .{cls} .hedge-tile {{
            /* the theme's button padding is generous; it was adding a chunk of
               dead space above the first icon row */
            padding: 4px;
            border: 2px solid transparent;
            border-radius: {tile_radius}px;
            background-color: transparent;
            /* the theme paints buttons with a gradient, which would cover the
               translucent hover fill below and render it near-black */
            background-image: none;
            box-shadow: none;
        }}
        .{cls} .hedge-tile:hover {{
            /* a glow rather than an outline: the border read as heavy, and at
               the top row it was clipped by the grid's own window. Inset, so
               it stays inside the tile and cannot be clipped. */
            border-color: transparent;
            background-color: {glow_mid};
            box-shadow: inset 0 0 12px 2px {glow_core};
        }}
        .{cls} .hedge-dockbar {{
            background-color: {accent};
            border-radius: {bar_radius};
            padding: {bar_pad};
            {bar_thickness}
            opacity: 1;
            transition: opacity 500ms ease;
        }}
        /* Rolled up, the title bar IS the whole hedge, so it becomes a pill:
           rounding only its top corners left the top half of an ellipse sitting
           on a square base. 999px is clamped by CSS to half the box, giving a
           true stadium at whatever height the bar is. Corner radius 0 stays
           square. */
        .{cls}.hedge-rolled .hedge-panel {{ border-radius: {radius}px; }}
        .{cls}.hedge-rolled .hedge-header {{ border-radius: {radius}px; }}
        .{cls} .hedge-badge {{
            background-color: {accent};
            color: {titletxt};
        }}
        .{cls} .hedge-placeholder {{
            border: 2px dashed {ph};
            border-radius: {tile_radius}px;
            background-color: {hover};
        }}
        "#
        );

        let screen = match WidgetExt::screen(&self.0.win) {
            Some(s) => s,
            None => return,
        };
        if let Some(old) = self.0.css.borrow_mut().take() {
            gtk::StyleContext::remove_provider_for_screen(&screen, &old);
        }
        let provider = gtk::CssProvider::new();
        provider.connect_parsing_error(|_, section, err| {
            eprintln!(
                "hedge: CSS parse error at line {}: {err}",
                section.start_line() + 1
            );
        });
        if let Err(e) = provider.load_from_data(css.as_bytes()) {
            log!(WARN, "per-hedge CSS failed to load: {e}");
        }
        {
            gtk::StyleContext::add_provider_for_screen(
                &screen,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            *self.0.css.borrow_mut() = Some(provider);
        }
    }

    // -- content builders ---------------------------------------------------
    fn make_image(icon: &str, size: i32) -> gtk::Image {
        if icon.contains('/') {
            if let Ok(pb) = gdk_pixbuf::Pixbuf::from_file_at_size(icon, size, size) {
                return gtk::Image::from_pixbuf(Some(&pb));
            }
        }
        let name = if icon.is_empty() {
            "application-x-executable"
        } else {
            icon
        };
        let img = gtk::Image::from_icon_name(Some(name), gtk::IconSize::Dialog);
        img.set_pixel_size(size);
        img
    }

    fn make_tile(&self, entry: &crate::config::AppEntry, index: usize, b: &Hedge) -> gtk::Button {
        let btn = gtk::Button::new();
        btn.style_context().add_class("hedge-tile");
        btn.set_relief(gtk::ReliefStyle::None);
        // The name on hover, always: with labels hidden it is the only place
        // the name exists at all, and even with labels on, a very long name
        // ellipsizes past three lines and the tooltip is where it survives.
        if !entry.name.is_empty() {
            btn.set_tooltip_text(Some(&entry.name));
        }

        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.pack_start(&Self::make_image(&entry.icon, b.icon_size as i32), false, false, 0);
        if b.show_labels && !entry.name.is_empty() {
            let lbl = gtk::Label::new(Some(&entry.name));
            lbl.style_context().add_class("hedge-label");
            // Wrap rather than cut: a launcher called "Blender Video Editor"
            // should read as such, not as "Blender Vid…". `max_width_chars`
            // still decides how wide a line gets before it wraps, so cells stay
            // a sensible width; the name just grows downwards instead.
            //
            // `set_lines` is the cap, and GTK only honours it on a label that
            // is *both* wrapping and ellipsizing — so ellipsize stays on purely
            // as the backstop for a name too long to fit three lines (past ~36
            // characters). Every realistic name wraps and is shown in full.
            lbl.set_max_width_chars(12);
            lbl.set_line_wrap(true);
            // WordChar, not Word: breaking mid-word is ugly, but a single word
            // wider than the tile has nowhere else to go, and showing all of it
            // beats trimming it. Pango would spell that break with a hyphen
            // ("LibreOffi-ce"), which reads as part of the name, so hyphen
            // insertion is turned off.
            lbl.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
            let attrs = gtk::pango::AttrList::new();
            attrs.insert(gtk::pango::AttrInt::new_insert_hyphens(false));
            lbl.set_attributes(Some(&attrs));
            lbl.set_lines(TILE_LABEL_LINES);
            lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
            lbl.set_justify(gtk::Justification::Center);
            // Height is equalised across tiles later, in `reserve_label_height`:
            // a label measures 0 until it is realized.
            // first line always directly under the icon, so names on one row
            // start at the same height however many lines each takes
            lbl.set_yalign(0.0);
            col.pack_start(&lbl, false, false, 0);
        }
        btn.add(&col);

        let app = self.0.app.clone();
        let cmd = entry.command.clone();
        btn.connect_clicked(move |_| app.launch(&cmd));

        // right-click a tile to remove it
        let this = self.clone();
        btn.connect_button_press_event(move |_, ev| {
            if ev.button() == 3 {
                this.tile_menu(index);
                return glib::Propagation::Stop;
            }
            // Whatever the cursor is showing is what the drag must do. On a small
            // hedge a tile reaches into the edge grab zone, and a tile is a drag
            // source, so the press landed here and started dragging the launcher
            // out — while the pointer was showing a resize arrow. Claim the press
            // for the resize instead. Stopping the emission is what keeps the drag
            // source (whose handlers are connected after this one) from arming.
            if ev.button() == 1 {
                let (ex, ey) = this.win_coords(ev.root());
                if let Some(edge) = this.resize_edge_at(ex, ey) {
                    this.begin_resize(edge, ev.root());
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });

        // drag OUT: to the desktop / a folder (uri) or to another hedge (tile)
        btn.drag_source_set(
            gdk::ModifierType::BUTTON1_MASK,
            &[tile_target()],
            gdk::DragAction::COPY | gdk::DragAction::MOVE,
        );
        btn.drag_source_add_uri_targets();
        let this = self.clone();
        let icon = entry.icon.clone();
        btn.connect_drag_begin(move |_, ctx| this.on_tile_drag_begin(ctx, &icon));
        let this = self.clone();
        btn.connect_drag_data_get(move |_, _ctx, data, info, _t| {
            this.on_tile_drag_data_get(index, data, info)
        });
        let this = self.clone();
        btn.connect_drag_failed(move |_, _ctx, _res| {
            this.0.drag_failed.set(true);
            glib::Propagation::Proceed
        });
        // Fires only when the target performed a MOVE and wants the source
        // gone — the strongest possible "your copy arrived".
        let this = self.clone();
        btn.connect_drag_data_delete(move |_, _ctx| this.0.drag_out_moved.set(true));
        let this = self.clone();
        btn.connect_drag_end(move |_, _ctx| this.on_tile_drag_end(index));
        btn
    }

    fn build_body(&self, b: &Hedge) -> gtk::Box {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.style_context().add_class("hedge-body");

        if b.apps.is_empty() {
            self.0.tiles.borrow_mut().clear();
            *self.0.fixed.borrow_mut() = None;
            *self.0.placeholder.borrow_mut() = None;
            *self.0.badge.borrow_mut() = None;
            self.0.pos.borrow_mut().clear();
            self.0.anims.borrow_mut().clear();
            let hint = gtk::Label::new(Some("Drag apps here"));
            hint.style_context().add_class("hedge-hint");
            body.pack_start(&hint, false, false, 0);
            return body;
        }

        let tiles: Vec<gtk::Button> = b
            .apps
            .iter()
            .enumerate()
            .map(|(i, e)| self.make_tile(e, i, b))
            .collect();
        let fixed = gtk::Layout::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        *self.0.placeholder.borrow_mut() = None;
        *self.0.badge.borrow_mut() = None;
        self.0.pos.borrow_mut().clear();
        self.0.anims.borrow_mut().clear();
        for t in &tiles {
            fixed.put(t, 0, 0);
        }
        *self.0.tiles.borrow_mut() = tiles;
        *self.0.fixed.borrow_mut() = Some(fixed.clone());
        // Deliberately NOT laid out here. The tiles aren't realized yet, so
        // cell_size() would fall back to icon_size+24 (72px) instead of their
        // real 60px, and the layout would visibly re-flow the moment the real
        // measurement arrived — every tile past the first sliding 12px left.
        // `render` calls `relayout_measured` right after show_all instead.
        body.pack_start(&fixed, true, true, 0);
        body
    }

    fn build_panel(&self) -> gtk::Box {
        let b = self.block();
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.style_context().add_class("hedge-panel");

        if b.show_title {
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            header.style_context().add_class("hedge-header");
            let title = gtk::Label::new(Some(&self.name()));
            title.style_context().add_class("hedge-title");
            // Rename is armed only by a click on the title TEXT: this inner
            // EventBox is exactly as wide as the label. Clicking the bar's
            // empty run is for dragging and rolling, not renaming.
            let title_ebox = gtk::EventBox::new();
            title_ebox.set_visible_window(false);
            title_ebox.add(&title);
            // Double-click the title TEXT to rename. Anywhere else along the
            // bar double-clicks to roll (handled by the outer EventBox); the
            // Stop below keeps the two from both firing.
            let this = self.clone();
            title_ebox.connect_button_press_event(move |_, ev| {
                if ev.button() == 1 && ev.event_type() == gdk::EventType::DoubleButtonPress {
                    let this = this.clone();
                    glib::idle_add_local_once(move || this.start_rename());
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            header.set_center_widget(Some(&title_ebox));

            // The roll double-click, by contrast, wraps the WHOLE header — as a
            // centre widget it would only be as wide as the title text, so a
            // double-click anywhere else along the bar hit nothing.
            let tbox = gtk::EventBox::new();
            tbox.set_visible_window(false);
            tbox.add(&header);
            // double-click the title bar to roll the hedge up/down
            let this = self.clone();
            tbox.connect_button_press_event(move |_, ev| {
                if std::env::var("HEDGE_DEBUG").is_ok() {
                    eprintln!(
                        "hedge: title press btn={} type={:?} at {:?}",
                        ev.button(),
                        ev.event_type(),
                        ev.position()
                    );
                }
                if ev.button() == 1 && ev.event_type() == gdk::EventType::DoubleButtonPress {
                    this.toggle_roll();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            panel.pack_start(&tbox, false, false, 0);
            *self.0.header.borrow_mut() = Some(header);
        }
        let body = self.build_body(&b);
        // The body lives inside a clipping Gtk::Layout so the hedge can roll
        // up: a Layout does not inherit its child's size request, so shrinking
        // the Layout's height genuinely shrinks the window's requisition.
        // (Squeezing the body directly cannot work — `set_size_request` is a
        // floor, and GTK clamps a window's minimum to `MAX(hint, requisition)`.)
        // A ScrolledWindow, NOT a Gtk::Layout: we need a container that clips
        // vertically (so the hedge can roll up) *and* allocates its child the
        // full width. A Layout does the first but not the second — it hands
        // children their natural size — which left the grid one cell wide while
        // its tiles spanned the whole hedge, so every tile but the first fell
        // outside the allocation: the rightmost icon was cut in half and only
        // tile 1 ever saw a hover.
        //
        // External policy = clip with no scrollbars; natural width propagates
        // so the hedge still auto-fits its content, while the height stays
        // under our control via the size request (see `set_body_visible_h`).
        let clip = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        clip.set_policy(gtk::PolicyType::External, gtk::PolicyType::External);
        clip.set_propagate_natural_width(true);
        clip.add(&body);
        *self.0.body.borrow_mut() = Some(body);
        *self.0.roll_clip.borrow_mut() = Some(clip.clone());
        panel.pack_start(&clip, true, true, 0);
        panel
    }

    fn build_dock_bar(&self, b: &Hedge) -> gtk::Box {
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bar.style_context().add_class("hedge-dockbar");
        let lbl = gtk::Label::new(Some(&self.name()));
        lbl.style_context().add_class("hedge-title");
        // A wrapped title doubles the rotated label's width, which is what made
        // some docked handles twice as thick as others: "Ab" fitted on one line
        // and measured 18px, "Hedge 2" wrapped to two and measured 32px.
        // Single-line only — deliberately NOT ellipsized: ellipsizing drops the
        // label's minimum width to almost nothing, so names came out truncated
        // ("Emp…") even with plenty of room.
        lbl.set_line_wrap(false);
        lbl.set_single_line_mode(true);
        match self.0.edge.borrow().as_deref() {
            Some("left") => lbl.set_angle(90.0),
            Some("right") => lbl.set_angle(270.0),
            _ => {}
        }
        let _ = b;
        // Same gesture as a floating hedge: double-click the title TEXT to
        // rename. Only reachable while the hedge is hovered open, since that
        // is the only time the handle's label is on screen.
        let tbox = gtk::EventBox::new();
        tbox.set_visible_window(false);
        tbox.add(&lbl);
        let this = self.clone();
        tbox.connect_button_press_event(move |_, ev| {
            if ev.button() == 1 && ev.event_type() == gdk::EventType::DoubleButtonPress {
                let this = this.clone();
                glib::idle_add_local_once(move || this.start_rename());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        bar.set_center_widget(Some(&tbox));
        bar
    }

    fn build_edge_layout(&self) -> gtk::Box {
        let b = self.block();
        let body = self.build_body(&b);
        let bar = self.build_dock_bar(&b);
        *self.0.bar.borrow_mut() = Some(bar.clone());
        let edge = self.0.edge.borrow().clone();
        let horizontal = matches!(edge.as_deref(), Some("left") | Some("right"));
        let panel = gtk::Box::new(
            if horizontal {
                gtk::Orientation::Horizontal
            } else {
                gtk::Orientation::Vertical
            },
            0,
        );
        panel.style_context().add_class("hedge-panel");
        // The body expands, the bar never does. Any slack — a docked hedge
        // given an explicit size has some — must collect on the body's side so
        // the handle stays flush against the screen edge. Packed
        // non-expanding, the slack landed past the bar instead and showed as a
        // dark strip of bare panel outside the handle.
        if matches!(edge.as_deref(), Some("left") | Some("top")) {
            panel.pack_start(&body, true, true, 0);
            panel.pack_start(&bar, false, false, 0);
        } else {
            panel.pack_start(&bar, false, false, 0);
            panel.pack_start(&body, true, true, 0);
        }
        panel
    }

    // -- animated tile layout ----------------------------------------------
    fn grid_cols(&self) -> usize {
        let b = self.block();
        if !(self.0.user_resized.get() || b.width.is_some()) {
            return b.columns.max(1) as usize;
        }
        let width = b.width.unwrap_or(self.0.win.allocated_width() as i64);
        let sp = b.tile_spacing as f64;
        let pad = b.padding;
        let (cw, _ch) = self.cell_size();
        let inner = (width - 2 * pad - 4) as f64;
        (((inner + sp) / (cw + sp)).floor() as i64).max(1) as usize
    }

    fn cell_size(&self) -> (f64, f64) {
        let tiles = self.0.tiles.borrow();
        let mut cw = 0;
        let mut ch = 0;
        for t in tiles.iter() {
            cw = cw.max(t.preferred_width().1);
            ch = ch.max(t.preferred_height().1);
        }
        if cw <= 0 || ch <= 0 {
            let s = self.block().icon_size as i32 + 24;
            return (s as f64, s as f64);
        }
        (cw as f64, ch as f64)
    }

    fn ensure_placeholder(&self, cw: i32, ch: i32) {
        let fixed = match self.0.fixed.borrow().clone() {
            Some(f) => f,
            None => return,
        };
        if self.0.placeholder.borrow().is_none() {
            let ph = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            ph.style_context().add_class("hedge-placeholder");
            ph.set_no_show_all(true);
            fixed.put(&ph, 0, 0);
            *self.0.placeholder.borrow_mut() = Some(ph);
        }
        if let Some(ph) = self.0.placeholder.borrow().clone() {
            ph.set_size_request(cw, ch);
        }
    }

    /// Re-run the layout now that the tiles are realized and measure correctly.
    fn relayout_measured(&self) {
        if self.0.fixed.borrow().is_some() {
            self.reserve_label_height();
            self.layout_tiles(self.0.hover_index.get(), false);
        }
    }

    /// Give every tile label the same height: the worst case of
    /// [`TILE_LABEL_LINES`] lines.
    ///
    /// Without it a tile is as tall as its own name happens to wrap — 94px for
    /// "Boot to Windows", 108px for "MQTT-Explorer-0.4.0-beta.6.AppImage" — and
    /// `cell_size` takes the tallest, so the grid is pitched for a cell most
    /// tiles don't fill and the rows read as uneven. Equalising makes a cell a
    /// property of the hedge's settings (icon size, font, line count) rather than
    /// of its contents.
    ///
    /// Runs from here rather than at build time because an unrealized label
    /// measures 0, which silently made an earlier attempt at this a no-op. One
    /// label is the probe for all of them — they share a font and a wrap width.
    fn reserve_label_height(&self) {
        let labels: Vec<gtk::Label> = self
            .0
            .tiles
            .borrow()
            .iter()
            .filter_map(|t| {
                let bx = t.child()?.downcast::<gtk::Box>().ok()?;
                bx.children()
                    .into_iter()
                    .rev()
                    .find_map(|c| c.downcast::<gtk::Label>().ok())
            })
            .collect();
        let Some(probe) = labels.first() else {
            return; // labels are off, or there are no tiles
        };
        let name = probe.text();
        let lines = vec!["X"; TILE_LABEL_LINES.max(1) as usize].join("\n");
        probe.set_text(&lines);
        let worst_case = probe.preferred_height().1;
        probe.set_text(&name);
        if worst_case <= 0 {
            return; // still not measurable; leave the natural heights alone
        }
        for l in &labels {
            l.set_size_request(-1, worst_case);
        }
    }

    fn layout_tiles(&self, gap_index: Option<usize>, animate: bool) {
        if std::env::var("HEDGE_DEBUG").is_ok() {
            let (dcw, dch) = self.cell_size();
            eprintln!(
                "hedge[{}]: layout_tiles gap={gap_index:?} animate={animate} cell={}x{}",
                self.name(),
                dcw as i32,
                dch as i32
            );
        }
        let fixed = match self.0.fixed.borrow().clone() {
            Some(f) => f,
            None => return,
        };
        let b = self.block();
        let sp = b.tile_spacing as i32;
        let cols = self.grid_cols();
        self.0.layout_cols.set(Some(cols));
        let (cwf, chf) = self.cell_size();
        let (cw, ch) = (cwf as i32, chf as i32);
        let n = self.0.tiles.borrow().len();

        // slot list: Some(tile_index) or None (the placeholder gap)
        let mut slots: Vec<Option<usize>> = (0..n).map(Some).collect();
        if let Some(gi) = gap_index {
            let ins = gi.min(slots.len());
            slots.insert(ins, None);
            self.ensure_placeholder(cw, ch);
        } else if let Some(ph) = self.0.placeholder.borrow().clone() {
            ph.hide();
        }

        let rows = ((slots.len() + cols - 1) / cols).max(1) as i32;
        let content_w = cols as i32 * (cw + sp) - sp;
        let content_h = rows * (ch + sp) - sp;
        if (self.0.user_resized.get() || b.width.is_some()) && !self.0.collapsed.get() {
            fixed.set_size_request(cw, ch);
        } else {
            fixed.set_size_request(content_w, content_h);
        }
        // Size the Layout's *bin window* to the full grid extent. It is not the
        // same thing as the size request above, and it defaults to 100x100 —
        // tiles are children of it, so any tile past x≈100 had its input window
        // clipped away and never saw a hover. It only started working once a
        // move or resize made gtk_layout_size_allocate grow the bin window to
        // the allocation, which is exactly the "fixes itself when I move it"
        // symptom.
        fixed.set_size(content_w.max(1) as u32, content_h.max(1) as u32);

        // Centre the grid in whatever space the hedge actually has. Without
        // this a hedge wider or taller than its contents pinned the icons to
        // the top-left corner instead of the middle.
        // The grid is anchored top-left. Centring it in the hedge's spare space
        // could not be done consistently: at build time the grid has no
        // allocation yet, so it laid out top-left, then jumped to centre once a
        // real allocation arrived mid-drag, then snapped back when
        // settle_position rebuilt it. Anchoring removes the jump outright.
        let cols_i = cols as i32;
        let tiles = self.0.tiles.borrow().clone();
        for (i, slot) in slots.iter().enumerate() {
            let i = i as i32;
            let (row, col) = (i / cols_i, i % cols_i);
            let x = col * (cw + sp);
            let y = row * (ch + sp);
            match slot {
                None => {
                    if let Some(ph) = self.0.placeholder.borrow().clone() {
                        fixed.move_(&ph, x, y);
                        ph.show();
                    }
                }
                // Tiles sit at the slot origin — no centring within the cell.
                // Centring there was unstable: `cell_size` measures a tile's
                // NATURAL width but Gtk::Layout allocates its MINIMUM (60px in
                // a 132px cell once a label ellipsizes). The two agree before
                // the labels are realized and diverge afterwards, so the icons
                // sat top-left until any re-layout, then shifted by half the
                // difference — the jump on drag.
                Some(ti) => self.move_tile(&tiles[*ti], *ti, x, y, animate),
            }
        }
    }

    fn move_tile(&self, tile: &gtk::Button, ti: usize, x: i32, y: i32, animate: bool) {
        if ti == 0 && std::env::var("HEDGE_DEBUG").is_ok() {
            eprintln!("hedge[{}]:   tile0 -> ({x},{y}) animate={animate}", self.name());
        }
        let fixed = match self.0.fixed.borrow().clone() {
            Some(f) => f,
            None => return,
        };
        let cur = self.0.pos.borrow().get(&ti).copied();
        let animating = self.0.anims.borrow().contains_key(&ti);
        if cur == Some((x, y)) && !animating {
            return;
        }
        if !animate || cur.is_none() {
            self.0.anims.borrow_mut().remove(&ti);
            fixed.move_(tile, x, y);
            self.0.pos.borrow_mut().insert(ti, (x, y));
            return;
        }
        let (cx, cy) = cur.unwrap();
        self.0
            .anims
            .borrow_mut()
            .insert(ti, (cx as f64, cy as f64, x as f64, y as f64, None));
        if !self.0.anim_running.get() {
            self.0.anim_running.set(true);
            let this = self.clone();
            let id = glib::timeout_add_local(Duration::from_millis(16), move || this.on_anim_step());
            *self.0.anim_id.borrow_mut() = Some(id);
        }
    }

    /// One step of the tile reflow tween.
    ///
    /// Driven by a glib timeout, not `add_tick_callback`: the compositor
    /// throttles the frame clock for obscured keep-below windows, so with any
    /// window stacked over the hedge the make-space animation crawled. Same
    /// reason `slide_dock` and the roll use a timer.
    fn on_anim_step(&self) -> glib::ControlFlow {
        let now = glib::monotonic_time();
        let fixed = match self.0.fixed.borrow().clone() {
            Some(f) => f,
            None => {
                self.0.anim_running.set(false);
                *self.0.anim_id.borrow_mut() = None;
                return glib::ControlFlow::Break;
            }
        };
        let tiles = self.0.tiles.borrow().clone();
        let keys: Vec<usize> = self.0.anims.borrow().keys().copied().collect();
        let mut finished = Vec::new();
        for ti in keys {
            let (x0, y0, x1, y1, t0opt) = *self.0.anims.borrow().get(&ti).unwrap();
            let t0 = match t0opt {
                Some(t) => t,
                None => {
                    self.0.anims.borrow_mut().get_mut(&ti).unwrap().4 = Some(now);
                    now
                }
            };
            let p = (((now - t0) as f64) / (ANIM_MS * 1000.0)).min(1.0);
            let e = 1.0 - (1.0 - p).powi(3);
            let nx = (x0 + (x1 - x0) * e).round() as i32;
            let ny = (y0 + (y1 - y0) * e).round() as i32;
            if let Some(t) = tiles.get(ti) {
                fixed.move_(t, nx, ny);
            }
            self.0.pos.borrow_mut().insert(ti, (nx, ny));
            if p >= 1.0 {
                finished.push(ti);
            }
        }
        for ti in finished {
            self.0.anims.borrow_mut().remove(&ti);
        }
        if self.0.anims.borrow().is_empty() {
            self.0.anim_running.set(false);
            *self.0.anim_id.borrow_mut() = None;
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    fn index_at(&self, x: f64, y: f64) -> usize {
        let n = self.0.tiles.borrow().len();
        if n == 0 {
            return 0;
        }
        let fixed = match self.0.fixed.borrow().clone() {
            Some(f) => f,
            None => return 0,
        };
        let (fx, fy) = fixed.translate_coordinates(&self.0.win, 0, 0).unwrap_or((0, 0));
        let sp = self.block().tile_spacing as f64;
        let cols = self.0.layout_cols.get().unwrap_or_else(|| self.grid_cols()).max(1) as i64;
        let (cw, ch) = self.cell_size();
        let col = (((x - fx as f64) / (cw + sp)).floor() as i64).clamp(0, cols - 1);
        let row = (((y - fy as f64) / (ch + sp)).floor() as i64).max(0);
        (row * cols + col).clamp(0, n as i64) as usize
    }

    /// The snap lattice for this hedge: `(base_w, base_h, inc_w, inc_h)`, where
    /// a hedge exactly `n` tiles wide is `base_w + n * inc_w` pixels.
    ///
    /// Used to *pull* an interactive resize toward whole-tile sizes, not to
    /// force it — any in-between size stays reachable. Note this deliberately
    /// isn't fed to `set_geometry_hints` as `RESIZE_INC`: those hints also
    /// constrain our own `resize()` calls, which would make in-between sizes
    /// unreachable even programmatically.
    ///
    /// Geometry: `width = 2*padding + 4 (panel border) + cols*(cell + spacing)
    /// - spacing`, which is exactly `base_w + cols * inc_w`.
    fn snap_grid(&self) -> Option<(i32, i32, i32, i32)> {
        // a shut docked hedge isn't resizable; an open one is (resize_edge_at)
        if self.0.collapsed.get() && !self.0.expanded.get() {
            return None;
        }
        // nothing to snap to until there are tiles to measure a cell from
        let fixed = self.0.fixed.borrow().clone()?;
        let b = self.block();
        let sp = b.tile_spacing.max(0) as i32;
        let (cwf, chf) = self.cell_size();
        let (inc_w, inc_h) = (cwf as i32 + sp, chf as i32 + sp);
        if inc_w <= 0 || inc_h <= 0 {
            return None;
        }
        // Derive the base from the window's natural size minus the grid size
        // `layout_tiles` actually *requests*, so that the auto-fit size is on
        // the grid by construction. Two wrong ways this was arrived at:
        // hand-computing the chrome came out 16px short, and measuring
        // `fixed.preferred_width()` is not the same number as the size request
        // we set on it — either way the hedge snapped to the next notch up and
        // carried a dead strip on its right edge.
        // The trailing `- sp` in each base folds the last cell's spacing in, so
        // that `base + n*inc` lands exactly on an n-column/row hedge.
        let pad = b.padding.max(0) as i32;
        let (base_w, base_h) = if self.0.user_resized.get() || b.width.is_some() {
            // Resized: the grid only requests a single cell, so the window's
            // natural size no longer describes the chrome. The analytic chrome
            // (body padding either side + the panel's 2px border) is exact
            // here — checked against true column widths: 2 cols = 236px.
            let chrome_w = 2 * pad + 4;
            let chrome_h = self.0.win.preferred_height().1 - chf as i32;
            ((chrome_w - sp).max(0), (chrome_h - sp).max(0))
        } else {
            // Auto-fit: the window hugs its content, so natural-size minus the
            // grid size `layout_tiles` requests *is* the chrome. Don't compute
            // it by hand here — the analytic value is 16px short in this state
            // (the GtkLayout reports more than the size request we set on it),
            // which snapped every hedge to the next notch up and left a dead
            // strip down its right edge.
            let n = self.0.tiles.borrow().len();
            let cols = self.grid_cols().max(1);
            let rows = ((n + cols - 1) / cols).max(1) as i32;
            let grid_w = cols as i32 * inc_w - sp;
            let grid_h = rows * inc_h - sp;
            (
                (self.0.win.preferred_width().1 - grid_w - sp).max(0),
                (self.0.win.preferred_height().1 - grid_h - sp).max(0),
            )
        };
        let _ = &fixed;
        Some((base_w, base_h, inc_w, inc_h))
    }

    /// The lattice to use *right now*: the one captured when the drag started,
    /// or a fresh measurement when no drag is in flight.
    fn active_snap_grid(&self) -> Option<(i32, i32, i32, i32)> {
        self.0.resize_grid.get().or_else(|| self.snap_grid())
    }

    /// Measure the lattice from the live allocation, for capture at drag start.
    ///
    /// The chrome is `window - grid`, taken from real allocations rather than
    /// preferred sizes. That matters because a resize *feeds back* into
    /// preferred size: `apply_resize` puts a size request on a docked hedge's
    /// panel, and `layout_tiles` puts one on the grid, so a lattice derived
    /// from `preferred_height()` mid-drag tracked the height being dragged.
    /// `base_h` then grew in lockstep with `h` and the row count was pinned
    /// forever at 1 — the badge read "3 × 1" however tall the hedge got.
    ///
    /// `fixed` is packed expand+fill, so its allocation is the whole content
    /// area — slack included — which keeps this exact for a hedge dragged
    /// taller than its tiles need.
    fn measure_snap_grid(&self) -> Option<(i32, i32, i32, i32)> {
        if self.0.collapsed.get() && !self.0.expanded.get() {
            return None;
        }
        let fixed = self.0.fixed.borrow().clone()?;
        let b = self.block();
        let sp = b.tile_spacing.max(0) as i32;
        let (cwf, chf) = self.cell_size();
        let (inc_w, inc_h) = (cwf as i32 + sp, chf as i32 + sp);
        if inc_w <= 0 || inc_h <= 0 {
            return None;
        }
        let gh = fixed.allocated_height();
        if gh <= 0 {
            return None; // not laid out yet — fall back to snap_grid()
        }
        // Only the height base is re-derived. `snap_grid`'s width figure is
        // calibrated against true column widths (see its comments — getting it
        // wrong leaves a dead strip down the hedge's right edge), and the
        // feedback bug is on the vertical axis, so the horizontal lattice is
        // taken from there unchanged.
        let (base_w, _, _, _) = self.snap_grid()?;
        // trailing `- sp` folds the last cell's spacing in, so that
        // `base + n * inc` lands exactly on an n-row hedge
        let base_h = (self.0.win.allocated_height() - gh - sp).max(0);
        Some((base_w, base_h, inc_w, inc_h))
    }

    /// Pull `v` onto the nearest lattice point, but only if it is already
    /// within `SNAP_TOL` — anything further away is left exactly as dragged.
    fn snap_axis(v: i32, base: i32, inc: i32) -> i32 {
        if inc <= 0 {
            return v;
        }
        let n = ((v - base) as f64 / inc as f64).round().max(1.0) as i32;
        let target = base + n * inc;
        if (v - target).abs() <= SNAP_TOL {
            target
        } else {
            v
        }
    }

    fn snap_size(&self, w: i32, h: i32) -> (i32, i32) {
        match self.active_snap_grid() {
            Some((bw, bh, iw, ih)) => (
                Self::snap_axis(w, bw, iw),
                Self::snap_axis(h, bh, ih),
            ),
            None => (w, h),
        }
    }

    fn sync_window_size(&self) {
        // a rolled-up hedge owns its own height; don't stretch it back open
        if self.0.rolled.get() && !self.0.collapsed.get() && self.0.roll.get().is_none() {
            let w = self.0.win.allocated_width().max(1);
            self.0.win.resize(w, self.header_h().max(1));
            return;
        }
        self.sync_clip_height();
        let b = self.block();
        if !self.0.collapsed.get() && b.width.is_some() && b.height.is_some() {
            self.0.win.resize(b.width.unwrap() as i32, b.height.unwrap() as i32);
        } else {
            self.0.win.resize(1, 1);
        }
    }

    // -- window move / resize ----------------------------------------------
    /// Pointer position relative to the hedge window, derived from ROOT
    /// coordinates.
    ///
    /// `event.position()` cannot be used: it is relative to whichever child
    /// GdkWindow delivered the event, and the roll clip's scrolledwindow
    /// viewport selects motion events for kinetic scrolling — so motion
    /// arrived offset by the body's origin (32px: title bar + border) while
    /// the resize test compares against the whole window. Button presses land
    /// on the EventBox and were correct, which is why resizing worked while
    /// the hover cursor never changed.
    fn win_coords(&self, ev_root: (f64, f64)) -> (f64, f64) {
        let (wx, wy) = self.0.win.position();
        (ev_root.0 - wx as f64, ev_root.1 - wy as f64)
    }

    fn resize_edge_at(&self, x: f64, y: f64) -> Option<gdk::WindowEdge> {
        // A docked hedge can be resized while it is hovered open — that is the
        // only moment its full extent is on screen to grab. Collapsed and shut,
        // the window is a sliver and the grab zones would sit off screen.
        if self.0.collapsed.get() && !self.0.expanded.get() {
            return None;
        }
        let w = self.0.win.allocated_width() as f64;
        let h = self.0.win.allocated_height() as f64;
        let m = RESIZE_MARGIN;
        let (left, right) = (x < m, x > w - m);
        let (top, bottom) = (y < m, y > h - m);
        // corners get their own, bigger catchment
        let c = RESIZE_CORNER;
        let (cleft, cright) = (x < c, x > w - c);
        let (ctop, cbottom) = (y < c, y > h - c);
        use gdk::WindowEdge::*;
        Some(if ctop && cleft {
            NorthWest
        } else if ctop && cright {
            NorthEast
        } else if cbottom && cleft {
            SouthWest
        } else if cbottom && cright {
            SouthEast
        } else if left {
            West
        } else if right {
            East
        } else if top {
            North
        } else if bottom {
            South
        } else {
            return None;
        })
    }

    fn set_cursor(&self, edge: Option<gdk::WindowEdge>) {
        let name = edge.and_then(edge_cursor);
        let name_s = name.map(str::to_string);
        if name_s == *self.0.cursor_name.borrow() {
            return;
        }
        *self.0.cursor_name.borrow_mut() = name_s;
        let Some(gdkwin) = self.0.win.window() else {
            return;
        };
        let cursor = name.and_then(|n| gdk::Cursor::from_name(&self.0.win.display(), n));
        gdkwin.set_cursor(cursor.as_ref());
    }

    fn on_press(&self, ev: &gdk::EventButton) -> glib::Propagation {
        if std::env::var("HEDGE_DEBUG").is_ok() {
            eprintln!(
                "hedge[{}]: press btn={} type={:?}",
                self.name(),
                ev.button(),
                ev.event_type()
            );
        }
        if ev.button() != 1 {
            return glib::Propagation::Proceed;
        }
        let (ex, ey) = self.win_coords(ev.root());
        if let Some(edge) = self.resize_edge_at(ex, ey) {
            self.begin_resize(edge, ev.root());
            return glib::Propagation::Stop;
        }
        let (wx, wy) = self.0.win.position();
        let (rx, ry) = ev.root();
        *self.0.press.borrow_mut() = Some(Press {
            root_x: rx,
            root_y: ry,
            win_x: wx,
            win_y: wy,
            moved: false,
        });
        glib::Propagation::Proceed
    }

    fn on_motion(&self, ev: &gdk::EventMotion) -> glib::Propagation {
        // A resize takes priority. Read the state out of the RefCell first —
        // drag_resize re-enters the window (see ARCHITECTURE.md).
        let rz: Option<ResizeState> = self
            .0
            .resize
            .borrow()
            .as_ref()
            .map(|r| (r.edge, r.root_x, r.root_y, r.win_x, r.win_y, r.w, r.h));
        if let Some(rz) = rz {
            self.drag_resize(rz, ev);
            return glib::Propagation::Stop;
        }
        let mut press = self.0.press.borrow_mut();
        let Some(p) = press.as_mut() else {
            drop(press);
            let (ex, ey) = self.win_coords(ev.root());
            let e = self.resize_edge_at(ex, ey);
            if std::env::var("HEDGE_DEBUG").is_ok() {
                eprintln!(
                    "hedge[{}]: motion at ({ex:.0},{ey:.0}) alloc={}x{} -> edge={:?}",
                    self.name(),
                    self.0.win.allocated_width(),
                    self.0.win.allocated_height(),
                    e
                );
            }
            self.set_cursor(e);
            return glib::Propagation::Proceed;
        };
        let (rx, ry) = ev.root();
        let dx = rx - p.root_x;
        let dy = ry - p.root_y;
        if !p.moved && (dx.abs() > DRAG_SLOP || dy.abs() > DRAG_SLOP) {
            p.moved = true;
            // the drag is real now: show any hidden hedges so the user can see
            // what they are about to bump into
            let app = self.0.app.clone();
            glib::idle_add_local_once(move || app.reveal_hidden_hedges(true));
        }
        if p.moved {
            // hedges never stack: slide around anything already there
            let (nx, ny) = self.avoid_overlap(p.win_x + dx as i32, p.win_y + dy as i32);
            self.0.win.move_(nx, ny);
        }
        glib::Propagation::Proceed
    }

    /// One step of a self-driven resize: turn the pointer delta into a new
    /// geometry, pull it toward whole-tile sizes, and apply it.
    fn drag_resize(&self, rz: ResizeState, ev: &gdk::EventMotion) {
        use gdk::WindowEdge::*;
        let (edge, rx0, ry0, wx0, wy0, w0, h0) = rz;
        let (rx, ry) = ev.root();
        let (dx, dy) = ((rx - rx0) as i32, (ry - ry0) as i32);

        let (mut x, mut y) = (wx0, wy0);
        let (mut w, mut h) = (w0, h0);
        match edge {
            NorthWest | West | SouthWest => {
                w = w0 - dx;
                x = wx0 + dx;
            }
            NorthEast | East | SouthEast => w = w0 + dx,
            _ => {}
        }
        match edge {
            NorthWest | North | NorthEast => {
                h = h0 - dy;
                y = wy0 + dy;
            }
            SouthWest | South | SouthEast => h = h0 + dy,
            _ => {}
        }

        let (sw, sh) = self.snap_size(w, h);
        // Floor the size before the origin is derived from it. For a left or top
        // drag the origin *is* the anchored edge minus the size, so the moment
        // the two disagree the anchored edge walks away from where it should be
        // standing still.
        //
        // The floor is one whole tile, taken from the snap lattice captured at
        // press. Explicitly NOT `preferred_width`: for a docked hedge this very
        // function puts a size request on the panel each motion, so the reported
        // minimum would ratchet up to whatever width was last applied and the
        // hedge could then only ever be widened, never narrowed.
        let (min_w, min_h) = match self.active_snap_grid() {
            Some((bw, bh, iw, ih)) => ((bw + iw).max(1), (bh + ih).max(1)),
            None => (1, 1),
        };
        let (sw, sh) = (sw.max(min_w), sh.max(min_h));
        // when dragging a top/left edge the opposite edge is anchored, so any
        // pull the snap applied has to come back out of the origin
        if matches!(edge, NorthWest | West | SouthWest) {
            x += w - sw;
        }
        if matches!(edge, NorthWest | North | NorthEast) {
            y += h - sh;
        }
        let (mut w, mut h) = (sw, sh);
        // then line the dragged edges up with any neighbouring hedge
        let others = self.0.app.other_hedge_rects(&self.name());
        let (nx, ny, nw, nh) = snap_to_neighbours(
            (
                matches!(edge, NorthWest | West | SouthWest),
                matches!(edge, NorthEast | East | SouthEast),
                matches!(edge, NorthWest | North | NorthEast),
                matches!(edge, SouthWest | South | SouthEast),
            ),
            x,
            y,
            w,
            h,
            &others,
        );
        let (x, y, sw, sh) = (nx, ny, nw, nh);
        let _ = (&mut w, &mut h);

        // The anchored corner is held by the *window manager*, via the gravity
        // set when the drag started, and this asks only for a size. Nothing here
        // moves the window, which is the whole point: any move is a second
        // operation, and the window manager applies the two in its own order.
        // Measured under Muffin, moving and sizing together — even as a single
        // XMoveResizeWindow — still surfaced frames carrying the new origin with
        // the *old* width, putting the edge that should be standing still 60px
        // out before the next frame corrected it. That is the jitter. With
        // NORTH_EAST gravity a bare resize keeps the top-right corner exactly
        // where it is, in one ConfigureNotify, so there is no wrong frame to see.
        //
        // `nx`/`ny` are still computed above because the neighbour snap works in
        // absolute edges; for a left or top drag it expresses its pull as a width
        // or height, which is what actually gets applied.
        let (sw, sh) = (sw.max(1), sh.max(1));
        let _ = (x, y);
        self.0.win.resize(sw, sh);
        // A docked hedge's content does NOT follow the window on its own: the
        // panel and its clipping layout are sized from `panel_size`, fixed at
        // render time. Without this the window grew while the visible panel
        // stayed put, so the resize looked like it did nothing at all.
        if self.0.collapsed.get() {
            if let (Some(dock), Some(panel)) = (
                self.0.dock_layout.borrow().clone(),
                self.0.dock_panel.borrow().clone(),
            ) {
                panel.set_size_request(sw.max(1), sh.max(1));
                dock.set_size_request(sw.max(1), sh.max(1));
                self.0.panel_size.set((sw.max(1), sh.max(1)));
            }
        }
        self.show_size_badge(sw, sh);
    }

    /// Show the "3 × 2" readout in the middle of the hedge, so the size being
    /// dragged is expressed in icon slots rather than pixels.
    fn show_size_badge(&self, w: i32, h: i32) {
        let Some((bw, bh, iw, ih)) = self.active_snap_grid() else {
            return;
        };
        let Some(fixed) = self.0.fixed.borrow().clone() else {
            return;
        };
        // Floor, not round, and the same base the lattice uses — which makes this
        // arithmetic identical to `grid_cols`. Rounding up meant a hedge 2.7
        // cells wide was announced as "3" while the layout, flooring, laid out
        // 2: the badge promised a column that never arrived.
        let cols = (((w - bw) as f64 / iw as f64).floor() as i32).max(1);
        let rows = (((h - bh) as f64 / ih as f64).floor() as i32).max(1);
        if std::env::var("HEDGE_DEBUG").is_ok() {
            log!("badge w={w} h={h} base=({bw},{bh}) inc=({iw},{ih}) -> {cols}x{rows}");
        }

        if self.0.badge.borrow().is_none() {
            let lbl = gtk::Label::new(None);
            lbl.style_context().add_class("hedge-badge");
            lbl.set_no_show_all(true);
            fixed.put(&lbl, 0, 0); // added after the tiles, so it draws on top
            *self.0.badge.borrow_mut() = Some(lbl);
        }
        let Some(lbl) = self.0.badge.borrow().clone() else {
            return;
        };
        lbl.set_text(&format!("{cols} × {rows}"));
        lbl.show();
        // centre it on the grid area using its own natural size
        let (lw, lh) = (lbl.preferred_width().1, lbl.preferred_height().1);
        let x = ((fixed.allocated_width() - lw) / 2).max(0);
        let y = ((fixed.allocated_height() - lh) / 2).max(0);
        fixed.move_(&lbl, x, y);
    }

    fn hide_size_badge(&self) {
        if let Some(lbl) = self.0.badge.borrow().clone() {
            lbl.hide();
        }
    }

    fn pointer_inside(&self) -> bool {
        let display = self.0.win.display();
        let Some(device) = display.default_seat().and_then(|s| s.pointer()) else {
            return false;
        };
        let (_s, px, py) = device.position();
        let (x, y) = self.0.win.position();
        let (w, h) = (self.0.win.allocated_width(), self.0.win.allocated_height());
        px >= x && px < x + w && py >= y && py < y + h
    }

    /// Re-evaluate the hover highlight after a drag, since `leave-notify` is
    /// suppressed while one is in flight.
    fn refresh_hover_class(&self) {
        let inside = self.pointer_inside();
        let ctx = self.0.win.style_context();
        if inside {
            ctx.add_class("hedge-hover");
        } else {
            ctx.remove_class("hedge-hover");
        }
    }

    fn on_release(&self, _ev: &gdk::EventButton) -> glib::Propagation {
        if self.0.resize.borrow_mut().take().is_some() {
            self.0.app.reveal_hidden_hedges(false);
            // back to top-left semantics before anything moves the window again
            self.0.win.set_gravity(gdk::Gravity::NorthWest);
            self.0.resize_grid.set(None); // next drag measures afresh
            self.hide_size_badge();
            // re-seat a docked hedge against its edge at the new size
            if self.0.collapsed.get() {
                self.0.vis_cache.set(0);
                self.snap_to_edge();
            }
            // Reflow to the size just settled on. `on_configure` only re-lays out
            // when the column count *changes*, and mid-drag it compares against a
            // config width that is still catching up, so a hedge could come to
            // rest still laid out for its previous width — with its last column
            // hanging off the right edge, clipped.
            self.layout_tiles(self.0.hover_index.get(), false);
            self.refresh_hover_class();
            // The pointer is often outside the hedge by the time a resize ends,
            // and X sends no further leave event from out there. The hover watch
            // covers a docked hedge (it polls, and was only held off during the
            // drag); a hide-until-hover one needs its fade asking for again.
            if self.block().hide_until_hover {
                self.schedule_hide();
            }
            self.0.app.save(); // on_configure already recorded the geometry
            return glib::Propagation::Proceed;
        }
        let press = self.0.press.borrow_mut().take();
        let Some(p) = press else {
            return glib::Propagation::Proceed;
        };
        if p.moved {
            self.0.app.reveal_hidden_hedges(false);
            self.settle_position();
        } else if self.0.collapsed.get() && !self.block().expand_on_hover {
            self.toggle_expand();
        }
        glib::Propagation::Proceed
    }

    /// Adjust a proposed position so this hedge doesn't cover another one.
    /// Docked hedges count too — via their visible sliver, see
    /// [`collision_rect`](Self::collision_rect).
    fn avoid_overlap(&self, x: i32, y: i32) -> (i32, i32) {
        let (_, _, w, h) = self.rect();
        let others = self.0.app.other_hedge_rects(&self.name());
        let (nx, ny) = resolve_overlap(x, y, w, h, &others);
        // Keep the result on screen. The resolver walks a hedge out of every
        // collision it finds and has no idea where the monitor ends, so two
        // tall overlapping hedges could push one clean off the desktop — it
        // simply vanished. Always leave enough on screen to grab.
        let m = self.monitor_geom();
        const KEEP: i32 = 80;
        (
            nx.clamp(m.x() - (w - KEEP).max(0), m.x() + m.width() - KEEP),
            ny.clamp(m.y(), m.y() + m.height() - KEEP),
        )
    }

    fn settle_position(&self) {
        let (x, y) = self.0.win.position();
        let w = self.0.win.allocated_width();
        let h = self.0.win.allocated_height();
        let mon = self.monitor_geom();
        let edge = choose_edge(x, y, w, h, mon.x(), mon.y(), mon.width(), mon.height());

        // Nudge clear of other hedges only when staying floating. Doing it
        // before the edge test pushed a hedge into the snap zone and docked it
        // when the user had only dropped it next to a neighbour; docked hedges
        // are excluded from collision anyway.
        let (x, y) = if edge.is_none() {
            let (nx, ny) = self.avoid_overlap(x, y);
            if (nx, ny) != (x, y) {
                self.0.win.move_(nx, ny);
            }
            (nx, ny)
        } else {
            (x, y)
        };

        *self.0.edge.borrow_mut() = edge.map(str::to_string);
        self.0.collapsed.set(edge.is_some());
        self.0.expanded.set(false);
        let name = self.name();
        self.0.app.with_hedge_mut(&name, |f| {
            f.edge = edge.map(str::to_string);
            f.x = x as i64;
            f.y = y as i64;
        });
        self.0.app.save();
        self.render();
        if self.0.collapsed.get() {
            self.show_handle();
        }
    }

    /// Full monitor rectangle, panels included (unlike `monitor_geom`, which
    /// is the work area).
    fn monitor_full(&self) -> gdk::Rectangle {
        let display = self.0.win.display();
        let monitor = self
            .0
            .win
            .window()
            .and_then(|w| display.monitor_at_window(&w))
            .or_else(|| display.primary_monitor());
        match monitor {
            Some(m) => m.geometry(),
            None => gdk::Rectangle::new(0, 0, 1920, 1080),
        }
    }

    fn monitor_geom(&self) -> gdk::Rectangle {
        let display = self.0.win.display();
        let monitor = self
            .0
            .win
            .window()
            .and_then(|w| display.monitor_at_window(&w))
            .or_else(|| display.primary_monitor());
        match monitor {
            Some(m) => m.workarea(),
            None => gdk::Rectangle::new(0, 0, 1920, 1080),
        }
    }

    // -- collapse / expand --------------------------------------------------
    fn show_handle(&self) {
        self.0.expanded.set(false);
        self.render();
        let this = self.clone();
        glib::idle_add_local_once(move || this.snap_to_edge());
    }

    fn toggle_expand(&self) {
        if self.0.expanded.get() {
            self.collapse();
        } else {
            self.expand();
        }
    }

    fn expand(&self) {
        if !self.0.collapsed.get() || self.0.expanded.get() {
            return;
        }
        self.0.expanded.set(true);
        self.undim();
        self.slide_dock(true);
        if self.block().expand_on_hover {
            self.start_hover_watch();
        }
    }

    fn collapse(&self) {
        if !self.0.collapsed.get() || !self.0.expanded.get() {
            return;
        }
        self.0.expanded.set(false);
        self.slide_dock(true);
        self.schedule_dim();
    }

    /// Take the press as the start of an interactive resize on `edge`.
    ///
    /// Driven here rather than handed to the WM via `begin_resize_drag`: we need
    /// to pull the size toward whole-tile multiples *during* the drag, and a
    /// client can't influence the WM's resize loop. Motion keeps arriving even
    /// outside the window thanks to X11's implicit pointer grab while a button is
    /// held (same as the move drag).
    ///
    /// Called from the hedge's own press handler and from a *tile's*: a tile sits
    /// under the grab zone at the edges of a small hedge, and being a drag source
    /// it would otherwise swallow the press and start dragging itself out — while
    /// the cursor was showing a resize arrow.
    fn begin_resize(&self, edge: gdk::WindowEdge, root: (f64, f64)) {
        // capture the lattice while the widget tree is still in its pre-drag
        // state: setting user_resized below switches which branch snap_grid
        // takes, and the drag then perturbs the sizes it reads
        self.0.resize_grid.set(self.measure_snap_grid());
        // Hand the anchoring to the window manager for the duration of the drag;
        // `on_release` puts it back, because every other bit of positioning here
        // (docking, settling, the rescue move) means the top-left corner when it
        // says a position.
        self.0.win.set_gravity(Self::resize_gravity(edge));
        self.0.user_resized.set(true);
        self.0.app.reveal_hidden_hedges(true);
        let (wx, wy) = self.0.win.position();
        *self.0.resize.borrow_mut() = Some(Resize {
            edge,
            root_x: root.0,
            root_y: root.1,
            win_x: wx,
            win_y: wy,
            w: self.0.win.allocated_width(),
            h: self.0.win.allocated_height(),
        });
    }

    /// Which corner a drag on `edge` must leave standing still, as the window
    /// gravity that pins it.
    ///
    /// Gravity is how a window manager is told "when this window's size changes,
    /// keep *that* corner put". Dragging a left edge is therefore expressed as a
    /// resize with the top-right pinned, never as a move plus a resize — see
    /// `drag_resize` for what the two-operation version looks like on screen.
    fn resize_gravity(edge: gdk::WindowEdge) -> gdk::Gravity {
        use gdk::WindowEdge::*;
        match edge {
            // left and top move: hold the bottom-right
            NorthWest => gdk::Gravity::SouthEast,
            // top moves, left does not: hold the bottom-left
            North | NorthEast => gdk::Gravity::SouthWest,
            // left moves, top does not: hold the top-right
            West | SouthWest => gdk::Gravity::NorthEast,
            // the origin stays put, so the default already anchors it
            _ => gdk::Gravity::NorthWest,
        }
    }

    /// Is the user in the middle of an interactive move or resize?
    ///
    /// A drag takes the pointer outside the window as a matter of course —
    /// dragging a left or top edge *inwards* moves the frame away from a pointer
    /// that has not moved, so it ends up outside by design. Everything that
    /// reacts to the pointer being gone has to hold still until the drag is
    /// over, or the hedge rolls up (or fades out) from under the hand resizing
    /// it, taking the edge being dragged with it.
    fn dragging(&self) -> bool {
        self.0.resize.borrow().is_some()
            || matches!(&*self.0.press.borrow(), Some(p) if p.moved)
    }

    fn start_hover_watch(&self) {
        if self.0.watch_id.borrow().is_none() {
            let this = self.clone();
            let id = glib::timeout_add_local(Duration::from_millis(120), move || this.check_pointer());
            *self.0.watch_id.borrow_mut() = Some(id);
        }
    }

    fn check_pointer(&self) -> glib::ControlFlow {
        if !(self.0.collapsed.get() && self.0.expanded.get()) {
            *self.0.watch_id.borrow_mut() = None;
            return glib::ControlFlow::Break;
        }
        // Keep watching without collapsing while a drag is in flight; the poll
        // resumes deciding once the button comes up, so a resize that ends with
        // the pointer away from the hedge still rolls it up on the next tick.
        if self.dragging() || self.pointer_near() {
            if let Some(id) = self.0.collapse_timer.borrow_mut().take() {
                id.remove();
            }
            return glib::ControlFlow::Continue;
        }
        *self.0.watch_id.borrow_mut() = None;
        self.collapse();
        glib::ControlFlow::Break
    }

    fn pointer_near(&self) -> bool {
        let display = self.0.win.display();
        let Some(device) = display.default_seat().and_then(|s| s.pointer()) else {
            return false;
        };
        let (_s, px, py) = device.position();
        let (x, y) = self.0.win.position();
        let w = self.0.win.allocated_width();
        let h = self.0.win.allocated_height();
        let m = self.monitor_full();
        let pad = HOVER_MARGIN;
        let mut x0 = x - pad;
        let mut y0 = y - pad;
        let mut x1 = x + w + pad;
        let mut y1 = y + h + pad;
        // On the docked edge, reach all the way to the physical screen edge —
        // not the work area, which stops below a panel and leaves a dead strip
        // exactly where the pointer lands when you slam it into that edge.
        match self.0.edge.borrow().as_deref() {
            Some("left") => x0 = m.x(),
            Some("right") => x1 = m.x() + m.width(),
            Some("top") => y0 = m.y(),
            Some("bottom") => y1 = m.y() + m.height(),
            _ => {}
        }
        x0 <= px && px <= x1 && y0 <= py && py <= y1
    }

    /// How much of the docked panel stays on screen: the title bar's own size.
    ///
    /// The clamp has to be applied *before* caching. It used to be applied only
    /// to the freshly-measured return value while the raw number went into the
    /// cache, so a bar that measured under the minimum reported 18px on the
    /// first call and the smaller raw value on every call after — the docked
    /// title bar visibly shrank once something recomputed it.
    fn bar_vis(&self) -> i32 {
        if self.0.vis_cache.get() != 0 {
            return self.0.vis_cache.get();
        }
        let bar = match self.0.bar.borrow().clone() {
            Some(b) => b,
            None => return 24,
        };
        // Exactly the handle's own size — no padding. The old `+ 2` allowed for
        // the panel's 2px border, which no longer exists (the highlight is an
        // inset shadow now), so those two pixels showed bare panel background
        // between the handle and the screen edge: a dark line along the edge.
        let vis = if matches!(self.0.edge.borrow().as_deref(), Some("left") | Some("right")) {
            bar.preferred_width().1
        } else {
            bar.preferred_height().1
        };
        let vis = vis.max(18);
        if std::env::var("HEDGE_DEBUG").is_ok() {
            eprintln!(
                "hedge[{}]: bar_vis measured {vis} (edge={:?}, mapped={}, realized={})",
                self.name(),
                self.0.edge.borrow(),
                self.0.win.is_mapped(),
                bar.is_realized()
            );
        }
        if vis > 4 {
            self.0.vis_cache.set(vis);
        }
        vis
    }

    fn dock_offset(&self) -> (i32, i32) {
        if self.0.expanded.get() || self.0.edge.borrow().is_none() {
            return (0, 0);
        }
        let (w, h) = self.0.panel_size.get();
        let vis = self.bar_vis();
        match self.0.edge.borrow().as_deref() {
            Some("left") => (-(w - vis), 0),
            Some("right") => (w - vis, 0),
            Some("top") => (0, -(h - vis)),
            Some("bottom") => (0, h - vis),
            _ => (0, 0),
        }
    }

    fn move_panel(&self, dx: i32, dy: i32) {
        if let (Some(dock), Some(panel)) =
            (self.0.dock_layout.borrow().clone(), self.0.dock_panel.borrow().clone())
        {
            dock.move_(&panel, dx, dy);
        }
        self.0.panel_pos.set((dx, dy));
        self.update_input_shape(dx, dy);
    }

    fn update_input_shape(&self, dx: i32, dy: i32) {
        let Some(gdkwin) = self.0.win.window() else {
            return;
        };
        let (w, h) = self.0.panel_size.get();
        let x0 = dx.max(0);
        let y0 = dy.max(0);
        let x1 = (dx + w).min(w);
        let y1 = (dy + h).min(h);
        let rect = cairo::RectangleInt::new(x0, y0, (x1 - x0).max(1), (y1 - y0).max(1));
        let region = cairo::Region::create_rectangle(&rect);
        gdkwin.input_shape_combine_region(&region, 0, 0);
    }

    /// Make the whole window clickable again (undo `update_input_shape`).
    ///
    /// Deliberately an *unbounded* region rather than the current allocation.
    /// X intersects the input shape with the window's bounds, so an oversized
    /// region means "all of it" no matter how the window is later resized.
    /// Sizing it to `allocated_width/height` here was a latent bug: this runs
    /// only from `render()`, so after any resize the shape kept the old
    /// rectangle and every pixel outside it went click-through — the hedge
    /// stopped responding. (gdk's NULL-unsets-the-shape needs an `Option` the
    /// 0.18 binding doesn't expose, and it isn't worth `unsafe` here.)
    fn clear_input_shape(&self) {
        if let Some(gdkwin) = self.0.win.window() {
            // 32767 = X11's signed 16-bit coordinate limit
            let rect = cairo::RectangleInt::new(0, 0, 32767, 32767);
            gdkwin.input_shape_combine_region(&cairo::Region::create_rectangle(&rect), 0, 0);
        }
    }

    fn slide_dock(&self, animate: bool) {
        if self.0.dock_layout.borrow().is_none() || self.0.dock_panel.borrow().is_none() {
            return;
        }
        let (tx, ty) = self.dock_offset();
        if !animate {
            self.0.slide.set(None);
            self.move_panel(tx, ty);
            return;
        }
        if let Some(s) = self.0.slide.get() {
            if (s.2 as i32, s.3 as i32) == (tx, ty) {
                return; // already tweening to this target
            }
        }
        let (cx, cy) = self.0.panel_pos.get();
        if (cx, cy) == (tx, ty) {
            return;
        }
        // driven by a GLib timer, NOT the frame clock — the compositor throttles
        // paint clocks for obscured keep-below windows, freezing tick tweens
        self.0
            .slide
            .set(Some((cx as f64, cy as f64, tx as f64, ty as f64, glib::monotonic_time())));
        if self.0.slide_id.borrow().is_none() {
            let this = self.clone();
            let id = glib::timeout_add_local(Duration::from_millis(16), move || this.on_slide_step());
            *self.0.slide_id.borrow_mut() = Some(id);
        }
    }

    fn on_slide_step(&self) -> glib::ControlFlow {
        let s = match self.0.slide.get() {
            Some(s) => s,
            None => {
                *self.0.slide_id.borrow_mut() = None;
                return glib::ControlFlow::Break;
            }
        };
        let now = glib::monotonic_time();
        let p = (((now - s.4) as f64) / (SLIDE_MS * 1000.0)).min(1.0);
        let e = 1.0 - (1.0 - p).powi(3);
        let nx = (s.0 + (s.2 - s.0) * e).round() as i32;
        let ny = (s.1 + (s.3 - s.1) * e).round() as i32;
        self.move_panel(nx, ny);
        if p >= 1.0 {
            self.0.slide.set(None);
            *self.0.slide_id.borrow_mut() = None;
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    fn snap_to_edge(&self) {
        if !self.0.collapsed.get() {
            return;
        }
        let edge = self.0.edge.borrow().clone();
        let Some(edge) = edge else {
            return;
        };
        let mon = self.monitor_geom();
        let (w, h) = self.0.panel_size.get();
        // The saved position, NOT `win.position()`. At login this runs from an
        // idle that can fire before the window manager has processed our map
        // and move, and X then reports the pre-move geometry — usually (0,0).
        // Every docked hedge would read the same bogus coordinate, snap to the
        // same spot, and come back stacked on top of its neighbours. The
        // config is the one value that is right whatever the WM is doing.
        let b = self.block();
        let (mut x, mut y) = (b.x as i32, b.y as i32);
        // Clamp along the edge as well as across it: a hedge taller than the
        // screen would otherwise dock with its handle hanging off the top or
        // bottom, out of reach.
        let cx = |v: i32| v.clamp(mon.x(), (mon.x() + mon.width() - w).max(mon.x()));
        let cy = |v: i32| v.clamp(mon.y(), (mon.y() + mon.height() - h).max(mon.y()));
        match edge.as_str() {
            "left" => {
                x = mon.x();
                y = cy(y);
            }
            "right" => {
                x = mon.x() + mon.width() - w;
                y = cy(y);
            }
            "top" => {
                y = mon.y();
                x = cx(x);
            }
            _ => {
                y = mon.y() + mon.height() - h;
                x = cx(x);
            }
        }
        self.0.win.move_(x, y);
        self.slide_dock(false);
    }

    fn on_size_allocate(&self, w: i32, h: i32) {
        if !self.0.collapsed.get() {
            return;
        }
        let size = (w, h);
        if size == self.0.last_alloc.get() {
            return;
        }
        self.0.last_alloc.set(size);
        // Never re-seat mid-drag. A resize changes the allocation on every
        // motion, so this fired continuously and each `snap_to_edge` yanked the
        // window back to the saved position while the drag pulled it away again
        // — a docked hedge dragged by its left edge juddered the whole way and
        // ended up back where it started. `on_release` re-seats once the drag is
        // over, which is the only moment the resting place is knowable.
        if self.0.slide.get().is_none() && !self.dragging() {
            let this = self.clone();
            glib::idle_add_local_once(move || this.snap_to_edge());
        }
    }

    fn on_configure(&self, ev_w: i32, ev_h: i32) {
        // a docked hedge only tracks geometry while open (see resize_edge_at)
        if self.0.collapsed.get() && !self.0.expanded.get() {
            return;
        }
        let b = self.block();
        if !(self.0.user_resized.get() || b.width.is_some()) {
            return;
        }
        // The event's own size, not `allocated_width`: the allocation trails the
        // ConfigureNotify by a cycle, so reading it here recorded the *previous*
        // size — and since that matched what was already stored, nothing ever
        // corrected it. `grid_cols` reads this width, so the hedge went on being
        // laid out for the width before last, leaving its final column clipped
        // off the edge.
        let w = ev_w as i64;
        let h = ev_h as i64;
        let (x, y) = self.0.win.position();
        let name = self.name();
        let mut changed = false;
        // A move must never rewrite the size. configure-event fires for both,
        // and recording a size here during a drag-move would change what
        // `grid_cols()` computes, tripping the reflow check below and sliding
        // every icon while the user is only repositioning the hedge.
        let moving = self.0.press.borrow().is_some() && self.0.resize.borrow().is_none();
        if !moving && (Some(w) != b.width || Some(h) != b.height) {
            self.0.app.with_hedge_mut(&name, |f| {
                f.width = Some(w);
                f.height = Some(h);
            });
            changed = true;
        }
        // A docked hedge's resting place is derived from its edge, so live
        // geometry is never worth persisting for one. Hovering it open used to
        // land here and commit whatever position the window happened to be at
        // — which, after a bad snap at login, is how a wrong position became
        // the saved one.
        //
        // A resize in flight is the exception: dragging a docked hedge's left or
        // top edge sets its position *along* the edge, and that is the user's
        // intent, not incidental live geometry. `snap_to_edge` reads this value
        // back when the drag ends, so leaving it stale is what made a docked
        // hedge spring back to its old position — keeping the new width, so the
        // hedge appeared to grow out of its far side instead.
        let resizing = self.0.resize.borrow().is_some();
        if (!self.0.collapsed.get() || resizing) && (x as i64, y as i64) != (b.x, b.y) {
            self.0.app.with_hedge_mut(&name, |f| {
                f.x = x as i64;
                f.y = y as i64;
            });
            changed = true;
        }
        if changed {
            self.queue_geom_save();
        }
        if Some(self.grid_cols()) != self.0.layout_cols.get() {
            // Snap, don't animate, while the user is dragging an edge: the
            // column count can change several times a second, and a 140ms ease
            // started on each one leaves the tiles permanently in flight —
            // reading as a flicker in the middle of the hedge. The animation is
            // for reflows the user didn't ask for frame by frame.
            self.layout_tiles(self.0.hover_index.get(), !self.dragging());
        }
    }

    fn queue_geom_save(&self) {
        if let Some(id) = self.0.geom_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(400), move || {
            *this.0.geom_timer.borrow_mut() = None;
            this.0.app.save();
            glib::ControlFlow::Break
        });
        *self.0.geom_timer.borrow_mut() = Some(id);
    }

    fn on_destroy(&self) {
        for slot in [
            &self.0.collapse_timer,
            &self.0.hover_timer,
            &self.0.hide_timer,
            &self.0.geom_timer,
            &self.0.watch_id,
            &self.0.dim_timer,
            &self.0.dim_fade_id,
            &self.0.slide_id,
            &self.0.roll_id,
            &self.0.anim_id,
        ] {
            if let Some(id) = slot.borrow_mut().take() {
                id.remove();
            }
        }
    }

    // -- hover / dim --------------------------------------------------------
    /// Hovering the title bar of a rolled-up hedge opens it, and holds it open
    /// for `OPEN_HOLD_SECS` so there's time to actually click something. The
    /// countdown restarts while the pointer is still inside, so the hedge never
    /// rolls up out from under the user mid-interaction.
    fn hover_open(&self) {
        if !self.0.rolled.get() || self.0.collapsed.get() || !self.block().open_on_hover {
            return;
        }
        self.set_rolled(false, false);
        self.arm_hold_timer();
    }

    fn arm_hold_timer(&self) {
        if let Some(id) = self.0.hold_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_seconds_local(OPEN_HOLD_SECS as u32, move || {
            *this.0.hold_timer.borrow_mut() = None;
            if this.pointer_inside() {
                this.arm_hold_timer(); // still in use — give them another window
            } else if this.block().open_on_hover {
                this.set_rolled(true, false);
            }
            glib::ControlFlow::Break
        });
        *self.0.hold_timer.borrow_mut() = Some(id);
    }

    fn on_enter(&self, ev: &gdk::EventCrossing) -> glib::Propagation {
        if ev.detail() == gdk::NotifyType::Inferior {
            return glib::Propagation::Proceed;
        }
        self.0.win.style_context().add_class("hedge-hover");
        if let Some(id) = self.0.hide_timer.borrow_mut().take() {
            id.remove();
        }
        if self.block().hide_until_hover {
            self.fade_to(1.0);
        }
        self.hover_open();
        self.undim();
        if let Some(id) = self.0.collapse_timer.borrow_mut().take() {
            id.remove();
        }
        if self.0.collapsed.get() && self.block().expand_on_hover {
            self.expand();
        }
        glib::Propagation::Proceed
    }

    fn on_leave(&self, ev: &gdk::EventCrossing) -> glib::Propagation {
        if ev.detail() == gdk::NotifyType::Inferior {
            return glib::Propagation::Proceed;
        }
        // Keep the highlight lit while a move or resize is in flight — the
        // pointer routinely leaves the window during one, and losing the
        // border mid-resize hides the thing you're aiming with.
        if self.dragging() {
            return glib::Propagation::Proceed;
        }
        self.set_cursor(None);
        // Pushing the pointer into the screen edge on top of a docked hedge is
        // still hovering it — the window may start below a panel, so the last
        // few pixels are outside it. `pointer_near` covers that (and a margin
        // all round), so hold the highlight and re-check shortly rather than
        // dropping the border the moment the pointer crosses the frame.
        if self.pointer_near() {
            self.arm_leave_recheck();
            return glib::Propagation::Proceed;
        }
        self.0.win.style_context().remove_class("hedge-hover");
        if self.block().hide_until_hover {
            self.schedule_hide();
        }
        if self.0.collapsed.get() && self.block().expand_on_hover {
            self.arm_leave_recheck();
        }
        glib::Propagation::Proceed
    }

    fn arm_leave_recheck(&self) {
        if let Some(id) = self.0.collapse_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(LEAVE_GRACE), move || {
            this.leave_timeout()
        });
        *self.0.collapse_timer.borrow_mut() = Some(id);
    }

    fn leave_timeout(&self) -> glib::ControlFlow {
        *self.0.collapse_timer.borrow_mut() = None;
        // Still in reach, or mid-drag: keep the highlight and look again shortly.
        if self.dragging() || self.pointer_near() {
            self.arm_leave_recheck();
            return glib::ControlFlow::Break;
        }
        self.0.win.style_context().remove_class("hedge-hover");
        if self.block().hide_until_hover {
            self.schedule_hide();
        }
        self.collapse();
        glib::ControlFlow::Break
    }

    /// Fade a hide-until-hover hedge back out, but only after a grace period
    /// and only if the pointer really has gone. `pointer_near` allows slack
    /// around the window and out to the screen edge, so overshooting while
    /// reaching for the hedge doesn't make it vanish under the cursor.
    fn schedule_hide(&self) {
        if let Some(id) = self.0.hide_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(LEAVE_GRACE), move || {
            *this.0.hide_timer.borrow_mut() = None;
            if this.dragging() || this.pointer_near() {
                this.schedule_hide(); // still in reach, or mid-drag — look again
            } else {
                this.fade_to(0.0);
            }
            glib::ControlFlow::Break
        });
        *self.0.hide_timer.borrow_mut() = Some(id);
    }

    fn schedule_dim(&self) {
        if let Some(id) = self.0.dim_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(2000), move || this.apply_dim());
        *self.0.dim_timer.borrow_mut() = Some(id);
    }

    fn apply_dim(&self) -> glib::ControlFlow {
        *self.0.dim_timer.borrow_mut() = None;
        if self.0.collapsed.get() && !self.0.expanded.get() {
            self.0.win.style_context().add_class("hedge-dim");
            // a hide-until-hover hedge goes all the way out, not just dim
            let target = if self.block().hide_until_hover {
                0.0
            } else {
                DIM_ALPHA
            };
            self.fade_to(target);
        }
        glib::ControlFlow::Break
    }

    fn undim(&self) {
        if let Some(id) = self.0.dim_timer.borrow_mut().take() {
            id.remove();
        }
        self.0.win.style_context().remove_class("hedge-dim");
        // back to full straight away — waking up should feel instant
        self.0.dim_fade.set(None);
        if let Some(id) = self.0.dim_fade_id.borrow_mut().take() {
            id.remove();
        }
        self.0.win.set_opacity(1.0);
    }

    /// What this hedge's opacity should settle to when the pointer is away.
    fn resting_opacity(&self) -> f64 {
        let b = self.block();
        if b.hide_until_hover {
            0.0
        } else if self.0.collapsed.get() && !self.0.expanded.get() && self.is_dimmed() {
            DIM_ALPHA
        } else {
            1.0
        }
    }

    fn is_dimmed(&self) -> bool {
        self.0.win.style_context().has_class("hedge-dim")
    }

    /// Settle to the resting opacity without animating — used after a rebuild.
    fn apply_resting_opacity(&self) {
        if self.pointer_inside() {
            return;
        }
        let target = self.resting_opacity();
        if (self.0.win.opacity() - target).abs() > 0.01 {
            self.0.win.set_opacity(target);
        }
    }

    /// Fade the whole window rather than a single widget.
    ///
    /// A CSS `opacity` on `.hedge-dockbar` composited only that widget, so the
    /// panel edge behind it stayed at full strength and drew a bright ring
    /// around the handle's rounded corner. Fading the toplevel composites the
    /// whole hedge once, so nothing can show through it.
    fn fade_to(&self, target: f64) {
        let from = self.0.win.opacity();
        if (from - target).abs() < 0.01 {
            return;
        }
        self.0
            .dim_fade
            .set(Some((from, target, glib::monotonic_time())));
        if self.0.dim_fade_id.borrow().is_none() {
            let this = self.clone();
            let id = glib::timeout_add_local(Duration::from_millis(16), move || this.on_fade_step());
            *self.0.dim_fade_id.borrow_mut() = Some(id);
        }
    }

    fn on_fade_step(&self) -> glib::ControlFlow {
        let Some((from, to, t0)) = self.0.dim_fade.get() else {
            *self.0.dim_fade_id.borrow_mut() = None;
            return glib::ControlFlow::Break;
        };
        let p = (((glib::monotonic_time() - t0) as f64) / (DIM_MS * 1000.0)).min(1.0);
        let e = 1.0 - (1.0 - p).powi(3);
        self.0.win.set_opacity(from + (to - from) * e);
        if p >= 1.0 {
            self.0.dim_fade.set(None);
            *self.0.dim_fade_id.borrow_mut() = None;
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    // -- roll up / down (window shade) --------------------------------------
    /// Natural height of the title bar; the hedge rolls down to exactly this.
    fn header_h(&self) -> i32 {
        self.0
            .header
            .borrow()
            .clone()
            .map(|h| h.preferred_height().1)
            .unwrap_or(0)
    }

    /// Natural height of the body when fully unrolled.
    fn body_full_h(&self) -> i32 {
        self.0
            .body
            .borrow()
            .clone()
            .map(|b| b.preferred_height().1)
            .unwrap_or(0)
    }


    fn toggle_roll(&self) {
        self.set_rolled(!self.0.rolled.get(), true);
    }

    /// Roll to `rolled`, animating. `persist` writes the new state to the
    /// config — deliberately false for hover-open/auto-close, so a hedge the
    /// user rolled up by hand comes back rolled up after a restart.
    fn sync_rolled_class(&self, rolled: bool) {
        let ctx = self.0.win.style_context();
        if rolled {
            ctx.add_class("hedge-rolled");
        } else {
            ctx.remove_class("hedge-rolled");
        }
    }

    fn set_rolled(&self, rolled: bool, persist: bool) {
        // no title bar to roll into, and a docked hedge already has its own
        // collapse gesture — don't stack the two
        if self.0.header.borrow().is_none() || self.0.collapsed.get() {
            return;
        }
        if rolled == self.0.rolled.get() && self.0.roll.get().is_none() {
            return;
        }
        self.0.rolled.set(rolled);
        if !rolled {
            self.sync_rolled_class(false);
        }
        if persist {
            let name = self.name();
            self.0.app.with_hedge_mut(&name, |f| f.rolled = rolled);
            self.0.app.save();
        }

        let full = self.body_full_h() as f64;
        let from = if rolled { full } else { 0.0 };
        let to = if rolled { 0.0 } else { full };
        // Driven by a glib timeout rather than the frame clock, for the same
        // reason as `slide_dock`: the compositor throttles paint clocks for
        // obscured keep-below windows and tick-driven tweens freeze.
        self.0.roll.set(Some((from, to, glib::monotonic_time())));
        if self.0.roll_id.borrow().is_none() {
            let this = self.clone();
            let id = glib::timeout_add_local(Duration::from_millis(16), move || this.on_roll_step());
            *self.0.roll_id.borrow_mut() = Some(id);
        }
    }

    fn on_roll_step(&self) -> glib::ControlFlow {
        let Some((from, to, t0)) = self.0.roll.get() else {
            *self.0.roll_id.borrow_mut() = None;
            return glib::ControlFlow::Break;
        };
        let p = (((glib::monotonic_time() - t0) as f64) / (ROLL_MS * 1000.0)).min(1.0);
        let e = 1.0 - (1.0 - p).powi(3);
        let vis = (from + (to - from) * e).round() as i32;
        self.set_body_visible_h(vis);
        if p >= 1.0 {
            self.0.roll.set(None);
            *self.0.roll_id.borrow_mut() = None;
            self.settle_roll();
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    /// Give the clip the body's current natural height.
    ///
    /// Must be re-measured rather than set once at build time: the body's
    /// preferred height is ~0 before it is realized, and since the clip's
    /// request (not the child's) is what drives the window's requisition, an
    /// auto-fit hedge would otherwise collapse to just its title bar.
    fn sync_clip_height(&self) {
        if self.0.rolled.get() || self.0.roll.get().is_some() {
            return;
        }
        let (Some(clip), Some(body)) = (
            self.0.roll_clip.borrow().clone(),
            self.0.body.borrow().clone(),
        ) else {
            return;
        };
        // height only — the width comes from propagate_natural_width
        let h = body.preferred_height().1;
        if h > 0 {
            clip.set_size_request(-1, h);
        }
    }

    /// Show `vis` px of the body by shrinking its clipping container, then
    /// pull the window down to match.
    fn set_body_visible_h(&self, vis: i32) {
        if let Some(clip) = self.0.roll_clip.borrow().clone() {
            clip.set_size_request(-1, vis.max(0));
        }
        let w = self.0.win.allocated_width().max(1);
        self.0.win.resize(w, (self.header_h() + vis).max(1));
    }

    /// Land on a clean end state once the tween finishes.
    fn settle_roll(&self) {
        if self.0.rolled.get() {
            self.set_body_visible_h(0);
            self.sync_rolled_class(true);
        } else {
            // hand the clip back its natural height, then let the window refit
            self.sync_clip_height();
            self.sync_window_size();
        }
    }

    /// Apply the persisted rolled state after a rebuild, with no animation.
    ///
    /// Deferred to an idle like `show_handle` does: a resize issued before the
    /// window is actually mapped gets recomputed from the natural size at map
    /// time, so doing this inline silently does nothing.
    fn apply_rolled_state(&self) {
        if !self.0.rolled.get() || self.0.collapsed.get() {
            return;
        }
        let this = self.clone();
        glib::idle_add_local_once(move || {
            if !this.0.rolled.get() || this.0.collapsed.get() {
                return;
            }
            this.set_body_visible_h(0);
            this.sync_rolled_class(true);
            if std::env::var("HEDGE_DEBUG").is_ok() {
                eprintln!(
                    "hedge: roll restore header_h={} -> {}x{}",
                    this.header_h(),
                    this.0.win.allocated_width(),
                    this.0.win.allocated_height()
                );
            }
        });
    }

    // -- inline rename ------------------------------------------------------

    fn start_rename(&self) {
        // A docked hedge renames in its handle, but only while hovered open —
        // shut, the handle is a sliver with nothing to click.
        if self.0.collapsed.get() && !self.0.expanded.get() {
            return;
        }
        let container = if self.0.collapsed.get() {
            self.0.bar.borrow().clone()
        } else {
            self.0.header.borrow().clone()
        };
        let header = match container {
            Some(h) => h,
            None => return,
        };
        let old_center = header.center_widget();
        if let Some(w) = &old_center {
            if w.is::<gtk::Entry>() {
                return;
            }
        }
        let entry = gtk::Entry::new();
        // share the title bar's font (see `.hedge-rename` in style.css)
        entry.style_context().add_class("hedge-rename");
        entry.set_text(&self.name());
        entry.set_width_chars((self.name().len() as i32 + 2).max(6));
        // a vertically docked handle is only ~30px wide, so the entry widens
        // the bar while editing and it shrinks back on commit
        entry.set_max_width_chars(24);
        entry.set_alignment(0.5);
        if let Some(w) = &old_center {
            header.remove(w);
        }
        header.set_center_widget(Some(&entry));
        entry.show();
        self.0.renaming.set(true);
        self.0.win.set_accept_focus(true);
        self.0.win.present();
        entry.grab_focus();

        let this = self.clone();
        let e = entry.clone();
        // Ending the edit tears this entry down — `render()` rebuilds the whole
        // header — and that is exactly what must not happen from inside the
        // entry's own signal handler. Committing on `activate` destroyed the
        // widget mid-emission, and dropping it made it lose focus, so
        // `focus-out` re-entered here and tried to finish a second time on a
        // half-dead widget. Hence both guards:
        //
        //   1. `renaming` is a one-shot latch, so whichever of Enter / Escape /
        //      focus-out arrives first is the only one that acts;
        //   2. the work is deferred to an idle callback, so GTK has finished
        //      emitting before anything is destroyed.
        let finish = move |commit: bool| {
            if !this.0.renaming.replace(false) {
                return;
            }
            let new = e.text().trim().to_string();
            this.0.win.set_accept_focus(false);
            let this = this.clone();
            glib::idle_add_local_once(move || {
                if commit && !new.is_empty() && new != this.name() {
                    // rename_hedge_live re-renders the hedge itself
                    if this.0.app.rename_hedge_live(&this.name(), &new) {
                        return;
                    }
                }
                this.render();
            });
        };

        let f = finish.clone();
        entry.connect_activate(move |_| f(true));
        let f = finish.clone();
        entry.connect_focus_out_event(move |_, _| {
            f(false);
            glib::Propagation::Proceed
        });
        let f = finish.clone();
        entry.connect_key_press_event(move |_, ev| {
            if ev.keyval() == gdk::keys::constants::Escape {
                f(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    // -- tile context menu / remove ----------------------------------------
    fn tile_menu(&self, index: usize) {
        let menu = gtk::Menu::new();
        let item = gtk::MenuItem::with_label("Remove from Hedge");
        let this = self.clone();
        item.connect_activate(move |_| this.remove_app(index));
        menu.append(&item);
        menu.show_all();
        menu.popup_at_pointer(None);
    }

    fn remove_app(&self, index: usize) {
        let name = self.name();
        let gone = self.0.app.with_hedge_mut(&name, |f| {
            (index < f.apps.len()).then(|| f.apps.remove(index))
        });
        if let Some(entry) = gone.flatten() {
            crate::desktop::restore_entry_to_view(&entry.command);
        }
        self.0.app.save();
        self.render();
    }

    // -- drag apps IN -------------------------------------------------------
    fn on_drag_motion(&self, x: i32, y: i32) -> bool {
        self.0.win.style_context().add_class("hedge-droptarget");
        if let Some(id) = self.0.hover_timer.borrow_mut().take() {
            id.remove();
        }
        if self.0.collapsed.get() && !self.0.expanded.get() {
            self.expand();
        }
        let idx = self.index_at(x as f64, y as f64);
        if Some(idx) != self.0.hover_index.get() {
            self.0.hover_index.set(Some(idx));
            self.layout_tiles(Some(idx), true);
        }
        false
    }

    fn on_drag_leave(&self) {
        self.0.win.style_context().remove_class("hedge-droptarget");
        if let Some(id) = self.0.hover_timer.borrow_mut().take() {
            id.remove();
        }
        let this = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(120), move || this.clear_hover());
        *self.0.hover_timer.borrow_mut() = Some(id);
    }

    fn clear_hover(&self) -> glib::ControlFlow {
        *self.0.hover_timer.borrow_mut() = None;
        self.0.hover_index.set(None);
        self.layout_tiles(None, true);
        if self.0.collapsed.get() && self.0.expanded.get() {
            self.collapse();
        } else {
            let this = self.clone();
            glib::idle_add_local_once(move || this.sync_window_size());
        }
        glib::ControlFlow::Break
    }

    fn on_drop(
        &self,
        ctx: &gdk::DragContext,
        _x: i32,
        _y: i32,
        data: &gtk::SelectionData,
        info: u32,
        time: u32,
    ) {
        self.0.win.style_context().remove_class("hedge-droptarget");
        if let Some(id) = self.0.hover_timer.borrow_mut().take() {
            id.remove();
        }
        let slot = self.0.hover_index.get();
        self.0.hover_index.set(None);

        if info == TILE_INFO {
            let mut ok = false;
            if let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&data.data()) {
                if let (Some(src), Some(idx)) = (payload["hedge"].as_str(), payload["index"].as_u64())
                {
                    self.0
                        .app
                        .set_pending_move(Some((src.to_string(), idx as usize, self.name(), slot)));
                    ok = true;
                }
            }
            ctx.drag_finish(ok, false, time);
            return;
        }

        let uris = data.uris();
        let text = if !uris.is_empty() {
            uris.iter().map(|u| u.to_string()).collect::<Vec<_>>().join("\n")
        } else {
            data.text().map(|t| t.to_string()).unwrap_or_default()
        };
        let entries = crate::desktop_apps::entries_from_uri_list(&text);
        let success = !entries.is_empty();
        if success {
            let name = self.name();
            self.0.app.with_hedge_mut(&name, |f| {
                let pos = slot.unwrap_or(f.apps.len()).min(f.apps.len());
                for (i, e) in entries.into_iter().enumerate() {
                    f.apps.insert(pos + i, e);
                }
            });
            self.0.app.save();
            if self.0.collapsed.get() {
                self.0.expanded.set(true);
            }
            self.render();
        } else {
            self.layout_tiles(None, true);
        }
        // Clear the original off the desktop ourselves. Asking the source to do
        // it (the `delete` flag below) is the protocol-correct route and was
        // what this did, but nemo-desktop ignores it, so the icon just stayed.
        // Gated on the item living directly in ~/Desktop: dragging a file out
        // of a Nemo folder window must never touch that folder. The list walked
        // is the one the entries were built from rather than `uris`, which a
        // source offering only `text/plain` leaves empty — the original used to
        // survive such a drop.
        if success {
            for uri in crate::desktop_apps::uri_list_lines(&text) {
                if uri_on_desktop(uri) {
                    clear_from_desktop(uri);
                }
            }
        }
        // false: we've handled the original, and a source that *did* honour the
        // flag would delete documents we deliberately kept in place.
        ctx.drag_finish(success, false, time);
    }

    // -- drag tiles OUT -----------------------------------------------------
    fn on_tile_drag_begin(&self, ctx: &gdk::DragContext, icon: &str) {
        *self.0.press.borrow_mut() = None;
        self.0.drag_failed.set(false);
        self.0.app.set_pending_move(None);
        let icon = if icon.is_empty() {
            "application-x-executable"
        } else {
            icon
        };
        if icon.contains('/') {
            if let Ok(pb) = gdk_pixbuf::Pixbuf::from_file_at_size(icon, 48, 48) {
                ctx.drag_set_icon_pixbuf(&pb, 0, 0);
                return;
            }
        }
        let name = if icon.contains('/') {
            "application-x-executable"
        } else {
            icon
        };
        ctx.drag_set_icon_name(name, 0, 0);
    }

    fn on_tile_drag_data_get(&self, index: usize, data: &gtk::SelectionData, info: u32) {
        if info == TILE_INFO {
            let payload = serde_json::json!({"hedge": self.name(), "index": index}).to_string();
            let target = data.target();
            data.set(&target, 8, payload.as_bytes());
        } else if let Some(uri) = self.entry_uri(index) {
            data.set_uris(&[&uri]);
            *self.0.drag_out_uri.borrow_mut() = Some(uri);
        }
    }

    fn on_tile_drag_end(&self, index: usize) {
        let uri = self.0.drag_out_uri.borrow_mut().take();
        let moved = self.0.drag_out_moved.replace(false);
        let failed = self.0.drag_failed.replace(false);

        if let Some(p) = self.0.app.take_pending_move() {
            self.0.app.finish_move(&p.0, p.1, &p.2, p.3);
            return;
        }
        if failed {
            return;
        }

        // Dragging a tile out used to remove it the moment the drag ended
        // without error — but "no error" is not "the file arrived". A target
        // that accepts URIs and does nothing with them (a text editor, say)
        // left the launcher gone with nothing to show for it. Only let the tile
        // go once the copy is accounted for.
        match uri {
            None => self.remove_app(index), // no file involved (an in-app drag)
            Some(uri) => match self.drag_out_landed(&uri, moved) {
                true => self.remove_app(index),
                false => {
                    eprintln!(
                        "hedge: keeping the tile — no copy of {uri} turned up on the desktop"
                    );
                    self.layout_tiles(None, true);
                }
            },
        }
    }

    /// Did the dragged-out file actually land somewhere?
    ///
    /// `moved` is the target's own word for it and settles the question. Beyond
    /// that all we can check is the desktop, since XDND never tells the source
    /// *where* a copy went — so a drop into a Nemo folder window keeps the tile.
    /// That's the safe direction: a tile that outstays its file is a nuisance,
    /// a launcher deleted for a copy that never happened is lost work.
    fn drag_out_landed(&self, uri: &str, moved: bool) -> bool {
        if moved {
            return true;
        }
        let Ok((path, _host)) = glib::filename_from_uri(uri) else {
            return false;
        };
        let desktop = crate::desktop::desktop_dir();
        let Some(name) = path.file_name() else {
            return false;
        };
        // The file the tile pointed at already lives on the desktop (a document
        // filed into this hedge earlier). Nothing was copied anywhere; putting
        // it back in the desktop view is what "dragged out" means for it.
        if path.parent() == Some(desktop.as_path()) {
            let name = name.to_string_lossy().into_owned();
            if let Err(e) = crate::desktop::unhide_from_view(&name) {
                log!(WARN, "could not put {name} back on the desktop: {e}");
                return false;
            }
            return true;
        }
        desktop.join(name).exists()
    }

    fn entry_uri(&self, index: usize) -> Option<String> {
        let b = self.block();
        let entry = b.apps.get(index)?.clone();
        let cmd = entry.command.trim().to_string();
        let mut path: Option<String> = None;
        if let Some(rest) = cmd.strip_prefix("gtk-launch ") {
            let app_id = rest.trim();
            let info = gio::DesktopAppInfo::new(&format!("{app_id}.desktop"))
                .or_else(|| gio::DesktopAppInfo::new(app_id));
            if let Some(info) = info {
                path = info.filename().map(|p| p.to_string_lossy().into_owned());
            }
        } else if let Some(cand) = crate::desktop_apps::command_target_path(&cmd) {
            // an "open this file" tile or a direct-exec tile (a dropped
            // executable): the dragged thing is the file itself
            if cand.exists() {
                path = Some(cand.to_string_lossy().into_owned());
            }
        }
        let path = match path {
            Some(p) => p,
            None => self.export_desktop_file(&entry)?,
        };
        glib::filename_to_uri(&path, None).ok().map(|g| g.to_string())
    }

    fn export_desktop_file(&self, entry: &crate::config::AppEntry) -> Option<String> {
        let exports = crate::config::config_dir().join("exports");
        std::fs::create_dir_all(&exports).ok()?;
        let slug = slugify(&entry.name);
        let path = exports.join(format!("hedge-{slug}.desktop"));
        let name = if entry.name.is_empty() { "App" } else { &entry.name };
        let exec = if entry.command.is_empty() { "true" } else { &entry.command };
        let icon = if entry.icon.is_empty() {
            "application-x-executable"
        } else {
            &entry.icon
        };
        let contents =
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nIcon={icon}\n");
        std::fs::write(&path, contents).ok()?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        Some(path.to_string_lossy().into_owned())
    }
}

/// Is this dropped URI a file sitting directly on the desktop?
fn uri_on_desktop(uri: &str) -> bool {
    let Ok((path, _host)) = glib::filename_from_uri(uri.trim()) else {
        return false;
    };
    path.parent() == Some(crate::desktop::desktop_dir().as_path())
}

/// Take one desktop item out of sight now that a hedge holds it.
///
/// Two kinds of item, two answers, and the difference matters:
///
/// * A **pointer** — a `.desktop` launcher or a symlink, whatever it points at
///   — is redundant once the hedge has it: the tile carries the command and
///   icon the pointer resolved to, not a reference to the pointer. It goes to
///   Trash, where the user can retrieve it. Trashing a symlink only ever moves
///   the link; whatever it pointed at is untouched.
/// * A **document or folder** is the opposite: the tile is `xdg-open <path>`,
///   so the file *is* the thing the tile points at. Deleting it would break the
///   tile and lose the file. It stays exactly where it is and is only hidden
///   from the desktop view, so it's still `~/Desktop/<name>` to every other
///   program (and Ctrl-H in Nemo).
fn clear_from_desktop(uri: &str) {
    let Ok((path, _host)) = glib::filename_from_uri(uri.trim()) else {
        return;
    };
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let is_pointer = path.extension().is_some_and(|e| e == "desktop") || path.is_symlink();
    if is_pointer {
        match gio::File::for_path(&path).trash(gio::Cancellable::NONE) {
            Ok(()) => {
                // An earlier build hid links instead of trashing them. A stale
                // entry would hide the next file to take this name, so drop it.
                let _ = crate::desktop::unhide_from_view(&name);
                return;
            }
            // No trash on this filesystem, say — fall through and hide it, so
            // the icon still leaves the desktop.
            Err(e) => log!(WARN, "could not trash {}: {e} — hiding it instead", path.display()),
        }
    }
    if let Err(e) = crate::desktop::hide_from_view(&name) {
        log!(WARN, "could not hide {name} from the desktop: {e}");
    }
}

/// `(x, y, w, h)` in root coordinates.
pub type Rect = (i32, i32, i32, i32);

/// Which edge, if any, a hedge dropped at `(x, y)` should dock to.
///
/// An axis the hedge very nearly spans is excluded: a hedge almost as tall as
/// the work area sits within `EDGE_THRESHOLD` of *both* the top and the bottom
/// wherever you drop it, and one taller than the work area is past both, so
/// taking the plain minimum of four distances docked every tall hedge
/// vertically and made floating one impossible.
pub fn choose_edge(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    mx: i32,
    my: i32,
    mw: i32,
    mh: i32,
) -> Option<&'static str> {
    let mut best: Option<(&'static str, i32)> = None;
    let mut consider = |name: &'static str, d: i32| {
        if d <= EDGE_THRESHOLD && best.map_or(true, |(_, bd)| d < bd) {
            best = Some((name, d));
        }
    };
    // Only the nearer edge of each axis is a candidate. A tall hedge sits
    // within the threshold of BOTH top and bottom wherever you drop it, so
    // taking each independently docked it to whichever happened to be checked
    // first; comparing them keeps the choice meaningful. An axis the hedge
    // overflows entirely is skipped — there is no "near" edge to pick.
    if w < mw {
        let (dl, dr) = (x - mx, (mx + mw) - (x + w));
        if dl <= dr {
            consider("left", dl);
        } else {
            consider("right", dr);
        }
    }
    if h < mh {
        let (dt, db) = (y - my, (my + mh) - (y + h));
        if dt <= db {
            consider("top", dt);
        } else {
            consider("bottom", db);
        }
    }
    best.map(|(name, _)| name)
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// Pull the edges being dragged onto a neighbouring hedge's edges.
///
/// `dragging` is `(left, right, top, bottom)` — only those edges move, so only
/// they are candidates for alignment. Each is snapped to the nearest edge of
/// any other hedge within [`NEIGHBOUR_SNAP`], which makes hedges line up flush
/// instead of landing a few pixels out.
pub fn snap_to_neighbours(
    dragging: (bool, bool, bool, bool),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    others: &[Rect],
) -> (i32, i32, i32, i32) {
    if others.is_empty() {
        return (x, y, w, h);
    }
    let nearest = |v: i32, cands: &[i32]| -> Option<i32> {
        cands
            .iter()
            .copied()
            .filter(|c| (c - v).abs() <= NEIGHBOUR_SNAP)
            .min_by_key(|c| (c - v).abs())
    };
    let xs: Vec<i32> = others.iter().flat_map(|o| [o.0, o.0 + o.2]).collect();
    let ys: Vec<i32> = others.iter().flat_map(|o| [o.1, o.1 + o.3]).collect();
    let (mut x, mut y, mut w, mut h) = (x, y, w, h);
    let (drag_l, drag_r, drag_t, drag_b) = dragging;

    if drag_r {
        if let Some(t) = nearest(x + w, &xs) {
            w = (t - x).max(1);
        }
    }
    if drag_l {
        if let Some(t) = nearest(x, &xs) {
            w = (w + (x - t)).max(1);
            x = t;
        }
    }
    if drag_b {
        if let Some(t) = nearest(y + h, &ys) {
            h = (t - y).max(1);
        }
    }
    if drag_t {
        if let Some(t) = nearest(y, &ys) {
            h = (h + (y - t)).max(1);
            y = t;
        }
    }
    (x, y, w, h)
}

/// Shift a `w`x`h` window at `(x, y)` by the smallest amount that clears every
/// rect in `others`.
///
/// Each pass pushes out of the first collision along whichever of the four
/// directions costs least, then re-checks — so a hedge squeezed between two
/// others keeps sliding until it finds a gap. Bounded so a hedge that cannot
/// possibly fit just stops rather than looping.
pub fn resolve_overlap(x: i32, y: i32, w: i32, h: i32, others: &[Rect]) -> (i32, i32) {
    let (mut x, mut y) = (x, y);
    for _ in 0..8 {
        let Some(&o) = others.iter().find(|&&o| overlaps((x, y, w, h), o)) else {
            break;
        };
        let candidates = [
            (o.0 - (x + w), 0),       // push left
            ((o.0 + o.2) - x, 0),     // push right
            (0, o.1 - (y + h)),       // push up
            (0, (o.1 + o.3) - y),     // push down
        ];
        let best = candidates
            .iter()
            .min_by_key(|(dx, dy)| dx.abs() + dy.abs())
            .copied()
            .unwrap_or((0, 0));
        x += best.0;
        y += best.1;
    }
    (x, y)
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "app".to_string()
    } else {
        s
    }
}

fn enable_transparency(win: &gtk::Window) {
    if let Some(screen) = WidgetExt::screen(win) {
        if screen.is_composited() {
            if let Some(visual) = screen.rgba_visual() {
                win.set_visual(Some(&visual));
            }
        }
    }
    win.set_app_paintable(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_when_clear() {
        assert_eq!(resolve_overlap(0, 0, 100, 100, &[(200, 200, 50, 50)]), (0, 0));
        assert_eq!(resolve_overlap(10, 10, 100, 100, &[]), (10, 10));
    }

    #[test]
    fn touching_edges_do_not_count_as_overlap() {
        // a hedge flush against another's right edge is fine
        assert_eq!(resolve_overlap(100, 0, 100, 100, &[(0, 0, 100, 100)]), (100, 0));
    }

    #[test]
    fn pushes_out_the_cheapest_way() {
        // overlapping by 10px on the left -> shoved 10px right, nothing vertical
        let other = (100, 0, 100, 100);
        assert_eq!(resolve_overlap(90, 0, 100, 100, &[other]), (0, 0));
        // deep horizontal overlap but shallow vertical -> resolved vertically
        let other = (0, 0, 200, 100);
        let (x, y) = resolve_overlap(0, 90, 200, 100, &[other]);
        assert_eq!(x, 0);
        assert_eq!(y, 100);
    }

    #[test]
    fn squeezes_past_a_second_hedge() {
        // pushing clear of the first lands on a second; it must keep going
        let others = [(100, 0, 100, 100), (200, 0, 100, 100)];
        let (x, y) = resolve_overlap(150, 0, 100, 100, &others);
        assert!(!others.iter().any(|&o| overlaps((x, y, 100, 100), o)),
                "still overlapping at {x},{y}");
    }

    #[test]
    fn gives_up_rather_than_looping() {
        // boxed in on all sides: must terminate, overlap or not
        let others = [(0, 0, 1000, 1000)];
        let _ = resolve_overlap(400, 400, 100, 100, &others);
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    const MW: i32 = 1920;
    const MH: i32 = 1080;
    fn edge(x: i32, y: i32, w: i32, h: i32) -> Option<&'static str> {
        choose_edge(x, y, w, h, 0, 0, MW, MH)
    }

    #[test]
    fn docks_to_the_nearest_edge() {
        assert_eq!(edge(4, 500, 300, 200), Some("left"));
        assert_eq!(edge(MW - 304, 500, 300, 200), Some("right"));
        assert_eq!(edge(800, 3, 300, 200), Some("top"));
        assert_eq!(edge(800, MH - 203, 300, 200), Some("bottom"));
    }

    #[test]
    fn stays_floating_in_open_space() {
        assert_eq!(edge(800, 500, 300, 200), None);
        // near a corner but not against it: must NOT dock
        assert_eq!(edge(12, 12, 300, 200), None);
        assert_eq!(edge(MW - 312, MH - 212, 300, 200), None);
    }

    #[test]
    fn a_tall_hedge_docks_to_whichever_edge_is_nearer() {
        // 1000px tall on 1080: within the threshold of both top and bottom,
        // so the nearer one wins rather than an arbitrary first match
        assert_eq!(edge(800, 2, 300, 1000), Some("top"));
        assert_eq!(edge(800, 78, 300, 1000), Some("bottom"));
        // and sideways still works
        assert_eq!(edge(3, 40, 300, 1000), Some("left"));
        // ...and a hedge merely NEAR an edge now stays floating
        assert_eq!(edge(20, 40, 300, 1000), None);
    }

    #[test]
    fn a_hedge_taller_than_the_screen_never_docks_vertically() {
        assert_eq!(edge(800, -100, 300, 1400), None);
        assert_eq!(edge(2, -100, 300, 1400), Some("left"));
    }

    #[test]
    fn a_hedge_wider_than_the_screen_does_not_dock_sideways() {
        assert_eq!(edge(-40, 500, 2000, 200), None);
        assert_eq!(edge(-40, 3, 2000, 200), Some("top"));
    }
}

#[cfg(test)]
mod neighbour_tests {
    use super::*;

    const NEIGHBOUR: Rect = (500, 300, 200, 150); // spans x 500..700, y 300..450

    #[test]
    fn right_edge_lines_up_with_a_neighbour() {
        // right edge at 495, neighbour's left edge at 500 -> pulled flush
        let (x, y, w, h) = snap_to_neighbours((false, true, false, false), 200, 300, 295, 150, &[NEIGHBOUR]);
        assert_eq!((x, y), (200, 300));
        assert_eq!(x + w, 500);
        assert_eq!(h, 150);
    }

    #[test]
    fn left_edge_snaps_and_keeps_the_far_edge_put() {
        // left edge at 705 snaps back to the neighbour's right edge, 700
        let (x, _, w, _) = snap_to_neighbours((true, false, false, false), 705, 300, 200, 150, &[NEIGHBOUR]);
        assert_eq!(x, 700);
        assert_eq!(x + w, 905, "the un-dragged edge must not move");
    }

    #[test]
    fn only_dragged_edges_snap() {
        // the right edge is in range but is not being dragged
        let before = (495, 800, 200, 100);
        let after = snap_to_neighbours((false, false, false, false), 495, 800, 200, 100, &[NEIGHBOUR]);
        assert_eq!(after, before);
    }

    #[test]
    fn far_away_edges_are_left_alone() {
        let after = snap_to_neighbours((false, true, false, false), 100, 300, 60, 150, &[NEIGHBOUR]);
        assert_eq!(after, (100, 300, 60, 150));
    }

    #[test]
    fn bottom_edge_snaps_vertically() {
        let (_, y, _, h) = snap_to_neighbours((false, false, false, true), 900, 200, 100, 105, &[NEIGHBOUR]);
        assert_eq!(y, 200);
        assert_eq!(y + h, 300, "flush with the neighbour's top");
    }
}

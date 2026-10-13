//! Detect a double-click on empty desktop and toggle the desktop icons.
//!
//! Three problems, three answers:
//!
//! * **Seeing the click at all.** nemo-desktop already owns button events on
//!   its window, so we can't select for them. The X11 RECORD extension taps
//!   input passively instead: it steals nothing, cannot freeze the pointer,
//!   and needs no cooperation from nemo.
//! * **Only on the desktop.** Each press is resolved against the window stack
//!   and ignored unless the topmost window at that point is
//!   `_NET_WM_WINDOW_TYPE_DESKTOP`.
//! * **Only on *empty* desktop.** Icons live inside that one window, so X
//!   can't tell us. We read their positions from the same GIO metadata the
//!   snapshot feature uses (`metadata::nemo-icon-position`) and drop clicks
//!   that land on one.
//!
//! RECORD needs a blocking read loop, so it runs on its own thread and hands
//! presses to the GTK main loop through a channel.

use std::sync::mpsc;
use std::time::Duration;

use gdk::prelude::MonitorExt;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::record::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};

use crate::app::HedgeApp;

/// Matches the desktop's own double-click setting.
const DOUBLE_CLICK_MS: u32 = 400;
/// Movement still counted as the same spot.
const SLOP: i16 = 8;
/// How long to wait, after an apparently-empty double-click, to see whether a
/// window opens. Long enough for nemo to map a folder window, short enough that
/// the peek gesture still feels immediate.
const OPENED_GRACE_MS: u64 = 400;
/// How much of an icon's cell counts as "on the icon". nemo's desktop grid is
/// roughly this per entry, and erring large is the safe direction: the cost of
/// guessing wrong is toggling when the user meant to open something.
const ICON_W: i32 = 110;
const ICON_H: i32 = 110;

/// A press seen by the tap: root coordinates and the server timestamp.
struct Press {
    x: i16,
    y: i16,
    time: u32,
}

/// Start watching. Returns false if the X server has no RECORD extension, in
/// which case the feature is simply unavailable and everything else still runs.
pub fn spawn(app: &HedgeApp) -> bool {
    let (tx, rx) = mpsc::channel::<Press>();

    let started = std::thread::Builder::new()
        .name("hedge-desktop-click".into())
        .spawn(move || {
            if let Err(e) = record_loop(tx) {
                log!(WARN, "desktop double-click watcher stopped: {e}");
            }
        })
        .is_ok();
    if !started {
        return false;
    }

    // Drain on the GTK thread; all app state is touched from here only.
    let app = app.clone();
    let mut last: Option<(i16, i16, u32)> = None;
    glib::timeout_add_local(Duration::from_millis(40), move || {
        while let Ok(p) = rx.try_recv() {
            let is_double = last.is_some_and(|(lx, ly, lt)| {
                p.time.wrapping_sub(lt) <= DOUBLE_CLICK_MS
                    && (p.x - lx).abs() <= SLOP
                    && (p.y - ly).abs() <= SLOP
            });
            if is_double {
                last = None; // a triple click must not fire twice
                // The desktop test is done HERE, not on the record thread: it
                // walks the whole window tree (hundreds of round trips) and
                // doing that per press starved the RECORD stream, which then
                // stopped delivering events after the first detection.
                let on_desktop = point_is_desktop(p.x, p.y);
                // no icons are on screen while peeking, so nothing to hit
                let hit = on_desktop && !app.is_peeking() && on_icon(p.x as i32, p.y as i32);
                if std::env::var("HEDGE_DEBUG").is_ok() {
                    eprintln!(
                        "hedge: double-click at ({},{}) on_desktop={on_desktop} on_icon={hit}",
                        p.x, p.y
                    );
                }
                if !on_desktop || hit {
                    continue;
                }
                // `on_icon` only knows about icons nemo has *stored* a position
                // for. An icon the user has never dragged has no
                // `nemo-icon-position` metadata at all, so an auto-arranged
                // desktop looks entirely empty and double-clicking a folder
                // both opened it and toggled the peek.
                //
                // So don't decide yet: give the double-click a moment to prove
                // it opened something. If a new top-level window appears, the
                // click was on an icon and the peek is off. Nothing opens when
                // you double-click bare desktop, which is the case we want.
                let before = client_list();
                let app = app.clone();
                glib::timeout_add_local_once(Duration::from_millis(OPENED_GRACE_MS), move || {
                    let after = client_list();
                    let opened = after.iter().any(|w| !before.contains(w));
                    if std::env::var("HEDGE_DEBUG").is_ok() {
                        log!("double-click: windows {} -> {} opened={opened}", before.len(), after.len());
                    }
                    if opened {
                        return; // it hit something; not our gesture
                    }
                    // peek: hide the hedges AND the desktop icons, restore on
                    // the next double-click. Deliberately not the stash-move
                    // toggle — that relocates real files and is far too heavy
                    // for a gesture you make by accident.
                    app.toggle_peek();
                });
            } else {
                last = Some((p.x, p.y, p.time));
            }
        }
        glib::ControlFlow::Continue
    });
    true
}

thread_local! {
    /// Connection for the main thread's desktop test, opened once.
    static QUERY: std::cell::RefCell<Option<(x11rb::rust_connection::RustConnection, xproto::Window, xproto::Atom, xproto::Atom)>> =
        const { std::cell::RefCell::new(None) };
}

/// Is the topmost mapped window at this point a desktop window?
fn point_is_desktop(px: i16, py: i16) -> bool {
    QUERY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let Ok((conn, screen)) = x11rb::connect(None) else {
                return false;
            };
            let root = conn.setup().roots[screen].root;
            let Some(t) = conn
                .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                return false;
            };
            let Some(dsk) = conn
                .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DESKTOP")
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                return false;
            };
            *slot = Some((conn, root, t.atom, dsk.atom));
        }
        let Some((conn, root, type_atom, desktop_atom)) = slot.as_ref() else {
            return false;
        };
        topmost_is_desktop(conn, *root, px, py, *type_atom, *desktop_atom).unwrap_or(false)
    })
}

/// The mapped top-level windows, from `_NET_CLIENT_LIST` on the root.
///
/// Used as an "did that click open anything?" probe: comparing the list either
/// side of a double-click catches an icon being opened without needing to know
/// where the icons are. Returns an empty list if anything goes wrong, which
/// reads as "nothing opened" — the pre-existing behaviour.
fn client_list() -> Vec<u32> {
    QUERY.with(|cell| {
        let slot = cell.borrow();
        let Some((conn, root, _, _)) = slot.as_ref() else {
            return Vec::new();
        };
        let Some(atom) = conn
            .intern_atom(false, b"_NET_CLIENT_LIST")
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return Vec::new();
        };
        conn.get_property(false, *root, atom.atom, xproto::AtomEnum::WINDOW, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|p| p.value32().map(|v| v.collect()))
            .unwrap_or_default()
    })
}

/// Is this root-coordinate point inside a desktop icon?
///
/// Positions come from GIO metadata and are relative to the monitor the icon
/// is on (`metadata::monitor`), so each is offset by that monitor's origin.
fn on_icon(px: i32, py: i32) -> bool {
    let monitors = monitor_origins();
    for item in crate::snapshot::read_desktop() {
        let Some(pos) = item.position else { continue };
        let Some((ix, iy)) = parse_pos(&pos) else {
            continue;
        };
        let idx: usize = item
            .monitor
            .as_deref()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        let (ox, oy) = monitors.get(idx).copied().unwrap_or((0, 0));
        let (ix, iy) = (ix + ox, iy + oy);
        if px >= ix && px < ix + ICON_W && py >= iy && py < iy + ICON_H {
            return true;
        }
    }
    false
}

fn parse_pos(s: &str) -> Option<(i32, i32)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn monitor_origins() -> Vec<(i32, i32)> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    (0..display.n_monitors())
        .filter_map(|i| display.monitor(i))
        .map(|m| {
            let g = m.geometry();
            (g.x(), g.y())
        })
        .collect()
}

/// The RECORD tap. Blocks, so it owns a thread.
fn record_loop(tx: mpsc::Sender<Press>) -> Result<(), Box<dyn std::error::Error>> {
    // One connection creates and owns the context, a second one streams the
    // data — RECORD requires the reading connection to do nothing else, so
    // this thread issues no other requests at all.
    let (ctrl, _) = x11rb::connect(None)?;
    let (data, _) = x11rb::connect(None)?;

    if ctrl.extension_information(record::X11_EXTENSION_NAME)?.is_none() {
        return Err("X server has no RECORD extension".into());
    }

    let ctx = ctrl.generate_id()?;
    let empty8 = record::Range8 { first: 0, last: 0 };
    let empty16 = record::Range16 { first: 0, last: 0 };
    let range = record::Range {
        core_requests: empty8,
        core_replies: empty8,
        ext_requests: record::ExtRange {
            major: empty8,
            minor: empty16,
        },
        ext_replies: record::ExtRange {
            major: empty8,
            minor: empty16,
        },
        delivered_events: empty8,
        // ButtonPress only; we time double-clicks from presses alone
        device_events: record::Range8 {
            first: xproto::BUTTON_PRESS_EVENT,
            last: xproto::BUTTON_PRESS_EVENT,
        },
        errors: empty8,
        client_started: false,
        client_died: false,
    };
    ctrl.record_create_context(ctx, 0, &[record::CS::ALL_CLIENTS.into()], &[range])?
        .check()?;

    for reply in data.record_enable_context(ctx)? {
        let reply = reply?;
        if reply.client_swapped || reply.data.len() < 32 {
            continue;
        }
        // Raw X event bytes: ButtonPress is 32 bytes, with the root-relative
        // position at offsets 20..24 and the timestamp at 4..8.
        let d = &reply.data;
        if d[0] != xproto::BUTTON_PRESS_EVENT {
            continue;
        }
        let button = d[1];
        if button != 1 {
            continue;
        }
        let time = u32::from_ne_bytes([d[4], d[5], d[6], d[7]]);
        let x = i16::from_ne_bytes([d[20], d[21]]);
        let y = i16::from_ne_bytes([d[22], d[23]]);
        if tx.send(Press { x, y, time }).is_err() {
            break; // main loop gone
        }
    }
    Ok(())
}

fn topmost_is_desktop<C: Connection>(
    conn: &C,
    root: xproto::Window,
    px: i16,
    py: i16,
    type_atom: xproto::Atom,
    desktop_atom: xproto::Atom,
) -> Result<bool, Box<dyn std::error::Error>> {
    let (sw, sh) = conn
        .setup()
        .roots
        .iter()
        .find(|s| s.root == root)
        .map(|s| (s.width_in_pixels as i32, s.height_in_pixels as i32))
        .unwrap_or((1920, 1080));
    let tree = conn.query_tree(root)?.reply()?;
    // children are bottom-to-top, so walk backwards for the topmost
    for &win in tree.children.iter().rev() {
        let Ok(attrs) = conn.get_window_attributes(win)?.reply() else {
            continue;
        };
        if attrs.map_state != xproto::MapState::VIEWABLE {
            continue;
        }
        let Ok(geo) = conn.get_geometry(win)?.reply() else {
            continue;
        };
        let inside = px >= geo.x
            && px < geo.x + geo.width as i16
            && py >= geo.y
            && py < geo.y + geo.height as i16;
        if !inside {
            continue;
        }
        return Ok(match window_type(conn, win, type_atom, 0) {
            Some(t) => t == desktop_atom,
            // Untyped and covering most of the screen: Cinnamon's own backdrop.
            // Switching off `show-desktop-icons` makes nemo-desktop QUIT, so
            // during a peek there is no DESKTOP-typed window at all and this
            // backdrop is what a click lands on. Without accepting it, the
            // gesture that starts a peek could never end one — no hedges, no
            // desktop, no way back. Application windows are always typed, and
            // the size test keeps stray untyped popups out.
            None => geo.width as i32 * 2 >= sw && geo.height as i32 * 2 >= sh,
        });
    }
    // nothing at all covers the point: bare root
    Ok(true)
}

/// First `_NET_WM_WINDOW_TYPE` found at or below `win`.
///
/// The property lives on the client window, which may be reparented below the
/// frame the stack handed us, so descend a few levels looking for it.
fn window_type<C: Connection>(
    conn: &C,
    win: xproto::Window,
    type_atom: xproto::Atom,
    depth: u8,
) -> Option<xproto::Atom> {
    if depth > 4 {
        return None;
    }
    if let Ok(Ok(prop)) = conn
        .get_property(false, win, type_atom, xproto::AtomEnum::ATOM, 0, 8)
        .map(|c| c.reply())
    {
        if let Some(mut atoms) = prop.value32() {
            if let Some(first) = atoms.next() {
                return Some(first);
            }
        }
    }
    let tree = conn.query_tree(win).ok()?.reply().ok()?;
    tree.children
        .iter()
        .find_map(|&c| window_type(conn, c, type_atom, depth + 1))
}

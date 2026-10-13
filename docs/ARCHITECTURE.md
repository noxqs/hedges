# Architecture

A module-for-module port of the Python `hedge_app` package to Rust + gtk3-rs.
The `reference/python-original/` tree is the source of truth for *behaviour*;
each Rust module maps to the same-named Python file.

| Rust | Python | Responsibility |
|---|---|---|
| `app.rs` | `app.py` | Application, shared state, tray, hedge management |
| `hedge_window.rs` | `hedge_window.py` | The desktop widget (the hard one) |
| `settings_window.rs` | `settings_window.py` | Settings dialog |
| `config.rs` | `config.py` | JSON config (+ backups, new) |
| `colors.rs` | `colors.py` | Colour helpers |
| `desktop_apps.rs` | `desktop_apps.py` | App discovery / dropped URIs |
| `desktop.rs` | — | Hide/show desktop icons (new) |
| `clock.rs` | — | The clock desklet (new) |
| `install.rs` | — | Self-install/upgrade/uninstall into `$HOME` (new) |
| `version.rs` | — | Build stamping and version comparison (new) |

## Installing (no root)

There is no install script — the binary installs itself via `hedge --install`,
and the release artifacts only carry that binary. That keeps one list of the
files an install owns
(binary, autostart, menu launcher, both Nemo right-click actions, icon,
manifest) in `install.rs`, so uninstall can't drift from install — the failure
mode we're avoiding is a right-click "Add a Hedge" action left pointing at a
binary that's gone.

`install.json` next to the binary records the installed `Build`. Comparison is
semver first, **build epoch second** (`build.rs` stamps compile time), so two
`3.0.0` binaries still order correctly — that's what the startup prompt, the
Settings VERSION section and `--status` all read.

Uninstall un-hides stashed desktop icons *first*: the stash is only navigable
by Hedge, so removing the app before emptying it would strand the user's files.

## Why GTK 3 (not GTK 4)

The whole identity of this app — an undecorated, translucent, **keep-below**,
**all-workspaces**, freely-**positioned** widget you drag around, **edge-snap**,
and give a **click-through input shape** — depends on window APIs that GTK 4
deleted from its public surface (`move`/`resize`/`get_position`, `keep_below`,
`stick`, `skip_taskbar`, `type_hint`, `input_shape_combine_region`). On X11 those
would require dropping to raw X11 FFI. **gtk3-rs keeps all of them**, so this is
a faithful port with no `unsafe`. Trade-off: gtk3-rs (0.18) is in maintenance
mode — stable, which is what a port of an existing GTK 3 app wants.

## Shared-state model

GTK signal callbacks are `'static`, so the mutable app state can't be borrowed
across them. The pattern used throughout:

```
struct State { app, cfg, hedges: HashMap<String, HedgeWindow>, settings, pending_move, ... }
#[derive(Clone)] pub struct HedgeApp(Rc<RefCell<State>>);   // cheap clonable handle
```

- `HedgeApp` is cloned into every signal closure (`let this = self.clone();`).
- `HedgeWindow` and `SettingsWindow` are likewise `Rc<Inner>` handles, `Clone`.
- Each window holds a `HedgeApp` and its own `name: RefCell<String>`; it reads
  its config block on demand via `app.hedge(&name)` (returns a cloned `Hedge`)
  and mutates via `app.with_hedge_mut(&name, |f| ...)`.

### ⚠️ Don't destroy a widget inside its own signal handler

A sibling of the RefCell footgun below, and it bites the same way. The inline
title rename ends by calling `render()`, which rebuilds the header — including
the `GtkEntry` being edited. Doing that straight from the entry's `activate`
handler destroys the widget mid-emission, and dropping it makes it lose focus,
so its own `focus-out` handler re-enters and tries to finish a second time on a
half-dead widget.

Two guards, both needed (see `start_rename`):

1. a one-shot latch (`renaming`), so whichever of Enter / Escape / focus-out
   arrives first is the only one that acts;
2. the teardown deferred with `glib::idle_add_local_once`, so GTK has finished
   emitting before anything is destroyed.

The same rule is why arming the rename is itself deferred to an idle callback
from the title's `button-press-event`.

### ⚠️ The RefCell re-entrancy footgun (read this)

The one class of bug that bit us on first run: **never hold a `borrow()` across
a call that re-enters the same `RefCell`.** Two Rust-specific traps:

1. A `borrow()` temporary in an **`if`/`while` condition lives through the
   block**:
   ```rust
   if self.0.borrow().hedges.is_empty() { self.sync_hedges(); } // PANIC: sync_hedges borrows_mut
   ```
   Fix: read into a local first — `let empty = self.0.borrow().hedges.is_empty();`
2. A chained call keeps the temporary alive for the whole statement:
   ```rust
   self.0.borrow().app.activate();   // activate() re-enters on_activate -> borrow_mut -> PANIC
   ```
   Fix: `let app = self.gtk_app(); app.activate();`

Rule of thumb: pull values **out** of the RefCell into locals, drop the borrow,
*then* call methods that touch the app. `with_hedge_mut` / `with_cfg_mut` /
`hedge()` / `cfg_snapshot()` all borrow briefly and release.

## hedge_window.rs — the interesting bits

- **Window**: `Gtk::Window` (Toplevel) with `set_keep_below`, `stick`,
  `set_skip_taskbar_hint`, `set_type_hint(Normal)`, `set_decorated(false)`,
  and an RGBA visual for transparency. A single `EventBox` hosts either the
  panel (floating) or the docked layout (collapsed).
- **The type hint must stay `Normal`.** Muffin/Mutter promote a window whose
  type is a *transient type* (dialog, toolbar, menu, **utility**) and which has
  no transient-parent to the highest layer occupied by any window in its WM
  group. The Settings window is `Normal` and shares the group, so with the
  `Utility` hint the original used, every hedge was lifted out of the
  below-layer and pinned on top of all other windows as soon as Settings was
  opened — and stayed there. `Normal` is not a transient type, so
  `_NET_WM_STATE_BELOW` holds; `skip_taskbar`/`skip_pager` already keep the
  window out of the window list and alt-tab, which was all `Utility` bought.
  Verify with `xprop -root _NET_CLIENT_LIST_STACKING` (bottom-to-top): the
  hedges must sit just above the `nemo-desktop` windows, never at the end.
- **Tile grid** is a `Gtk::Layout` (manual x/y placement) so tiles can be
  animated. `layout_tiles(gap_index, animate)` computes slot positions; a
  frame-clock **tick callback** (`add_tick_callback`) eases tiles to their slots
  (ease-out cubic, `ANIM_MS`). `gap_index` opens a placeholder slot under the
  drag cursor.
- **Collapse / slide**: when docked to an edge, the panel lives inside a
  clipping `Gtk::Layout`; collapse/expand **slides the panel** within it so only
  the title bar peeks out. The slide is driven by a **`glib::timeout` (16 ms),
  not the frame clock** — the compositor throttles paint clocks for obscured
  keep-below windows, which froze tick-driven tweens. `update_input_shape`
  restricts clicks to the visible sliver via `input_shape_combine_region`
  (a full-window region "clears" it when floating).
- **Move / resize**: manual — press+drag on the background moves the window
  (`move_`); a grab zone near the edges starts a resize we drive ourselves
  (`begin_resize` / `drag_resize`), *not* `begin_resize_drag`, because the size
  has to be pulled toward whole-tile multiples while the drag is in flight and a
  client cannot influence the WM's own resize loop. Geometry is saved debounced
  (`queue_geom_save`, 400 ms) on `configure-event`. The rules that make that
  drive-it-yourself resize behave are in the boxed section below — every one of
  them was arrived at by measuring, and undoing any of them brings back a
  specific visible bug.
- **Edge snap**: on release, `settle_position` measures distance to each
  work-area edge; within `EDGE_THRESHOLD` it docks and persists `edge`.

### ⚠️ Interactive resize: ask for a size, let gravity hold the rest

Dragging a left or top edge means the *opposite* edge has to stand perfectly
still while the origin moves. Six rules make that true. Each was measured under
Muffin, and each has a symptom attached, so if one is ever "simplified" away the
symptom tells you which.

1. **Never move and resize.** They are two operations and the window manager
   applies them in its own order — even when issued as a single
   `XMoveResizeWindow`. Measured: frames carrying the *new* origin with the
   *old* width, putting the anchored edge 60px out, then a corrective frame.
   That is the jitter. Instead `begin_resize` sets the window **gravity** to the
   corner that must not move (`resize_gravity`) and `drag_resize` asks only for
   a size; the WM keeps the corner put, in one `ConfigureNotify`. `on_release`
   restores `NORTH_WEST`, because everything else here (docking, settling, the
   rescue move) means the top-left corner when it says a position.
2. **`gtk_window_resize` must still be called.** It is what makes GtkWindow
   re-allocate the widget tree. Skip it — talking only to the GdkWindow — and
   the window changes shape while its contents and, fatally, the event box's
   input window keep the old geometry: the hedge stops responding to the pointer
   part-way through the drag, and a second drag does nothing at all.
3. **The minimum comes from the snap lattice, not `preferred_width`.** For a
   docked hedge `drag_resize` itself puts a size request on the panel each
   motion, so the reported minimum ratchets up to whatever width was last
   applied and the hedge can then only ever be widened. The floor is one whole
   tile, from the lattice captured at press.
4. **`on_configure` reads the event's size, not `allocated_width`.** The
   allocation trails the `ConfigureNotify` by a cycle, so reading it there
   records the *previous* size — and because that matches what is already
   stored, nothing ever corrects it. `grid_cols` reads that width, so the hedge
   goes on being laid out for the width before last and its final column hangs
   off the edge, clipped.
5. **Nothing may re-seat or fold a hedge mid-drag.** `on_size_allocate` fires on
   every motion for a docked hedge and used to schedule `snap_to_edge`, which
   yanked the window back to the saved position while the drag pulled it away —
   judder for the whole drag, ending where it started. The pointer also leaves
   the window *by design* during a left-edge drag, so `check_pointer`,
   `leave_timeout` and the hide-until-hover fade all consult `dragging()` and
   hold off. A docked hedge's position *along* its edge is the one piece of live
   geometry a resize is allowed to persist — `snap_to_edge` reads it back on
   release, and leaving it stale is what made the hedge spring back.
6. **The grab zone outranks a tile.** On a small hedge a tile reaches into the
   edge zone, and a tile is a drag source, so it swallowed the press and started
   dragging the launcher out while the cursor showed a resize arrow. The tile's
   own press handler claims the press for `begin_resize` and stops the emission,
   which is what keeps the drag source (connected after it) from arming.

The `3 × 2` badge is read off the same lattice and must use the **same
arithmetic as `grid_cols`** — floor, with `base_w = 2·padding + 4 − spacing` and
`inc_w = cell + spacing`. It used to round, so a hedge 2.7 cells wide was
announced as "3" while the layout floored to 2: the badge promised a column that
never arrived.

Tile labels are what make a cell's size interesting. A label reserves
`TILE_LABEL_LINES` lines whether its name needs them or not
(`reserve_label_height`), so a cell depends on the hedge's settings rather than
on which launchers it happens to hold. Without that, tiles were as tall as their
own names wrapped to — 94px here, 122px there — `cell_size` took the tallest,
and the grid was pitched for a cell most tiles did not fill. The reservation
runs after realization: **an unrealized label measures 0**, which silently made
an earlier attempt at this a no-op.

## Drag and drop

- Each tile is a `DragSource` offering two targets: the custom same-app target
  `application/x-hedge-tile` (a JSON `{hedge, index}` payload) and URI targets.
- Each hedge window is a `DropTarget` (`DestDefaults::ALL`) for the same two.
- **Between hedges**: the drop records `pending_move` on the app; the *source's*
  `drag-end` calls `finish_move` (so the source widget isn't destroyed
  mid-drag). **In from the desktop**: URIs →
  `desktop_apps::entries_from_uri_list` → inserted at the hovered slot.

### Clearing the original off the desktop

The protocol-correct route is `gtk_drag_finish(delete = TRUE)`, which asks the
*source* to remove the original. **nemo-desktop ignores it**, so the icon simply
stayed. Hedge does the removal itself instead, and the two cases are not alike:

- a **`.desktop` launcher** is redundant once a tile holds its command → Trash
  (`gio::File::trash`, so it's recoverable);
- a **document or folder** is what the tile *points at* (`xdg-open <path>`), so
  deleting or moving it would break the tile and lose the file. It stays exactly
  where it is and is only hidden from the desktop view, by adding its name to
  `~/Desktop/.hidden` — a list Nemo/Nautilus honour. The path never changes.

Both are gated on the item living *directly* in `~/Desktop`, so dragging out of
a Nemo folder window can't empty that folder. Every route a tile can leave a
hedge by (drag out, tile menu, Settings) calls `desktop::restore_entry_to_view`,
or a file could end up hidden with nothing left pointing at it. `desktop::hide`
(the icon stash) skips `.hidden` and everything listed in it — moving a hedged
document into the stash would break its tile until the icons were toggled back.

### Out to the desktop

`drag-end` used to drop the tile whenever the drag ended without error, but "no
error" is not "the file arrived": a target that accepts URIs and does nothing
with them left the launcher deleted for nothing. The tile now goes only if the
target reported a move (`drag-data-delete`) or the file turns up in `~/Desktop`.
XDND never tells the source *where* a copy went, so a drop into a folder window
keeps the tile — the safe direction.

## config.rs

- `Config { hedges, desktop_hidden, install_prompt_dismissed, clock, ping,
  sysmon, panel_open }`. `IndexMap` preserves hedge order;
  `#[serde(flatten)] extra` round-trips unknown keys untouched.
- **The top level is snake_case; every nested block is camelCase.** `Config`
  itself has no `rename_all`, so its keys are `desktop_hidden`,
  `install_prompt_dismissed`, `panel_open` — while `Hedge`, `Clock`, `Ping` and
  `Sysmon` all carry `#[serde(rename_all = "camelCase")]` and so use
  `bgColor`, `intervalSec`, `chartBgOpacity`. That split is inherited from the
  Python original and has to stay: changing it would silently reset every
  existing config to defaults. A hand-written `"panelOpen"` is not read.
- New blocks are `#[serde(default)]` and default to *off*, so an upgrade never
  changes what an existing desktop looks like.
- `save()` writes atomically (tmp + rename) with a 4-space `PrettyFormatter`,
  matching the Python `json.dump(indent=4)` byte-for-byte.
- Backups: `make_backup()` copies `config.json` into `backups/config-<epoch>.json`
  (keeps newest 15); `restore_latest()` snapshots the current state first, so a
  restore is itself undoable.

## Single instance: the lock, then the bus

Two guards, because the D-Bus one is not sufficient on its own.

`lock.rs` takes an advisory `flock` on `$XDG_RUNTIME_DIR/<app-id>.lock` before
anything else and holds it for the life of the process, writing its pid into the
file so the loser can name the winner. The kernel releases it however the holder
dies, so there is no stale lock to detect or clean up. If the lock is held but
D-Bus claims we are the only instance — a different session bus, a hand-started
binary, a name that never got registered — the second copy refuses to start
rather than run a second daemon writing the same `config.json`, where the last
save silently wins. The lock is keyed on the app id, so the `HEDGE_APP_ID` trick
for running a dev copy alongside the installed one still works, while two copies
of the *same* identity remain impossible. A copy that has just asked the
previous instance to stand down (`HEDGE_TOOK_OVER`) retries for a few seconds
instead of refusing, because it is racing that instance's shutdown.

`HedgeApp::run` then calls `register()` before `run()` — `run()` unregisters on the
way out, so `is_remote()` is only answerable while registered. A bare second
launch then offers to replace the running copy: it activates the primary's
`quit` GAction (exported over D-Bus for free by GApplication) and re-execs
itself after a short delay. Re-exec rather than continuing in-process, because
this process is already registered as the remote end and GApplication has no way
back. `HEDGE_TOOK_OVER` marks the new process so a failed takeover reports
itself instead of looping. An older build has no `quit` action and won't respond.

## Peek: telling an icon from empty desktop

`on_icon` can only see icons nemo has stored a `nemo-icon-position` for, so an
auto-arranged desktop looks empty and double-clicking a folder both opened it
and toggled the peek. Rather than model nemo's auto-arrange grid, the gesture
asks the *outcome*: snapshot `_NET_CLIENT_LIST`, wait `OPENED_GRACE_MS`, and
compare. A new top-level window means the click hit something. Clicks on icons
we can see still short-circuit with no delay.

## clock.rs — the desklet

A much smaller window than a hedge: no grid, no drop target, no docking, no
resize. What it *does* share is the recipe that makes a window read as desktop
furniture — undecorated, RGBA visual, `type_hint(Normal)` (see the note above on
why not `Utility`), keep-below, sticky, skip taskbar/pager — plus
`set_accept_focus(false)`, since a clock should never take the keyboard.

It lives in `Config::clock`, a single block rather than a map, and is `enabled:
false` by default so an existing config gains nothing on upgrade. `sync_clock`
creates / drops / refreshes it from that block, and every Settings control calls
it. The tick is a plain 1-second timeout that only touches the labels when the
rendered text changes: cheaper than it sounds, and unlike a minute-aligned timer
it cannot drift out of step after a suspend.

### The control strip beside the time

Four icons sit in a column to the right of the time: a gear that opens Settings
on its own Desklet page, and one switch each for the host table, the charts and
the busiest programs. They replaced a single chevron that folded the lot.

- The head row is **centred as a unit**, so the icons stay against the right of
  the time instead of drifting to the panel's far edge when the table below is
  wider. Their size follows the clock's font, and their colour is the clock's own
  pulled halfway to grey (`colors::muted`) — deliberately derived rather than a
  fixed grey, because a fixed light grey vanishes on a pale panel and a fixed
  dark one disappears on a dark panel. Deriving it from the colour the user chose
  *because it reads* inherits that contrast for free.
- A switch whose feature is off in Settings **hides itself** rather than sitting
  there greyed: no dead controls. Which sections are up is legible from the panel
  itself, so the icons carry no lit state — only tooltips say which way a click
  goes.
- The panel's inset is equal on all four sides. It used to carry 6px extra left
  and right, which read as a *thin* bottom border whenever the last thing in the
  panel was a chart card, whose fill makes the gap below it exact and obvious.
  The gap under the host table is a **separator, not a border**: `refresh_panel`
  drops it when the table is the last thing showing.

> ⚠️ **A setting the sampler reads must go through `sync_sysmon`.**
> `Monitor` keeps its *own* copy of `Sysmon` and skips reading processes when
> that copy says they aren't wanted. The programs switch flips
> `sysmon.show_processes`, so saving the config and repainting is not enough —
> if the instance started with the section off, the list comes on screen and
> stays permanently empty, and no amount of clicking recovers it. Every Settings
> control already goes through `sync_sysmon` for exactly this reason.

## The desklet panel — hosts, charts, processes

Each section switches on independently, and the desklet's own icons decide what
is *shown* while Settings decides what *runs* (`panel_hosts`, `panel_charts` and
`sysmon.show_processes`; a `panel_open` from before the split is migrated into
the first two by `config::migrate` and never written back). Charts and the
process list are asked for separately — `SysmonPanel::refresh(charts, procs)` —
so either can be on screen without the other. The clock owns the
widgets; `ping.rs` and `sysmon.rs` own the data and know nothing about GTK.

### Sampling never runs on the main loop

Both monitors follow the same shape: a GLib timeout fires, a worker thread does
the blocking work, and one batch of results comes back over a `glib` channel to
be folded into history on the main thread. A `ping` waiting out its timeout or
an `nvidia-smi` taking 300 ms would otherwise freeze the desktop. Only one round
is ever in flight — if a round outlives its interval the next tick is skipped
rather than queued, so a dead host cannot build a backlog.

`MainContext::channel` is deprecated in glib 0.18 in favour of an async channel
from another crate. It is kept deliberately: this is a thread → main-loop
handoff of one small message every few seconds, 0.18 is the last GTK3 series so
it will not be removed underneath us, and the alternative is a dependency and an
executor for that.

### Blocked is not down

The host table is a tree, and that is the whole point of it. A round walks the
tree parents-first, level by level; a host whose parent did not answer is not
pinged at all and is recorded as **blocked**, drawn grey. Calling it *down*
would be a lie — nothing about it has been tested — and a column of red under a
dead router tells you less than one red row and a grey branch.

Badges chunk that history: the row always spans `SPAN_SECS` (three hours)
however many badges it is split into, so adding badges buys resolution, not
reach, and the row means the same thing at a 5-second interval as at a 5-minute
one. A badge is green only if every round in its period answered; any mixture is
orange; all-blocked is grey.

### Filling from a traceroute

`traceroute -q 1 -w 2 -m 15` with reverse lookups left **on** — knowing a delay
is inside `singnet.com.sg` is the reason to watch a route at all, and traceroute
resolves while it probes so the names are near-free. The parser handles both
output shapes (`1  name (1.2.3.4)` and `-n`'s bare address), keeps only the first
address of a hop that answered from several (alternate paths, not extra hops),
and skips hops that answered from nowhere rather than adding a host that could
only ever look down.

Hops are added **flat, not chained**, even though the tree exists. Routes are
full of hops that refuse ICMP while forwarding perfectly; parenting them would
let one such hop grey out the entire rest of the path. Parenting is for a
topology you actually know — the router your NAS sits behind — and stays
available in the host editor.

### Charts

`sysmon_view.rs` draws each series into a `DrawingArea` with cairo: a card, a
grid, the name top-left, the reading top-right, and a Catmull-Rom spline through
the readings converted to cubic beziers — it passes *through* every sample
rather than smoothing them away. The series slides left continuously between
samples, driven by a plain GLib timer rather than the frame clock, for the same
reason the hedge dock slide is: the compositor throttles the paint clock of an
obscured keep-below window and the charts would freeze.

Style that the draw handler needs — card colour, corner radius, whether to show
clock speed, the sample interval — is resolved once when the config changes and
cached in `Cell`s. `draw` runs twenty times a second per card, and cloning the
whole `Config` (every hedge, every app) in there would be absurd.

The **vertical grid lines are 30 seconds apart** (`GRID_SECS`) rather than at
fractions of the card's width: a line you can put a number on. They travel left
with the curve, which is the only way a line can keep meaning the same moment,
and are skipped entirely when a long sample interval would pack them tighter
than they are readable. Their anchor is a **monotonic count of samples taken**,
not the newest sample's position: the curve gets its motion from the history
shifting along by one sample with `phase` animating the gap, which is enough for
a shape with no fixed reference point, but a line tied to that alone just
oscillates inside one sample's width — the grid sat still while the curve moved
under it.

Two readings that share an axis get colours from opposite sides of the wheel,
not neighbours: network in/out were amber and salmon, technically different and
visually identical. The caption is drawn as coloured runs so `in` and `out` are
painted like the curves they name.

Process CPU is a delta between rounds, divided by the *same* jiffy total the CPU
chart used, so the rows roughly add up to the line above them. `ps`'s `%CPU` is
an average over a process's whole lifetime, which would rank a browser that was
busy an hour ago above the compile pinning the machine now. The list always
draws five rows, padded with blanks: a widget you drag must not change height
under the pointer because the machine went quiet.

### The Claude section — claude_mon.rs

Ported from the standalone `claudeManager` desklet (its README documents the
data sources in full). Three pollers on three clocks, each following the
sampling rule above (worker thread, one in flight, batch over a channel):

- **Sessions** (3 s): `~/.claude/sessions/<pid>.json`, verified against
  `/proc/<pid>` *and* the recorded process start time — the file survives an
  unclean exit and PIDs get recycled. The model tag comes from the newest
  assistant entry in the session's transcript tail, skipping `<synthetic>` and
  sidechain entries.
- **Usage** (5 min): `GET api.anthropic.com/api/oauth/usage` with the OAuth
  token from `~/.claude/.credentials.json`, read fresh each poll and sent over
  `ureq` — a real HTTP client on purpose, so the token never appears in a
  process command line. Never written or refreshed; a 401 renders as
  "signed out" until Claude Code renews it.
- **Service status** (30 s): the public statuspage JSON. History is kept and
  chunked into `SERVER_DOTS` round dots, worst-verdict-per-period, with
  page-unreachable (`Unknown`) only shown when a whole period was unknown —
  an unreachable status *page* proves nothing about the API.

Display is two columns: sessions left (model tag, then the name coloured by
status), readings right — usage and API rows share one shape (label, dot
strip, value), usage filling its dots like a bar while the API strip is
time. `panel_claude` is
display-only — the pollers follow `claude.enabled` and its per-reading
switches, so folding the section away keeps the strip's history. The section
shares the host table's palette: green working, orange idle, red needs-you.

### ⚠️ `set_no_show_all` makes `show_all` a no-op on that widget

The panel's containers carry `set_no_show_all(true)` so the clock's own
`show_all()` cannot unfold them. That *also* means calling `show_all()` on those
containers does nothing — including to their children. Show the children
individually and then `show()` the container. This was two separate bugs: a
panel that rendered as an empty strip, and a chevron button whose label had
never been shown, so it drew as an invisible box.

## Tray

`libappindicator` (Ayatana). The bundled `assets/icons/hedge.svg` is embedded
with `include_bytes!` and written to `~/.config/hedge/icons/` at startup so the
indicator can theme it. The art fills nearly the whole 64×64 viewBox on purpose:
the panel scales the entire box to its height, so padding baked into the SVG
just makes the tray icon look small.

**There is no click or double-click handler, and there cannot be.**
StatusNotifierItem has no click concept, and the menu is exported over DBusMenu
as a *model* — the panel renders its own menu, so our `GtkMenu` is never mapped.
Detecting a double-click on the panel strut via the X RECORD tap was tried and
removed: nothing at the X level can tell our icon from any other item on the
panel, so it fired on every panel double-click. Use the menu, or bind a key to
`hedge --settings`.

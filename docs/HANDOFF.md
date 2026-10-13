# Handoff

The "continue here" note for picking the project up in RustRover. Four entries:
the initial Rust port, the desklet work of 9 Aug 2026, the polish pass of
11 Aug 2026, then the Claude section of 15 Aug 2026.

## Current status: shipped & live

The Rust build **replaced the Python daemon** and is the live Hedge on the dev
machine. All original Python features are ported, plus the desktop panel below.
`cargo test` passes (61 tests). No warnings, no clippy errors.

### Session of 15 Aug 2026 — the Claude section

Ported the standalone `claudeManager` desklet (`~/RustroverProjects/
claudeManager`, whose README documents the data sources in full) into the
desklet panel as a fourth section. The design notes live in `ARCHITECTURE.md`
(*The Claude section — claude_mon.rs*); this is the inventory.

**The section** (`claude_mon.rs` new, `clock.rs`, `config.rs`,
`settings_window.rs`)
- Two columns: sessions left (model tag in the muted colour, then the project
  name coloured green working / orange idle / red waiting-on-you), readings
  right — every reading row the same shape: label, 10-dot strip, value.
  Usage strips (5-hour, week, per-model e.g. Fable) fill like a bar, orange
  past 70% and red past 90%; the API strip is time, three minutes a dot,
  half an hour end to end, polled every 30 s from status.anthropic.com.
- Three pollers on three clocks (sessions 3 s, usage 5 min, status 30 s),
  each following the sampling rule: worker thread, one in flight, batch back
  over a glib channel. New dependency `ureq` so the OAuth token never touches
  a process command line.
- Config block `claude` (camelCase, off by default), `panel_claude` display
  flag, a ✱ glyph in the control strip (dev note: no stock Claude icon; a
  text glyph takes the strip's CSS colour where a raw SVG would not), and a
  CLAUDE group on the Settings Desklet page.

**Width discipline — the bug worth remembering.** The section's natural width
briefly *became* the desklet's width and everything stretched ~100 px. The rule
now: this section never sets the panel's width. Session names ellipsize
(hexpand, full name in the tooltip), muted columns draw at 0.75× the reading
size, dots are fewer and smaller than the ping badges, and the value column
reserves exactly 4 chars ("100%"; statuses are `okay/warn/down/crit/n/a`), so
neither a status flip nor a usage jump can move desktop furniture. Verified by
measuring the window with the section on vs off: identical (504 px on the dev
machine).

**Hover hints** (`clock.rs`, `hedge_window.rs`)
- A usage row's whole width (label, dots, value) shows
  `week window — 88% used — 6d 13h left (resets Fri 08:00)`.
- Every hedge tile shows its app name on hover — the only place the name
  exists when a hedge hides labels, and the rescue for names past the
  three-line cap.

**One-dialog install/upgrade** (`app.rs`) — the startup offer's accept button
is now "Install & restart" / "Upgrade & restart" and does both; the second
"restart?" dialog is gone (failure is the only dialog left). A dev instance
(`HEDGE_APP_ID` set) never shows the prompt — mid-session an unguarded dev
launch upgraded the real install and popped dialogs on the desktop.

### Session of 11 Aug 2026 — desktop links, the control strip, resize

A polish pass driven entirely by things that looked wrong on screen. The
reasoning for each lives in `ARCHITECTURE.md` (the boxed *Interactive resize*
section and *The control strip beside the time*); this is the inventory.

**Desktop links and executables** (`desktop_apps.rs`, `hedge_window.rs`)
- A dropped **executable** (AppImage, native binary) becomes a tile that *runs*
  it, with the file manager's thumbnail as its icon — which for an AppImage is
  the application's own icon, the same one the desktop shows. `xdg-open` was
  never going to launch one.
- A dropped **link** is a pointer, not content: the tile is built from whatever
  it points at, so the link itself is redundant and goes to Trash with the
  launchers. Trashing a symlink moves only the link.
- Themed-icon fallback now picks the first name the icon theme can actually
  render. `application-vnd.appimage` isn't in Papirus, and GTK draws a missing
  name as the broken-image glyph.
- Clearing the original off the desktop walks the URI list the entries were built
  from, not `data.uris()` — a source offering only `text/plain` left the original
  sitting there.

**The desklet's control strip** (`clock.rs`, `sysmon_view.rs`, `colors.rs`)
- Four icons replace the fold chevron; per-section show/hide; Settings opens on
  the Desklet page (`open_desklet_settings`).
- `SysmonPanel::refresh` takes charts and processes separately, so the process
  list can be up with no charts.
- 30-second vertical grid lines on the charts.
- `colors::muted` derives chrome colour from the text colour it sits beside.
- **The bug worth remembering:** the programs switch flips a setting the
  *sampler* reads, and the monitor keeps its own copy of the config — see the
  boxed `sync_sysmon` warning in `ARCHITECTURE.md`.

**Resize** (`hedge_window.rs`) — six invariants, all in the boxed section:
gravity instead of move+resize; keep `gtk_window_resize`; floor from the lattice
not `preferred_width`; `on_configure` reads the event size; nothing re-seats or
folds mid-drag; the grab zone outranks a tile. Plus: the badge floors like
`grid_cols`, tiles reserve a fixed label height, and the grid reflows on release.

**Install/upgrade** (`app.rs`) — the result message and the restart offer are one
dialog. An in-place upgrade used to get a plain message ending in "Restart Hedge
to run the new build", which was the one thing it could have done for you.

**Pre-existing flake fixed** (`lock.rs`) — `lock::tests` failed about 1 run in 4.
`ping::tests` forks real `ping` children on background threads, and between a
fork and its `exec` the child holds a copy of every open descriptor, including a
held lock file's, so `flock` returned `EWOULDBLOCK` on a lock nobody owned. The
test retries, the way the app's own `acquire_retrying` does.

### Session of 9 Aug 2026 — the desklet panel

Everything below the clock, plus the settings restructure that houses it:

- **`ping.rs`** — host monitoring. Tree of hosts, parents pinged first, children
  of a dead parent recorded *blocked* (grey) rather than down. Badge row spans a
  fixed three hours however many badges it is split into. Fill-from-traceroute
  types the table from a route, naming hops by owning domain.
- **`sysmon.rs` / `sysmon_view.rs`** — CPU/GPU/MEM/NET cards drawn with cairo,
  smooth spline, continuous scroll, per-section switches, temperature and clock
  speed, NVIDIA/AMD auto-detect with the vendor shown on the card, click-CPU to
  open the system monitor, top-five processes by *recent* CPU.
- **`lock.rs`** — `flock` single-instance guard, added after two copies ran at
  once and fought over `config.json`.
- **Settings restructure** — the stack switcher across the top is gone. One
  navigation down the left: `General` and `Desklet` above the hedge list.
- **Docked-hedge position bug fixed** (see below).

The read to do first is `ARCHITECTURE.md` § *The desklet panel* — the reasoning
behind blocked-vs-down, flat traceroute fills, and the cached draw style is
there, not in the code.

### Deployment that was done
- `install.sh` installed the release binary to `~/.local/share/hedge/hedge` and
  replaced the autostart entry to point at it (was the Python launcher).
- The Python daemon (`python3 -m hedge_app`) was stopped.
- A Cinnamon custom keybinding **`Super+Shift+D` → `hedge --toggle-desktop`** was
  registered via gsettings.
- A pre-switch copy of the live config was saved as
  `~/.config/hedge/config.json.pre-rust-*`.

### Revert path
Rerun the *Python* project's `install.sh` (in the original repo at
`~/WebstormProjects/hedge`) and restart the session, or just relaunch it — the
config is shared and compatible.

## Feature checklist

| Feature | State |
|---|---|
| Full window widget: keep-below/sticky/translucent, drag-move, edge-resize | ✅ |
| Tile grid, launch, remove (right-click menu) | ✅ |
| Drag-in / drag-out / move-between-hedges | ✅ |
| Animated tile reflow + placeholder gap | ✅ |
| Collapse to edge + smooth slide in/out + click-through input shape | ✅ (needs on-screen tuning) |
| Inline title rename | ✅ |
| Settings dialog (all per-hedge options + apps editor) | ✅ |
| Tray icon + menu | ✅ |
| Auto-save (position/size debounced, settings immediate) | ✅ |
| Config backup/restore | ✅ |
| Hide/show desktop icons (safe stash move) | ✅ |
| About shows "3.0 (Rust)" | ✅ |
| Clock desklet | ✅ |
| Host monitor (tree, badges, latency) | ✅ |
| Fill hosts from traceroute | ✅ |
| CPU/GPU/MEM/NET charts + top processes | ✅ |
| Memory fence: desktop memory reserve via pkexec + tiles in own scopes (`memfence.rs`) | ✅ (scripts unit-tested; pkexec flow not yet run live) |
| Out-of-memory forecast + notification (`sysmon::seconds_until_full`) | ✅ (logic unit-tested; notification not yet seen live) |
| Single-instance lock file | ✅ |

## What to verify / tune on-screen

The logic compiles and runs, but a few things are inherently
"looks-right-on-screen" and were flagged for tuning under Muffin:

1. **Collapse slide** (`hedge_window.rs`: `slide_dock`, `on_slide_step`,
   `dock_offset`, `bar_vis`) — the roll in/out timing and the exact resting
   position of the collapsed sliver. Check all four edges.
2. **Input shape** (`update_input_shape` / `clear_input_shape`) — confirm the
   clipped part of a collapsed hedge is click-through and the visible sliver is
   clickable; confirm a floating hedge is fully clickable.
3. **Multi-monitor** positioning / edge-snap (`monitor_geom`, `settle_position`,
   `repair_positions` in `app.rs`).
4. **AppIndicator** icon actually appears in the Cinnamon tray.

## Fixed after the initial port

- **Hedges floated on top of every window once Settings had been opened.**
  The window type hint was `Utility`, which Muffin group-promotes out of the
  keep-below layer (see the type-hint note in `ARCHITECTURE.md`). Changed to
  `Normal`. Confirmed with `_NET_CLIENT_LIST_STACKING`: hedges now hold
  position 3+ from the bottom (just above `nemo-desktop`) with Settings open.
  **The Python original has the same bug** (`hedge_window.py:91`) — if you ever
  revert to it, change `Gdk.WindowTypeHint.UTILITY` to `NORMAL` there too.

- **Docked hedges came back stacked on each other after a reboot.**
  `snap_to_edge` derived a docked hedge's resting place from
  `gtk_window_get_position()`. At login that idle can fire before the window
  manager has processed the map and move, so X reports the pre-move geometry —
  usually (0,0) — and every docked hedge read the same bogus coordinate and
  snapped to the same spot. It then *stuck*, because hovering a docked hedge
  open let `on_configure` write that position back to the config as the truth.
  Fixed in two places: `snap_to_edge` takes the saved coordinate, and a
  collapsed hedge never persists live geometry. Diagnosis is in the git-less
  record of the session; the code comments carry the reasoning.

## Known issues / cleanups

- The literal "double-click on empty desktop" trigger was **not** implemented —
  Cinnamon/nemo-desktop doesn't hand that event to us cleanly, and a background
  catcher window would interfere with normal desktop use. We used the keyboard
  shortcut instead. If you still want double-click, the option is a transparent
  full-monitor catcher window (`type_hint=Desktop`, keep-below) that only
  reacts to double-click and forwards everything else — expect fiddly event
  pass-through work.
- No animated gap is shown while hovering a *collapsed* hedge during a drag
  (it springs open first, then the gap works) — matches Python.

- **Unreproduced:** one report that the "Replace the N hosts…" confirm dialog
  did nothing. Could not be reproduced, and the log shows a successful fill with
  hosts present, so the dialog did work at least once. The no-button "Tracing…"
  modal that followed it was the likelier culprit (indistinguishable from a hung
  app) and has been replaced by in-place progress on the button. The dialog's
  response is now logged — check for `traceroute fill: replace N host(s)? ->`
  if it resurfaces.
- Fifteen traced hops means fifteen `ping` subprocesses a round. Fine at the
  default 30 s; watch it if the interval is dropped a long way.
- **Tile names now trim past ~36 characters** instead of ~60:
  `TILE_LABEL_LINES` went 5 → 3 so that a cell's height is predictable and the
  resize lattice can be trusted. That is a real trade — a few long AppImage
  names show an ellipsis where they used to wrap further. Raising the constant
  back to 4 or 5 costs nothing but a taller cell; the reservation and the
  lattice follow it automatically.
- The resize lattice is pitched on the *widest* tile's natural width while
  `GtkLayout` allocates each tile its minimum, so a hedge holding one wide label
  has visible gaps between narrow tiles. Cosmetic, and the snapping is honest
  about it now, but equalising width the way `reserve_label_height` equalises
  height would tidy it.
- `MainContext::channel` is deprecated in glib 0.18 and used in three places
  behind `#[allow(deprecated)]`. Deliberate — see `ARCHITECTURE.md`.

## Suggested next steps

1. Open in RustRover, `cargo run` (foreground) to iterate; use the
   `HEDGE_APP_ID` + `XDG_CONFIG_HOME` trick (see README) to test without
   disturbing the installed instance.
2. Tune the slide/input-shape items above.
3. Consider `cargo clippy` and a rustfmt pass (the code is already
   rustfmt-style).
4. `hedge_window.rs` is past 3,300 lines and `settings_window.rs` past 1,400.
   Splitting them (`layout` / `collapse` / `dnd`; a page per settings tab) is
   the obvious next refactor if either grows again.
5. **The repository has no commits.** Everything is untracked. `git init` and a
   baseline commit is the single most valuable thing to do next — an evening
   like this one is currently unrecoverable if a file is lost.

## Dev workflow cheatsheet

```bash
cargo build                 # debug
cargo test                  # 57 unit tests
cargo run                   # foreground; Ctrl-C to stop
cargo build --release && ./target/release/hedge --install

# run a dev copy alongside the installed one, fully isolated — its own config,
# its own stash, its own log, its own lock. This is the only safe way to try
# desklet changes: a dev copy sharing the real config will fight the installed
# instance over saves, and a bare second launch will ask the installed one to
# quit (which is how the live desktop got wiped out twice during development).
SCRATCH=/tmp/hedge-dev
XDG_CONFIG_HOME=$SCRATCH/cfg XDG_DATA_HOME=$SCRATCH/data \
  XDG_STATE_HOME=$SCRATCH/state HEDGE_APP_ID=org.hedge.HedgeDev cargo run

# stop only the dev copy — never `pkill -f target/debug/hedge`, which also
# matches the shell running it:
for p in $(pgrep -f target/debug/hedge); do
  tr '\0' '\n' < /proc/$p/environ | grep -q HEDGE_APP_ID=org.hedge.HedgeDev && kill $p
done

# the edit → see-it loop (the installed copy is what the desktop runs; a change
# is invisible until this happens):
cargo build --release && pkill -TERM -x hedge && sleep 2 \
  && cp -f target/release/hedge ~/.local/share/hedge/hedge \
  && setsid nohup ~/.local/share/hedge/hedge >/dev/null 2>&1 &

# restart the installed instance:
pkill -x hedge; DISPLAY=:0 setsid ~/.local/share/hedge/hedge &
tail -f /tmp/hedge-rust.log   # if launched with the redirect used during setup
```

## Testing geometry off-screen (learned the hard way, 11 Aug)

A headless copy is the only safe way to drive resizes and drags with `xdotool`,
but a bare `Xvfb` is *not* a faithful stage. These four cost hours between them.

```bash
# a virtual screen WITH the real window manager — this is the part that matters
Xvfb :88 -screen 0 1200x700x24 &
LIBGL_ALWAYS_SOFTWARE=1 DISPLAY=:88 muffin --replace &
```

1. **Run Muffin.** With no WM, geometry bugs vanish: a move+resize pair looked
   perfectly atomic on bare Xvfb and produced two visibly wrong frames under
   Muffin. Anything about position, size, gravity or docking is untestable
   without it.
2. **Suppress the install nag** (`install_prompt_dismissed: true`) in the test
   config. It maps a 635×232 dialog at +0+0 three seconds after start-up, on top
   of whatever you are clicking, and swallows every synthetic click in silence.
3. **Don't dwell before clicking.** Hovering a control long enough pops a GTK
   tooltip — a toplevel window — which then eats the click. Use a single
   `xdotool mousemove X Y click 1`.
4. **Kill probe copies by identity, not pattern.** `pkill -f target/debug/hedge`
   matches the shell running it (it killed this session's own commands twice),
   and `readlink /proc/PID/exe` reports `…/hedge (deleted)` once a rebuild has
   replaced the file — a `case` arm without a trailing `*` skips exactly the
   stale instances you are trying to remove, so five of them piled up and the
   thing under test was an old binary. The environ check in the cheatsheet above
   is the reliable form.

Two habits that paid off: read the app's *own* view of events (a temporary
`eprintln!` of every `ConfigureNotify` next to every geometry request is what
found the jitter, after sampled `xdotool getwindowgeometry` readings had
suggested there was none), and confirm a fix by reproducing the bug first — one
"before" run here was ambiguous because the press had missed the window, which
looks identical to the fix not working.

## Gotchas to remember

- **RefCell re-entrancy** — see the boxed section in `ARCHITECTURE.md`. This was
  the only class of runtime bug hit; grep for `self.0.borrow()` in an `if`
  condition before adding new state access.
- gtk3-rs 0.18 uses `glib::Propagation` (not the old `Inhibit`) for event
  handler returns; `drag_finish` / `drag_set_icon_*` are **methods on
  `DragContext`**, not free `gtk::` functions.

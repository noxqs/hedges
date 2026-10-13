# Hedge 3.0 (Rust)

A zen, drag-friendly **app launcher for the Linux desktop**. Hedges are slim,
translucent widgets that sit on your desktop holding a grid of apps. Drag a
desktop icon, a menu app, or a file straight onto a Hedge to add it. Drag a
Hedge to a screen edge and it rolls up to a name handle until you hover it.

This is the **Rust + GTK 3** rewrite of the original Python app. It is a faithful,
behaviour-preserving port (same `~/.config/hedge/config.json`) plus a few new
features. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design and
[`docs/HANDOFF.md`](docs/HANDOFF.md) for current status and next steps.

## Features

- Translucent, undecorated, always-below, all-workspaces desktop widgets.
- Drag apps **in** (from the menu, desktop, or file manager) and **out**
  (to the desktop/a folder). Move tiles between hedges; reorder within one.
- **Filing an icon into a hedge clears it off the desktop.** A **pointer** — a
  `.desktop` launcher or a symlink, whichever it points at — goes to Trash,
  where you can retrieve it: the tile carries the command and icon the pointer
  resolved to, so the pointer itself is redundant. Trashing a link only ever
  moves the link; whatever it pointed at is untouched, and the tile points
  straight at that target. An **executable** — an AppImage, a binary, or a link
  to one — keeps working too: the tile runs it directly and shows the same icon
  the desktop does (the file manager's thumbnail).
  A document or folder is *not* moved or deleted: the tile is
  `xdg-open <path>`, so it stays at `~/Desktop/<name>` for every other program
  and is only hidden from the desktop view (via `.hidden`; Ctrl-H shows it in
  Nemo). Take the tile back out — drag, tile menu, or Settings — and it
  reappears on the desktop.
- Dragging a tile **out** only removes it from the hedge once the copy is
  accounted for, so a drop on something that ignores it can't lose a launcher.
- Drag a hedge to a screen edge → it **collapses to a name handle** and rolls
  in/out with a smooth slide; the collapsed sliver is click-through.
- Animated tile reflow with a live placeholder gap while dragging.
- Per-hedge settings (accent/background/label/title colour, transparency,
  corner radius, columns, icon size, spacing, padding, labels/title/background
  toggles).
- Tile names **wrap instead of being cut off** — up to 3 lines, no `…` and no
  mid-word hyphens. Only a name past ~36 characters is ever trimmed. Every tile
  reserves all 3 lines whether its name needs them or not, so a cell is the same
  size whatever the hedge holds — which is what keeps the rows even and the
  `3 × 2` readout while resizing honest.

### Two kinds of text: labels vs. title

They're separate settings and separate things:

| | `showLabels` / `labelColor` / `labelFontSize` | `showTitle` / `titleColor` / `titleFontSize` |
|---|---|---|
| What | the **app name under each icon** | the **hedge's own name**, in its title bar |
| How many | one per tile | one per hedge |
| Colour | `labelColor`, default `#c7cad6` | `titleColor`; empty = chosen automatically, black or white, to contrast with the accent |
| Size | `labelFontSize` (6–24, default 10; the tile widens with it) | `titleFontSize` (6–20; above 11 the bar grows) |

So `titleFontSize` has no effect on tile labels, and hiding labels leaves the
hedge's own name alone. The title bar is also what a hedge rolls up into, and
double-clicking that name renames the hedge.
- **System-tray menu** (Ayatana AppIndicator): add hedge, settings, toggle
  desktop icons, back up / restore config, about, quit.
- **Auto-save** of position/size (debounced) and every settings change.
- **Config backup/restore** — rotating timestamped backups under
  `~/.config/hedge/backups/`, plus a startup auto-backup.
- **Clock desklet** — an optional time (and date) readout on the desktop.
  Off by default; switch it on in **Settings → Desklet → CLOCK**. Drag it to
  place it. 12/24-hour, seconds, date, text size and colour are all there, as is
  the panel it sits on: background colour and opacity, an optional border,
  corner rounding and margin — a border with the opacity at zero gives an
  outline with the wallpaper showing through. It hides with the hedges during a
  peek. A column of four controls sits to the right of the time: a **gear** that
  opens Settings on the Desklet page, and three switches — **network** for the
  host table, **chart** for the system charts, **list** for the busiest programs
  — that show and hide each of those independently, so you can have the process
  list up with no charts, or any other combination. What you leave up is
  remembered across a restart, and a switch whose section is turned off in
  Settings hides itself rather than sitting there doing nothing.
- **Hide/show desktop icons** — safely *moves* `~/Desktop` contents to a stash
  and back (never deletes; survives logout). Bound to `Super+Shift+D`.
- **Host monitor** — a compressed table under the clock: one row per host, a
  strip of badges for the last three hours, and the latest round-trip time.
  Green means up for that whole period, orange up for part of it, red down, and
  grey *unreachable* — a host behind something that was down has not been
  proven down, so it is neither pinged nor blamed. Hosts form a tree: put a
  machine behind the router it sits behind and the whole branch greys out
  together. **Fill from traceroute** types the table for you from the route to
  any address, naming hops by the network that owns them (`Router`,
  `singnet.com.sg`, …) rather than by their address where it can.
- **System charts** — CPU, GPU, memory and network under the host table, each a
  card with its own grid — the vertical lines are **30 seconds apart** and
  travel left with the curve, so a glance tells you how long ago a spike was —
  a smooth curve of the last readings, and a live
  caption: load, clock speed, temperature. The GPU chart says which card it is
  reading (`NV` / `AMD`) because a machine can have both. Clicking the CPU card
  opens the system monitor. Under the charts, the five processes using the most
  CPU *right now* — measured as a delta, not `ps`'s lifetime average.
  Everything here is off by default and switches on per section. Settings is
  what decides whether a section *runs*; the desklet's own icons decide whether
  it is on screen.
- **Out-of-memory forecast** — the memory trend over the last 30 seconds is
  run forward; if it would run out within 5 minutes (configurable, 1–60) you
  get an urgent desktop notification naming the largest process, and the MEM
  card shows `full in 3m` in red. Nothing is said while less than half of
  memory is in use (configurable), however fast it climbs — a program loading
  gigabytes at start-up is not a leak. "Out" means 3% short of total — the desktop
  freezes before zero. On by default, and runs even with the charts switched off.
- **Memory fence** — Settings → *Protect the desktop…* asks for your password
  once and reserves memory (cgroup `MemoryMin`) for the display server,
  Cinnamon + Hedge and the system services, so a runaway program can't push
  them out of RAM and freeze the desktop. Applied live and kept across reboots
  as `fence-memory.conf` drop-ins under `/etc/systemd/system`; *Remove
  protection…* deletes exactly those. Tiles also launch each app in its own
  scope (`systemd-run --user --scope`), so an app never shares the desktop's
  reserve.
- **Claude section** — at the bottom of the desklet, toggled by a tiny ✱ in
  the control strip. Two columns: every Claude Code session on this machine
  down the left, coloured by what it is doing — green working, orange idle,
  red waiting on you (hover for why and for how long), each with its model
  tag to the left — and the readings down the right, every row the same
  shape: a label, a strip of dots, a value. The plan-usage rows (5-hour,
  week, and per-model such as Fable) fill their dots like a bar — orange past
  70%, red past 90% — and the API row's dots are time: the service status
  over the last half hour, polled every 30 s from status.anthropic.com, with
  the current verdict after it. Sessions are read from `~/.claude`; usage is
  fetched with the
  sign-in Claude Code already stores, read-only, and sent nowhere but
  `api.anthropic.com`.
- **Double-click empty desktop to peek** — hedges and desktop icons get out of
  the way until you double-click again. A click that *opens* something never
  triggers it: Hedge waits ~400 ms to see whether a window appeared, which is
  the only reliable way to tell an auto-arranged icon from bare desktop (nemo
  stores no position for icons you have never dragged).
- New hedges start **empty** — a frame to drag things into, not a set of
  starter apps to clear out first. (The one hedge a brand-new config comes with
  is still seeded, so a first run isn't a blank rectangle.)

### Settings

One navigation, down the left: **General** and **Desklet** above the list of
hedges. General is everything about the desktop that isn't drawn on it —
recalling hedges, hiding the desktop icons, snapshots, the installed build.
Desklet is everything that *is* drawn on it, in the order it appears on screen:
the clock, the host table under it, the charts under that.

## Requirements

- Rust toolchain (`rustup`).
- GTK 3 development libraries:
  ```bash
  sudo apt install build-essential pkg-config libgtk-3-dev libglib2.0-dev \
    libcairo2-dev libgdk-pixbuf-2.0-dev libayatana-appindicator3-dev
  ```
- An **X11** session (drag-and-drop from the file manager uses XDND; the
  desktop-widget window hints are X11-oriented). Developed on Linux Mint /
  Cinnamon (Muffin).

## Build & run

```bash
cargo build --release
./target/release/hedge            # run in the foreground (Ctrl-C to stop)
```

Command-line flags (a second invocation forwards to the running instance):

| Flag | Effect |
|---|---|
| `--settings` | open the settings window |
| `--add` | add a new hedge at the pointer |
| `--recall` | bring every hedge back onto the primary monitor |
| `--save-desktop` | snapshot all hedges + desktop icon positions |
| `--toggle-desktop` / `--hide-desktop` / `--show-desktop` | desktop icons |

Install-related flags are handled locally (never forwarded, and they work
without a display): `--install`, `--uninstall [--purge]`, `--status`,
`--version`, `--help`, `--log`.

### When it disappears: the log

Hedge runs from autostart with no terminal, so anything it printed used to go
nowhere. It now keeps a log at **`~/.local/state/hedge/hedge.log`**
(`hedge --log` prints it; `--status` shows the path and size). It rotates at
1 MB, keeping one previous file as `hedge.log.1`.

What lands in it: startup (version, pid, binary path, `DISPLAY`), every warning
Hedge itself emits, **GTK/GLib warnings and criticals** — normally invisible and
usually the last thing said before the widget layer misbehaves — and **Rust
panics with a backtrace**, captured whether or not `RUST_BACKTRACE` is set.

Reading it, the key is the last line of a session:

| Last line | What happened |
|---|---|
| `--- session ended (exit 0) ---` | quit normally |
| `received SIGTERM — shutting down` then session ended | logged out, or something killed it |
| a `PANIC` line + backtrace | Rust crash — the backtrace names the spot |
| stops mid-stream with no ending | died hard (a segfault in GTK's C code takes the process out before any Rust hook can run) — the last lines say what it was doing |

`HEDGE_DEBUG=1` adds GLib's debug chatter, which is off by default because it
buries everything else.

### Run a dev build alongside another instance

The app is single-instance per D-Bus name (`org.hedge.Hedge`). To run a dev
build next to an already-running one, give it a different id and a throwaway
config:

```bash
XDG_CONFIG_HOME=/tmp/hedge-test HEDGE_APP_ID=org.hedge.HedgeDev \
  ./target/release/hedge
```

Without that, a second launch forwards its command line to the instance that
already owns the D-Bus name and exits 0 — your build never runs. A bare launch
now **asks** instead of exiting silently:

> **Hedge is already running** — `[Cancel]` `[Close it and start this one]`

Taking over asks the running instance to quit (via a `quit` GAction that
GApplication exports over D-Bus), then re-execs this binary once the name is
free. It can't just carry on in-process: GApplication has already registered it
as the remote end, and there is no way back from that. If the name is *still*
held on the second try you get a "quit it from the tray" message rather than two
copies bouncing off each other forever. Flagged launches (`--settings`, `--add`,
…) are meant to be forwarded and go through untouched.

RustRover run configurations are checked in under `.idea/runConfigurations/`:

| Configuration | Cargo command |
|---|---|
| Build (release) | `build --release` |
| Run (release) | `run --release` — real config; hands off if a Hedge is running |
| **Run (release, dev instance)** | `run --release` with `HEDGE_APP_ID` + throwaway `XDG_CONFIG_HOME` — the one to develop with |
| Install (release) | `run --release -- --install` |

## Install

Everything goes in your home directory — **no root, no package manager**.

There is no install script. The binary installs *itself*, so there is exactly
one list of the files an install owns — in `src/install.rs`:

```bash
cargo build --release
./target/release/hedge --install      # install (or upgrade) this build
./target/release/hedge --status       # installed version vs. this build
./target/release/hedge --uninstall    # remove it (--purge also drops the config)
```

| Path | What it is |
|---|---|
| `~/.local/share/hedge/hedge` | the binary |
| `~/.local/share/hedge/install.json` | the manifest — version, build time, file list |
| `~/.local/bin/hedge` | symlink, so `hedge` works in a terminal |
| `~/.config/autostart/hedge.desktop` | start at login |
| `~/.local/share/applications/hedge-settings.desktop` | "Hedge Settings" in the menu |
| `~/.local/share/nemo/actions/hedge.nemo_action` | desktop right-click → **Add a Hedge** |
| `~/.local/share/nemo/actions/hedge-toggle-desktop.nemo_action` | desktop right-click → toggle icons |
| `~/.local/share/icons/hicolor/scalable/apps/hedge.svg` | the icon |

Uninstall removes exactly that set (including both right-click actions), and
first puts back any desktop icons that were hidden into Hedge's stash — the
only thing that knows how to un-hide them is the app being removed. Your
config and backups survive unless you pass `--purge`.

### Versions and upgrades

Every binary is stamped with the UTC time it was built (`build.rs`), so two
copies with the same `3.0.0` version can still be ordered: **semver first,
build time as the tie-break**. That's what powers:

- **at startup** — a build that isn't the installed one asks whether to
  **Install & restart** or **Run standalone**, or offers to upgrade the
  installed copy when this build is newer. One dialog, one decision:
  accepting installs *and* restarts into the new build, with no second
  prompt. Standalone is a first-class answer: Hedge runs the same, it just
  registers nothing. The installed daemon never prompts, "Don't ask again"
  is remembered, and a dev instance (`HEDGE_APP_ID` set) never asks at all.
- **Settings → General → VERSION** — this build, the installed build, and
  Install / Upgrade / Reinstall / Uninstall buttons.
- `hedge --status` for the same thing on the command line.

### Building a distributable installer

```bash
./scripts/build-release.sh
```

Runs the tests, builds `--release`, and writes to `dist/`:

- `hedge-<version>-<stamp>-<arch>-linux.tar.gz` — the binary, which self-installs
- `hedge-<version>-<stamp>-<arch>-linux.run` — **one-file self-extracting
  installer**; copy it anywhere and run it. It forwards flags, so
  `./hedge-….run --uninstall` works too.
- `SHA256SUMS`

`SOURCE_DATE_EPOCH` pins the stamp baked into the binary to the one in the file
name. `.github/workflows/release.yml` runs the same script on every push and
attaches these artifacts to a `v*` tag release.

## Project layout

```
Cargo.toml
src/
├── main.rs            # entrypoint + module map
├── app.rs             # HedgeApp: shared state, lifecycle, tray, management
├── hedge_window.rs    # the desktop widget (grid, DnD, collapse/slide, resize)
├── settings_window.rs # master-detail settings dialog
├── clock.rs           # the clock desklet + the panel under it
├── ping.rs            # host monitoring; traceroute fill (no GTK)
├── sysmon.rs          # CPU/GPU/memory/network/process sampling (no GTK)
├── sysmon_view.rs     # the mini charts, drawn with cairo
├── lock.rs            # single-instance flock
├── snapshot.rs        # save/restore the whole desktop layout
├── desktop_click.rs   # double-click-the-desktop peek (X RECORD)
├── config.rs          # ~/.config/hedge/config.json + backups
├── colors.rs          # colour → rgba() helpers
├── desktop_apps.rs    # scan installed apps; parse dropped URIs
├── desktop.rs         # hide/show desktop icons (safe stash move)
├── install.rs         # self-install/upgrade/uninstall into $HOME
├── version.rs         # build stamping + "is this newer?" comparison
├── log.rs             # ~/.local/state/hedge/hedge.log: panics, GTK criticals
└── style.css          # base styling (per-hedge colours injected at runtime)
build.rs               # stamps the build time/git hash into the binary
assets/icons/hedge.svg # tray icon (embedded via include_bytes!)
scripts/               # build-release.sh + the self-extracting header
docs/                  # ARCHITECTURE.md, HANDOFF.md
reference/             # the original Python app, for behaviour comparison
```

## Config

Everything lives in one hand-editable file: `~/.config/hedge/config.json`.
The schema is byte-compatible with the original Python version, so both can
read each other's config.

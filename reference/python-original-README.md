# Fence

A zen, drag-friendly **app launcher** for the Linux desktop. Fences are slim,
translucent widgets that sit on your desktop holding a grid of apps. **Drag a
desktop icon, a menu app, or a file straight onto a Fence to add it.** Drag a
Fence to a screen edge and it rolls up to a name handle until you hover it.

Built as a standalone **GTK 3** app (Python), so it's a real drag-and-drop target
— unlike a Cinnamon desklet, which can't accept drops from the file manager.

## Requirements

- `python3-gi` + GTK 3 (preinstalled on Linux Mint). If missing:
  ```bash
  sudo apt install python3-gi gir1.2-gtk-3.0
  ```
- X11 session (drag-and-drop from nemo-desktop uses XDND).

## Install

```bash
./install.sh
./.local/share/fence/app/fence &   # or just log out/in — it autostarts
```

`install.sh` puts the app in `~/.local/share/fence/app`, seeds
`~/.config/fence/config.json`, and registers:
- an **autostart** entry (Fences appear on login),
- a **Fence Settings** launcher in your app menu,
- a desktop right-click **"Add a Fence"** (Nemo action).

## Using it

- **Add apps** — drag onto a Fence from anywhere:
  - an app from the **menu**, a **desktop icon**, or a **file/folder** from Files.
  - `.desktop` launchers run the app; plain files open with `xdg-open`.
  - or use **Fence Settings → Apps → +** to pick from installed apps.
- **Launch** — click a tile. **Remove** — right-click a tile.
- **Move a Fence** — drag it by its title/background (no window border).
- **Add a Fence** — right-click the desktop → *Add a Fence* (drops at the
  pointer), or *Fence Settings → +*.
- **Collapse to an edge** — drag a Fence within ~48px of a screen edge; it
  becomes a name handle. Hover (or click, per-Fence setting) to roll it out.
- **Settings** — open *Fence Settings*: pick a Fence on the left; set its accent
  & background colour, transparency, corner smoothness, columns, icon size,
  spacing, padding, and toggles on the right. Changes apply live.

Everything lives in one file you can also hand-edit:
`~/.config/fence/config.json`.

## Architecture

```
            Fence Settings (GTK window)
                     │ writes
                     ▼
        ~/.config/fence/config.json   ◄── single source of truth
                     │ read
                     ▼
        FenceApp (one process)  ──►  Fence windows: Work · Media · Dev
        (autostarts, single-instance; --settings / --add)
```

- One process (`FenceApp`, a `Gtk.Application`) owns the config and every Fence
  window. A second invocation just sends `--settings` / `--add` to it.
- Each Fence is an undecorated, translucent, keep-below, all-workspaces window.

## Source layout

```
app/
├── fence                       # launcher (python3 -m fence_app)
└── fence_app/
    ├── app.py                  # Gtk.Application: owns config + fence windows
    ├── fence_window.py         # the desktop widget: grid, drag-in, collapse
    ├── settings_window.py      # master-detail settings
    ├── config.py               # ~/.config/fence/config.json
    ├── desktop_apps.py         # scan installed apps; parse dropped URIs
    ├── colors.py               # colour → rgba() helpers
    └── style.css               # base styling (per-fence colours injected live)
install.sh · README.md
```

The logic is split into small modules around a language-agnostic JSON config, so
a future port to **C++/gtkmm** is a mechanical rewrite against the same GTK APIs.

## Retiring the old Cinnamon version

This repo also contains an earlier **Cinnamon desklet/applet** prototype
(`applet/`, `desklet/`, `nemo-action/`, `default-config.json`). It's superseded
by the GTK app (the desklet couldn't accept desktop-icon drags). To remove it if
you installed it earlier:

```bash
rm -rf ~/.local/share/cinnamon/applets/fence@targhan \
       ~/.local/share/cinnamon/desklets/fence@targhan
# then remove the Fence applet/desklet from the panel/desktop via Cinnamon
```

## Uninstall

```bash
rm -rf ~/.local/share/fence \
       ~/.config/autostart/fence.desktop \
       ~/.local/share/applications/fence-settings.desktop \
       ~/.local/share/nemo/actions/fence.nemo_action \
       ~/.config/fence
pkill -f 'fence_app' || true
```

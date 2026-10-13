"""Gtk.Application that owns the config and all Fence widgets, plus the
single-instance command-line actions (--settings, --add)."""
import os
import sys

import gi
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk, Gio, GLib

from . import config
from .fence_window import FenceWindow
from .settings_window import SettingsWindow

APP_ID = "org.fence.Fence"


class FenceApp(Gtk.Application):
    def __init__(self):
        super().__init__(application_id=APP_ID,
                         flags=Gio.ApplicationFlags.HANDLES_COMMAND_LINE)
        self.cfg = None
        self.fences = {}          # name -> FenceWindow
        self._settings = None
        self._indicator = None
        self._status_icon = None
        self._tray_menu = None
        self.pending_move = None      # (src_fence, index, dst_fence) mid-drag
        self.add_main_option("settings", 0, GLib.OptionFlags.NONE,
                             GLib.OptionArg.NONE, "Open the settings window", None)
        self.add_main_option("add", 0, GLib.OptionFlags.NONE,
                             GLib.OptionArg.NONE, "Add a new Fence", None)

    # -- lifecycle -----------------------------------------------------
    def do_startup(self):
        Gtk.Application.do_startup(self)
        self.cfg = config.load()
        self._repair_positions()
        self._load_base_css()
        self._build_tray()
        self.hold()               # stay alive as a desktop daemon

    def do_activate(self):
        if not self.fences:
            self.sync_fences()

    def do_command_line(self, command_line):
        self.activate()
        opts = command_line.get_options_dict()
        if opts.contains("add"):
            self.add_fence(at_pointer=True)
        if opts.contains("settings"):
            self.open_settings()
        return 0

    def _load_base_css(self):
        provider = Gtk.CssProvider()
        path = os.path.join(os.path.dirname(__file__), "style.css")
        try:
            provider.load_from_path(path)
            Gtk.StyleContext.add_provider_for_screen(
                Gdk.Screen.get_default(), provider,
                Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        except GLib.Error as e:
            sys.stderr.write("fence: css load failed: %s\n" % e)

    # -- system tray icon ----------------------------------------------
    def _build_tray(self):
        menu = Gtk.Menu()

        def add_item(label, cb):
            mi = Gtk.MenuItem(label=label)
            mi.connect("activate", lambda *_a: cb())
            menu.append(mi)

        title = Gtk.MenuItem(label="Fences")
        title.set_sensitive(False)
        title.get_child().set_markup("<b>Fences</b>")
        menu.append(title)
        menu.append(Gtk.SeparatorMenuItem())
        add_item("Add a Fence", self.add_fence)
        add_item("Config…", self.open_settings)
        add_item("About…", self.show_about)
        menu.append(Gtk.SeparatorMenuItem())
        add_item("Quit", self.quit)
        menu.show_all()
        self._tray_menu = menu

        icons_dir = os.path.join(os.path.dirname(__file__), "icons")
        icon_file = os.path.join(icons_dir, "fence.svg")

        # Prefer AppIndicator (shown by Cinnamon's xapp-status applet); fall
        # back to the legacy XEmbed status icon.
        try:
            gi.require_version("AyatanaAppIndicator3", "0.1")
            from gi.repository import AyatanaAppIndicator3 as AI
            ind = AI.Indicator.new("fence", "fence",
                                   AI.IndicatorCategory.APPLICATION_STATUS)
            if os.path.isdir(icons_dir):
                ind.set_icon_theme_path(icons_dir)
            ind.set_status(AI.IndicatorStatus.ACTIVE)
            ind.set_title("Fences")
            ind.set_icon_full("fence", "Fences")
            ind.set_menu(menu)
            self._indicator = ind
            return
        except Exception as e:
            sys.stderr.write("fence: appindicator unavailable (%s); "
                             "trying StatusIcon\n" % e)

        try:
            if os.path.exists(icon_file):
                si = Gtk.StatusIcon.new_from_file(icon_file)
            else:
                si = Gtk.StatusIcon.new_from_icon_name("preferences-desktop")
            si.set_title("Fences")
            si.set_tooltip_text("Fences")
            si.connect("activate", lambda _i: self.open_settings())
            si.connect("popup-menu",
                       lambda _i, button, time: menu.popup(
                           None, None, None, None, button, time))
            self._status_icon = si
        except Exception as e:
            sys.stderr.write("fence: no tray icon available (%s)\n" % e)

    # -- fence management ----------------------------------------------
    def sync_fences(self):
        """Make the live Fence windows match the config."""
        names = set(config.fence_names(self.cfg))
        for name in list(self.fences):
            if name not in names:
                self.fences.pop(name).destroy()
        for name in config.fence_names(self.cfg):
            block = self.cfg["fences"][name]
            if name in self.fences:
                w = self.fences[name]
                w.name = name
                w.block = block
                w.render()
            else:
                self.fences[name] = FenceWindow(self, name, block)

    def refresh_fence(self, name, mode="render"):
        w = self.fences.get(name)
        if w is None:
            return
        w.block = self.cfg["fences"][name]
        w.name = name
        if mode == "style":
            w.restyle()          # CSS only — no layout churn
        elif mode == "layout":
            w.relayout()         # animated tile reflow
        else:
            w.render()           # full rebuild

    def rename_fence_live(self, old, new):
        """Rename a fence from anywhere (config sidebar or the fence's own
        title), keeping the live window and open settings in sync."""
        new = (new or "").strip()
        if not new or new == old:
            return False
        if not config.rename_fence(self.cfg, old, new):
            return False            # empty or duplicate name
        self.save()
        w = self.fences.pop(old, None)
        if w is not None:
            self.fences[new] = w
            w.name = new
            w.block = self.cfg["fences"][new]
            w.set_title(new)
            w.render()
        if self._settings is not None and self._settings.get_visible():
            self._settings.on_fences_changed(select=new)
        return True

    def finish_move(self, src, index, dst, insert_at=None):
        """Complete a tile drag from fence `src` to fence `dst` (may be the
        same fence — that's a reorder), inserting at slot `insert_at`."""
        try:
            src_apps = self.cfg["fences"][src]["apps"]
            entry = src_apps.pop(index)
        except (KeyError, IndexError):
            return

        if src == dst:
            pos = len(src_apps) if insert_at is None else insert_at
            if pos > index:
                pos -= 1              # account for the popped entry
            src_apps.insert(max(0, min(pos, len(src_apps))), entry)
            self.save()
            self.refresh_fence(src)
            return

        if dst not in self.cfg["fences"]:
            src_apps.insert(index, entry)      # destination vanished; revert
            return
        dst_apps = self.cfg["fences"][dst].setdefault("apps", [])
        pos = len(dst_apps) if insert_at is None else min(insert_at, len(dst_apps))
        dst_apps.insert(pos, entry)
        self.save()
        self.refresh_fence(src)
        self.refresh_fence(dst)

    def add_fence(self, at_pointer=False):
        name = config.add_fence(self.cfg, seed_starter=True)
        x, y = self._default_spot()
        if at_pointer:
            pos = self._pointer()
            if pos:
                x, y = pos[0] - 12, pos[1] - 12
        x, y = self._clamp_to_monitors(x, y)
        self.cfg["fences"][name]["x"] = x
        self.cfg["fences"][name]["y"] = y
        self.save()
        self.sync_fences()
        return name

    # -- monitor-aware positioning -------------------------------------
    def _monitor_rects(self):
        display = Gdk.Display.get_default()
        return [display.get_monitor(i).get_geometry()
                for i in range(display.get_n_monitors())]

    def _default_spot(self):
        display = Gdk.Display.get_default()
        mon = display.get_primary_monitor() or display.get_monitor(0)
        g = mon.get_geometry()
        return g.x + 80, g.y + 80

    def _clamp_to_monitors(self, x, y):
        rects = self._monitor_rects()
        if not rects:
            return max(0, x), max(0, y)
        if any(r.x <= x < r.x + r.width and r.y <= y < r.y + r.height
               for r in rects):
            return x, y
        # outside every monitor -> snap onto the primary one
        return self._default_spot()

    def _repair_positions(self):
        """Nudge floating fences that ended up outside every monitor back
        into view. Docked fences are left alone (their edge snap owns their
        position), and repairs CLAMP to the nearest monitor instead of
        teleporting — a mis-detected display scale must not scatter fences."""
        changed = False
        rects = self._monitor_rects()
        if not rects:
            return
        min_x = min(r.x for r in rects)
        max_x = max(r.x + r.width for r in rects)
        min_y = min(r.y for r in rects)
        max_y = max(r.y + r.height for r in rects)
        for block in self.cfg.get("fences", {}).values():
            if block.get("edge"):
                continue
            x, y = int(block.get("x", 120)), int(block.get("y", 120))
            nx = max(min_x, min(x, max_x - 60))
            ny = max(min_y, min(y, max_y - 60))
            if (nx, ny) != (x, y):
                block["x"], block["y"] = nx, ny
                changed = True
        if changed:
            self.save()

    def _pointer(self):
        try:
            seat = Gdk.Display.get_default().get_default_seat()
            _screen, x, y = seat.get_pointer().get_position()
            return int(x), int(y)
        except Exception:
            return None

    # -- shared services for windows -----------------------------------
    def save(self):
        config.save(self.cfg)

    def launch(self, command):
        if not command:
            return
        try:
            GLib.spawn_command_line_async(command)
        except GLib.Error:
            try:
                GLib.spawn_async(["/bin/sh", "-c", command], [], None,
                                 GLib.SpawnFlags.SEARCH_PATH, None, None)
            except GLib.Error as e:
                sys.stderr.write("fence: launch failed: %s (%s)\n" % (command, e))

    def open_settings(self):
        if self._settings is None:
            self._settings = SettingsWindow(self)
        self._settings.show_all()
        self._settings.present()

    def show_about(self):
        from . import __version__
        dlg = Gtk.AboutDialog()
        dlg.set_program_name("Fences")
        dlg.set_version(__version__)
        dlg.set_comments("A zen, drag-friendly app launcher for your desktop.\n"
                         "Drag apps in, drag them out, fence them off.")
        dlg.set_logo_icon_name("preferences-desktop")
        dlg.set_keep_above(True)
        dlg.set_position(Gtk.WindowPosition.CENTER)
        dlg.connect("response", lambda d, _r: d.destroy())
        dlg.show()


def main(argv=None):
    GLib.set_prgname("fence")
    try:
        Gdk.set_program_class("Fence")
    except Exception:
        pass
    return FenceApp().run(argv if argv is not None else sys.argv)

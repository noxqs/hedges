"""Master-detail settings: a sidebar of Fences on the left, the selected
Fence's appearance + apps on the right. Edits save immediately and refresh the
live Fence widgets.
"""
import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk, Gdk, GdkPixbuf

from . import config, desktop_apps


def _hex_to_rgba(h):
    rgba = Gdk.RGBA()
    rgba.parse(h if h else "#000000")
    return rgba


def _rgba_to_hex(rgba):
    return "#%02x%02x%02x" % (int(rgba.red * 255 + 0.5),
                              int(rgba.green * 255 + 0.5),
                              int(rgba.blue * 255 + 0.5))


class SettingsWindow(Gtk.Window):
    def __init__(self, app):
        super().__init__(title="Fence Settings")
        self._app = app
        self._current = None
        self._loading = False

        self.set_default_size(760, 540)
        self.get_style_context().add_class("fence-settings")
        self.connect("delete-event", self._on_close)

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.add(outer)

        stack = Gtk.Stack()
        stack.set_transition_type(Gtk.StackTransitionType.SLIDE_LEFT_RIGHT)
        stack.set_transition_duration(150)
        switcher = Gtk.StackSwitcher()
        switcher.set_stack(stack)
        switcher.set_halign(Gtk.Align.CENTER)
        switcher.set_margin_top(8)
        switcher.set_margin_bottom(4)
        outer.pack_start(switcher, False, False, 0)
        outer.pack_start(stack, True, True, 0)

        root = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        stack.add_titled(root, "fences", "Fences")
        stack.add_titled(self._build_general_page(), "general", "General")

        root.pack_start(self._build_sidebar(), False, False, 0)
        sep = Gtk.Separator(orientation=Gtk.Orientation.VERTICAL)
        root.pack_start(sep, False, False, 0)

        self._detail_scroll = Gtk.ScrolledWindow()
        self._detail_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self._detail_scroll.set_hexpand(True)
        root.pack_start(self._detail_scroll, True, True, 0)

        self._reload_sidebar(select_first=True)

    def _build_general_page(self):
        """App-wide options — intentionally empty for now; features land here."""
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_valign(Gtk.Align.CENTER)
        title = Gtk.Label(label="GENERAL")
        title.get_style_context().add_class("fence-section-title")
        hint = Gtk.Label(label="Nothing here yet — app-wide options will land here.")
        hint.get_style_context().add_class("fence-dim")
        box.pack_start(title, False, False, 0)
        box.pack_start(hint, False, False, 0)
        return box

    # ------------------------------------------------------------------
    # sidebar
    # ------------------------------------------------------------------
    def _build_sidebar(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        box.get_style_context().add_class("fence-sidebar")
        box.set_size_request(200, -1)

        title = Gtk.Label(label="FENCES", xalign=0)
        title.get_style_context().add_class("fence-section-title")
        box.pack_start(title, False, False, 0)

        self._list = Gtk.ListBox()
        self._list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._list.connect("row-selected", self._on_row_selected)
        # double-click a fence to rename it
        self._list.set_activate_on_single_click(False)
        self._list.connect("row-activated", lambda _l, _r: self._on_rename(None))
        scroll = Gtk.ScrolledWindow()
        scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scroll.set_vexpand(True)
        scroll.add(self._list)
        box.pack_start(scroll, True, True, 0)

        btns = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        btns.set_homogeneous(True)
        for icon, tip, cb in (
            ("list-add-symbolic", "Add Fence", self._on_add),
            ("list-remove-symbolic", "Remove Fence", self._on_remove),
            ("document-edit-symbolic", "Rename Fence", self._on_rename),
        ):
            b = Gtk.Button.new_from_icon_name(icon, Gtk.IconSize.BUTTON)
            b.set_tooltip_text(tip)
            b.connect("clicked", cb)
            btns.pack_start(b, True, True, 0)
        box.pack_start(btns, False, False, 6)
        return box

    def _reload_sidebar(self, select_first=False, select_name=None):
        for child in self._list.get_children():
            self._list.remove(child)
        names = config.fence_names(self._app.cfg)
        for name in names:
            row = Gtk.ListBoxRow()
            lbl = Gtk.Label(label=name, xalign=0)
            row.add(lbl)
            row.fence_name = name
            self._list.add(row)
        self._list.show_all()
        target = select_name or (names[0] if (select_first and names) else None)
        if target:
            for row in self._list.get_children():
                if row.fence_name == target:
                    self._list.select_row(row)
                    break

    def _on_row_selected(self, _list, row):
        if row is None:
            self._current = None
            self._set_detail(Gtk.Label(label="No Fence selected"))
            return
        self._current = row.fence_name
        self._build_detail(row.fence_name)

    # ------------------------------------------------------------------
    # sidebar actions
    # ------------------------------------------------------------------
    def _on_add(self, _b):
        name = config.add_fence(self._app.cfg, seed_starter=True)
        self._app.save()
        self._app.sync_fences()
        self._reload_sidebar(select_name=name)

    def _on_remove(self, _b):
        if not self._current:
            return
        dlg = Gtk.MessageDialog(
            transient_for=self, modal=True, buttons=Gtk.ButtonsType.OK_CANCEL,
            text="Remove Fence “%s”?" % self._current)
        if dlg.run() == Gtk.ResponseType.OK:
            config.remove_fence(self._app.cfg, self._current)
            self._app.save()
            self._app.sync_fences()
            self._current = None
            self._reload_sidebar(select_first=True)
        dlg.destroy()

    def _on_rename(self, _b):
        if not self._current:
            return
        new = self._prompt_text("Rename Fence", "New name:", self._current)
        if new:
            self._app.rename_fence_live(self._current, new)

    def on_fences_changed(self, select=None):
        """Called by the app when fences were changed elsewhere (e.g. a
        fence was renamed from its own title)."""
        self._reload_sidebar(select_name=select, select_first=select is None)

    def _prompt_text(self, title, label, initial=""):
        dlg = Gtk.Dialog(title=title, transient_for=self, modal=True)
        dlg.add_buttons("Cancel", Gtk.ResponseType.CANCEL, "OK", Gtk.ResponseType.OK)
        box = dlg.get_content_area()
        box.set_spacing(8)
        box.set_border_width(12)
        box.add(Gtk.Label(label=label, xalign=0))
        entry = Gtk.Entry()
        entry.set_text(initial)
        entry.set_activates_default(True)
        dlg.set_default_response(Gtk.ResponseType.OK)
        box.add(entry)
        dlg.show_all()
        result = entry.get_text().strip() if dlg.run() == Gtk.ResponseType.OK else None
        dlg.destroy()
        return result

    # ------------------------------------------------------------------
    # detail panel
    # ------------------------------------------------------------------
    def _set_detail(self, widget):
        old = self._detail_scroll.get_child()
        if old is not None:
            self._detail_scroll.remove(old)
        self._detail_scroll.add(widget)
        self._detail_scroll.show_all()

    # which refresh a key needs on the live fence
    _STYLE_KEYS = {"accent", "bgColor", "bgOpacity", "cornerRadius",
                   "padding", "showBackground", "expandOnHover"}
    _LAYOUT_KEYS = {"columns", "tileSpacing"}

    def _block(self):
        return self._app.cfg["fences"][self._current]

    def _changed(self, key=None):
        if self._loading or not self._current:
            return
        self._app.save()
        if key in self._STYLE_KEYS:
            mode = "style"
        elif key in self._LAYOUT_KEYS:
            mode = "layout"
        else:
            mode = "render"
        self._app.refresh_fence(self._current, mode)

    def _build_detail(self, name):
        self._loading = True
        b = self._app.cfg["fences"][name]

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        outer.get_style_context().add_class("fence-detail")

        grid = Gtk.Grid(column_spacing=12, row_spacing=10)
        r = 0

        def add_row(label, widget):
            nonlocal r
            lab = Gtk.Label(label=label, xalign=0)
            grid.attach(lab, 0, r, 1, 1)
            grid.attach(widget, 1, r, 1, 1)
            r += 1

        # colours
        accent = Gtk.ColorButton.new_with_rgba(_hex_to_rgba(b.get("accent")))
        accent.connect("color-set", lambda w: (b.__setitem__(
            "accent", _rgba_to_hex(w.get_rgba())), self._changed("accent")))
        add_row("Accent colour", accent)

        bgc = Gtk.ColorButton.new_with_rgba(_hex_to_rgba(b.get("bgColor")))
        bgc.connect("color-set", lambda w: (b.__setitem__(
            "bgColor", _rgba_to_hex(w.get_rgba())), self._changed("bgColor")))
        add_row("Background colour", bgc)

        add_row("Transparency", self._slider(b, "bgOpacity", 0, 1, 0.05, pct=True))
        add_row("Corner smoothness", self._slider(b, "cornerRadius", 0, 60, 1))
        add_row("Columns", self._slider(b, "columns", 1, 12, 1))
        add_row("Icon size", self._slider(b, "iconSize", 16, 128, 2))
        add_row("Tile spacing", self._slider(b, "tileSpacing", 0, 40, 1))
        add_row("Padding", self._slider(b, "padding", 0, 60, 1))
        add_row("Show labels", self._switch(b, "showLabels"))
        add_row("Show title", self._switch(b, "showTitle"))
        add_row("Show background", self._switch(b, "showBackground"))
        add_row("Open on hover (when collapsed)", self._switch(b, "expandOnHover"))

        outer.pack_start(grid, False, False, 0)

        sect = Gtk.Label(label="APPS", xalign=0)
        sect.get_style_context().add_class("fence-section-title")
        outer.pack_start(sect, False, False, 0)
        outer.pack_start(self._build_apps(b), True, True, 0)

        self._set_detail(outer)
        self._loading = False

    def _slider(self, b, key, lo, hi, step, pct=False):
        """A slider with a small synced text field beside it."""
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        adj = Gtk.Adjustment(value=float(b.get(key, lo)), lower=lo, upper=hi,
                             step_increment=step, page_increment=step)
        scale = Gtk.Scale(orientation=Gtk.Orientation.HORIZONTAL, adjustment=adj)
        scale.set_hexpand(True)
        scale.set_size_request(160, -1)
        scale.set_draw_value(False)
        entry = Gtk.Entry()
        entry.set_width_chars(4)
        entry.set_max_width_chars(4)
        entry.set_alignment(1.0)
        guard = [False]

        def fmt(v):
            return str(int(round(v * 100))) if pct else str(int(round(v)))

        entry.set_text(fmt(adj.get_value()))

        def on_scale(_w):
            if guard[0]:
                return
            guard[0] = True
            entry.set_text(fmt(adj.get_value()))
            guard[0] = False
            b[key] = round(adj.get_value(), 3) if pct else int(adj.get_value())
            self._changed(key)

        def on_entry(_w, *_a):
            try:
                v = float(entry.get_text())
            except ValueError:
                entry.set_text(fmt(adj.get_value()))
                return False
            v = v / 100.0 if pct else v
            v = max(lo, min(hi, v))
            guard[0] = True
            adj.set_value(v)
            guard[0] = False
            entry.set_text(fmt(v))
            b[key] = round(v, 3) if pct else int(v)
            self._changed(key)
            return False

        scale.connect("value-changed", on_scale)
        entry.connect("activate", on_entry)
        entry.connect("focus-out-event", on_entry)
        box.pack_start(scale, True, True, 0)
        box.pack_start(entry, False, False, 0)
        return box

    def _switch(self, b, key):
        sw = Gtk.Switch()
        sw.set_halign(Gtk.Align.START)
        sw.set_active(bool(b.get(key, True)))

        def on_toggle(_w, _state):
            b[key] = sw.get_active()
            self._changed(key)
            return False
        sw.connect("state-set", on_toggle)
        return sw

    # ------------------------------------------------------------------
    # apps editor
    # ------------------------------------------------------------------
    def _build_apps(self, b):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        self._apps_list = Gtk.ListBox()
        self._apps_list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        frame = Gtk.Frame()
        frame.add(self._apps_list)
        box.pack_start(frame, True, True, 0)
        self._reload_apps(b)

        bar = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        for icon, tip, cb in (
            ("list-add-symbolic", "Add app", self._on_app_add),
            ("list-remove-symbolic", "Remove app", self._on_app_remove),
            ("go-up-symbolic", "Move up", self._on_app_up),
            ("go-down-symbolic", "Move down", self._on_app_down),
        ):
            btn = Gtk.Button.new_from_icon_name(icon, Gtk.IconSize.BUTTON)
            btn.set_tooltip_text(tip)
            btn.connect("clicked", cb)
            bar.pack_start(btn, False, False, 0)
        hint = Gtk.Label(label="  …or drag apps from the menu/desktop onto a Fence",
                         xalign=0)
        hint.get_style_context().add_class("fence-dim")
        bar.pack_start(hint, False, False, 0)
        box.pack_start(bar, False, False, 0)
        return box

    def _reload_apps(self, b):
        for child in self._apps_list.get_children():
            self._apps_list.remove(child)
        for i, app in enumerate(b.get("apps", [])):
            row = Gtk.ListBoxRow()
            row.app_index = i
            rb = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
            rb.get_style_context().add_class("fence-app-row")
            img = Gtk.Image.new_from_icon_name(
                app.get("icon") or "application-x-executable", Gtk.IconSize.LARGE_TOOLBAR)
            rb.pack_start(img, False, False, 0)
            txt = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
            nm = Gtk.Label(label=app.get("name") or app.get("command", ""), xalign=0)
            cmd = Gtk.Label(label=app.get("command", ""), xalign=0)
            cmd.get_style_context().add_class("fence-dim")
            txt.pack_start(nm, False, False, 0)
            txt.pack_start(cmd, False, False, 0)
            rb.pack_start(txt, True, True, 0)
            row.add(rb)
            self._apps_list.add(row)
        self._apps_list.show_all()

    def _selected_app_index(self):
        row = self._apps_list.get_selected_row()
        return row.app_index if row is not None else None

    def _on_app_add(self, _b):
        entry = self._app_picker()
        if entry:
            self._block().setdefault("apps", []).append(entry)
            self._changed()
            self._reload_apps(self._block())

    def _on_app_remove(self, _b):
        idx = self._selected_app_index()
        if idx is None:
            return
        apps = self._block().get("apps", [])
        if 0 <= idx < len(apps):
            apps.pop(idx)
            self._changed()
            self._reload_apps(self._block())

    def _move(self, delta):
        idx = self._selected_app_index()
        if idx is None:
            return
        apps = self._block().get("apps", [])
        j = idx + delta
        if 0 <= j < len(apps):
            apps[idx], apps[j] = apps[j], apps[idx]
            self._changed()
            self._reload_apps(self._block())

    def _on_app_up(self, _b):
        self._move(-1)

    def _on_app_down(self, _b):
        self._move(1)

    def _app_picker(self):
        dlg = Gtk.Dialog(title="Add an app", transient_for=self, modal=True)
        dlg.add_buttons("Cancel", Gtk.ResponseType.CANCEL, "Add", Gtk.ResponseType.OK)
        dlg.set_default_size(420, 460)
        area = dlg.get_content_area()
        area.set_border_width(8)

        search = Gtk.SearchEntry()
        area.pack_start(search, False, False, 0)
        listbox = Gtk.ListBox()
        scroll = Gtk.ScrolledWindow()
        scroll.set_vexpand(True)
        scroll.add(listbox)
        area.pack_start(scroll, True, True, 0)

        apps = desktop_apps.scan_apps()

        def populate(term=""):
            for c in listbox.get_children():
                listbox.remove(c)
            term = term.lower()
            for a in apps:
                if term and term not in a["name"].lower():
                    continue
                row = Gtk.ListBoxRow()
                row.entry = a
                hb = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
                hb.pack_start(Gtk.Image.new_from_icon_name(
                    a["icon"], Gtk.IconSize.LARGE_TOOLBAR), False, False, 0)
                hb.pack_start(Gtk.Label(label=a["name"], xalign=0), True, True, 0)
                row.add(hb)
                listbox.add(row)
            listbox.show_all()

        populate()
        search.connect("search-changed", lambda s: populate(s.get_text()))
        listbox.connect("row-activated", lambda _l, _r: dlg.response(Gtk.ResponseType.OK))

        result = None
        if dlg.run() == Gtk.ResponseType.OK:
            row = listbox.get_selected_row()
            if row is not None:
                e = row.entry
                result = {"name": e["name"], "command": e["command"], "icon": e["icon"]}
        dlg.destroy()
        return result

    def _on_close(self, _w, _e):
        self.hide()
        return True   # keep the app alive; just hide the settings window

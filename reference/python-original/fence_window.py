"""A single Fence: an undecorated, translucent desktop widget that shows a grid
of app tiles, accepts apps dragged in from the desktop/file manager, and can be
dragged to a screen edge where it collapses to a slim name handle.
"""
import json
import os
import re
import shlex

import cairo

import gi
gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk, GdkPixbuf, Gio, GLib

from . import colors, config, desktop_apps

EDGE_THRESHOLD = 48     # px from a monitor edge that triggers collapse
LEAVE_GRACE = 250       # ms before a hovered Fence rolls back up
DRAG_SLOP = 6           # px of movement before a press counts as a drag
ANIM_MS = 140           # tile shift animation duration (ease-out)
SLIDE_MS = 160          # docked fence slide in/out duration
RESIZE_MARGIN = 8       # px grab zone along sides/corners for resizing

_EDGE_CURSORS = {
    Gdk.WindowEdge.NORTH_WEST: "nw-resize", Gdk.WindowEdge.NORTH: "n-resize",
    Gdk.WindowEdge.NORTH_EAST: "ne-resize", Gdk.WindowEdge.WEST: "w-resize",
    Gdk.WindowEdge.EAST: "e-resize", Gdk.WindowEdge.SOUTH_WEST: "sw-resize",
    Gdk.WindowEdge.SOUTH: "s-resize", Gdk.WindowEdge.SOUTH_EAST: "se-resize",
}

# custom DnD target for moving tiles between fences (same process)
TILE_TARGET = "application/x-fence-tile"
TILE_INFO = 1

_instance_counter = [0]


class FenceWindow(Gtk.Window):
    def __init__(self, app, name, block):
        super().__init__(type=Gtk.WindowType.TOPLEVEL)
        self._app = app
        self.name = name
        self.block = block

        _instance_counter[0] += 1
        self._css_class = "fence-i%d" % _instance_counter[0]

        self._collapsed = bool(block.get("edge"))
        self._edge = block.get("edge") or None
        self._expanded = False
        self._bar = None            # the docked title bar (visible sliver)
        self._dock_layout = None    # clips the panel while it slides
        self._dock_panel = None     # the panel being slid
        self._panel_size = (0, 0)
        self._panel_pos = (0, 0)    # panel offset inside the dock layout
        self._slide = None          # slide tween state
        self._slide_id = 0
        self._last_alloc = (0, 0)
        self._watch_id = 0          # pointer watcher while expanded
        self._dim_timer = 0         # dulls the collapsed label when idle
        self._vis_cache = 0         # cached collapsed-sliver thickness
        self._collapse_timer = 0
        self._press = None          # (root_x, root_y, win_x, win_y, moved)
        self._css_provider = None
        self._tiles = []            # live tile widgets, parallel to apps
        self._fixed = None          # their container (manual layout, animated)
        self._pos = {}              # widget -> current (x, y) in the container
        self._anims = {}            # widget -> (x0, y0, x1, y1, t0) tween state
        self._anim_id = 0
        self._placeholder = None
        self._hover_index = None    # insertion slot while a drag hovers us
        self._hover_timer = 0
        self._user_resized = False  # becomes True on the first resize drag
        self._layout_cols = None    # columns used by the current layout
        self._geom_timer = 0        # debounced geometry save
        self._cursor_name = None

        # --- window behaviour: a desktop widget -------------------------
        self.set_title(name)
        try:
            self.set_wmclass("fence", "Fence")
        except Exception:
            pass
        self.set_decorated(False)
        self.set_resizable(True)
        self.set_skip_taskbar_hint(True)
        self.set_skip_pager_hint(True)
        self.set_keep_below(True)
        self.set_type_hint(Gdk.WindowTypeHint.UTILITY)
        self.set_focus_on_map(False)
        self.set_accept_focus(False)
        self.stick()                # show on all workspaces
        self.get_style_context().add_class("fence-window")
        self.get_style_context().add_class(self._css_class)

        self._enable_transparency()

        # --- single event box hosts either the panel or the handle ------
        self._box = Gtk.EventBox()
        self._box.set_above_child(False)   # let tile buttons get events first
        self._box.add_events(
            Gdk.EventMask.BUTTON_PRESS_MASK
            | Gdk.EventMask.BUTTON_RELEASE_MASK
            | Gdk.EventMask.POINTER_MOTION_MASK)
        self._box.connect("button-press-event", self._on_press)
        self._box.connect("button-release-event", self._on_release)
        self._box.connect("motion-notify-event", self._on_motion)
        self.add(self._box)

        # --- drag IN: apps/files from the desktop, tiles from other fences
        self.drag_dest_set(
            Gtk.DestDefaults.ALL,
            [Gtk.TargetEntry.new(TILE_TARGET, Gtk.TargetFlags.SAME_APP, TILE_INFO)],
            Gdk.DragAction.COPY | Gdk.DragAction.MOVE)
        self.drag_dest_add_uri_targets()
        self._drag_failed = False
        self.connect("drag-data-received", self._on_drop)
        self.connect("drag-motion", self._on_drag_motion)
        self.connect("drag-leave", self._on_drag_leave)

        # --- hover expand when collapsed --------------------------------
        self.connect("enter-notify-event", self._on_enter)
        self.connect("leave-notify-event", self._on_leave)
        self.add_events(Gdk.EventMask.ENTER_NOTIFY_MASK
                        | Gdk.EventMask.LEAVE_NOTIFY_MASK)

        self.connect("size-allocate", self._on_size_allocate)
        self.connect("configure-event", self._on_configure)
        self.connect("destroy", self._on_destroy)

        self.render()
        self.move(int(block.get("x", 120)), int(block.get("y", 120)))
        if block.get("width") and block.get("height"):
            self.resize(int(block["width"]), int(block["height"]))
        self.show_all()
        if self._collapsed:
            self._show_handle()

    # ------------------------------------------------------------------
    # transparency / styling
    # ------------------------------------------------------------------
    def _enable_transparency(self):
        screen = self.get_screen()
        visual = screen.get_rgba_visual()
        if visual is not None and screen.is_composited():
            self.set_visual(visual)
        self.set_app_paintable(True)

    def _apply_css(self):
        b = self.block
        accent = b.get("accent", "#7aa2f7")
        radius = int(b.get("cornerRadius", 20))
        pad = int(b.get("padding", 18))
        bg = colors.rgba(b.get("bgColor", "#12141c"),
                         b.get("bgOpacity", 0.55) if b.get("showBackground", True) else 0)
        css = """
        .%(cls)s .fence-panel {
            background-color: %(bg)s;
            border-radius: %(panel_radius)s;
            padding: 0;
            border: 2px solid transparent;
            transition: border-color 150ms ease;
        }
        .%(cls)s.fence-hover .fence-panel { border-color: %(bord)s; }
        .%(cls)s .fence-header {
            background-color: %(accent)s;
            border-radius: %(hradius)dpx %(hradius)dpx 0 0;
            padding: 1px 12px;
        }
        .%(cls)s .fence-title { color: %(titletxt)s; }
        .%(cls)s .fence-body { padding: %(pad)dpx; }
        .%(cls)s .fence-tile {
            border: 2px solid transparent;
            border-radius: %(radius)dpx;
            background-color: transparent;
        }
        .%(cls)s .fence-tile:hover {
            border-color: %(accent)s;
            background-color: %(hover)s;
        }
        .%(cls)s .fence-dockbar {
            background-color: %(accent)s;
            border-radius: %(bar_radius)s;
            padding: %(bar_pad)s;
            opacity: 1;
            transition: opacity 500ms ease;
        }
        .%(cls)s.fence-dim .fence-dockbar { opacity: 0.35; }
        .%(cls)s .fence-placeholder {
            border: 2px dashed %(ph)s;
            border-radius: %(radius)dpx;
            background-color: %(hover)s;
        }
        """
        # docked: fence + title bar are rounded on the interior-facing side
        # only, flat against the screen edge — one continuous shape
        hr = max(0, radius - 2)
        bar_radius = {
            "left":   "0 %dpx %dpx 0" % (hr, hr),   # bar sits on the right
            "right":  "%dpx 0 0 %dpx" % (hr, hr),   # bar sits on the left
            "top":    "0 0 %dpx %dpx" % (hr, hr),   # bar sits at the bottom
            "bottom": "%dpx %dpx 0 0" % (hr, hr),   # bar sits at the top
        }.get(self._edge, "%dpx" % hr)
        panel_radius = {
            "left":   "0 %dpx %dpx 0" % (radius, radius),
            "right":  "%dpx 0 0 %dpx" % (radius, radius),
            "top":    "0 0 %dpx %dpx" % (radius, radius),
            "bottom": "%dpx %dpx 0 0" % (radius, radius),
        }.get(self._edge, "%dpx" % radius)
        bar_pad = "12px 1px" if self._edge in ("left", "right") else "1px 12px"
        css = css % {
            "cls": self._css_class, "bg": bg, "radius": radius, "pad": pad,
            "accent": accent, "hover": colors.rgba(accent, 0.16),
            "ph": colors.rgba(accent, 0.5), "bord": colors.rgba(accent, 0.6),
            "hradius": hr,
            "titletxt": colors.contrast_text(accent),
            "bar_radius": bar_radius,
            "panel_radius": panel_radius,
            "bar_pad": bar_pad,
        }
        if self._css_provider is not None:
            Gtk.StyleContext.remove_provider_for_screen(
                self.get_screen(), self._css_provider)
        self._css_provider = Gtk.CssProvider()
        self._css_provider.load_from_data(css.encode("utf-8"))
        Gtk.StyleContext.add_provider_for_screen(
            self.get_screen(), self._css_provider,
            Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION + 1)

    # ------------------------------------------------------------------
    # rendering
    # ------------------------------------------------------------------
    def _make_image(self, icon, size):
        if icon and "/" in icon:
            try:
                pb = GdkPixbuf.Pixbuf.new_from_file_at_size(icon, size, size)
                return Gtk.Image.new_from_pixbuf(pb)
            except Exception:
                pass
        img = Gtk.Image.new_from_icon_name(
            icon or "application-x-executable", Gtk.IconSize.DIALOG)
        img.set_pixel_size(size)
        return img

    def _make_tile(self, appentry):
        b = self.block
        btn = Gtk.Button()
        btn.get_style_context().add_class("fence-tile")
        btn.set_relief(Gtk.ReliefStyle.NONE)
        col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        col.pack_start(self._make_image(appentry.get("icon"),
                                        int(b.get("iconSize", 48))), False, False, 0)
        if b.get("showLabels", True) and appentry.get("name"):
            lbl = Gtk.Label(label=appentry["name"])
            lbl.get_style_context().add_class("fence-label")
            lbl.set_max_width_chars(12)
            lbl.set_ellipsize(3)  # Pango.EllipsizeMode.END
            lbl.set_justify(Gtk.Justification.CENTER)
            col.pack_start(lbl, False, False, 0)
        btn.add(col)
        btn.connect("clicked", lambda *_a: self._app.launch(appentry.get("command")))
        # right-click a tile to remove it
        btn.connect("button-press-event", self._on_tile_press, appentry)

        # drag OUT: to the desktop / a folder (as a .desktop launcher or the
        # original file) or to another fence (moves the tile).
        btn.drag_source_set(
            Gdk.ModifierType.BUTTON1_MASK,
            [Gtk.TargetEntry.new(TILE_TARGET, Gtk.TargetFlags.SAME_APP, TILE_INFO)],
            Gdk.DragAction.COPY | Gdk.DragAction.MOVE)
        btn.drag_source_add_uri_targets()
        btn.connect("drag-begin", self._on_tile_drag_begin, appentry)
        btn.connect("drag-data-get", self._on_tile_drag_data_get, appentry)
        btn.connect("drag-failed", self._on_tile_drag_failed)
        btn.connect("drag-end", self._on_tile_drag_end, appentry)
        return btn

    def _build_body(self):
        """The padded grid area (or the drag-here hint)."""
        b = self.block
        body = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        body.get_style_context().add_class("fence-body")

        apps = b.get("apps", [])
        if not apps:
            self._tiles = []
            self._fixed = None
            self._placeholder = None
            self._pos = {}
            self._anims = {}
            hint = Gtk.Label(label="Drag apps here")
            hint.get_style_context().add_class("fence-hint")
            body.pack_start(hint, False, False, 0)
            return body

        self._tiles = [self._make_tile(a) for a in apps]
        # Gtk.Layout = Fixed-style placement, but its minimum size is under
        # our control (a ScrolledWindow with NEVER policy propagates the
        # child's min size, which made a widened fence impossible to shrink)
        self._fixed = Gtk.Layout()
        self._placeholder = None
        self._pos = {}
        self._anims = {}
        for t in self._tiles:
            self._fixed.put(t, 0, 0)
        self._layout_tiles(self._hover_index, animate=False)
        body.pack_start(self._fixed, True, True, 0)
        return body

    def _build_panel(self, show_header=True):
        b = self.block
        panel = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        panel.get_style_context().add_class("fence-panel")

        # slim solid title bar spanning the full width (docked fences use the
        # dock bar on the interior side instead)
        if show_header and b.get("showTitle", True):
            header = Gtk.Box()
            header.get_style_context().add_class("fence-header")
            title = Gtk.Label(label=self.name)
            title.get_style_context().add_class("fence-title")
            tbox = Gtk.EventBox()          # click the name to rename
            tbox.set_visible_window(False)
            tbox.add(title)
            tbox.connect("button-release-event", self._on_title_release)
            header.set_center_widget(tbox)
            panel.pack_start(header, False, False, 0)
            self._header = header

        panel.pack_start(self._build_body(), True, True, 0)
        return panel

    def _grid_cols(self):
        """Effective column count: from the fence's width once it has been
        resized by hand, otherwise from the configured columns."""
        if not (self._user_resized or self.block.get("width")):
            return max(1, int(self.block.get("columns", 4)))
        width = int(self.block.get("width") or self.get_allocated_width())
        sp = int(self.block.get("tileSpacing", 12))
        pad = int(self.block.get("padding", 18))
        cw, _ch = self._cell_size()
        inner = width - 2 * pad - 4          # minus body padding + border
        return max(1, int((inner + sp) // (cw + sp)))

    def _cell_size(self):
        cw = ch = 0
        for t in self._tiles:
            cw = max(cw, t.get_preferred_width()[1])
            ch = max(ch, t.get_preferred_height()[1])
        if cw <= 0 or ch <= 0:
            cw = ch = int(self.block.get("iconSize", 48)) + 24
        return cw, ch

    def _layout_tiles(self, gap_index=None, animate=True):
        """Place the tiles in their slots, optionally leaving a gap — tiles
        glide to their new spots (ease-out) instead of jumping."""
        if self._fixed is None:
            return
        sp = int(self.block.get("tileSpacing", 12))
        cols = self._grid_cols()
        self._layout_cols = cols
        cw, ch = self._cell_size()

        slots = list(self._tiles)
        ph = None
        if gap_index is not None:
            ph = self._ensure_placeholder(cw, ch)
            slots.insert(min(gap_index, len(slots)), ph)
        elif self._placeholder is not None:
            self._placeholder.hide()

        rows = (len(slots) + cols - 1) // cols or 1
        content_w = cols * (cw + sp) - sp
        content_h = rows * (ch + sp) - sp
        if (self._user_resized or self.block.get("width")) and not self._collapsed:
            # user-sized floating fence: keep the min tiny so it resizes
            # freely in both directions; overflow simply clips
            self._fixed.set_size_request(cw, ch)
        else:
            # auto fences and docked fences hug their content
            self._fixed.set_size_request(content_w, content_h)

        for i, w in enumerate(slots):
            x = (i % cols) * (cw + sp)
            y = (i // cols) * (ch + sp)
            if w is ph:                       # the gap itself appears in place
                self._fixed.move(ph, x, y)
                self._pos[ph] = (x, y)
                ph.show()
            else:
                self._move_tile(w, x, y, animate)

    def _ensure_placeholder(self, cw, ch):
        if self._placeholder is None:
            ph = Gtk.Box()
            ph.get_style_context().add_class("fence-placeholder")
            ph.set_no_show_all(True)
            self._fixed.put(ph, 0, 0)
            self._placeholder = ph
        self._placeholder.set_size_request(cw, ch)
        return self._placeholder

    def _move_tile(self, w, x, y, animate):
        cur = self._pos.get(w)
        if cur == (x, y) and w not in self._anims:
            return
        if not animate or cur is None:
            self._anims.pop(w, None)
            self._fixed.move(w, x, y)
            self._pos[w] = (x, y)
            return
        # retarget from wherever the tile currently is (mid-flight included)
        self._anims[w] = (cur[0], cur[1], x, y, None)
        if self._anim_id == 0:
            self._anim_id = self.add_tick_callback(self._on_anim_tick)

    def _on_anim_tick(self, _widget, clock):
        now = clock.get_frame_time()          # microseconds
        finished = []
        for w, (x0, y0, x1, y1, t0) in list(self._anims.items()):
            if t0 is None:
                t0 = now
                self._anims[w] = (x0, y0, x1, y1, t0)
            p = min(1.0, (now - t0) / (ANIM_MS * 1000.0))
            e = 1.0 - (1.0 - p) ** 3          # ease-out cubic
            self._fixed.move(w, round(x0 + (x1 - x0) * e),
                             round(y0 + (y1 - y0) * e))
            self._pos[w] = (round(x0 + (x1 - x0) * e),
                            round(y0 + (y1 - y0) * e))
            if p >= 1.0:
                finished.append(w)
        for w in finished:
            self._anims.pop(w, None)
        if not self._anims:
            self._anim_id = 0
            return GLib.SOURCE_REMOVE
        return GLib.SOURCE_CONTINUE

    def _index_at(self, x, y):
        """Insertion index for a pointer at window coords (x, y).

        Pure cell math against the (stable) slot geometry, so the answer
        doesn't wobble while tiles are animating between slots.
        """
        if not self._tiles or self._fixed is None:
            return 0
        res = self._fixed.translate_coordinates(self, 0, 0)
        fx, fy = (res[-2], res[-1]) if res else (0, 0)
        sp = int(self.block.get("tileSpacing", 12))
        cols = self._layout_cols or self._grid_cols()
        cw, ch = self._cell_size()
        col = max(0, min(int((x - fx) / (cw + sp)), cols - 1))
        row = max(0, int((y - fy) / (ch + sp)))
        return max(0, min(row * cols + col, len(self._tiles)))

    def _build_dock_bar(self):
        """The docked fence's title bar — same look as the header, placed on
        the interior side; doubles as the collapsed handle."""
        bar = Gtk.Box()
        bar.get_style_context().add_class("fence-dockbar")
        lbl = Gtk.Label(label=self.name)
        lbl.get_style_context().add_class("fence-title")
        if self._edge in ("left", "right"):
            lbl.set_angle(90 if self._edge == "left" else 270)
        bar.set_center_widget(lbl)
        return bar

    def render(self):
        """(Re)build the visible content for the current state."""
        self._apply_css()
        child = self._box.get_child()
        if child is not None:
            self._box.remove(child)
        self._bar = None
        self._header = None
        self._dock_layout = None
        self._dock_panel = None
        if self._collapsed:
            # the panel slides INSIDE a clipping layout; the window itself
            # stays fully on-screen (Muffin clamps off-screen window moves)
            panel = self._build_edge_layout()
            self._dock_panel = panel
            self._dock_layout = Gtk.Layout()
            self._dock_layout.put(panel, 0, 0)
            self._box.add(self._dock_layout)
            self._box.show_all()
            # measure only AFTER parenting under the window (and showing —
            # hidden widgets report 0): the per-fence CSS (body padding,
            # borders) resolves against the window's class, so an orphan
            # panel under-reports by exactly the padding + border
            pw = panel.get_preferred_width()[1]
            ph = panel.get_preferred_height()[1]
            self._panel_size = (pw, ph)
            self._vis_cache = 0
            self._bar_vis()          # cache the sliver while freshly styled
            self._dock_layout.set_size_request(pw, ph)
            dx, dy = self._dock_offset()
            self._panel_pos = (dx, dy)
            self._dock_layout.move(panel, dx, dy)
            self._sync_window_size()
            GLib.idle_add(self._snap_to_edge)
            if not self._expanded:
                self._schedule_dim()
        else:
            self._undim()
            self._box.add(self._build_panel())
            self._box.show_all()
            self._sync_window_size()
            gdkwin = self.get_window()
            if gdkwin is not None:       # clear any leftover input shape
                gdkwin.input_shape_combine_region(None, 0, 0)

    def _build_edge_layout(self):
        """Docked-at-an-edge layout: ONE fence panel (background, border,
        radius) containing the grid body and the title bar, with the bar on
        the INTERIOR side (bottom for top-dock, top for bottom-dock, right
        for left-dock, left for right-dock). The fence stays fully rendered;
        collapse/expand SLIDES the whole window off/on screen, leaving only
        the bar peeking in."""
        body = self._build_body()
        bar = self._build_dock_bar()
        self._bar = bar
        horizontal = self._edge in ("left", "right")
        panel = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL if horizontal
                        else Gtk.Orientation.VERTICAL)
        panel.get_style_context().add_class("fence-panel")
        if self._edge in ("left", "top"):
            # screen edge first, bar on the interior side
            panel.pack_start(body, False, False, 0)
            panel.pack_start(bar, False, True, 0)
        else:
            panel.pack_start(bar, False, True, 0)
            panel.pack_start(body, False, False, 0)
        return panel

    def restyle(self):
        """Colours/transparency/corners changed: update CSS only — no tile
        rebuild, so nothing shifts or flashes."""
        self._apply_css()
        self._sync_window_size()

    def relayout(self):
        """Columns/spacing changed: glide the existing tiles to new slots."""
        if self._fixed is None:
            self.render()
            return
        self._layout_tiles(self._hover_index)
        self._sync_window_size()

    def _sync_window_size(self):
        """Match the window to its content, or the stored size for a floating
        user-resized fence. Docked fences always hug their content — their
        stored floating size doesn't fit the docked shape."""
        if (not self._collapsed and self.block.get("width")
                and self.block.get("height")):
            self.resize(int(self.block["width"]), int(self.block["height"]))
        else:
            self.resize(1, 1)      # clamps to the preferred (content) size
        return False

    # ------------------------------------------------------------------
    # drag the Fence around (manual move, so we can detect edges)
    # ------------------------------------------------------------------
    def _resize_edge_at(self, x, y):
        if self._collapsed:
            return None
        w = self.get_allocated_width()
        h = self.get_allocated_height()
        m = RESIZE_MARGIN
        left, right = x < m, x > w - m
        top, bottom = y < m, y > h - m
        if top and left:
            return Gdk.WindowEdge.NORTH_WEST
        if top and right:
            return Gdk.WindowEdge.NORTH_EAST
        if bottom and left:
            return Gdk.WindowEdge.SOUTH_WEST
        if bottom and right:
            return Gdk.WindowEdge.SOUTH_EAST
        if left:
            return Gdk.WindowEdge.WEST
        if right:
            return Gdk.WindowEdge.EAST
        if top:
            return Gdk.WindowEdge.NORTH
        if bottom:
            return Gdk.WindowEdge.SOUTH
        return None

    def _set_cursor(self, edge):
        name = _EDGE_CURSORS.get(edge)
        if name == self._cursor_name:
            return
        self._cursor_name = name
        gdkwin = self.get_window()
        if gdkwin is None:
            return
        cursor = (Gdk.Cursor.new_from_name(self.get_display(), name)
                  if name else None)
        gdkwin.set_cursor(cursor)

    def _on_press(self, _w, event):
        if event.button != 1:
            return False
        edge = self._resize_edge_at(event.x, event.y)
        if edge is not None:
            self._user_resized = True
            self.begin_resize_drag(edge, event.button,
                                   int(event.x_root), int(event.y_root),
                                   event.time)
            return True
        wx, wy = self.get_position()
        self._press = [event.x_root, event.y_root, wx, wy, False]
        return False

    def _on_motion(self, _w, event):
        if not self._press:
            self._set_cursor(self._resize_edge_at(event.x, event.y))
            return False
        dx = event.x_root - self._press[0]
        dy = event.y_root - self._press[1]
        if not self._press[4] and (abs(dx) > DRAG_SLOP or abs(dy) > DRAG_SLOP):
            self._press[4] = True
        if self._press[4]:
            self.move(int(self._press[2] + dx), int(self._press[3] + dy))
        return False

    def _on_release(self, _w, event):
        if not self._press:
            return False
        moved = self._press[4]
        self._press = None
        if moved:
            self._settle_position()
        elif self._collapsed:
            # a plain click on the handle toggles (click-to-open mode)
            if not self.block.get("expandOnHover", True):
                self._toggle_expand()
        return False

    def _settle_position(self):
        x, y = self.get_position()
        w = self.get_allocated_width()
        h = self.get_allocated_height()
        mon = self._monitor_geom()
        edge = None
        dl = x - mon.x
        dr = (mon.x + mon.width) - (x + w)
        dt = y - mon.y
        db = (mon.y + mon.height) - (y + h)
        mind = min(dl, dr, dt, db)
        if mind <= EDGE_THRESHOLD:
            edge = {dl: "left", dr: "right", dt: "top", db: "bottom"}[mind]

        self._edge = edge
        self._collapsed = edge is not None
        self._expanded = False
        self.block["edge"] = edge
        self.block["x"], self.block["y"] = x, y
        self._app.save()
        self.render()
        if self._collapsed:
            self._show_handle()

    def _monitor_geom(self):
        display = self.get_display()
        win = self.get_window()
        monitor = (display.get_monitor_at_window(win) if win
                   else display.get_primary_monitor())
        # workarea = monitor minus panels/docks, so a bottom-docked fence
        # sits ON the taskbar instead of underneath it
        return monitor.get_workarea()

    # ------------------------------------------------------------------
    # collapse / expand
    # ------------------------------------------------------------------
    def _show_handle(self):
        self._expanded = False
        self.render()
        GLib.idle_add(self._snap_to_edge)

    def _toggle_expand(self):
        if self._expanded:
            self._collapse()
        else:
            self._expand()

    def _expand(self):
        if not self._collapsed or self._expanded:
            return
        self._expanded = True
        self._undim()
        self._slide_dock(animate=True)
        if self.block.get("expandOnHover", True):
            self._start_hover_watch()

    def _collapse(self):
        if not self._collapsed or not self._expanded:
            return
        self._expanded = False
        self._slide_dock(animate=True)
        self._schedule_dim()

    # While expanded, poll the pointer instead of trusting raw enter/leave:
    # the strip between the fence and the physical screen edge counts as
    # "inside", so parking the cursor at the edge can't flicker-collapse it.
    def _start_hover_watch(self):
        if self._watch_id == 0:
            self._watch_id = GLib.timeout_add(120, self._check_pointer)

    def _check_pointer(self):
        if not (self._collapsed and self._expanded):
            self._watch_id = 0
            return False
        if self._pointer_near():
            if self._collapse_timer:      # cancel any pending roll-up
                GLib.source_remove(self._collapse_timer)
                self._collapse_timer = 0
            return True
        self._watch_id = 0
        self._collapse()
        return False

    def _pointer_near(self):
        try:
            seat = self.get_display().get_default_seat()
            _s, px, py = seat.get_pointer().get_position()
        except Exception:
            return False
        x, y = self.get_position()
        w = self.get_allocated_width()
        h = self.get_allocated_height()
        m = self._monitor_geom()
        pad = 4                            # hysteresis on the open sides
        x0, y0 = x - pad, y - pad
        x1, y1 = x + w + pad, y + h + pad
        # extend to the screen edge so the gap strip counts as inside
        if self._edge == "left":
            x0 = m.x
        elif self._edge == "right":
            x1 = m.x + m.width
        elif self._edge == "top":
            y0 = m.y
        elif self._edge == "bottom":
            y1 = m.y + m.height
        return x0 <= px <= x1 and y0 <= py <= y1

    def _bar_vis(self):
        """Thickness of the visible sliver when collapsed (the bar + border).
        Measured once per render and cached: live re-measures can disagree
        (styling/allocation timing), which made slide targets flap around."""
        if self._vis_cache:
            return self._vis_cache
        if self._bar is None:
            return 24
        if self._edge in ("left", "right"):
            vis = self._bar.get_preferred_width()[1] + 2
        else:
            vis = self._bar.get_preferred_height()[1] + 2
        if vis > 4:                  # only trust a plausible measurement
            self._vis_cache = vis
        return max(vis, 18)

    def _dock_offset(self):
        """Panel offset inside the clipping layout for the current state:
        (0, 0) expanded; pushed past the window edge (clipped) when collapsed
        so only the bar remains visible."""
        if self._expanded or not self._edge:
            return (0, 0)
        w, h = self._panel_size
        vis = self._bar_vis()
        return {
            "left":   (-(w - vis), 0),
            "right":  (w - vis, 0),
            "top":    (0, -(h - vis)),
            "bottom": (0, h - vis),
        }.get(self._edge, (0, 0))

    def _move_panel(self, dx, dy):
        if self._dock_layout is not None and self._dock_panel is not None:
            self._dock_layout.move(self._dock_panel, int(dx), int(dy))
        self._panel_pos = (dx, dy)
        self._update_input_shape(dx, dy)

    def _update_input_shape(self, dx, dy):
        """Restrict clicks/hover to the visible part of the panel so the
        clipped (empty) part of the window doesn't block the desktop."""
        gdkwin = self.get_window()
        if gdkwin is None:
            return
        w, h = self._panel_size
        x0, y0 = max(0, int(dx)), max(0, int(dy))
        x1, y1 = min(w, int(dx) + w), min(h, int(dy) + h)
        rect = cairo.RectangleInt(x0, y0, max(1, x1 - x0), max(1, y1 - y0))
        gdkwin.input_shape_combine_region(cairo.Region(rect), 0, 0)

    def _slide_dock(self, animate=True):
        if self._dock_layout is None or self._dock_panel is None:
            return
        tx, ty = self._dock_offset()
        if not animate:
            self._slide = None
            self._move_panel(tx, ty)
            return
        # already tweening to this exact target? let it finish — restarting
        # from the current spot turns the ease-out into an asymptotic crawl
        if (self._slide is not None
                and (self._slide[2], self._slide[3]) == (tx, ty)):
            return
        cx, cy = self._panel_pos
        if (cx, cy) == (tx, ty):
            return
        # NOTE: driven by a GLib timer, NOT the frame clock — the compositor
        # throttles paint clocks for obscured keep-below windows, which froze
        # tick-driven tweens mid-flight
        self._slide = [cx, cy, tx, ty, GLib.get_monotonic_time()]
        if self._slide_id == 0:
            self._slide_id = GLib.timeout_add(16, self._on_slide_step)

    def _on_slide_step(self):
        s = self._slide
        if s is None:
            self._slide_id = 0
            return False
        p = min(1.0, (GLib.get_monotonic_time() - s[4]) / (SLIDE_MS * 1000.0))
        e = 1.0 - (1.0 - p) ** 3
        self._move_panel(round(s[0] + (s[2] - s[0]) * e),
                         round(s[1] + (s[3] - s[1]) * e))
        if p >= 1.0:
            self._slide = None
            self._slide_id = 0
            return False
        return True

    def _snap_to_edge(self):
        """Pin the window flush to its edge (fully on-screen) and place the
        panel/input-shape for the current state."""
        if not self._collapsed or not self._edge:
            return False
        mon = self._monitor_geom()
        w, h = self._panel_size
        x, y = self.get_position()
        if self._edge == "left":
            x = mon.x
        elif self._edge == "right":
            x = mon.x + mon.width - w
        elif self._edge == "top":
            y = mon.y
        else:
            y = mon.y + mon.height - h
        self.move(x, y)
        self._slide_dock(animate=False)
        return False

    def _on_size_allocate(self, _w, alloc):
        if not self._collapsed:
            return
        # allocations fire for every panel move inside the layout too —
        # only re-pin on REAL size changes, and never mid-slide (the
        # instant snap was killing the animation on its first frame)
        size = (alloc.width, alloc.height)
        if size == self._last_alloc:
            return
        self._last_alloc = size
        if self._slide is None:
            GLib.idle_add(self._snap_to_edge)

    def _on_configure(self, _w, event):
        if self._collapsed:
            return False
        if self._user_resized or self.block.get("width"):
            if (event.width != self.block.get("width")
                    or event.height != self.block.get("height")):
                self.block["width"] = event.width
                self.block["height"] = event.height
                self._queue_geom_save()
            # resizing from the left/top edges also moves the window
            x, y = self.get_position()
            if (x, y) != (self.block.get("x"), self.block.get("y")):
                self.block["x"], self.block["y"] = x, y
                self._queue_geom_save()
            if self._grid_cols() != self._layout_cols:
                self._layout_tiles(self._hover_index)   # animated reflow
        return False

    def _queue_geom_save(self):
        if self._geom_timer:
            GLib.source_remove(self._geom_timer)
        self._geom_timer = GLib.timeout_add(400, self._flush_geom)

    def _flush_geom(self):
        self._geom_timer = 0
        self._app.save()
        return False

    def _on_destroy(self, _w):
        for attr in ("_collapse_timer", "_hover_timer", "_geom_timer",
                     "_watch_id", "_dim_timer", "_slide_id"):
            tid = getattr(self, attr, 0)
            if tid:
                GLib.source_remove(tid)
                setattr(self, attr, 0)

    def _on_enter(self, _w, event):
        if event.detail == Gdk.NotifyType.INFERIOR:
            return False
        self.get_style_context().add_class("fence-hover")
        self._undim()
        if self._collapse_timer:
            GLib.source_remove(self._collapse_timer)
            self._collapse_timer = 0
        if self._collapsed and self.block.get("expandOnHover", True):
            self._expand()
        return False

    def _on_leave(self, _w, event):
        if event.detail == Gdk.NotifyType.INFERIOR:
            return False
        self.get_style_context().remove_class("fence-hover")
        self._set_cursor(None)
        if self._collapsed and self.block.get("expandOnHover", True):
            if self._collapse_timer:
                GLib.source_remove(self._collapse_timer)
            self._collapse_timer = GLib.timeout_add(LEAVE_GRACE, self._leave_timeout)
        return False

    def _leave_timeout(self):
        self._collapse_timer = 0
        # stay open if the pointer only slipped into the screen-edge strip
        if not self._pointer_near():
            self._collapse()
        return False

    # ---- inline rename (click the title text) ------------------------
    def _on_title_release(self, _w, _event):
        # a clean click (no drag) on the name opens an inline rename entry
        if self._press and not self._press[4] and not self._collapsed:
            GLib.idle_add(self._start_rename)
        return False

    def _start_rename(self):
        header = getattr(self, "_header", None)
        if header is None or self._collapsed:
            return False
        old_center = header.get_center_widget()
        if isinstance(old_center, Gtk.Entry):
            return False
        entry = Gtk.Entry()
        entry.set_text(self.name)
        entry.set_width_chars(max(6, len(self.name) + 2))
        entry.set_alignment(0.5)
        header.remove(old_center)
        header.set_center_widget(entry)
        entry.show()
        # fences never take focus normally — allow it just for typing
        self.set_accept_focus(True)
        self.present()
        entry.grab_focus()

        def finish(commit):
            new = entry.get_text().strip()
            self.set_accept_focus(False)
            if commit and new and new != self.name:
                if self._app.rename_fence_live(self.name, new):
                    return          # rename re-renders the window
            self.render()           # restore the label

        entry.connect("activate", lambda _e: finish(True))
        entry.connect("focus-out-event", lambda _e, _ev: (finish(False), False)[1])
        entry.connect("key-press-event", lambda _e, ev: (
            finish(False) or True if ev.keyval == Gdk.KEY_Escape else False))
        return False

    # ---- collapsed-label dimming ------------------------------------
    def _schedule_dim(self):
        if self._dim_timer:
            GLib.source_remove(self._dim_timer)
        self._dim_timer = GLib.timeout_add(2000, self._apply_dim)

    def _apply_dim(self):
        self._dim_timer = 0
        if self._collapsed and not self._expanded:
            self.get_style_context().add_class("fence-dim")
        return False

    def _undim(self):
        if self._dim_timer:
            GLib.source_remove(self._dim_timer)
            self._dim_timer = 0
        self.get_style_context().remove_class("fence-dim")

    # ------------------------------------------------------------------
    # drag apps IN
    # ------------------------------------------------------------------
    def _on_drag_motion(self, _w, _ctx, x, y, _time):
        self.get_style_context().add_class("fence-droptarget")
        if self._hover_timer:
            GLib.source_remove(self._hover_timer)
            self._hover_timer = 0
        # spring-loaded: hovering a collapsed fence rolls it open
        if self._collapsed and not self._expanded:
            self._expand()
        idx = self._index_at(x, y)
        if idx != self._hover_index:
            self._hover_index = idx
            self._layout_tiles(idx)
        return False

    def _on_drag_leave(self, _w, _ctx, _time):
        self.get_style_context().remove_class("fence-droptarget")
        # drag-leave also fires right before the drop lands, so clear lazily;
        # _on_drop cancels this timer when the drop is really for us
        if self._hover_timer:
            GLib.source_remove(self._hover_timer)
        self._hover_timer = GLib.timeout_add(120, self._clear_hover)

    def _clear_hover(self):
        self._hover_timer = 0
        self._hover_index = None
        self._layout_tiles(None)
        if self._collapsed and self._expanded:
            self._collapse()
        else:
            GLib.idle_add(self._sync_window_size)
        return False

    def _on_drop(self, _w, ctx, _x, _y, data, info, time):
        self.get_style_context().remove_class("fence-droptarget")
        if self._hover_timer:
            GLib.source_remove(self._hover_timer)
            self._hover_timer = 0
        slot = self._hover_index
        self._hover_index = None

        # a tile from another fence: record the move; the source's drag-end
        # completes it (so the source widget isn't destroyed mid-drag)
        if info == TILE_INFO:
            ok = False
            try:
                payload = json.loads(data.get_data().decode("utf-8"))
                self._app.pending_move = (payload["fence"], payload["index"],
                                          self.name, slot)
                ok = True
            except (ValueError, KeyError, AttributeError):
                self._app.pending_move = None
            Gtk.drag_finish(ctx, ok, False, time)
            return

        uris = data.get_uris()
        text = "\n".join(uris) if uris else (data.get_text() or "")
        entries = desktop_apps.entries_from_uri_list(text)
        if entries:
            apps = self.block.setdefault("apps", [])
            pos = len(apps) if slot is None else min(slot, len(apps))
            apps[pos:pos] = entries
            self._app.save()
            if self._collapsed:
                self._expanded = True
            self.render()
        else:
            self._layout_tiles(None)
        Gtk.drag_finish(ctx, bool(entries), False, time)

    # ------------------------------------------------------------------
    # drag tiles OUT (to the desktop, a folder, or another fence)
    # ------------------------------------------------------------------
    def _on_tile_drag_begin(self, _w, ctx, appentry):
        self._press = None            # never also drag the window itself
        self._drag_failed = False
        self._app.pending_move = None
        icon = appentry.get("icon") or "application-x-executable"
        if "/" in icon:
            try:
                pb = GdkPixbuf.Pixbuf.new_from_file_at_size(icon, 48, 48)
                Gtk.drag_set_icon_pixbuf(ctx, pb, 0, 0)
                return
            except Exception:
                pass
        Gtk.drag_set_icon_name(ctx, icon if "/" not in icon
                               else "application-x-executable", 0, 0)

    def _on_tile_drag_data_get(self, _w, _ctx, data, info, _time, appentry):
        if info == TILE_INFO:
            try:
                index = self.block.get("apps", []).index(appentry)
            except ValueError:
                return
            payload = json.dumps({"fence": self.name, "index": index})
            data.set(data.get_target(), 8, payload.encode("utf-8"))
        else:
            try:
                data.set_uris([self._entry_uri(appentry)])
            except Exception:
                pass

    def _on_tile_drag_failed(self, _w, _ctx, _result):
        self._drag_failed = True
        return False

    def _on_tile_drag_end(self, _w, _ctx, appentry):
        pending = getattr(self._app, "pending_move", None)
        if pending:
            self._app.pending_move = None
            self._app.finish_move(*pending)
        elif not self._drag_failed:
            # dropped outside Fence (desktop / file manager): the launcher
            # was copied there, so complete the "move" by removing the tile
            self._remove_app(None, appentry)
        self._drag_failed = False

    def _entry_uri(self, appentry):
        """A file URI representing this tile, for drops onto the desktop."""
        cmd = (appentry.get("command") or "").strip()
        path = None
        if cmd.startswith("gtk-launch "):
            app_id = cmd.split(None, 1)[1].strip()
            info = (Gio.DesktopAppInfo.new(app_id + ".desktop")
                    or Gio.DesktopAppInfo.new(app_id))
            if info is not None:
                path = info.get_filename()
        elif cmd.startswith(("xdg-open ", "gio launch ")):
            args = shlex.split(cmd)
            cand = args[-1]
            if cand.startswith("/") and os.path.exists(cand):
                path = cand
        if path is None:
            path = self._export_desktop_file(appentry)
        return GLib.filename_to_uri(path, None)

    def _export_desktop_file(self, appentry):
        """Custom command: materialise a .desktop file that can live outside."""
        exports = os.path.join(config.config_dir(), "exports")
        os.makedirs(exports, exist_ok=True)
        slug = re.sub(r"[^A-Za-z0-9]+", "-",
                      appentry.get("name") or "app").strip("-").lower() or "app"
        path = os.path.join(exports, "fence-%s.desktop" % slug)
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("[Desktop Entry]\nType=Application\n"
                     "Name=%s\nExec=%s\nIcon=%s\n"
                     % (appentry.get("name") or "App",
                        appentry.get("command") or "true",
                        appentry.get("icon") or "application-x-executable"))
        os.chmod(path, 0o755)
        return path

    # ------------------------------------------------------------------
    # tile context menu (remove)
    # ------------------------------------------------------------------
    def _on_tile_press(self, _w, event, appentry):
        if event.button != 3:
            return False
        menu = Gtk.Menu()
        item = Gtk.MenuItem(label="Remove from Fence")
        item.connect("activate", self._remove_app, appentry)
        menu.append(item)
        menu.show_all()
        menu.popup_at_pointer(event)
        return True

    def _remove_app(self, _item, appentry):
        try:
            self.block.get("apps", []).remove(appentry)
        except ValueError:
            pass
        self._app.save()
        self.render()

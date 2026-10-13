"""Discover installed applications and turn dropped URIs into app entries.

An "app entry" is the dict stored in a fence: ``{name, command, icon}``.
``icon`` is a themed icon name or an absolute image path; ``command`` is a
shell command.
"""
import shlex

import gi
from gi.repository import Gio, GLib


def _icon_name(gicon, fallback="application-x-executable"):
    if gicon is None:
        return fallback
    if isinstance(gicon, Gio.ThemedIcon):
        names = gicon.get_names()
        return names[0] if names else fallback
    if isinstance(gicon, Gio.FileIcon):
        return gicon.get_file().get_path()
    try:
        return gicon.to_string()
    except Exception:
        return fallback


def _command_for_id(app_id):
    """Launch installed apps by desktop id — robust against %field codes."""
    if app_id and app_id.endswith(".desktop"):
        app_id = app_id[:-len(".desktop")]
    return "gtk-launch " + app_id


def scan_apps():
    """All visible installed applications, sorted by name.

    Returns a list of ``{id, name, command, icon}``.
    """
    out = []
    seen = set()
    for info in Gio.AppInfo.get_all():
        try:
            if not info.should_show():
                continue
            app_id = info.get_id() or ""
            if app_id in seen:
                continue
            seen.add(app_id)
            out.append({
                "id": app_id,
                "name": info.get_display_name() or app_id,
                "command": _command_for_id(app_id),
                "icon": _icon_name(info.get_icon()),
            })
        except Exception:
            continue
    out.sort(key=lambda a: (a["name"] or "").lower())
    return out


def _entry_from_desktop_file(path):
    info = Gio.DesktopAppInfo.new_from_filename(path)
    if info is None:
        return None
    app_id = info.get_id()
    if app_id:
        command = _command_for_id(app_id)
    else:
        # A .desktop file outside the standard dirs: launch it by path.
        command = "gio launch " + shlex.quote(path)
    return {
        "name": info.get_name() or info.get_display_name() or "App",
        "command": command,
        "icon": _icon_name(info.get_icon()),
    }


def _entry_from_file(path):
    gfile = Gio.File.new_for_path(path)
    name = gfile.get_basename() or path
    icon = "text-x-generic"
    try:
        info = gfile.query_info("standard::icon,standard::type",
                                Gio.FileQueryInfoFlags.NONE, None)
        if info.get_file_type() == Gio.FileType.DIRECTORY:
            icon = "folder"
        else:
            icon = _icon_name(info.get_icon(), "text-x-generic")
    except Exception:
        pass
    return {
        "name": name,
        "command": "xdg-open " + shlex.quote(path),
        "icon": icon,
    }


def entry_from_uri(uri):
    """Convert a single dropped URI into an app entry, or None."""
    uri = (uri or "").strip()
    if not uri:
        return None
    try:
        path, _host = GLib.filename_from_uri(uri)
    except Exception:
        return None  # non-file URIs (http://, etc.) are not launchers here
    if path.endswith(".desktop"):
        entry = _entry_from_desktop_file(path)
        if entry:
            return entry
    return _entry_from_file(path)


def entries_from_uri_list(text):
    """Parse a ``text/uri-list`` payload into a list of app entries."""
    entries = []
    for line in (text or "").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        entry = entry_from_uri(line)
        if entry:
            entries.append(entry)
    return entries

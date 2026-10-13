//! Discover installed applications and turn dropped URIs into app entries.
//!
//! A faithful port of `desktop_apps.py`. An "app entry" is [`config::AppEntry`]
//! (`{name, command, icon}`); `icon` is a themed icon name or an absolute image
//! path, and `command` is a shell command.

use gio::prelude::*;
use gtk::prelude::IconThemeExt;

use crate::config::AppEntry;

/// One installed application, as surfaced by the app picker.
#[derive(Clone, Debug)]
pub struct InstalledApp {
    /// `firefox.desktop` — carried so a picked app can be identified later
    /// (dedupe, re-resolve an icon); nothing reads it yet.
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub command: String,
    pub icon: String,
}

/// Best icon name / path for a `GIcon`, falling back to `fallback`.
fn icon_name(icon: Option<gio::Icon>, fallback: &str) -> String {
    let Some(icon) = icon else {
        return fallback.to_string();
    };
    if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
        return themed
            .names()
            .first()
            .map(|g| g.to_string())
            .unwrap_or_else(|| fallback.to_string());
    }
    if let Some(file_icon) = icon.downcast_ref::<gio::FileIcon>() {
        return file_icon
            .file()
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| fallback.to_string());
    }
    // Any other GIcon kind: fall back to its GIcon string form.
    IconExt::to_string(&icon)
        .map(|s| s.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

fn quote(path: &str) -> String {
    shlex::try_quote(path)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Launch installed apps by desktop id — robust against `%field` codes.
fn command_for_id(app_id: &str) -> String {
    let id = app_id.strip_suffix(".desktop").unwrap_or(app_id);
    format!("gtk-launch {}", id)
}

/// All visible installed applications, sorted by name.
pub fn scan_apps() -> Vec<InstalledApp> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for info in gio::AppInfo::all() {
        if !info.should_show() {
            continue;
        }
        let app_id = info.id().map(|g| g.to_string()).unwrap_or_default();
        if !seen.insert(app_id.clone()) {
            continue;
        }
        let display = info.display_name();
        let name = if display.is_empty() {
            app_id.clone()
        } else {
            display.to_string()
        };
        out.push(InstalledApp {
            command: command_for_id(&app_id),
            icon: icon_name(info.icon(), "application-x-executable"),
            id: app_id,
            name,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn entry_from_desktop_file(path: &str) -> Option<AppEntry> {
    let info = gio::DesktopAppInfo::from_filename(path)?;
    let app_id = info.id().map(|g| g.to_string()).unwrap_or_default();
    let command = if !app_id.is_empty() {
        command_for_id(&app_id)
    } else {
        // A .desktop file outside the standard dirs: launch it by path.
        format!("gio launch {}", quote(path))
    };
    let name = {
        let n = info.name();
        if !n.is_empty() {
            n.to_string()
        } else {
            let dn = info.display_name();
            if dn.is_empty() {
                "App".to_string()
            } else {
                dn.to_string()
            }
        }
    };
    Some(AppEntry::new(
        &name,
        &command,
        &icon_name(info.icon(), "application-x-executable"),
    ))
}

/// Build a tile for a plain file or directory.
///
/// `path` is what the tile will point at — for a dropped link, the link's
/// target, already resolved — while `name` is what the tile is called, which
/// for a link is the name the user gave the link rather than the target's.
fn entry_from_file(path: &str, name: &str) -> AppEntry {
    let gfile = gio::File::for_path(path);
    let open = |icon: &str| AppEntry::new(name, &format!("xdg-open {}", quote(path)), icon);
    let Ok(info) = gfile.query_info(
        "standard::icon,standard::type,standard::content-type,\
         access::can-execute,thumbnail::path,thumbnail::is-valid",
        gio::FileQueryInfoFlags::NONE,
        gio::Cancellable::NONE,
    ) else {
        return open("text-x-generic");
    };
    if info.file_type() == gio::FileType::Directory {
        return open("folder");
    }
    // A program — a native binary or an AppImage, possibly reached through a
    // desktop symlink. The tile must *run* it (xdg-open won't), and its icon is
    // the file manager's thumbnail — for an AppImage that is the application's
    // own icon, exactly what the desktop shows.
    if info.boolean("access::can-execute")
        && is_program(info.content_type().as_deref().unwrap_or(""))
    {
        let icon = fresh_thumbnail(&info)
            .unwrap_or_else(|| themed_icon_in_theme(&info, "application-x-executable"));
        return AppEntry::new(name, &quote(path), &icon);
    }
    open(&themed_icon_in_theme(&info, "text-x-generic"))
}

/// Content types that mean "a program to run" rather than a document to open.
/// Scripts are deliberately absent: the desktop opens them for viewing on a
/// double-click, and silently *running* one instead would be a surprise.
/// `x-sharedlib` is here because older shared-mime databases report PIE
/// binaries as it; the `can-execute` gate keeps actual libraries out.
fn is_program(content_type: &str) -> bool {
    matches!(
        content_type,
        "application/vnd.appimage"
            | "application/x-executable"
            | "application/x-pie-executable"
            | "application/x-sharedlib"
    )
}

/// The file manager's cached thumbnail for the file, if it is up to date.
fn fresh_thumbnail(info: &gio::FileInfo) -> Option<String> {
    if !info.boolean("thumbnail::is-valid") {
        return None;
    }
    let p = info.attribute_byte_string("thumbnail::path")?;
    (!p.is_empty()).then(|| p.to_string())
}

/// A themed icon name for `info` that the current icon theme can actually
/// render. `standard::icon` lists candidates best-first, but the best one is
/// often not in the theme at all (`application-vnd.appimage`, say), and GTK
/// draws a missing name as the broken-image glyph.
fn themed_icon_in_theme(info: &gio::FileInfo, fallback: &str) -> String {
    if gtk::is_initialized() {
        if let Some(themed) = info.icon().and_then(|i| i.downcast::<gio::ThemedIcon>().ok()) {
            if let Some(theme) = gtk::IconTheme::default() {
                for name in themed.names() {
                    if theme.has_icon(&name) {
                        return name.to_string();
                    }
                }
            }
        }
    }
    icon_name(info.icon(), fallback)
}

/// The file a tile's command points at, if it points at one.
///
/// The inverse of what `entry_from_file` builds: `xdg-open <path>` opens a
/// file, and a bare quoted absolute path *is* one (a dropped executable). An
/// installed launcher (`firefox`, `gtk-launch foo`) doesn't own a file, so
/// this returns `None` for it.
pub fn command_target_path(command: &str) -> Option<std::path::PathBuf> {
    let cmd = command.trim();
    let rest = cmd
        .strip_prefix("xdg-open ")
        .or_else(|| cmd.strip_prefix("gio open "))
        .or_else(|| cmd.strip_prefix("gio launch "))
        .unwrap_or(cmd);
    let path = shlex::split(rest)?.into_iter().next()?;
    path.starts_with('/').then(|| std::path::PathBuf::from(path))
}

/// Convert a single dropped URI into an app entry, or `None`.
pub fn entry_from_uri(uri: &str) -> Option<AppEntry> {
    let uri = uri.trim();
    if uri.is_empty() {
        return None;
    }
    // non-file URIs (http://, etc.) are not launchers here
    let (path, _host) = glib::filename_from_uri(uri).ok()?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let path = path.to_string_lossy().into_owned();
    // A dropped *link* is a pointer, not content, so the tile is built from
    // whatever the link points at — then the link itself is redundant and can be
    // cleared off the desktop without breaking the tile. Only the path itself is
    // resolved, never its parent directories: canonicalising those would move a
    // plain `~/Desktop/report.pdf` out of the desktop directory on a setup where
    // `~/Desktop` is a link, and the round-trip back to the desktop keys on the
    // tile's path still being there.
    let target = match is_link(&path) {
        true => std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.clone()),
        false => path.clone(),
    };
    // Whatever it was reached through, a launcher is a launcher.
    if target.ends_with(".desktop") {
        if let Some(entry) = entry_from_desktop_file(&target) {
            return Some(entry);
        }
    }
    Some(entry_from_file(&target, &name))
}

/// Is this path itself a symlink? (Not whether it *leads* to one.)
fn is_link(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// The URIs in a `text/uri-list` payload, comments and blanks dropped.
pub fn uri_list_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// Parse a `text/uri-list` payload into a list of app entries.
pub fn entries_from_uri_list(text: &str) -> Vec<AppEntry> {
    uri_list_lines(text).filter_map(entry_from_uri).collect()
}

#[cfg(test)]
mod tests {
    use super::{command_target_path, is_program};

    #[test]
    fn finds_the_file_an_open_command_points_at() {
        assert_eq!(
            command_target_path("xdg-open /home/x/Desktop/report.pdf"),
            Some("/home/x/Desktop/report.pdf".into())
        );
        // names with spaces survive, because the command is shell-quoted
        assert_eq!(
            command_target_path("xdg-open '/home/x/Desktop/my notes.md'"),
            Some("/home/x/Desktop/my notes.md".into())
        );
    }

    #[test]
    fn a_launcher_owns_no_file() {
        assert_eq!(command_target_path("firefox"), None);
        assert_eq!(command_target_path("gtk-launch firefox"), None);
        // relative paths are not ours to touch
        assert_eq!(command_target_path("xdg-open notes.md"), None);
    }

    #[test]
    fn a_dropped_executable_is_its_own_target() {
        // a direct-exec tile's command is just the quoted path
        assert_eq!(
            command_target_path("'/home/x/Desktop/RustDesk x86.AppImage'"),
            Some("/home/x/Desktop/RustDesk x86.AppImage".into())
        );
    }

    /// A dropped link must produce a tile that points at the link's *target*,
    /// so clearing the link off the desktop can't break the tile — while still
    /// being called whatever the user called the link.
    #[test]
    fn a_dropped_link_resolves_to_what_it_points_at() {
        let dir = std::env::temp_dir().join("hedge-link-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("report.pdf");
        std::fs::write(&target, b"%PDF-1.4\n").unwrap();
        let link = dir.join("My Report");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let uri = glib::filename_to_uri(&link, None).unwrap();
        let entry = super::entry_from_uri(&uri).expect("a link is droppable");
        assert_eq!(entry.name, "My Report", "the link's own name is kept");
        assert_eq!(
            super::command_target_path(&entry.command).as_deref(),
            Some(target.as_path()),
            "the tile points past the link at the real file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }


    #[test]
    fn programs_are_binaries_and_appimages_not_scripts() {
        assert!(is_program("application/vnd.appimage"));
        assert!(is_program("application/x-executable"));
        assert!(!is_program("application/x-shellscript"));
        assert!(!is_program("application/pdf"));
    }
}

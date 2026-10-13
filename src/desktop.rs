//! Hide/show desktop icons by SAFELY moving the contents of ~/Desktop into a
//! persistent stash folder and back. Files are only ever *moved*, never
//! deleted, and the stash lives under XDG data so it survives logout/shutdown —
//! hidden files can always be recovered.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// The user's Desktop directory (XDG-aware, falling back to ~/Desktop).
pub fn desktop_dir() -> PathBuf {
    glib::user_special_dir(glib::UserDirectory::Desktop)
        .unwrap_or_else(|| home().join("Desktop"))
}

/// Where hidden desktop entries are parked.
pub fn stash_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".local/share"));
    base.join("hedge/desktop-stash")
}

// -- hiding one entry from the desktop view ---------------------------------
//
// Filing a document into a hedge should clear it off the desktop *without*
// moving it: the tile launches `xdg-open <path>`, and the user must still find
// it at `~/Desktop/<name>` in any file manager or terminal. Nemo (like
// Nautilus) reads a `.hidden` file in a directory — one filename per line — and
// omits those entries from the view. Nothing is moved, renamed or deleted, so
// the tile keeps working and Ctrl-H still reveals the file in Nemo.

/// `~/Desktop/.hidden`
fn hidden_list_path() -> PathBuf {
    desktop_dir().join(".hidden")
}

fn read_hidden_list() -> Vec<String> {
    read_list_at(&hidden_list_path())
}

// The list functions take the file path so the tests can drive them against a
// temp directory — `desktop_dir()` is the user's real Desktop, which a unit
// test must never write to.
fn read_list_at(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn write_list_at(path: &PathBuf, names: &[String]) -> std::io::Result<()> {
    if names.is_empty() {
        // don't leave an empty control file lying around
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut body = names.join("\n");
    body.push('\n');
    std::fs::write(path, body)
}

/// Hide one entry from the desktop view, leaving the file exactly where it is.
/// Returns whether the list changed.
pub fn hide_from_view(name: &str) -> std::io::Result<bool> {
    hide_in_list(&hidden_list_path(), name)
}

fn hide_in_list(path: &PathBuf, name: &str) -> std::io::Result<bool> {
    let mut names = read_list_at(path);
    if names.iter().any(|n| n == name) {
        return Ok(false);
    }
    names.push(name.to_string());
    write_list_at(path, &names)?;
    Ok(true)
}

/// Put one entry back in the desktop view. Only ever removes an exact match, so
/// entries the user hid themselves are left alone.
pub fn unhide_from_view(name: &str) -> std::io::Result<bool> {
    unhide_in_list(&hidden_list_path(), name)
}

fn unhide_in_list(path: &PathBuf, name: &str) -> std::io::Result<bool> {
    let mut names = read_list_at(path);
    let before = names.len();
    names.retain(|n| n != name);
    if names.len() == before {
        return Ok(false);
    }
    write_list_at(path, &names)?;
    Ok(true)
}

/// A tile is leaving a hedge: if it was a desktop document we hid, show it
/// again. Anything else (a real launcher, a file elsewhere) is left alone.
///
/// This has to run on *every* route out of a hedge — dragged out, removed from
/// the tile menu, deleted in Settings — or a file could end up hidden with
/// nothing left pointing at it.
pub fn restore_entry_to_view(command: &str) {
    let Some(path) = crate::desktop_apps::command_target_path(command) else {
        return;
    };
    if path.parent() != Some(desktop_dir().as_path()) {
        return;
    }
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    if let Err(e) = unhide_from_view(&name) {
        log!(WARN, "could not put {name} back on the desktop: {e}");
    }
}

/// Move each Desktop entry into the stash. Collisions are skipped (never
/// clobbered). Returns how many entries were moved.
///
/// Two kinds of entry stay put. `.hidden` is the control file itself, and
/// anything listed in it is a document a hedge already holds: it is invisible
/// on the desktop either way, and moving it would break the tile that opens it
/// by absolute path until the icons were toggled back.
pub fn hide() -> std::io::Result<usize> {
    let listed = read_hidden_list();
    move_all(&desktop_dir(), &stash_dir(), |name| {
        name != ".hidden" && !listed.iter().any(|n| n == name)
    })
}

/// Move each stashed entry back onto the Desktop. Collisions are skipped.
pub fn show() -> std::io::Result<usize> {
    let moved = move_all(&stash_dir(), &desktop_dir(), |_| true)?;
    // tidy up an emptied stash
    let stash = stash_dir();
    if let Ok(mut rd) = std::fs::read_dir(&stash) {
        if rd.next().is_none() {
            let _ = std::fs::remove_dir(&stash);
        }
    }
    Ok(moved)
}

fn move_all(
    src: &PathBuf,
    dst: &PathBuf,
    keep: impl Fn(&str) -> bool,
) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(src) {
        for entry in rd.flatten() {
            if !keep(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if to.exists() {
                continue; // never overwrite an existing target
            }
            // rename is atomic on the same filesystem (the normal case); if it
            // fails (e.g. cross-device) we leave the file untouched, never delete
            if std::fs::rename(&from, &to).is_ok() {
                n += 1;
            } else {
                log!(WARN, "could not move {} (left in place)", from.display());
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up, so no test ever touches the
    /// real ~/Desktop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!("hedge-hidden-test-{tag}"));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn list(&self) -> PathBuf {
            self.0.join(".hidden")
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hiding_is_one_name_per_line_and_idempotent() {
        let t = Tmp::new("basic");
        assert!(hide_in_list(&t.list(), "report.pdf").unwrap());
        assert!(hide_in_list(&t.list(), "Photos").unwrap());
        // hiding the same entry twice must not duplicate the line
        assert!(!hide_in_list(&t.list(), "report.pdf").unwrap());
        assert_eq!(std::fs::read_to_string(t.list()).unwrap(), "report.pdf\nPhotos\n");
    }

    #[test]
    fn unhiding_leaves_other_entries_alone() {
        let t = Tmp::new("unhide");
        std::fs::write(t.list(), "mine.txt\nreport.pdf\n").unwrap();
        assert!(unhide_in_list(&t.list(), "report.pdf").unwrap());
        assert_eq!(std::fs::read_to_string(t.list()).unwrap(), "mine.txt\n");
        // an entry we never hid is not ours to remove, and is not an error
        assert!(!unhide_in_list(&t.list(), "never-hidden").unwrap());
        assert_eq!(std::fs::read_to_string(t.list()).unwrap(), "mine.txt\n");
    }

    #[test]
    fn the_list_file_goes_when_the_last_entry_does() {
        let t = Tmp::new("empty");
        hide_in_list(&t.list(), "only.pdf").unwrap();
        unhide_in_list(&t.list(), "only.pdf").unwrap();
        assert!(!t.list().exists(), ".hidden should not linger empty");
    }

    #[test]
    fn entries_the_user_hid_by_hand_survive_a_round_trip() {
        let t = Tmp::new("preserve");
        std::fs::write(t.list(), "  spaced.txt  \n\nnotes.md\n").unwrap();
        hide_in_list(&t.list(), "report.pdf").unwrap();
        assert_eq!(
            std::fs::read_to_string(t.list()).unwrap(),
            "spaced.txt\nnotes.md\nreport.pdf\n"
        );
    }
}

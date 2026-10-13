//! Install, upgrade and uninstall Hedge **into the user's home** — no root,
//! no package manager, nothing outside `$HOME`.
//!
//! The binary installs *itself*: `hedge --install` copies the running
//! executable to `~/.local/share/hedge/hedge` and writes the four integration
//! files that make it feel installed —
//!
//! | file | what it does |
//! |---|---|
//! | `~/.config/autostart/hedge.desktop` | starts the daemon at login |
//! | `~/.local/share/applications/hedge-settings.desktop` | "Hedge Settings" in the app menu |
//! | `~/.local/share/nemo/actions/hedge.nemo_action` | desktop right-click → **Add a Hedge** |
//! | `~/.local/share/nemo/actions/hedge-toggle-desktop.nemo_action` | desktop right-click → toggle icons |
//!
//! Keeping this in the binary rather than in `install.sh` means there is one
//! list of installed files, and the same code both writes and removes it: the
//! Settings buttons, the startup prompt and the shell installer all call here,
//! so an uninstall can never miss a right-click action the installer added.
//!
//! Everything written is recorded in `install.json` next to the binary, which
//! doubles as the "what version is installed?" record.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::version::{self, Build};

/// Embedded so a lone binary can install itself with no source tree around.
const ICON_SVG: &[u8] = include_bytes!("../assets/icons/hedge.svg");

// -- locations ---------------------------------------------------------------

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".local/share"))
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".config"))
}

/// Where the installed copy lives (also holds `install.json` and the stash).
pub fn install_dir() -> PathBuf {
    data_home().join("hedge")
}

/// The installed executable.
pub fn binary_path() -> PathBuf {
    install_dir().join("hedge")
}

/// Record of what is installed.
pub fn manifest_path() -> PathBuf {
    install_dir().join("install.json")
}

/// Convenience symlink so `hedge` works from a terminal.
fn link_path() -> PathBuf {
    home().join(".local/bin/hedge")
}

fn autostart_path() -> PathBuf {
    config_home().join("autostart/hedge.desktop")
}

fn launcher_path() -> PathBuf {
    data_home().join("applications/hedge-settings.desktop")
}

fn icon_path() -> PathBuf {
    data_home().join("icons/hicolor/scalable/apps/hedge.svg")
}

fn nemo_add_path() -> PathBuf {
    data_home().join("nemo/actions/hedge.nemo_action")
}

fn nemo_toggle_path() -> PathBuf {
    data_home().join("nemo/actions/hedge-toggle-desktop.nemo_action")
}

/// Every path an install can own, whether or not a manifest lists it. Used by
/// uninstall so a pre-manifest install (or a half-written one) still comes out
/// cleanly — the right-click actions in particular.
fn known_paths() -> Vec<PathBuf> {
    vec![
        autostart_path(),
        launcher_path(),
        icon_path(),
        nemo_add_path(),
        nemo_toggle_path(),
        binary_path(),
    ]
}

// -- the manifest ------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Which build is installed — the thing the upgrade check compares.
    pub build: Build,
    /// Unix seconds when it was installed.
    pub installed_at: u64,
    /// The installed executable.
    pub binary: PathBuf,
    /// Everything the installer wrote, so uninstall removes exactly that.
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

/// Read the manifest, or `None` when Hedge isn't installed.
///
/// A manifest whose binary has gone missing counts as *not installed* — that's
/// the state left behind by deleting `~/.local/share/hedge` by hand.
pub fn installed() -> Option<Manifest> {
    let text = std::fs::read_to_string(manifest_path()).ok()?;
    let m: Manifest = serde_json::from_str(&text).ok()?;
    m.binary.exists().then_some(m)
}

/// Is this binary running from a system directory — i.e. did it arrive via a
/// package (the .deb) rather than the self-installer? A packaged Hedge is
/// upgraded by the package manager, so it must not offer to copy itself into
/// `$HOME` on top of that.
pub fn packaged() -> bool {
    std::env::current_exe()
        .ok()
        .map(|p| p.starts_with("/usr") || p.starts_with("/opt"))
        .unwrap_or(false)
}

/// Is the process we're in the installed copy?
pub fn running_is_installed() -> bool {
    match (
        std::env::current_exe().and_then(std::fs::canonicalize),
        std::fs::canonicalize(binary_path()),
    ) {
        (Ok(exe), Ok(bin)) => exe == bin,
        _ => false,
    }
}

/// What the UI (and the startup check) needs to know in one shot.
pub struct State {
    /// The build we're running right now.
    pub running: Build,
    /// The installed build, if any.
    pub installed: Option<Manifest>,
    /// True when this process *is* the installed binary.
    pub is_installed_copy: bool,
}

impl State {
    /// Installing would put a strictly newer build in place.
    pub fn upgrade_available(&self) -> bool {
        match &self.installed {
            Some(m) => self.running.is_newer_than(&m.build),
            None => false,
        }
    }
}

/// What the startup prompt would offer.
pub enum Offer {
    Install,
    Upgrade,
}

/// The whole "should we say anything at login?" decision, in one place.
///
/// `None` for the installed copy, for an installed copy that's already current
/// (or newer, i.e. we're the stale dev build), and hence for the common case.
pub fn install_state_if_prompt_worthwhile() -> Option<(State, Offer)> {
    if packaged() {
        return None;
    }
    let st = state();
    if st.is_installed_copy {
        return None;
    }
    let offer = if st.installed.is_none() {
        Offer::Install
    } else if st.upgrade_available() {
        Offer::Upgrade
    } else {
        return None;
    };
    Some((st, offer))
}

pub fn state() -> State {
    State {
        running: version::current(),
        installed: installed(),
        is_installed_copy: running_is_installed(),
    }
}

// -- results -----------------------------------------------------------------

/// Outcome of an install/uninstall: what changed, plus anything worth saying.
pub struct Report {
    pub touched: Vec<PathBuf>,
    pub notes: Vec<String>,
}

type Res<T> = Result<T, String>;

fn write_file(path: &Path, body: &str, touched: &mut Vec<PathBuf>) -> Res<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    touched.push(path.to_path_buf());
    Ok(())
}

// -- install -----------------------------------------------------------------

/// Copy this binary into place and write every integration file.
///
/// Safe to run over an existing install (that's what an upgrade is) and safe to
/// run *from* the installed copy: the executable is written to a temporary name
/// and renamed over the target, so a running Hedge never hits `ETXTBSY`.
pub fn install() -> Res<(Manifest, Report)> {
    let mut touched: Vec<PathBuf> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    // -- the executable ------------------------------------------------------
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    let bin = binary_path();
    if running_is_installed() {
        notes.push("Re-registered the already-installed binary.".into());
    } else {
        let tmp = bin.with_extension("new");
        let _ = std::fs::remove_file(&tmp);
        std::fs::copy(&exe, &tmp).map_err(|e| format!("copying binary: {e}"))?;
        set_executable(&tmp)?;
        // rename over the target: works even while the old copy is running
        std::fs::rename(&tmp, &bin).map_err(|e| format!("installing binary: {e}"))?;
    }
    touched.push(bin.clone());

    // -- `hedge` on PATH -----------------------------------------------------
    match link_hedge(&bin) {
        Ok(true) => touched.push(link_path()),
        Ok(false) => notes.push(format!(
            "Left {} alone — it exists and isn't ours.",
            link_path().display()
        )),
        Err(e) => notes.push(format!("Could not link {}: {e}", link_path().display())),
    }
    if !path_has_local_bin() {
        notes.push(format!(
            "{} isn't on your PATH, so the `hedge` command won't resolve in a terminal.",
            link_path().parent().unwrap_or(&home()).display()
        ));
    }

    // -- icon ----------------------------------------------------------------
    let icon = icon_path();
    if let Some(p) = icon.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    std::fs::write(&icon, ICON_SVG).map_err(|e| format!("{}: {e}", icon.display()))?;
    touched.push(icon);

    let b = bin.display().to_string();

    // -- autostart -----------------------------------------------------------
    write_file(
        &autostart_path(),
        &format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Hedge\n\
             Comment=Desktop app launcher\n\
             Exec={b}\n\
             Icon=hedge\n\
             X-GNOME-Autostart-enabled=true\n\
             NoDisplay=true\n"
        ),
        &mut touched,
    )?;

    // -- app-menu launcher ---------------------------------------------------
    write_file(
        &launcher_path(),
        &format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Hedge Settings\n\
             Comment=Configure your Hedges\n\
             Exec={b} --settings\n\
             Icon=hedge\n\
             Terminal=false\n\
             Categories=Settings;Utility;\n"
        ),
        &mut touched,
    )?;

    // -- desktop right-click (Nemo) ------------------------------------------
    write_file(
        &nemo_add_path(),
        &format!(
            "[Nemo Action]\n\
             Name=Add a Hedge\n\
             Comment=Add a new Hedge to the desktop\n\
             Exec={b} --add\n\
             Icon-Name=list-add\n\
             Selection=None\n\
             Extensions=any;\n"
        ),
        &mut touched,
    )?;
    write_file(
        &nemo_toggle_path(),
        &format!(
            "[Nemo Action]\n\
             Name=Toggle desktop icons\n\
             Comment=Hide or show the desktop icons\n\
             Exec={b} --toggle-desktop\n\
             Icon-Name=view-hidden\n\
             Selection=None\n\
             Extensions=any;\n"
        ),
        &mut touched,
    )?;

    // -- record it -----------------------------------------------------------
    let manifest = Manifest {
        build: version::current(),
        installed_at: now(),
        binary: bin,
        files: touched.clone(),
    };
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(manifest_path(), json).map_err(|e| format!("writing manifest: {e}"))?;

    refresh_desktop_database();

    Ok((manifest, Report { touched, notes }))
}

/// `~/.local/bin/hedge` -> the installed binary. Returns whether we linked.
///
/// An existing symlink is ours to replace (that's how upgrades and stale links
/// from older versions get fixed); a *real* file at that path is left alone.
fn link_hedge(bin: &Path) -> Res<bool> {
    let link = link_path();
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    match std::fs::symlink_metadata(&link) {
        Ok(md) if md.file_type().is_symlink() => {
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        }
        Ok(_) => return Ok(false), // a real file we didn't create
        Err(_) => {}
    }
    std::os::unix::fs::symlink(bin, &link).map_err(|e| e.to_string())?;
    Ok(true)
}

fn path_has_local_bin() -> bool {
    let want = link_path();
    let Some(dir) = want.parent() else { return false };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|e| e == dir))
        .unwrap_or(false)
}

fn set_executable(path: &Path) -> Res<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

/// Best-effort menu refresh; every desktop environment that needs it ships it.
fn refresh_desktop_database() {
    let dir = data_home().join("applications");
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// -- uninstall ---------------------------------------------------------------

/// Remove the installed copy and everything it registered.
///
/// Hidden desktop icons are un-hidden **first**: they live in a stash folder
/// that only Hedge knows how to empty, so removing the app while icons are
/// stashed would strand the user's Desktop files. Config and backups survive
/// unless `purge` is set.
pub fn uninstall(purge: bool) -> Res<Report> {
    let mut touched = Vec::new();
    let mut notes = Vec::new();

    // give the Desktop back before the tool that can do it disappears
    match crate::desktop::show() {
        Ok(0) => {}
        Ok(n) => notes.push(format!("Put {n} hidden desktop item(s) back on the Desktop.")),
        Err(e) => notes.push(format!("Could not restore hidden desktop icons: {e}")),
    }

    // manifest first, then the known set — a pre-manifest install still goes
    let mut targets: Vec<PathBuf> = installed().map(|m| m.files).unwrap_or_default();
    targets.extend(known_paths());
    targets.push(manifest_path());
    targets.push(link_path());
    // the pre-3.1 layout kept no manifest and no symlink; nothing else to add.
    targets.sort();
    targets.dedup();

    for p in targets {
        // only drop the symlink if it still points at our binary
        if p == link_path() {
            let ours = std::fs::read_link(&p)
                .map(|t| t == binary_path())
                .unwrap_or(false);
            if !ours {
                continue;
            }
        }
        match std::fs::remove_file(&p) {
            Ok(()) => touched.push(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => notes.push(format!("Could not remove {}: {e}", p.display())),
        }
    }

    if purge {
        for dir in [
            crate::config::config_dir(),
            crate::log::path().parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        ] {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => touched.push(dir),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => notes.push(format!("Could not remove {}: {e}", dir.display())),
            }
        }
    } else {
        notes.push(format!(
            "Your config and backups are still in {}.",
            crate::config::config_dir().display()
        ));
    }

    // leave the directory only if nothing of the user's is left in it
    let dir = install_dir();
    if let Ok(mut rd) = std::fs::read_dir(&dir) {
        if rd.next().is_none() {
            let _ = std::fs::remove_dir(&dir);
        } else if !purge {
            notes.push(format!("Left {} in place — it still has files.", dir.display()));
        }
    }

    refresh_desktop_database();
    Ok(Report { touched, notes })
}

// -- restart -----------------------------------------------------------------

/// Launch the installed binary a moment from now, detached from this process.
///
/// Used after an in-place upgrade: Hedge is single-instance, so the caller must
/// quit immediately after — the delay is what lets the old instance drop its
/// D-Bus name before the new one claims it.
pub fn spawn_delayed_restart() {
    let bin = binary_path();
    if !bin.exists() {
        return;
    }
    let Ok(quoted) = shlex::try_quote(&bin.to_string_lossy()).map(|s| s.into_owned()) else {
        return;
    };
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("sleep 1; exec {quoted}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

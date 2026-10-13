//! Fencing memory around the parts of the desktop that must survive a
//! runaway program: the display server, the Cinnamon session (which is where
//! Hedge itself runs) and the system services under them.
//!
//! The freeze you get when memory runs out is not the OOM killer being slow to
//! pick a victim. Long before it acts, the kernel evicts whatever it can — and
//! the code pages of Xorg and Cinnamon are as evictable as anything — so every
//! mouse move faults them back in from disk and the desktop stops answering.
//! cgroup v2 memory protection (`memory.min` / `memory.low`) is what keeps
//! those pages resident: memory under the floor is reclaimed last, or never.
//!
//! Two halves:
//!
//! - **Protection** — systemd drop-ins under `/etc/systemd/system`, written
//!   once with `pkexec` and applied live with `set-property --runtime`.
//!   Protection only reaches as deep as every ancestor allows (a child's
//!   effective floor is capped by its parent's), so the whole chain from the
//!   slice down to the scope gets a floor, not just the leaf.
//! - **Separation** — tiles launch into their own scope under `app.slice`
//!   ([`scoped_command`]). An app started in the session scope would share the
//!   session's floor, and the floor would protect the very program that is
//!   eating the machine.

use std::path::Path;

/// Name of every drop-in we write, so removal can find exactly ours.
const CONF: &str = "fence-memory.conf";
const DROPIN_ROOT: &str = "/etc/systemd/system";

/// One floor: the unit (or drop-in prefix) it goes on, its section, and the
/// properties.
struct Floor {
    /// Drop-in directory stem, e.g. `session-.scope` for every session.
    dropin: String,
    /// The unit running right now, to apply the floor live — `None` when there
    /// is no live unit to touch (a prefix whose instance we already cover).
    live: Option<String>,
    section: &'static str,
    props: Vec<(&'static str, String)>,
}

/// The floors, sized to this machine. Ancestors are listed before their
/// children so a live apply never gives a child more than its parent holds.
fn floors() -> Vec<Floor> {
    let total_kb = total_ram_kb().unwrap_or(0);
    // A small machine can't spare a gigabyte per layer; half of that is still
    // several times what Xorg and Cinnamon need to stay resident.
    let (desk, disp) = if total_kb >= 12 * 1024 * 1024 {
        ("1G", "512M")
    } else {
        ("512M", "256M")
    };
    let uid = current_uid();
    let session = current_session_scope();
    let display = display_manager();

    let mut out = vec![
        Floor {
            dropin: "system.slice".into(),
            live: Some("system.slice".into()),
            section: "Slice",
            props: vec![("MemoryMin", disp.into()), ("MemoryLow", desk.into())],
        },
        Floor {
            dropin: "user.slice".into(),
            live: Some("user.slice".into()),
            section: "Slice",
            props: vec![("MemoryMin", desk.into())],
        },
        Floor {
            dropin: "user-.slice".into(),
            live: uid.map(|u| format!("user-{u}.slice")),
            section: "Slice",
            props: vec![("MemoryMin", desk.into())],
        },
        Floor {
            dropin: "session-.scope".into(),
            live: session,
            section: "Scope",
            props: vec![("MemoryMin", desk.into())],
        },
    ];
    if let Some(dm) = display {
        out.insert(
            1,
            Floor {
                dropin: dm.clone(),
                live: Some(dm),
                section: "Service",
                // the OOM score only takes at the next start; the script
                // below sets it on the running processes as well
                props: vec![("MemoryMin", disp.into()), ("OOMScoreAdjust", "-900".into())],
            },
        );
    }
    out
}

/// What `status` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Our drop-ins are on disk.
    pub installed: bool,
    /// Bytes actually reserved for the session right now — what matters, as
    /// opposed to what the files say.
    pub session_min: u64,
    /// The same for the display server.
    pub display_min: u64,
}

pub fn status() -> Status {
    let installed = Path::new(DROPIN_ROOT)
        .join("session-.scope.d")
        .join(CONF)
        .exists();
    let read_min = |unit_path: Option<String>| -> u64 {
        unit_path
            .and_then(|p| std::fs::read_to_string(format!("/sys/fs/cgroup{p}/memory.min")).ok())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    Status {
        installed,
        session_min: read_min(own_cgroup()),
        display_min: read_min(display_manager().map(|dm| format!("/system.slice/{dm}"))),
    }
}

/// Shell script that writes the drop-ins and applies them live. Run as root.
///
/// Everything interpolated is either a constant or a unit name that passed
/// [`unit_name_ok`], so nothing here can be steered by file contents.
pub fn install_script() -> String {
    let mut s = String::from("set -e\n");
    for f in floors() {
        let dir = format!("{DROPIN_ROOT}/{}.d", f.dropin);
        s += &format!("mkdir -p '{dir}'\ncat > '{dir}/{CONF}' <<'EOF'\n");
        s += "# Written by Hedge: memory reserved so the desktop survives a runaway program.\n";
        s += &format!("[{}]\n", f.section);
        for (k, v) in &f.props {
            s += &format!("{k}={v}\n");
        }
        s += "EOF\n";
    }
    s += "systemctl daemon-reload\n";
    for f in floors() {
        let Some(unit) = f.live else { continue };
        let props: Vec<String> = f
            .props
            .iter()
            .filter(|(k, _)| k.starts_with("Memory"))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        // live apply is best effort: the files are what lasts
        s += &format!(
            "systemctl set-property --runtime '{unit}' {} || true\n",
            props.join(" ")
        );
    }
    if let Some(dm) = display_manager() {
        s += &oom_adjust(&dm, -900);
    }
    s
}

/// Shell script that removes every drop-in we wrote and lifts the floors.
pub fn remove_script() -> String {
    let mut s = String::new();
    for f in floors() {
        s += &format!("rm -f '{DROPIN_ROOT}/{}.d/{CONF}'\n", f.dropin);
        s += &format!("rmdir '{DROPIN_ROOT}/{}.d' 2>/dev/null || true\n", f.dropin);
    }
    s += "systemctl daemon-reload\n";
    // children first on the way down, the mirror of installing
    for f in floors().into_iter().rev() {
        let Some(unit) = f.live else { continue };
        let props: Vec<String> = f
            .props
            .iter()
            .filter(|(k, _)| k.starts_with("Memory"))
            .map(|(k, _)| format!("{k}=0"))
            .collect();
        s += &format!(
            "systemctl set-property --runtime '{unit}' {} || true\n",
            props.join(" ")
        );
    }
    if let Some(dm) = display_manager() {
        s += &oom_adjust(&dm, 0);
    }
    s
}

/// Set the OOM score of every process already in a system service.
fn oom_adjust(unit: &str, score: i32) -> String {
    format!(
        "for p in $(cat '/sys/fs/cgroup/system.slice/{unit}/cgroup.procs' 2>/dev/null); do \
         echo {score} > /proc/$p/oom_score_adj 2>/dev/null || true; done\n"
    )
}

/// Wrap a tile's command so it starts in its own scope under `app.slice`,
/// out of the protected session. `None` when there is no user systemd to ask,
/// in which case the caller launches it the old way.
pub fn scoped_command(command: &str) -> Option<String> {
    if !user_systemd_available() {
        return None;
    }
    let argv = gtk::glib::shell_parse_argv(command)
        .ok()
        .map(|v| v.into_iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        // anything the parser won't take is shell syntax; let a shell have it
        .unwrap_or_else(|| vec!["/bin/sh".into(), "-c".into(), command.into()]);
    let name = unit_stem(&argv[0]);
    let unit = format!("app-hedge-{name}-{:x}", gtk::glib::monotonic_time());
    let mut out = vec![
        "systemd-run".to_string(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        format!("--unit={unit}"),
        "--".into(),
    ];
    out.extend(argv);
    Some(
        out.iter()
            .map(|a| gtk::glib::shell_quote(a).to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// A program path reduced to what a unit name may hold.
fn unit_stem(program: &str) -> String {
    let base = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app");
    let s: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    if s.is_empty() {
        "app".into()
    } else {
        s
    }
}

fn user_systemd_available() -> bool {
    let bus = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| Path::new(&d).join("systemd/private").exists())
        .unwrap_or(false);
    bus && std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("systemd-run").is_file()))
}

/// Unit names go into a root shell script; only the characters systemd
/// itself allows get through.
fn unit_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-:".contains(c))
}

/// The unit behind `display-manager.service`, e.g. `lightdm.service`.
fn display_manager() -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["show", "-p", "Id", "--value", "display-manager.service"])
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (id.ends_with(".service") && unit_name_ok(&id)).then_some(id)
}

/// This process's cgroup path, e.g. `/user.slice/user-1000.slice/session-c4.scope`.
fn own_cgroup() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
}

/// The session scope Hedge runs in, when it runs in one.
fn current_session_scope() -> Option<String> {
    let cg = own_cgroup()?;
    let leaf = cg.rsplit('/').next()?;
    (leaf.starts_with("session-") && leaf.ends_with(".scope") && unit_name_ok(leaf))
        .then(|| leaf.to_string())
}

fn current_uid() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn total_ram_kb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Bytes in the form the Settings status line uses.
pub fn human_bytes(b: u64) -> String {
    let mb = b / 1024 / 1024;
    if mb >= 1024 && mb.is_multiple_of(1024) {
        format!("{} GB", mb / 1024)
    } else {
        format!("{mb} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_never_exceed_their_parent() {
        let parse = |v: &str| -> u64 {
            let (n, unit) = v.split_at(v.len() - 1);
            n.parse::<u64>().unwrap() * if unit == "G" { 1024 } else { 1 }
        };
        let min_of = |dropin: &str| {
            floors()
                .into_iter()
                .find(|f| f.dropin == dropin)
                .and_then(|f| f.props.iter().find(|(k, _)| *k == "MemoryMin").map(|(_, v)| parse(v)))
                .unwrap()
        };
        assert!(min_of("user-.slice") <= min_of("user.slice"));
        assert!(min_of("session-.scope") <= min_of("user-.slice"));
        if let Some(dm) = display_manager() {
            assert!(min_of(&dm) <= min_of("system.slice"));
        }
    }

    #[test]
    fn scripts_touch_only_our_files() {
        let install = install_script();
        let remove = remove_script();
        for line in remove.lines().filter(|l| l.starts_with("rm ")) {
            assert!(line.contains(&format!("/{CONF}'")), "{line}");
        }
        assert!(install.contains("session-.scope.d/fence-memory.conf"));
        assert!(!install.contains("$(") || install.contains("cgroup.procs"));
    }

    #[test]
    fn unit_names_with_shell_syntax_are_refused() {
        assert!(unit_name_ok("lightdm.service"));
        assert!(unit_name_ok("session-c4.scope"));
        assert!(!unit_name_ok("x'; rm -rf /; '.service"));
        assert!(!unit_name_ok("a b.service"));
        assert!(!unit_name_ok(""));
    }

    #[test]
    fn a_scoped_command_keeps_its_arguments() {
        if !user_systemd_available() {
            return;
        }
        let cmd = scoped_command("xdg-open '/home/me/My File.pdf'").unwrap();
        let argv: Vec<String> = gtk::glib::shell_parse_argv(&cmd)
            .unwrap()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv[0], "systemd-run");
        let dash = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[dash + 1..], ["xdg-open", "/home/me/My File.pdf"]);
        assert!(argv.iter().any(|a| a.starts_with("--unit=app-hedge-xdg_open-")));
    }

    #[test]
    fn unit_stems_are_safe() {
        assert_eq!(unit_stem("/usr/bin/google-chrome"), "google_chrome");
        assert_eq!(unit_stem("x'y"), "x_y");
    }
}

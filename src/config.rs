//! Read/write the single source of truth: `~/.config/hedge/config.json`.
//!
//! A faithful port of `config.py`. Hedges preserve insertion order
//! (`IndexMap`), JSON keys are camelCase, and any unknown keys on a hedge or
//! app entry round-trip untouched via `#[serde(flatten)]`.

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// -- per-hedge defaults ----------------------------------------------------
fn d_accent() -> String {
    "#7aa2f7".to_string()
}
fn d_bg_color() -> String {
    "#12141c".to_string()
}
fn d_bg_opacity() -> f64 {
    0.55
}
fn d_corner_radius() -> i64 {
    20
}
fn d_columns() -> i64 {
    4
}
fn d_icon_size() -> i64 {
    48
}
fn d_tile_spacing() -> i64 {
    12
}
fn d_padding() -> i64 {
    18
}
fn d_title_font_size() -> i64 {
    11
}
fn d_label_font_size() -> i64 {
    // matches the built-in .hedge-label rule in style.css
    10
}
fn d_label_color() -> String {
    // matches the built-in .hedge-label rule in style.css
    "#c7cad6".to_string()
}
fn d_true() -> bool {
    true
}
fn d_x() -> i64 {
    120
}
fn d_y() -> i64 {
    120
}

/// One launcher stored in a hedge: `{name, command, icon}` (+ any extra keys).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub icon: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AppEntry {
    pub fn new(name: &str, command: &str, icon: &str) -> Self {
        AppEntry {
            name: name.to_string(),
            command: command.to_string(),
            icon: icon.to_string(),
            extra: Map::new(),
        }
    }
}

/// A single hedge block. Field defaults mirror Python's `DEFAULT_HEDGE`, so a
/// missing key deserialises to the same value `_hedge_with_defaults` supplied.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hedge {
    #[serde(default = "d_accent")]
    pub accent: String,
    #[serde(default = "d_bg_color")]
    pub bg_color: String,
    #[serde(default = "d_bg_opacity")]
    pub bg_opacity: f64,
    #[serde(default = "d_corner_radius")]
    pub corner_radius: i64,
    #[serde(default = "d_columns")]
    pub columns: i64,
    #[serde(default = "d_icon_size")]
    pub icon_size: i64,
    #[serde(default = "d_tile_spacing")]
    pub tile_spacing: i64,
    #[serde(default = "d_padding")]
    pub padding: i64,
    /// Title bar font size in px (default 11).
    ///
    /// 11 is the largest size that fits the fixed 30px title bar — the bar's
    /// height is the font's line height, the padding is already 1px, and CSS
    /// has no max-height, so anything larger grows the bar by roughly 2px per
    /// step. Sizes below 11 keep the bar at exactly 30px.
    #[serde(default = "d_title_font_size")]
    pub title_font_size: i64,
    /// Font size in px of the app name under each tile (default 10).
    #[serde(default = "d_label_font_size")]
    pub label_font_size: i64,
    /// Colour of the app name under each tile.
    #[serde(default = "d_label_color")]
    pub label_color: String,
    /// Colour of the hedge's own name in the title bar. Empty means "pick one
    /// automatically" — black or white, whichever reads on the accent.
    #[serde(default)]
    pub title_color: String,
    #[serde(default = "d_true")]
    pub show_labels: bool,
    #[serde(default = "d_true")]
    pub show_title: bool,
    #[serde(default = "d_true")]
    pub show_background: bool,
    #[serde(default = "d_true")]
    pub expand_on_hover: bool,
    /// Rolled up to just its title bar (double-click the title bar to toggle).
    #[serde(default)]
    pub rolled: bool,
    /// When rolled up, hovering the title bar opens the hedge and holds it
    /// open for [`OPEN_HOLD_SECS`](crate::hedge_window::OPEN_HOLD_SECS).
    #[serde(default)]
    pub open_on_hover: bool,
    /// Draw the accent border permanently, not only while hovered.
    #[serde(default)]
    pub show_border: bool,
    /// Invisible until the pointer is over it. The window stays mapped and
    /// clickable — opacity does not affect input — so hovering brings it back.
    #[serde(default)]
    pub hide_until_hover: bool,
    #[serde(default = "d_x")]
    pub x: i64,
    #[serde(default = "d_y")]
    pub y: i64,
    #[serde(default)]
    pub edge: Option<String>,
    /// Explicit size once the user resizes; `None` = auto-fit.
    #[serde(default)]
    pub width: Option<i64>,
    #[serde(default)]
    pub height: Option<i64>,
    #[serde(default)]
    pub apps: Vec<AppEntry>,
    /// Any keys we don't model — preserved verbatim on save.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Hedge {
    fn default() -> Self {
        Hedge {
            accent: d_accent(),
            bg_color: d_bg_color(),
            bg_opacity: d_bg_opacity(),
            corner_radius: d_corner_radius(),
            columns: d_columns(),
            icon_size: d_icon_size(),
            tile_spacing: d_tile_spacing(),
            padding: d_padding(),
            title_font_size: d_title_font_size(),
            label_font_size: d_label_font_size(),
            label_color: d_label_color(),
            title_color: String::new(),
            show_labels: true,
            show_title: true,
            show_background: true,
            expand_on_hover: true,
            rolled: false,
            open_on_hover: false,
            show_border: false,
            hide_until_hover: false,
            x: d_x(),
            y: d_y(),
            edge: None,
            width: None,
            height: None,
            apps: Vec::new(),
            extra: Map::new(),
        }
    }
}

fn d_clock_x() -> i64 {
    60
}
fn d_clock_y() -> i64 {
    60
}
fn d_clock_font() -> i64 {
    48
}
fn d_clock_radius() -> i64 {
    16
}
fn d_clock_padding() -> i64 {
    14
}
fn d_clock_color() -> String {
    "#f4f5f9".to_string()
}

/// The clock desklet: one per desktop, switched on in Settings -> General.
///
/// Unlike a hedge this isn't a map — there is exactly one, and it is off until
/// asked for, so an existing config gains nothing visible on upgrade.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clock {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_clock_x")]
    pub x: i64,
    #[serde(default = "d_clock_y")]
    pub y: i64,
    /// 24-hour time. Off gives 12-hour with am/pm.
    #[serde(default = "d_true")]
    pub twenty_four_hour: bool,
    #[serde(default)]
    pub show_seconds: bool,
    #[serde(default = "d_true")]
    pub show_date: bool,
    /// Size of the time text in px; the date is drawn proportionally smaller.
    #[serde(default = "d_clock_font")]
    pub font_size: i64,
    #[serde(default = "d_clock_color")]
    pub color: String,
    /// Off by default: a clock reads best as bare text on the wallpaper.
    #[serde(default)]
    pub show_background: bool,
    /// Hairline border around the desklet panel, drawn in the text colour.
    #[serde(default)]
    pub show_border: bool,
    /// Corner rounding of the desklet panel; 0 is a square card. Only visible
    /// with the background switched on.
    #[serde(default = "d_clock_radius")]
    pub corner_radius: i64,
    /// Space between the panel edge and its contents — likewise only visible
    /// with the background on, which is why it isn't applied without it: a
    /// transparent panel has no edge to be inside of, and padding it would
    /// just shift the clock away from where it was dropped.
    #[serde(default = "d_clock_padding")]
    pub padding: i64,
    #[serde(default = "d_bg_color")]
    pub bg_color: String,
    #[serde(default = "d_bg_opacity")]
    pub bg_opacity: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Clock {
    fn default() -> Self {
        Clock {
            enabled: false,
            x: d_clock_x(),
            y: d_clock_y(),
            twenty_four_hour: true,
            show_seconds: false,
            show_date: true,
            font_size: d_clock_font(),
            color: d_clock_color(),
            show_background: false,
            show_border: false,
            corner_radius: d_clock_radius(),
            padding: d_clock_padding(),
            bg_color: d_bg_color(),
            bg_opacity: d_bg_opacity(),
            extra: Map::new(),
        }
    }
}

fn d_ping_interval() -> i64 {
    30
}
fn d_trace_target() -> String {
    "google.com".to_string()
}
fn d_ping_segments() -> i64 {
    // enough to fill the width of a row beside the name and the latency
    18
}

/// One monitored host in the ping table.
///
/// `parent` names another host by its `name`, which is what makes the table a
/// tree: a host behind a router that is itself down cannot be judged, so it is
/// reported as blocked (grey) rather than down (red). Empty = top level.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingHost {
    /// Display label. Unique across the table — it is what `parent` points at.
    #[serde(default)]
    pub name: String,
    /// Hostname or IP actually pinged.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub parent: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Host reachability monitoring, drawn as a compressed table under the clock.
///
/// Off and empty by default, so an existing config gains nothing on upgrade
/// and no subprocess ever runs unless the user asks for it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ping {
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between rounds. One round pings every host (parents first).
    #[serde(default = "d_ping_interval")]
    pub interval_sec: i64,
    /// Badges drawn per host. The row always covers the same span, so more
    /// badges means a finer-grained history, not a longer one.
    #[serde(default = "d_ping_segments")]
    pub segments: i64,
    /// What "fill from traceroute" traces to. Remembered so the next fill
    /// doesn't have to be retyped.
    #[serde(default = "d_trace_target")]
    pub trace_target: String,
    #[serde(default)]
    pub hosts: Vec<PingHost>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Ping {
    fn default() -> Self {
        Ping {
            enabled: false,
            interval_sec: d_ping_interval(),
            segments: d_ping_segments(),
            trace_target: d_trace_target(),
            hosts: Vec::new(),
            extra: Map::new(),
        }
    }
}

impl Ping {
    /// Interval clamped to something a desktop poller can sanely do.
    pub fn interval(&self) -> u64 {
        self.interval_sec.clamp(5, 3600) as u64
    }

    /// Badge count, clamped to what a desklet row can actually show.
    pub fn segment_count(&self) -> usize {
        self.segments.clamp(4, 48) as usize
    }

    /// `hosts` reordered so every host follows its parent, each paired with its
    /// indent depth — the order both the settings table and the desklet draw.
    ///
    /// Anything unreachable from a root (a dangling or circular parent) is
    /// appended at depth 0 rather than dropped: a host you can't see is a host
    /// you can't fix.
    pub fn tree_order(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut placed = vec![false; self.hosts.len()];
        // depth-first from each root, in config order
        fn walk(
            p: &Ping,
            parent: &str,
            depth: usize,
            placed: &mut Vec<bool>,
            out: &mut Vec<(usize, usize)>,
        ) {
            for (i, h) in p.hosts.iter().enumerate() {
                if placed[i] || h.parent != parent {
                    continue;
                }
                placed[i] = true;
                out.push((i, depth));
                walk(p, &h.name, depth + 1, placed, out);
            }
        }
        walk(self, "", 0, &mut placed, &mut out);
        for (i, _) in self.hosts.iter().enumerate() {
            if !placed[i] {
                out.push((i, 0));
            }
        }
        out
    }
}

fn d_sysmon_interval() -> i64 {
    2
}
fn d_mem_warning_minutes() -> i64 {
    5
}
fn d_mem_warning_min_percent() -> i64 {
    50
}
fn d_chart_bg() -> String {
    // darker than the hedge panels: a chart is read at a glance, and the
    // curve wants a quiet ground to sit on
    "#0b0d14".to_string()
}
fn d_chart_bg_opacity() -> f64 {
    0.45
}
fn d_chart_radius() -> i64 {
    6
}
fn d_gpu() -> String {
    "auto".to_string()
}
fn d_auto() -> String {
    "auto".to_string()
}

/// The mini charts under the clock: CPU, GPU, memory, network and the busiest
/// processes. Every section switches on and off on its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sysmon {
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between readings. Every value on a chart is a rate over this.
    #[serde(default = "d_sysmon_interval")]
    pub interval_sec: i64,
    #[serde(default = "d_true")]
    pub show_cpu: bool,
    #[serde(default = "d_true")]
    pub show_gpu: bool,
    #[serde(default = "d_true")]
    pub show_mem: bool,
    #[serde(default = "d_true")]
    pub show_net: bool,
    #[serde(default = "d_true")]
    pub show_processes: bool,
    /// Show the CPU and GPU clock speed beside their temperature.
    #[serde(default = "d_true")]
    pub show_clock_speed: bool,
    /// Card background behind each chart.
    #[serde(default = "d_chart_bg")]
    pub chart_bg_color: String,
    #[serde(default = "d_chart_bg_opacity")]
    pub chart_bg_opacity: f64,
    /// Corner rounding of each chart card; 0 is a square card.
    #[serde(default = "d_chart_radius")]
    pub chart_corner_radius: i64,
    /// `auto`, `off`, or an id from `sysmon::list_gpus` — `nvidia:0`, `amd:1`.
    #[serde(default = "d_gpu")]
    pub gpu: String,
    /// `auto` follows the default route; otherwise an interface name.
    #[serde(default = "d_auto")]
    pub net_interface: String,
    /// What clicking the CPU chart runs. Empty = whichever system monitor is
    /// installed.
    #[serde(default)]
    pub task_manager: String,
    /// Warn when memory use, at its current rate of climb, will run out.
    /// Independent of `enabled`: the warning matters most when nobody is
    /// looking at the charts.
    #[serde(default = "d_true")]
    pub mem_warning: bool,
    /// How far ahead the warning looks, in minutes.
    #[serde(default = "d_mem_warning_minutes")]
    pub mem_warning_minutes: i64,
    /// No warning while less than this share of memory is in use, however
    /// fast it climbs: a program loading gigabytes at start-up is not a leak.
    #[serde(default = "d_mem_warning_min_percent")]
    pub mem_warning_min_percent: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Sysmon {
    fn default() -> Self {
        Sysmon {
            enabled: false,
            interval_sec: d_sysmon_interval(),
            show_cpu: true,
            show_gpu: true,
            show_mem: true,
            show_net: true,
            show_processes: true,
            show_clock_speed: true,
            chart_bg_color: d_chart_bg(),
            chart_bg_opacity: d_chart_bg_opacity(),
            chart_corner_radius: d_chart_radius(),
            gpu: d_gpu(),
            net_interface: d_auto(),
            task_manager: String::new(),
            mem_warning: true,
            mem_warning_minutes: d_mem_warning_minutes(),
            mem_warning_min_percent: d_mem_warning_min_percent(),
            extra: Map::new(),
        }
    }
}

impl Sysmon {
    pub fn interval(&self) -> u64 {
        self.interval_sec.clamp(1, 60) as u64
    }

    /// The warning's horizon in seconds.
    pub fn mem_warning_secs(&self) -> f64 {
        self.mem_warning_minutes.clamp(1, 60) as f64 * 60.0
    }

    /// Whether the sampler has anything to do at all.
    pub fn sampling(&self) -> bool {
        self.enabled || self.mem_warning
    }
}

/// The Claude section of the desklet: local Claude Code sessions, plan usage,
/// and the service status. Each reading switches on and off on its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claude {
    #[serde(default)]
    pub enabled: bool,
    /// The live sessions on this machine, with a status dot each.
    #[serde(default = "d_true")]
    pub show_sessions: bool,
    /// The 5-hour / weekly / per-model usage bars. This is the one reading
    /// that talks to the network with the user's Claude Code sign-in.
    #[serde(default = "d_true")]
    pub show_usage: bool,
    /// The public status page's verdict on the service itself.
    #[serde(default = "d_true")]
    pub show_server_status: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Claude {
    fn default() -> Self {
        Claude {
            enabled: false,
            show_sessions: true,
            show_usage: true,
            show_server_status: true,
            extra: Map::new(),
        }
    }
}

/// The whole config: an insertion-ordered map of hedge name -> block.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    // `fences` is the key configs written before the fence → hedge rename use.
    #[serde(default, alias = "fences")]
    pub hedges: IndexMap<String, Hedge>,
    /// Whether desktop icons are currently stashed away (see `desktop` module).
    #[serde(default)]
    pub desktop_hidden: bool,
    /// The user said "never ask again" to the install/upgrade prompt. Settings
    /// still offers both, so this only silences the startup nag.
    #[serde(default)]
    pub install_prompt_dismissed: bool,
    /// The clock desklet.
    #[serde(default)]
    pub clock: Clock,
    /// Host monitoring, drawn under the clock desklet.
    #[serde(default)]
    pub ping: Ping,
    /// The mini charts, drawn under the host table.
    #[serde(default)]
    pub sysmon: Sysmon,
    /// Claude Code sessions, plan usage and service status, at the bottom of
    /// the desklet.
    #[serde(default)]
    pub claude: Claude,
    /// Whether the desklet shows the host table, and whether it shows the
    /// charts. The desklet's own icons drive these, and they are remembered
    /// across a restart. Shown by default, so switching a feature on in
    /// Settings puts something on screen without a second click.
    #[serde(default = "d_true")]
    pub panel_hosts: bool,
    #[serde(default = "d_true")]
    pub panel_charts: bool,
    #[serde(default = "d_true")]
    pub panel_claude: bool,
    /// Superseded by the two fields above, which split what used to be one
    /// chevron over the whole info panel. Only ever present in a file written
    /// by that build — it is never written back — so `load` can take it as the
    /// starting value for both and let it go.
    #[serde(default, skip_serializing)]
    pub panel_open: Option<bool>,
    /// Launch tiles straight into the desktop session instead of each into
    /// its own scope. Off — scopes are the default — because an app in the
    /// session would share the memory reserved for the desktop (see
    /// `memfence`). Stored inverted so a config written before the setting
    /// existed, and `Config::default()`, both get scopes.
    #[serde(default)]
    pub launch_unscoped: bool,
}

/// The four seed launchers dropped into a freshly created hedge.
pub fn starter_apps() -> Vec<AppEntry> {
    vec![
        AppEntry::new("Files", "nemo", "system-file-manager"),
        AppEntry::new("Terminal", "gnome-terminal", "utilities-terminal"),
        AppEntry::new("Browser", "xdg-open https://", "web-browser"),
        AppEntry::new("Settings", "cinnamon-settings", "preferences-system"),
    ]
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".config")
        });
    base.join("hedge")
}

/// Carry a config over from before the fence → hedge rename: when
/// `~/.config/hedge` doesn't exist yet but `~/.config/fence` does, move the
/// whole directory (config, backups, icons, snapshot) across. Nothing is
/// touched once `hedge` exists, so this only ever runs once.
pub fn migrate_from_fence() {
    let dir = config_dir();
    let old = dir.with_file_name("fence");
    if dir.exists() || !old.join("config.json").is_file() {
        return;
    }
    match std::fs::rename(&old, &dir) {
        Ok(()) => crate::log!("moved config {} -> {}", old.display(), dir.display()),
        Err(e) => crate::log!(WARN, "could not move config {}: {e}", old.display()),
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// A config holding one starter hedge named "Hedge".
pub fn default_config() -> Config {
    let mut hedges = IndexMap::new();
    let mut hedge = Hedge::default();
    hedge.apps = starter_apps();
    hedges.insert("Hedge".to_string(), hedge);
    Config {
        hedges,
        ..Default::default()
    }
}

/// Load the config, always returning a well-formed value. Any read/parse
/// error, or an empty/missing `hedges` map, falls back to `default_config`.
pub fn load() -> Config {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return default_config(),
    };
    let cfg = match serde_json::from_str::<Config>(&text) {
        Ok(cfg) if !cfg.hedges.is_empty() => cfg,
        // Parsed, but the user has no hedges left: re-seed the starter hedge
        // and keep everything else they'd set — deleting your last hedge must
        // not undo "never ask me about installing again".
        Ok(cfg) => Config {
            hedges: default_config().hedges,
            ..cfg
        },
        Err(_) => default_config(),
    };
    migrate(cfg)
}

/// Fold superseded keys into the ones that replaced them.
fn migrate(mut cfg: Config) -> Config {
    // One fold state over hosts *and* charts became one per section. Whatever
    // the old chevron was left at is where both start, so a desklet that was
    // folded away doesn't spring open on upgrade.
    if let Some(open) = cfg.panel_open.take() {
        cfg.panel_hosts = open;
        cfg.panel_charts = open;
    }
    cfg
}

/// Atomically write the config (pretty-printed with 4-space indent, matching
/// the Python `json.dump(..., indent=4)`).
pub fn save(cfg: &Config) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;

    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    cfg.serialize(&mut ser)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let path = config_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// -- backups ---------------------------------------------------------------

const KEEP_BACKUPS: usize = 15;

pub fn backup_dir() -> PathBuf {
    config_dir().join("backups")
}

/// Copy the current config.json into a timestamped backup, keeping the newest
/// `KEEP_BACKUPS`. Returns the backup path (None if there's nothing to back up).
pub fn make_backup() -> std::io::Result<Option<PathBuf>> {
    let src = config_path();
    if !src.exists() {
        return Ok(None);
    }
    let dir = backup_dir();
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut path = dir.join(format!("config-{ts}.json"));
    let mut i = 1;
    while path.exists() {
        path = dir.join(format!("config-{ts}-{i}.json"));
        i += 1;
    }
    std::fs::copy(&src, &path)?;
    rotate_backups(KEEP_BACKUPS);
    Ok(Some(path))
}

fn rotate_backups(keep: usize) {
    for p in list_backups().into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}

/// All backups, newest first (by modification time).
pub fn list_backups() -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(backup_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            let is_backup = p.extension().and_then(|s| s.to_str()) == Some("json")
                && p.file_name()
                    .and_then(|s| s.to_str())
                    .map_or(false, |n| n.starts_with("config-"));
            if is_backup {
                let mt = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                v.push((mt, p));
            }
        }
    }
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, p)| p).collect()
}

/// Restore the most recent backup, first snapshotting the current config as a
/// fresh backup (so a restore is itself undoable). Returns false if none exist.
pub fn restore_latest() -> std::io::Result<bool> {
    let latest = match list_backups().into_iter().next() {
        Some(p) => p,
        None => return Ok(false),
    };
    let _ = make_backup(); // safety snapshot of the current state
    std::fs::copy(&latest, config_path())?;
    Ok(true)
}

// -- convenience operations on a loaded config -----------------------------

pub fn hedge_names(cfg: &Config) -> Vec<String> {
    cfg.hedges.keys().cloned().collect()
}

/// The first free `base`, `base 2`, `base 3`, … name.
pub fn unique_name(cfg: &Config, base: &str) -> String {
    if !cfg.hedges.contains_key(base) {
        return base.to_string();
    }
    let mut i = 2;
    loop {
        let cand = format!("{} {}", base, i);
        if !cfg.hedges.contains_key(&cand) {
            return cand;
        }
        i += 1;
    }
}

/// Add a new hedge (optionally seeded with the starter apps); returns its name.
pub fn add_hedge(cfg: &mut Config, name: Option<&str>, seed_starter: bool) -> String {
    let name = name
        .map(str::to_string)
        .unwrap_or_else(|| unique_name(cfg, "Hedge"));
    let mut hedge = Hedge::default();
    if seed_starter {
        hedge.apps = starter_apps();
    }
    cfg.hedges.insert(name.clone(), hedge);
    name
}

pub fn remove_hedge(cfg: &mut Config, name: &str) {
    cfg.hedges.shift_remove(name);
}

/// Rename a hedge, preserving its position in the order. Returns false if the
/// old name is missing, the new name is empty, or it collides.
pub fn rename_hedge(cfg: &mut Config, old: &str, new: &str) -> bool {
    if !cfg.hedges.contains_key(old) || new.is_empty() || cfg.hedges.contains_key(new) {
        return false;
    }
    // rebuild the map so the renamed entry keeps its slot
    let rebuilt: IndexMap<String, Hedge> = cfg
        .hedges
        .drain(..)
        .map(|(k, v)| {
            if k == old {
                (new.to_string(), v)
            } else {
                (k, v)
            }
        })
        .collect();
    cfg.hedges = rebuilt;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let cfg = default_config();
        let json = {
            let mut buf = Vec::new();
            let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
            let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
            cfg.serialize(&mut ser).unwrap();
            String::from_utf8(buf).unwrap()
        };
        assert!(json.contains("\"bgColor\": \"#12141c\""));
        assert!(json.contains("\"expandOnHover\": true"));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hedges.len(), 1);
        assert_eq!(back.hedges["Hedge"].apps.len(), 4);
    }

    /// A config saved before the fence → hedge rename keeps its blocks, and
    /// is written back under the new key.
    #[test]
    fn a_pre_rename_config_still_loads() {
        let cfg: Config = serde_json::from_str(r#"{"fences": {"Work": {}}}"#).unwrap();
        assert_eq!(hedge_names(&cfg), vec!["Work"]);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"hedges\"") && !json.contains("\"fences\""));
    }

    /// The desklet's one chevron became two icons. Whatever the chevron was
    /// left at has to carry over to both, and the dead key must not come back.
    #[test]
    fn the_old_single_fold_state_seeds_both_sections() {
        let folded: Config = serde_json::from_str(r#"{"panel_open": false}"#).unwrap();
        let folded = migrate(folded);
        assert!(!folded.panel_hosts, "a folded desklet stays folded");
        assert!(!folded.panel_charts);
        assert_eq!(folded.panel_open, None, "the superseded key is consumed");

        let open = migrate(serde_json::from_str(r#"{"panel_open": true}"#).unwrap());
        assert!(open.panel_hosts && open.panel_charts);

        // written by this build: the two keys stand on their own
        let split: Config =
            serde_json::from_str(r#"{"panel_hosts": true, "panel_charts": false}"#).unwrap();
        let split = migrate(split);
        assert!(split.panel_hosts && !split.panel_charts);

        // a fresh config shows both, so switching a feature on shows something
        let fresh = migrate(serde_json::from_str("{}").unwrap());
        assert!(fresh.panel_hosts && fresh.panel_charts);

        // and the retired key is never written back
        let json = serde_json::to_string(&fresh).unwrap();
        assert!(!json.contains("panel_open"), "got: {json}");
    }

    #[test]
    fn missing_keys_get_defaults() {
        let f: Hedge = serde_json::from_str(r#"{"apps": []}"#).unwrap();
        assert_eq!(f.accent, "#7aa2f7");
        assert_eq!(f.columns, 4);
        assert!(f.show_labels);
        assert_eq!(f.edge, None);
    }

    #[test]
    fn unknown_keys_survive() {
        let f: Hedge = serde_json::from_str(r#"{"mystery": 7, "apps": []}"#).unwrap();
        assert_eq!(f.extra.get("mystery"), Some(&Value::from(7)));
    }

    #[test]
    fn rename_preserves_order() {
        let mut cfg = Config::default();
        add_hedge(&mut cfg, Some("A"), false);
        add_hedge(&mut cfg, Some("B"), false);
        add_hedge(&mut cfg, Some("C"), false);
        assert!(rename_hedge(&mut cfg, "B", "Bee"));
        assert_eq!(hedge_names(&cfg), vec!["A", "Bee", "C"]);
        assert!(!rename_hedge(&mut cfg, "A", "C")); // collision
    }

    #[test]
    fn unique_names() {
        let mut cfg = Config::default();
        assert_eq!(unique_name(&cfg, "Hedge"), "Hedge");
        add_hedge(&mut cfg, None, false);
        assert_eq!(unique_name(&cfg, "Hedge"), "Hedge 2");
    }
}

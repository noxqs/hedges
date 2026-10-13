//! Claude Code monitoring for the clock desklet: the live sessions on this
//! machine, plan usage, and whether the service itself is up.
//!
//! Three readings, three sources:
//!
//! - **Sessions** come from `~/.claude/sessions/<pid>.json`, which Claude Code
//!   rewrites on every state change. Files are read directly rather than via
//!   `claude agents --json` — that command reads the same files, after a
//!   multi-second binary startup. A session file outlives an unclean exit and
//!   PIDs get recycled, so a session only counts as live if `/proc/<pid>`
//!   exists *and* its start time matches the recorded one.
//! - **Usage** (the rolling 5-hour window, the week, and per-model weeks such
//!   as Fable) exists nowhere on disk; Claude Code learns it from
//!   `GET /api/oauth/usage` and keeps it in memory. So this asks the same
//!   endpoint with the OAuth token Claude Code already stores, read fresh each
//!   poll and sent only to `api.anthropic.com` — over a real HTTP client, so
//!   the token never appears in a process command line. Never written or
//!   refreshed here: a 401 just reads "signed out" until Claude Code renews it.
//! - **Service status** is the public statuspage summary.
//!
//! Same shape as `ping`/`sysmon`: timers on the main loop, the actual work on
//! a worker thread, one batch back over a `glib` channel, and only one round
//! of each kind ever in flight.

use std::cell::{Cell, RefCell};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Claude;

/// Session files are local reads; keep the rows close to live.
const SESSION_POLL_SECS: u32 = 3;
/// Usage barely moves inside five minutes, and this endpoint carries the
/// user's sign-in — poll it gently.
const USAGE_POLL_SECS: u32 = 300;
/// One quicker retry after a poll *starts* failing, so an error caught at
/// login — the first poll fires before the network is up — doesn't sit on
/// the desktop for five minutes. Only the first failure retries early;
/// while the error persists, the normal interval is gentle enough.
const USAGE_RETRY_SECS: u32 = 15;
/// The status page is public, cached, and the whole point of its strip is
/// seeing an incident start and end.
const SERVER_POLL_SECS: u32 = 30;
/// The strip keeps this many polls: 60 × 30 s = the last half hour.
const SERVER_HIST_CAP: usize = 60;
/// ...chunked into this many dots, so one dot is three minutes. Few enough
/// that the section stays narrower than the clock and the charts — the strip
/// must never be what decides the desklet's width.
pub const SERVER_DOTS: usize = 10;
/// How much of a transcript's tail is scanned for the newest model id.
const TAIL_BYTES: u64 = 64 * 1024;

const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const STATUS_ENDPOINT: &str = "https://status.anthropic.com/api/v2/status.json";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// -- sessions ---------------------------------------------------------------

/// Session status as Claude Code writes it. The set of values is fixed by the
/// CLI itself (`busy`, `shell`, `idle`, `waiting`); `Unknown` covers a future
/// value we don't recognise yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Blocked on the user: a question, a permission prompt, an open dialog.
    Waiting,
    /// Working on a turn.
    Busy,
    /// Idle, but a background shell is still running.
    Shell,
    /// Turn finished, waiting for the next prompt.
    Idle,
    Unknown,
}

impl Status {
    fn parse(s: &str) -> Self {
        match s {
            "waiting" => Status::Waiting,
            "busy" => Status::Busy,
            "shell" => Status::Shell,
            "idle" => Status::Idle,
            _ => Status::Unknown,
        }
    }

    /// Style-class suffix for the row's dot and name.
    pub fn css_class(self) -> &'static str {
        match self {
            Status::Waiting => "claude-waiting",
            Status::Busy => "claude-busy",
            Status::Shell | Status::Idle => "claude-idle",
            Status::Unknown => "claude-unknown",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Waiting => "needs you",
            Status::Busy => "working",
            Status::Shell => "idle — background shell running",
            Status::Idle => "idle",
            Status::Unknown => "unknown",
        }
    }
}

/// One live session, as the desklet wants to draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRow {
    /// The `claude` process, so a click can find the window hosting it.
    pub pid: i32,
    /// The name Claude derived for the session, falling back to the project.
    pub title: String,
    pub project: String,
    /// The model behind the most recent reply, e.g. `Opus 5`.
    pub model: Option<String>,
    pub status: Status,
    /// Why it is waiting: "permission prompt", "input needed", ...
    pub waiting_for: Option<String>,
    /// When the current status began, ms since the epoch.
    pub status_since: i64,
    /// Another live session shares the project, so the title alone is ambiguous.
    pub shares_project: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSession {
    pid: i32,
    session_id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    started_at: Option<i64>,
    status_updated_at: Option<i64>,
    /// Process start time in clock ticks, used to detect a recycled PID.
    proc_start: Option<String>,
}

fn claude_home() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".claude")
}

/// Field 22 of `/proc/<pid>/stat` — the process start time. The `comm` field
/// can contain spaces and parentheses, so parse after the final `)`.
fn proc_start_ticks(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` begins at field 3, so starttime (field 22) is index 19.
    rest.split_whitespace().nth(19).map(str::to_owned)
}

fn is_alive(pid: i32, recorded_start: Option<&str>) -> bool {
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return false;
    }
    match (recorded_start, proc_start_ticks(pid)) {
        (Some(recorded), Some(actual)) => recorded == actual,
        // no recorded start time to compare against: existence is all we have
        _ => true,
    }
}

/// Claude encodes a project path into a directory name by replacing every
/// non-alphanumeric character with `-`: `/home/u/My Proj` → `-home-u-My-Proj`.
fn encode_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The model behind the newest reply in a transcript's tail. `<synthetic>`
/// marks a message Claude Code composed itself, so it names no model and is
/// skipped; sidechain entries belong to subagents, not this conversation.
fn newest_model(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    let mut lines: Vec<&str> = text.lines().collect();
    // the first line is probably a fragment of an entry we seeked into
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
        .iter()
        .rev()
        // cheap pre-filter: parsing 64KB of JSON per session every poll adds up
        .filter(|l| l.contains("\"model\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .filter_map(|v| {
            v.get("message")?
                .get("model")?
                .as_str()
                .map(str::to_owned)
        })
        .find(|model| !model.starts_with('<'))
        .map(|id| short_model_name(&id))
}

/// Locate a session's transcript, falling back to a scan when the encoding
/// rule doesn't reproduce the directory name for this particular path.
fn find_transcript(cwd: &str, session_id: &str) -> Option<PathBuf> {
    let projects = claude_home().join("projects");
    let direct = projects
        .join(encode_project_dir(cwd))
        .join(format!("{session_id}.jsonl"));
    if direct.is_file() {
        return Some(direct);
    }
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Turn a model id into a row tag: `claude-opus-5` → `Opus 5`,
/// `claude-haiku-4-5-20251001` → `Haiku 4.5`, `claude-opus-5[1m]` → `Opus 5 1M`.
pub fn short_model_name(id: &str) -> String {
    let (base, suffix) = match id.split_once('[') {
        Some((base, rest)) => (base, rest.trim_end_matches(']').to_uppercase()),
        None => (id, String::new()),
    };
    let base = base.strip_prefix("claude-").unwrap_or(base);

    let parts: Vec<&str> = base
        .split('-')
        // drop the dated build suffix; it says nothing a person needs here
        .filter(|part| !(part.len() == 8 && part.chars().all(|c| c.is_ascii_digit())))
        .filter(|part| !part.is_empty())
        .collect();
    let Some((family, version)) = parts.split_first() else {
        return id.to_string();
    };

    let mut name = String::new();
    let mut letters = family.chars();
    if let Some(first) = letters.next() {
        name.extend(first.to_uppercase());
        name.push_str(letters.as_str());
    }
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    if !suffix.is_empty() {
        name.push(' ');
        name.push_str(&suffix);
    }
    name
}

/// All live sessions, alphabetical by project so a row stays put as its
/// status changes.
fn scan_sessions() -> Vec<SessionRow> {
    let dir = claude_home().join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut raws = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(file_pid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<RawSession>(&text) else {
            continue;
        };
        if raw.pid != file_pid || !is_alive(raw.pid, raw.proc_start.as_deref()) {
            continue;
        }
        if raw.session_id.is_none() {
            continue;
        }
        raws.push(raw);
    }

    let mut out: Vec<SessionRow> = raws
        .iter()
        .map(|raw| {
            let cwd = raw.cwd.clone().unwrap_or_default();
            let project = Path::new(&cwd)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.clone());
            let model = raw
                .session_id
                .as_deref()
                .and_then(|id| find_transcript(&cwd, id))
                .and_then(|p| newest_model(&p));
            SessionRow {
                pid: raw.pid,
                title: raw.name.clone().unwrap_or_else(|| project.clone()),
                project,
                model,
                status: raw
                    .status
                    .as_deref()
                    .map(Status::parse)
                    .unwrap_or(Status::Unknown),
                waiting_for: raw.waiting_for.clone(),
                status_since: raw
                    .status_updated_at
                    .or(raw.started_at)
                    .unwrap_or_default(),
                shares_project: false,
            }
        })
        .collect();

    for i in 0..out.len() {
        out[i].shares_project = out
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && other.project == out[i].project);
    }
    out.sort_by(|a, b| {
        a.project
            .to_lowercase()
            .cmp(&b.project.to_lowercase())
            .then(a.status_since.cmp(&b.status_since))
    });
    out
}

// -- click-to-focus ---------------------------------------------------------
// Ported from claudeManager's focus.rs. A `claude` process is a grandchild of
// whatever terminal or IDE it runs in, so walk up the process tree until a
// PID owns a window; when one process owns several (an IDE with several
// projects open), prefer the window whose title mentions the project.

fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` begins at field 3 (state), so ppid (field 4) is index 1
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn ancestry(pid: i32) -> Vec<i32> {
    let mut chain = vec![pid];
    let mut current = pid;
    for _ in 0..12 {
        match parent_pid(current) {
            Some(p) if p > 1 => {
                chain.push(p);
                current = p;
            }
            _ => break,
        }
    }
    chain
}

struct Win {
    id: String,
    pid: i32,
    title: String,
}

fn window_list() -> Vec<Win> {
    let Ok(out) = std::process::Command::new("wmctrl").arg("-lp").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 {
                return None;
            }
            Some(Win {
                id: parts[0].to_string(),
                pid: parts[2].parse().ok()?,
                title: parts[4..].join(" "),
            })
        })
        .collect()
}

/// Raise and focus the window hosting `pid`, best-effort. Blocking (runs
/// `wmctrl` twice): call it off the main loop.
pub fn focus_session(pid: i32, project: &str) -> bool {
    let windows = window_list();
    if windows.is_empty() {
        return false;
    }
    let project = project.to_lowercase();
    for ancestor in ancestry(pid) {
        let owned: Vec<&Win> = windows.iter().filter(|w| w.pid == ancestor).collect();
        if owned.is_empty() {
            continue;
        }
        let chosen = owned
            .iter()
            .find(|w| !project.is_empty() && w.title.to_lowercase().contains(&project))
            .unwrap_or(&owned[0]);
        return std::process::Command::new("wmctrl")
            .args(["-ia", &chosen.id])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    false
}

// -- plan usage -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindow {
    /// 0–100.
    pub percent: f64,
    /// When the window rolls over, ms since the epoch.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    /// Per-model weekly windows, e.g. ("Fable", 12%).
    pub per_model: Vec<(String, UsageWindow)>,
}

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Oauth,
}

#[derive(Deserialize)]
struct Oauth {
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[derive(Deserialize)]
struct RawWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct RawLimit {
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<RawScope>,
}

#[derive(Deserialize)]
struct RawScope {
    model: Option<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
    #[serde(default)]
    limits: Vec<RawLimit>,
}

/// Parse the RFC 3339 stamps the endpoint returns, e.g.
/// `2026-08-09T13:40:00.230417+00:00`, without a date library: only the UTC
/// offsets the API actually uses are handled, and anything else is ignored.
fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;

    let time = rest
        .split(['+', 'Z'])
        .next()?
        .split('-')
        .next()?
        .split('.')
        .next()?;
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next().unwrap_or("0").parse().ok()?;

    // days since the Unix epoch, by the usual civil-from-days algorithm
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    Some(((days * 86_400) + hour * 3_600 + minute * 60 + second) * 1000)
}

impl RawWindow {
    fn into_window(self) -> Option<UsageWindow> {
        Some(UsageWindow {
            percent: self.utilization?,
            resets_at: self.resets_at.as_deref().and_then(parse_timestamp),
        })
    }
}

/// Ask Anthropic for the current usage. Blocking: worker thread only.
fn fetch_usage() -> Result<Usage, String> {
    let path = claude_home().join(".credentials.json");
    let text = std::fs::read_to_string(path).map_err(|_| "not signed in".to_string())?;
    let credentials: Credentials =
        serde_json::from_str(&text).map_err(|e| format!("bad credentials file: {e}"))?;

    let response = ureq::get(USAGE_ENDPOINT)
        .header(
            "Authorization",
            &format!("Bearer {}", credentials.oauth.access_token),
        )
        .header("Content-Type", "application/json")
        .config()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(401) => "signed out".to_string(),
            // no route, no DNS, no answer: the machine is offline (or just
            // woke up), which is not worth a raw io error on the desktop
            ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed => "offline".to_string(),
            other => other.to_string(),
        })?;

    let raw: RawUsage = response
        .into_body()
        .read_json()
        .map_err(|e| format!("unreadable response: {e}"))?;

    // per-model weekly windows arrive in `limits`, tagged `weekly_scoped`
    let per_model = raw
        .limits
        .into_iter()
        .filter(|limit| limit.kind.as_deref() == Some("weekly_scoped"))
        .filter_map(|limit| {
            let name = limit.scope?.model?.display_name?;
            Some((
                name,
                UsageWindow {
                    percent: limit.percent?,
                    resets_at: limit.resets_at.as_deref().and_then(parse_timestamp),
                },
            ))
        })
        .collect();

    Ok(Usage {
        five_hour: raw.five_hour.and_then(RawWindow::into_window),
        seven_day: raw.seven_day.and_then(RawWindow::into_window),
        per_model,
    })
}

// -- service status ---------------------------------------------------------

/// The statuspage summary indicator, worst incident first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerHealth {
    Operational,
    Minor,
    Major,
    Critical,
    /// The page itself was unreachable — says nothing about the API.
    Unknown,
}

impl ServerHealth {
    pub fn css_class(self) -> &'static str {
        match self {
            ServerHealth::Operational => "claude-up",
            ServerHealth::Minor => "claude-mixed",
            ServerHealth::Major | ServerHealth::Critical => "claude-down",
            ServerHealth::Unknown => "claude-unknown",
        }
    }

    /// The verdict for one dot of the strip: the worst thing seen in its
    /// period. `Unknown` (the status *page* was unreachable) only shows when
    /// the whole period was unknown — it says nothing about the API, so a
    /// period that also holds a real reading is judged on that reading.
    pub fn of_period(chunk: &[ServerHealth]) -> ServerHealth {
        let known = chunk.iter().filter(|h| **h != ServerHealth::Unknown);
        known
            .max_by_key(|h| match h {
                ServerHealth::Operational => 0,
                ServerHealth::Minor => 1,
                ServerHealth::Major => 2,
                ServerHealth::Critical => 3,
                ServerHealth::Unknown => unreachable!(),
            })
            .copied()
            .unwrap_or(ServerHealth::Unknown)
    }

    /// Four characters, like "100%": the value column reserves the widest of
    /// these, so the panel cannot change width when the status flips.
    pub fn label(self) -> &'static str {
        match self {
            ServerHealth::Operational => "okay",
            ServerHealth::Minor => "warn",
            ServerHealth::Major => "down",
            ServerHealth::Critical => "crit",
            ServerHealth::Unknown => "n/a",
        }
    }
}

fn fetch_server_health() -> ServerHealth {
    let response = ureq::get(STATUS_ENDPOINT)
        .config()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .call();
    let Ok(response) = response else {
        return ServerHealth::Unknown;
    };
    let Ok(raw) = response.into_body().read_json::<Value>() else {
        return ServerHealth::Unknown;
    };
    match raw
        .get("status")
        .and_then(|s| s.get("indicator"))
        .and_then(Value::as_str)
    {
        Some("none") => ServerHealth::Operational,
        Some("minor") => ServerHealth::Minor,
        Some("major") => ServerHealth::Major,
        Some("critical") => ServerHealth::Critical,
        _ => ServerHealth::Unknown,
    }
}

// -- the monitor ------------------------------------------------------------

enum Msg {
    Sessions(Vec<SessionRow>),
    Usage(Result<Usage, String>),
    Server(ServerHealth),
}

struct Inner {
    /// The `claude` block as of the last `sync`, so a round never re-reads disk.
    cfg: RefCell<Claude>,
    sessions: RefCell<Vec<SessionRow>>,
    /// None until the first poll answers.
    usage: RefCell<Option<Result<Usage, String>>>,
    /// Status-page polls, oldest first — what the dot strip chunks.
    server_hist: RefCell<std::collections::VecDeque<ServerHealth>>,
    session_tick: RefCell<Option<glib::SourceId>>,
    usage_tick: RefCell<Option<glib::SourceId>>,
    server_tick: RefCell<Option<glib::SourceId>>,
    sessions_in_flight: Cell<bool>,
    usage_in_flight: Cell<bool>,
    server_in_flight: Cell<bool>,
    tx: glib::Sender<Msg>,
    /// Called on the main loop after a batch lands.
    on_update: RefCell<Option<Box<dyn Fn()>>>,
}

/// Clonable handle to the monitor.
#[derive(Clone)]
pub struct Monitor(Rc<Inner>);

impl Monitor {
    // Deprecated in favour of an async channel from another crate; kept for
    // the same reason ping.rs keeps it — see the note there.
    #[allow(deprecated)]
    pub fn new() -> Self {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let this = Monitor(Rc::new(Inner {
            cfg: RefCell::new(Claude::default()),
            sessions: RefCell::new(Vec::new()),
            usage: RefCell::new(None),
            server_hist: RefCell::new(std::collections::VecDeque::new()),
            session_tick: RefCell::new(None),
            usage_tick: RefCell::new(None),
            server_tick: RefCell::new(None),
            sessions_in_flight: Cell::new(false),
            usage_in_flight: Cell::new(false),
            server_in_flight: Cell::new(false),
            tx,
            on_update: RefCell::new(None),
        }));
        let m = this.clone();
        rx.attach(None, move |msg| {
            m.absorb(msg);
            glib::ControlFlow::Continue
        });
        this
    }

    pub fn connect_update(&self, f: impl Fn() + 'static) {
        *self.0.on_update.borrow_mut() = Some(Box::new(f));
    }

    /// Start, stop or re-scope the pollers to match the config. Cheap to call
    /// on every settings change: a poller already running is left alone, so
    /// toggling one reading doesn't blank another's history.
    pub fn sync(&self, cfg: &Claude) {
        *self.0.cfg.borrow_mut() = cfg.clone();

        // Each reading is its own poller on its own clock, and they all follow
        // the same shape: fire straight away when switched on so the panel
        // isn't blank for a whole interval, then tick.
        self.retime(
            cfg.enabled && cfg.show_sessions,
            &self.0.session_tick,
            SESSION_POLL_SECS,
            |m| m.session_round(),
            |m| m.0.sessions.borrow_mut().clear(),
        );
        self.retime(
            cfg.enabled && cfg.show_usage,
            &self.0.usage_tick,
            USAGE_POLL_SECS,
            |m| m.usage_round(),
            |m| *m.0.usage.borrow_mut() = None,
        );
        self.retime(
            cfg.enabled && cfg.show_server_status,
            &self.0.server_tick,
            SERVER_POLL_SECS,
            |m| m.server_round(),
            |m| m.0.server_hist.borrow_mut().clear(),
        );
    }

    fn retime(
        &self,
        want: bool,
        tick: &RefCell<Option<glib::SourceId>>,
        period: u32,
        round: fn(&Monitor),
        clear: fn(&Monitor),
    ) {
        if want && tick.borrow().is_none() {
            round(self);
            let this = self.clone();
            *tick.borrow_mut() = Some(glib::timeout_add_seconds_local(period, move || {
                round(&this);
                glib::ControlFlow::Continue
            }));
        }
        if !want {
            if let Some(id) = tick.borrow_mut().take() {
                id.remove();
            }
            clear(self);
        }
    }

    pub fn sessions(&self) -> Vec<SessionRow> {
        self.0.sessions.borrow().clone()
    }

    pub fn usage(&self) -> Option<Result<Usage, String>> {
        self.0.usage.borrow().clone()
    }

    /// The newest status-page verdict, or None before the first poll lands.
    pub fn server_now(&self) -> Option<ServerHealth> {
        self.0.server_hist.borrow().back().copied()
    }

    /// The strip: `SERVER_DOTS` periods, oldest left, each the worst thing
    /// seen in it. Dots the history can't fill yet come back Unknown.
    pub fn server_segments(&self) -> Vec<ServerHealth> {
        let hist = self.0.server_hist.borrow();
        let per_dot = SERVER_HIST_CAP / SERVER_DOTS;
        let samples: Vec<ServerHealth> = hist.iter().copied().collect();
        (0..SERVER_DOTS)
            .map(|dot| {
                // fill from the right, so "now" is the rightmost dot
                let have = samples.len();
                let end = have.saturating_sub((SERVER_DOTS - 1 - dot) * per_dot);
                let start = end.saturating_sub(per_dot);
                ServerHealth::of_period(&samples[start..end])
            })
            .collect()
    }

    fn absorb(&self, msg: Msg) {
        match msg {
            Msg::Sessions(rows) => {
                self.0.sessions_in_flight.set(false);
                *self.0.sessions.borrow_mut() = rows;
            }
            Msg::Usage(u) => {
                self.0.usage_in_flight.set(false);
                let newly_failed =
                    u.is_err() && !matches!(&*self.0.usage.borrow(), Some(Err(_)));
                *self.0.usage.borrow_mut() = Some(u);
                if newly_failed {
                    let this = self.clone();
                    glib::timeout_add_seconds_local_once(USAGE_RETRY_SECS, move || {
                        // only if the poller is still switched on
                        if this.0.usage_tick.borrow().is_some() {
                            this.usage_round();
                        }
                    });
                }
            }
            Msg::Server(s) => {
                self.0.server_in_flight.set(false);
                let mut hist = self.0.server_hist.borrow_mut();
                hist.push_back(s);
                while hist.len() > SERVER_HIST_CAP {
                    hist.pop_front();
                }
            }
        }
        // after the borrows: the callback redraws, which reads the state back
        if let Some(f) = self.0.on_update.borrow().as_ref() {
            f();
        }
    }

    fn session_round(&self) {
        if self.0.sessions_in_flight.get() {
            return;
        }
        self.0.sessions_in_flight.set(true);
        let tx = self.0.tx.clone();
        std::thread::spawn(move || {
            // a closed channel just means we're shutting down
            let _ = tx.send(Msg::Sessions(scan_sessions()));
        });
    }

    fn usage_round(&self) {
        if self.0.usage_in_flight.get() {
            log!(WARN, "claude usage round still running; skipping this tick");
            return;
        }
        self.0.usage_in_flight.set(true);
        let tx = self.0.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Usage(fetch_usage()));
        });
    }

    fn server_round(&self) {
        if self.0.server_in_flight.get() {
            // a poll outliving 30s is answered by the timeout below it
            return;
        }
        self.0.server_in_flight.set(true);
        let tx = self.0.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Server(fetch_server_health()));
        });
    }
}

/// Milliseconds as a compact age: "8s", "34m", "2h".
pub fn human_age(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    match secs {
        0..=99 => format!("{secs}s"),
        100..=5999 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// Milliseconds until a reset, as the usage row quotes it: "3h 14m left".
pub fn human_left(ms: i64) -> String {
    let mins = ms / 60_000;
    if mins < 1 {
        return "resets now".to_string();
    }
    let (d, h, m) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    match (d, h, m) {
        (0, 0, m) => format!("{m}m left"),
        (0, h, m) => format!("{h}h {m}m left"),
        (d, h, _) => format!("{d}d {h}h left"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_the_ids_that_actually_appear() {
        assert_eq!(short_model_name("claude-opus-5"), "Opus 5");
        assert_eq!(short_model_name("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(short_model_name("claude-fable-5"), "Fable 5");
        assert_eq!(short_model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(short_model_name("claude-opus-5[1m]"), "Opus 5 1M");
    }

    #[test]
    fn parses_the_api_timestamp_format() {
        assert_eq!(
            parse_timestamp("2026-08-09T13:40:00.230417+00:00"),
            Some(1_786_282_800_000)
        );
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("nonsense"), None);
    }

    #[test]
    fn a_dot_shows_the_worst_of_its_period_but_ignores_gaps() {
        use ServerHealth::*;
        assert_eq!(ServerHealth::of_period(&[Operational, Minor, Operational]), Minor);
        assert_eq!(ServerHealth::of_period(&[Minor, Major]), Major);
        // an unreachable status page says nothing about the API
        assert_eq!(ServerHealth::of_period(&[Operational, Unknown]), Operational);
        assert_eq!(ServerHealth::of_period(&[Unknown, Unknown]), Unknown);
        assert_eq!(ServerHealth::of_period(&[]), Unknown);
    }

    #[test]
    fn ages_and_resets_read_at_a_glance() {
        assert_eq!(human_age(8_000), "8s");
        assert_eq!(human_age(34 * 60_000), "34m");
        assert_eq!(human_age(3 * 3_600_000), "3h");
        assert_eq!(human_left(194 * 60_000), "3h 14m left");
        assert_eq!(human_left(30_000), "resets now");
        assert_eq!(human_left((6 * 1440 + 13 * 60) * 60_000), "6d 13h left");
    }
}

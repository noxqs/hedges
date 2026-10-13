//! System sampling for the desklet's mini charts: CPU, GPU, memory, network
//! and the processes currently burning the most CPU.
//!
//! Everything here is a rate, and a rate needs two readings — so the monitor
//! keeps the previous raw counters and every published [`Sample`] is the delta
//! since the last round. The first round after a start has nothing to compare
//! against and reports zero.
//!
//! Rounds run on a worker thread for the same reason [`crate::ping`] does:
//! `nvidia-smi` is a subprocess that can take a few hundred milliseconds, and
//! a desktop widget must never make the desktop wait.
//!
//! The memory history also feeds a forecast: the recent rate of climb, run
//! forward to the point where nothing is left, so a runaway process is
//! announced while there is still time to stop it rather than after the
//! desktop has frozen.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gtk::glib;

use crate::config::Sysmon;

/// Samples kept per series — the width of a chart's history.
pub const MAX_POINTS: usize = 60;
/// Processes listed under the charts.
pub const TOP_PROCS: usize = 5;
/// Seconds of memory history the forecast fits its trend to. Short enough
/// that a sudden leak shows up within a few rounds, long enough that one
/// allocation burst doesn't read as a trend.
const FORECAST_SECS: f64 = 30.0;

/// One round of readings. Percentages are 0-100, network is bytes per second.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub cpu: f64,
    pub cpu_temp: Option<f64>,
    /// Average core clock in MHz.
    pub cpu_clock: Option<f64>,
    pub gpu: Option<f64>,
    pub gpu_temp: Option<f64>,
    /// Graphics clock in MHz.
    pub gpu_clock: Option<f64>,
    /// Which card the reading actually came from — the chart says so, because
    /// `auto` picks for you and a machine can have both.
    pub gpu_vendor: Option<&'static str>,
    pub mem: f64,
    /// Bytes in use and total, for the memory chart's caption.
    pub mem_used_kb: u64,
    pub mem_total_kb: u64,
    pub net_rx: f64,
    pub net_tx: f64,
}

/// A process in the top-N list.
#[derive(Clone, Debug)]
pub struct Proc {
    pub name: String,
    /// Share of the whole machine's CPU time this round, 0-100.
    pub cpu: f64,
}

/// Which series a chart draws. Also the click target for CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Series {
    Cpu,
    Gpu,
    Mem,
    Net,
}

impl Series {
    pub fn title(self) -> &'static str {
        match self {
            Series::Cpu => "CPU",
            Series::Gpu => "GPU",
            Series::Mem => "MEM",
            Series::Net => "NET",
        }
    }
}

/// One GPU reading. A struct rather than a tuple: four `Option`s in a row is
/// four chances to swap two of them.
#[derive(Clone, Copy, Default)]
pub struct GpuReading {
    pub util: Option<f64>,
    pub temp: Option<f64>,
    pub clock: Option<f64>,
    pub vendor: Option<&'static str>,
}

/// A GPU the machine actually has, as offered in Settings.
pub struct Gpu {
    /// Config value: `"nvidia:0"`, `"amd:1"`, …
    pub id: String,
    pub label: String,
}

/// Raw counters from the previous round, shared with the worker thread.
#[derive(Default)]
struct Prev {
    /// (busy jiffies, total jiffies)
    cpu: Option<(u64, u64)>,
    /// (rx bytes, tx bytes, when)
    net: Option<(u64, u64, Instant)>,
    /// pid -> utime+stime jiffies
    procs: HashMap<i32, u64>,
}

type MemWarningFn = Box<dyn Fn(Option<f64>)>;

struct Inner {
    cfg: RefCell<Sysmon>,
    history: RefCell<VecDeque<Sample>>,
    procs: RefCell<Vec<Proc>>,
    prev: Arc<Mutex<Prev>>,
    tick: RefCell<Option<glib::SourceId>>,
    period: Cell<u64>,
    in_flight: Rc<Cell<bool>>,
    tx: glib::Sender<(Sample, Vec<Proc>)>,
    on_update: RefCell<Option<Box<dyn Fn()>>>,
    /// Seconds until memory runs out at the current rate, while that is
    /// inside the warning horizon.
    mem_eta: Cell<Option<f64>>,
    on_mem_warning: RefCell<Option<MemWarningFn>>,
}

/// Clonable handle to the sampler.
#[derive(Clone)]
pub struct Monitor(Rc<Inner>);

impl Monitor {
    // See the note on `crate::ping::Monitor::new` about this channel.
    #[allow(deprecated)]
    pub fn new() -> Self {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let this = Monitor(Rc::new(Inner {
            cfg: RefCell::new(Sysmon::default()),
            history: RefCell::new(VecDeque::new()),
            procs: RefCell::new(Vec::new()),
            prev: Arc::new(Mutex::new(Prev::default())),
            tick: RefCell::new(None),
            period: Cell::new(0),
            in_flight: Rc::new(Cell::new(false)),
            tx,
            on_update: RefCell::new(None),
            mem_eta: Cell::new(None),
            on_mem_warning: RefCell::new(None),
        }));
        let m = this.clone();
        rx.attach(None, move |(sample, procs)| {
            m.absorb(sample, procs);
            glib::ControlFlow::Continue
        });
        this
    }

    pub fn connect_update(&self, f: impl Fn() + 'static) {
        *self.0.on_update.borrow_mut() = Some(Box::new(f));
    }

    /// Called when the memory forecast enters the warning horizon (`Some`
    /// seconds left) and once more when it leaves it (`None`).
    pub fn connect_mem_warning(&self, f: impl Fn(Option<f64>) + 'static) {
        *self.0.on_mem_warning.borrow_mut() = Some(Box::new(f));
    }

    /// Seconds until memory runs out, while that is inside the warning horizon.
    pub fn mem_eta(&self) -> Option<f64> {
        self.0.mem_eta.get()
    }

    /// Start, stop or re-time sampling to match the config.
    pub fn sync(&self, cfg: &Sysmon) {
        *self.0.cfg.borrow_mut() = cfg.clone();
        if !cfg.mem_warning {
            self.set_mem_eta(None);
        }
        if !cfg.sampling() {
            self.stop();
            self.0.history.borrow_mut().clear();
            self.0.procs.borrow_mut().clear();
            *self.0.prev.lock().unwrap() = Prev::default();
            return;
        }
        let period = cfg.interval();
        if self.0.tick.borrow().is_some() && self.0.period.get() == period {
            return;
        }
        self.stop();
        self.0.period.set(period);
        let this = self.clone();
        this.round();
        let id = glib::timeout_add_seconds_local(period as u32, move || {
            this.round();
            glib::ControlFlow::Continue
        });
        *self.0.tick.borrow_mut() = Some(id);
    }

    pub fn stop(&self) {
        if let Some(id) = self.0.tick.borrow_mut().take() {
            id.remove();
        }
        self.0.period.set(0);
    }

    /// The whole history, oldest first.
    pub fn history(&self) -> Vec<Sample> {
        self.0.history.borrow().iter().copied().collect()
    }

    pub fn top_procs(&self) -> Vec<Proc> {
        self.0.procs.borrow().clone()
    }

    /// Ceiling the network chart is drawn against: the busiest second in
    /// history, so an idle link doesn't render as noise magnified to full
    /// scale. Floored at 64 KB/s.
    pub fn net_scale(&self) -> f64 {
        self.0
            .history
            .borrow()
            .iter()
            .map(|s| s.net_rx.max(s.net_tx))
            .fold(64.0 * 1024.0, f64::max)
    }

    fn absorb(&self, sample: Sample, procs: Vec<Proc>) {
        self.0.in_flight.set(false);
        {
            let mut hist = self.0.history.borrow_mut();
            hist.push_back(sample);
            while hist.len() > MAX_POINTS {
                hist.pop_front();
            }
        }
        *self.0.procs.borrow_mut() = procs;
        self.forecast();
        if let Some(f) = self.0.on_update.borrow().as_ref() {
            f();
        }
    }

    /// Re-run the memory forecast over the newest history.
    fn forecast(&self) {
        let cfg = self.0.cfg.borrow().clone();
        if !cfg.mem_warning {
            return;
        }
        let interval = cfg.interval() as f64;
        let horizon = cfg.mem_warning_secs();
        let (used, total, percent) = {
            let hist = self.0.history.borrow();
            let n = ((FORECAST_SECS / interval).ceil() as usize + 1).max(4);
            let used: Vec<f64> = hist
                .iter()
                .skip(hist.len().saturating_sub(n))
                .map(|s| s.mem_used_kb as f64)
                .collect();
            let last = hist.back();
            (used, last.map_or(0, |s| s.mem_total_kb), last.map_or(0.0, |s| s.mem))
        };
        let floor = cfg.mem_warning_min_percent.clamp(0, 95) as f64;
        let eta = seconds_until_full(&used, interval, total).filter(|_| percent >= floor);
        // Hysteresis: a warning raised stays up until the forecast is well
        // clear of the horizon, so a trend hovering at the line doesn't
        // pop a notification every other round.
        let limit = match self.0.mem_eta.get() {
            Some(_) => horizon * 2.0,
            None => horizon,
        };
        self.set_mem_eta(eta.filter(|&t| t <= limit));
    }

    fn set_mem_eta(&self, eta: Option<f64>) {
        let was = self.0.mem_eta.replace(eta);
        if was.is_some() != eta.is_some() {
            if let Some(f) = self.0.on_mem_warning.borrow().as_ref() {
                f(eta);
            }
        }
    }

    fn round(&self) {
        if self.0.in_flight.get() {
            return;
        }
        let cfg = self.0.cfg.borrow().clone();
        let prev = self.0.prev.clone();
        let tx = self.0.tx.clone();
        self.0.in_flight.set(true);
        std::thread::spawn(move || {
            let out = sample(&cfg, &prev);
            let _ = tx.send(out);
        });
    }
}

// -- sampling ---------------------------------------------------------------

/// One round, on the worker thread.
fn sample(cfg: &Sysmon, prev: &Arc<Mutex<Prev>>) -> (Sample, Vec<Proc>) {
    let mut out = Sample::default();
    let mut prev = prev.lock().unwrap();

    if let Some((used, total)) = read_mem_kb() {
        out.mem_used_kb = used;
        out.mem_total_kb = total;
        if total > 0 {
            out.mem = used as f64 / total as f64 * 100.0;
        }
    }
    // with the charts off only the memory warning is listening, and it
    // needs nothing else — least of all an nvidia-smi launch every round
    if !cfg.enabled {
        return (out, Vec::new());
    }

    // Kept for the process scan below: it has to divide by the *same* jiffy
    // delta the chart used, or the rows won't add up to the line.
    let mut cpu_total_delta = 0;
    if let Some((busy, total)) = read_cpu_jiffies() {
        if let Some((pb, pt)) = prev.cpu {
            cpu_total_delta = total.saturating_sub(pt);
            if cpu_total_delta > 0 {
                out.cpu = (busy.saturating_sub(pb) as f64 / cpu_total_delta as f64 * 100.0)
                    .clamp(0.0, 100.0);
            }
        }
        prev.cpu = Some((busy, total));
    }
    out.cpu_temp = read_cpu_temp();
    out.cpu_clock = read_cpu_clock();

    if cfg.show_gpu {
        let g = read_gpu(&cfg.gpu);
        out.gpu = g.util;
        out.gpu_temp = g.temp;
        out.gpu_clock = g.clock;
        out.gpu_vendor = g.vendor;
    }

    if let Some(iface) = resolve_interface(&cfg.net_interface) {
        if let Some((rx, tx)) = read_net_bytes(&iface) {
            let now = Instant::now();
            if let Some((prx, ptx, when)) = prev.net {
                let secs = now.duration_since(when).as_secs_f64().max(0.001);
                out.net_rx = rx.saturating_sub(prx) as f64 / secs;
                out.net_tx = tx.saturating_sub(ptx) as f64 / secs;
            }
            prev.net = Some((rx, tx, now));
        }
    }

    let procs = if cfg.show_processes {
        read_top_procs(&mut prev, cpu_total_delta)
    } else {
        Vec::new()
    };
    (out, procs)
}

fn read_cpu_jiffies() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut it = line.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let f: Vec<u64> = it.filter_map(|v| v.parse().ok()).collect();
    if f.len() < 4 {
        return None;
    }
    let total: u64 = f.iter().sum();
    // idle + iowait is the only part that isn't work
    let idle = f[3] + f.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// Package temperature, whichever driver this machine happens to have.
fn read_cpu_temp() -> Option<f64> {
    // AMD reports Tctl/Tdie, Intel "Package id 0"; anything else falls back to
    // the driver's first sensor.
    for driver in ["k10temp", "coretemp", "zenpower"] {
        if let Some(dir) = hwmon_dir(driver) {
            if let Some(t) = hwmon_temp(&dir, &["Tctl", "Tdie", "Package id 0"]) {
                return Some(t);
            }
        }
    }
    None
}

/// Average current core clock, in MHz.
///
/// `scaling_cur_freq` is the governor's live view and is what every other
/// monitor shows; `/proc/cpuinfo` is the fallback for a kernel built without
/// cpufreq, where the same number is only available per core as text.
fn read_cpu_clock() -> Option<f64> {
    let mut total = 0.0;
    let mut cores = 0u32;
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for e in entries.flatten() {
            let khz = read_trimmed(&e.path().join("cpufreq/scaling_cur_freq"))
                .and_then(|v| v.parse::<f64>().ok());
            if let Some(khz) = khz {
                total += khz / 1000.0;
                cores += 1;
            }
        }
    }
    if cores > 0 {
        return Some(total / cores as f64);
    }
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let mhz: Vec<f64> = text
        .lines()
        .filter(|l| l.starts_with("cpu MHz"))
        .filter_map(|l| l.split(':').nth(1)?.trim().parse().ok())
        .collect();
    (!mhz.is_empty()).then(|| mhz.iter().sum::<f64>() / mhz.len() as f64)
}

/// The hwmon directory for a driver name, e.g. `k10temp`.
fn hwmon_dir(name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if read_trimmed(&p.join("name")).as_deref() == Some(name) {
            return Some(p);
        }
    }
    None
}

/// A `tempN_input` from a hwmon directory, in °C. Prefers a sensor whose label
/// is one of `want`, else takes the lowest-numbered one.
fn hwmon_temp(dir: &Path, want: &[&str]) -> Option<f64> {
    let mut fallback = None;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("temp") && n.ends_with("_input"))
        })
        .collect();
    entries.sort();
    for input in entries {
        let millidegrees: f64 = read_trimmed(&input)?.parse().ok()?;
        let celsius = millidegrees / 1000.0;
        let label = read_trimmed(&input.with_file_name(
            input
                .file_name()?
                .to_str()?
                .replace("_input", "_label"),
        ));
        if label.as_deref().is_some_and(|l| want.contains(&l)) {
            return Some(celsius);
        }
        fallback.get_or_insert(celsius);
    }
    fallback
}

/// Every GPU we can read, for the Settings dropdown.
pub fn list_gpus() -> Vec<Gpu> {
    let mut out = Vec::new();
    if let Ok(o) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,name", "--format=csv,noheader"])
        .output()
    {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let (idx, name) = line.split_once(',').unwrap_or((line, ""));
                out.push(Gpu {
                    id: format!("nvidia:{}", idx.trim()),
                    label: format!("NVIDIA {}", name.trim()),
                });
            }
        }
    }
    for card in amd_cards() {
        let n = card
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("card")
            .to_string();
        out.push(Gpu {
            id: format!("amd:{}", n.trim_start_matches("card")),
            label: format!("AMD {n}"),
        });
    }
    out
}

/// `/sys/class/drm/cardN` paths that expose a busy percentage, lowest first.
fn amd_cards() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && !n.contains('-'))
                && p.join("device/gpu_busy_percent").exists()
        })
        .collect();
    out.sort();
    out
}

/// Utilisation, temperature and graphics clock for the configured GPU.
///
/// `auto` prefers NVIDIA: a machine with both usually has the AMD part as an
/// idle integrated GPU, and the discrete card is the one worth watching.
fn read_gpu(id: &str) -> GpuReading {
    let id = id.trim();
    if id == "off" {
        return GpuReading::default();
    }
    if let Some(n) = id.strip_prefix("nvidia:") {
        return read_nvidia(n);
    }
    if let Some(n) = id.strip_prefix("amd:") {
        return read_amd(&PathBuf::from(format!("/sys/class/drm/card{n}")));
    }
    // auto (or anything unrecognised)
    let nv = read_nvidia("0");
    if nv.util.is_some() {
        return nv;
    }
    match amd_cards().first() {
        Some(card) => read_amd(card),
        None => GpuReading::default(),
    }
}

fn read_nvidia(index: &str) -> GpuReading {
    // one query for all three: nvidia-smi costs a process launch, and asking
    // it three separate questions would cost three
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu,temperature.gpu,clocks.gr",
            "--format=csv,noheader,nounits",
            "-i",
            index,
        ])
        .output();
    let Ok(o) = out else {
        return GpuReading::default();
    };
    if !o.status.success() {
        return GpuReading::default();
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let Some(line) = text.lines().next() else {
        return GpuReading::default();
    };
    let mut f = line.split(',').map(|v| v.trim().parse::<f64>().ok());
    let util = f.next().flatten();
    GpuReading {
        util,
        temp: f.next().flatten(),
        clock: f.next().flatten(),
        vendor: util.map(|_| "NV"),
    }
}

fn read_amd(card: &Path) -> GpuReading {
    let util = read_trimmed(&card.join("device/gpu_busy_percent")).and_then(|v| v.parse().ok());
    // the card's own hwmon lives under its device directory, and carries both
    // the temperature and the shader clock (`freq1_input`, in hertz)
    let hwmon = std::fs::read_dir(card.join("device/hwmon"))
        .ok()
        .and_then(|mut d| d.next())
        .and_then(|e| e.ok())
        .map(|e| e.path());
    let temp = hwmon
        .as_deref()
        .and_then(|d| hwmon_temp(d, &["edge", "junction"]));
    let clock = hwmon
        .as_deref()
        .and_then(|d| read_trimmed(&d.join("freq1_input")))
        .and_then(|v| v.parse::<f64>().ok())
        .map(|hz| hz / 1_000_000.0);
    GpuReading {
        util,
        temp,
        clock,
        vendor: util.map(|_| "AMD"),
    }
}

fn read_mem_kb() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |key: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = field("MemTotal:")?;
    // MemAvailable is the kernel's own estimate of what a new allocation could
    // get, which is the number a user means by "free" — free+cached is not.
    let available = field("MemAvailable:").unwrap_or_else(|| field("MemFree:").unwrap_or(0));
    Some((total.saturating_sub(available), total))
}

/// Seconds until memory runs out if it keeps climbing at its recent rate.
///
/// `used` is kilobytes in use, one reading per `interval` seconds, oldest
/// first. The trend is a least-squares line rather than first-to-last, so a
/// single spike at either end can't fake or hide one. "Out" is a reserve
/// short of the total: the desktop stops responding while the kernel is
/// still scraping the last few percent together, not at zero.
///
/// `None` when memory isn't climbing, or there is too little history to say.
pub fn seconds_until_full(used: &[f64], interval: f64, total_kb: u64) -> Option<f64> {
    if used.len() < 4 || total_kb == 0 || interval <= 0.0 {
        return None;
    }
    let n = used.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = used.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (i, y) in used.iter().enumerate() {
        let dx = i as f64 - mean_x;
        sxy += dx * (y - mean_y);
        sxx += dx * dx;
    }
    let per_sec = sxy / sxx / interval;
    if per_sec <= 0.0 {
        return None;
    }
    let total = total_kb as f64;
    let reserve = (total * 0.03).max(256.0 * 1024.0);
    // measured from the fitted line's newest point, not the raw last reading
    let now = mean_y + sxy / sxx * (n - 1.0 - mean_x);
    let headroom = total - reserve - now;
    Some((headroom / per_sec).max(0.0))
}

/// The process holding the most memory right now, with its resident size in
/// kilobytes — the usual suspect when the forecast fires.
pub fn biggest_process() -> Option<(String, u64)> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut best: Option<(String, u64)> = None;
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue; // exited, or not ours to read
        };
        let field = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim().to_string())
        };
        let Some(rss) = field("VmRSS:")
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        else {
            continue; // kernel threads have no VmRSS
        };
        if best.as_ref().is_none_or(|(_, b)| rss > *b) {
            best = Some((field("Name:").unwrap_or_default(), rss));
        }
    }
    best
}

/// A forecast in the narrowest sensible form: `45s`, `4m`.
pub fn human_eta(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs.max(0.0))
    } else {
        format!("{:.0}m", (secs / 60.0).round())
    }
}

/// The configured interface, or the one carrying the default route.
fn resolve_interface(configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() && configured != "auto" {
        return Some(configured.to_string());
    }
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    routes
        .lines()
        .skip(1)
        .find(|l| l.split_whitespace().nth(1) == Some("00000000"))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_string)
}

/// Interfaces worth offering in Settings: everything but loopback.
pub fn list_interfaces() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n != "lo")
        .collect();
    out.sort();
    out
}

fn read_net_bytes(iface: &str) -> Option<(u64, u64)> {
    let base = PathBuf::from("/sys/class/net").join(iface).join("statistics");
    let rx = read_trimmed(&base.join("rx_bytes"))?.parse().ok()?;
    let tx = read_trimmed(&base.join("tx_bytes"))?.parse().ok()?;
    Some((rx, tx))
}

/// The busiest processes since the previous round.
///
/// Deltas, not `ps`: the `%CPU` column of `ps` is an average over the
/// process's whole lifetime, so a browser that was busy an hour ago outranks
/// the compile that is pinning the machine right now.
fn read_top_procs(prev: &mut Prev, cpu_total_delta: u64) -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut now: HashMap<i32, u64> = HashMap::new();
    let mut names: HashMap<i32, String> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // a process that exited between readdir and read
        };
        // comm is parenthesised and may itself contain spaces or brackets, so
        // the fields after it are found from the *last* ')'
        let Some(open) = stat.find('(') else { continue };
        let Some(close) = stat.rfind(')') else { continue };
        let name = stat[open + 1..close].to_string();
        let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        if rest.len() < 13 {
            continue;
        }
        let utime: u64 = rest[11].parse().unwrap_or(0);
        let stime: u64 = rest[12].parse().unwrap_or(0);
        now.insert(pid, utime + stime);
        names.insert(pid, name);
    }

    let mut rows: Vec<Proc> = now
        .iter()
        .filter_map(|(pid, jiffies)| {
            if cpu_total_delta == 0 {
                return None; // first round: nothing to compare against yet
            }
            let before = prev.procs.get(pid)?;
            let delta = jiffies.saturating_sub(*before);
            let cpu = delta as f64 / cpu_total_delta as f64 * 100.0;
            (cpu >= 0.05).then(|| Proc {
                name: names.get(pid).cloned().unwrap_or_default(),
                cpu,
            })
        })
        .collect();
    rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    rows.truncate(TOP_PROCS);
    prev.procs = now;
    rows
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The command to run when the CPU chart is clicked.
///
/// The config wins; otherwise the first system monitor actually installed.
pub fn task_manager_command(configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    [
        "gnome-system-monitor",
        "mate-system-monitor",
        "plasma-systemmonitor",
        "xfce4-taskmanager",
        "ksysguard",
    ]
    .into_iter()
    .find(|c| which(c))
    .map(str::to_string)
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Bytes per second, in the narrowest sensible form.
pub fn human_rate(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["B/s", "K/s", "M/s", "G/s"];
    let mut v = bytes;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 || v >= 100.0 {
        format!("{v:.0}{}", UNITS[unit])
    } else {
        format!("{v:.1}{}", UNITS[unit])
    }
}

/// A clock speed in MHz, in the narrowest sensible form.
pub fn human_clock(mhz: f64) -> String {
    if mhz >= 1000.0 {
        format!("{:.1}GHz", mhz / 1000.0)
    } else {
        format!("{mhz:.0}MHz")
    }
}

/// Kilobytes as gigabytes, for the memory caption.
pub fn human_kb(kb: u64) -> String {
    let gb = kb as f64 / 1024.0 / 1024.0;
    if gb >= 10.0 {
        format!("{gb:.0}G")
    } else {
        format!("{gb:.1}G")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_jiffies_are_busy_over_total() {
        // the running machine always has a first /proc/stat line
        let (busy, total) = read_cpu_jiffies().expect("/proc/stat");
        assert!(total > 0 && busy <= total);
    }

    #[test]
    fn memory_is_used_out_of_total() {
        let (used, total) = read_mem_kb().expect("/proc/meminfo");
        assert!(total > 0 && used < total);
    }

    #[test]
    fn a_steady_climb_is_run_forward_to_the_reserve() {
        const GB: f64 = 1024.0 * 1024.0;
        let total = (16.0 * GB) as u64;
        // 8 GB used, climbing 100 MB a second, sampled every 2 s
        let used: Vec<f64> = (0..10).map(|i| 8.0 * GB + i as f64 * 200.0 * 1024.0).collect();
        let eta = seconds_until_full(&used, 2.0, total).unwrap();
        let headroom = 16.0 * GB - 16.0 * GB * 0.03 - used[9];
        assert!((eta - headroom / (100.0 * 1024.0)).abs() < 0.5, "eta {eta}");
    }

    #[test]
    fn flat_or_falling_memory_has_no_forecast() {
        let total = 16 * 1024 * 1024;
        assert_eq!(seconds_until_full(&[4e6; 10], 2.0, total), None);
        let falling: Vec<f64> = (0..10).map(|i| 8e6 - i as f64 * 1e5).collect();
        assert_eq!(seconds_until_full(&falling, 2.0, total), None);
        // too short a history to call a trend
        assert_eq!(seconds_until_full(&[1e6, 2e6, 3e6], 2.0, total), None);
    }

    #[test]
    fn already_past_the_reserve_is_zero_not_negative() {
        let total = 16 * 1024 * 1024;
        let used: Vec<f64> = (0..6).map(|i| total as f64 - 1000.0 + i as f64 * 100.0).collect();
        assert_eq!(seconds_until_full(&used, 1.0, total), Some(0.0));
    }

    #[test]
    fn some_process_holds_memory() {
        let (_, rss) = biggest_process().expect("this test process at least");
        assert!(rss > 0);
    }

    #[test]
    fn the_cpu_reports_a_plausible_clock() {
        let mhz = read_cpu_clock().expect("cpufreq or /proc/cpuinfo");
        assert!(mhz > 100.0 && mhz < 20_000.0, "implausible clock: {mhz}");
    }

    #[test]
    fn clocks_pick_a_readable_unit() {
        assert_eq!(human_clock(600.0), "600MHz");
        assert_eq!(human_clock(4424.0), "4.4GHz");
        assert_eq!(human_clock(1000.0), "1.0GHz");
    }

    #[test]
    fn rates_pick_a_readable_unit() {
        assert_eq!(human_rate(512.0), "512B/s");
        assert_eq!(human_rate(2048.0), "2.0K/s");
        assert_eq!(human_rate(5.0 * 1024.0 * 1024.0), "5.0M/s");
        assert_eq!(human_rate(300.0 * 1024.0), "300K/s");
    }

    #[test]
    fn a_second_scan_ranks_processes_by_recent_cpu() {
        let mut prev = Prev::default();
        let before = read_cpu_jiffies().expect("/proc/stat").1;
        // the first scan has no deltas to work from, only priming to do
        assert!(read_top_procs(&mut prev, 0).is_empty());
        assert!(!prev.procs.is_empty());

        // burn a little CPU so there is something to attribute
        let mut n: u64 = 0;
        for i in 0..20_000_000u64 {
            n = n.wrapping_add(i);
        }
        assert!(n > 0);

        let delta = read_cpu_jiffies().expect("/proc/stat").1 - before;
        let rows = read_top_procs(&mut prev, delta.max(1));
        assert!(rows.len() <= TOP_PROCS);
        assert!(rows.iter().all(|p| p.cpu >= 0.0 && p.cpu <= 100.0));
        assert!(rows.windows(2).all(|w| w[0].cpu >= w[1].cpu));
    }
}

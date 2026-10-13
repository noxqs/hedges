//! Host reachability monitoring for the clock desklet.
//!
//! One round every `ping.intervalSec` walks the host tree parents-first and
//! shells out to `ping -c 1` per host. The point of the tree is honesty: a
//! machine behind a router that is itself down has not been proven down, it
//! simply cannot be reached, so it is recorded as **blocked** (grey) instead
//! of **down** (red) and isn't pinged at all that round.
//!
//! Rounds run on a worker thread — a `ping` that has to wait out its timeout
//! would otherwise freeze the desktop — and the batch of results comes back to
//! the main loop over a `glib` channel. Only one round is ever in flight; if a
//! round outlives its interval the next tick is skipped rather than queued.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use gtk::glib;

use crate::config::Ping;

/// How far back the whole badge row reaches, however many badges it is split
/// into: the row means the same span at a 5-second interval as at a 5-minute
/// one, and adding badges buys resolution rather than history.
pub const SPAN_SECS: u64 = 3 * 3600;

/// Rounds one badge covers at a given interval and badge count, at least one.
pub fn samples_per_segment(interval: u64, segments: usize) -> usize {
    ((SPAN_SECS / segments.max(1) as u64) / interval.max(1)).max(1) as usize
}

/// Seconds one badge covers — what the tooltip quotes.
pub fn segment_span(segments: usize) -> u64 {
    SPAN_SECS / segments.max(1) as u64
}

/// One round's verdict for one host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    /// Answered, with the round-trip time when `ping` reported one.
    Up(Option<f64>),
    /// Pinged and did not answer.
    Down,
    /// Not pinged: something it sits behind was down this round.
    Blocked,
}

/// What a badge says about the rounds it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Up for every round in the period.
    Up,
    /// Up for some of them, not all.
    Mixed,
    /// Down for every round.
    Down,
    /// Unreachable for every round because an ancestor was down.
    Blocked,
    /// No rounds recorded yet.
    Unknown,
}

impl Health {
    /// Style-class suffix, shared by the desklet badges and the settings table.
    pub fn css_class(self) -> &'static str {
        match self {
            Health::Up => "ping-up",
            Health::Mixed => "ping-mixed",
            Health::Down => "ping-down",
            Health::Blocked => "ping-blocked",
            Health::Unknown => "ping-unknown",
        }
    }

    fn of(chunk: &[Sample]) -> Health {
        if chunk.is_empty() {
            return Health::Unknown;
        }
        let up = chunk.iter().filter(|s| matches!(s, Sample::Up(_))).count();
        let down = chunk.iter().filter(|s| **s == Sample::Down).count();
        let blocked = chunk.len() - up - down;
        match (up, down, blocked) {
            (_, 0, 0) => Health::Up,
            (0, _, 0) => Health::Down,
            (0, 0, _) => Health::Blocked,
            _ => Health::Mixed,
        }
    }
}

/// A host as the desklet and the settings table want to draw it.
pub struct HostStatus {
    pub name: String,
    pub host: String,
    /// Indent level in the parent tree.
    pub depth: usize,
    /// The most recent round, or None before the first one lands.
    pub last: Option<Sample>,
    /// Oldest period first, newest last.
    pub segments: Vec<Health>,
}

impl HostStatus {
    /// Latency of the last round, rendered for a very narrow column.
    pub fn latency_label(&self) -> String {
        match self.last {
            Some(Sample::Up(Some(ms))) if ms >= 100.0 => format!("{ms:.0}ms"),
            Some(Sample::Up(Some(ms))) => format!("{ms:.1}ms"),
            Some(Sample::Up(None)) => "up".to_string(),
            Some(Sample::Down) => "down".to_string(),
            Some(Sample::Blocked) => "—".to_string(),
            None => "…".to_string(),
        }
    }

    /// Health of the newest period — what the row's name is coloured by.
    pub fn current(&self) -> Health {
        match self.last {
            Some(Sample::Up(_)) => Health::Up,
            Some(Sample::Down) => Health::Down,
            Some(Sample::Blocked) => Health::Blocked,
            None => Health::Unknown,
        }
    }
}

/// What the worker thread needs to run a round; owned, so it can cross threads.
#[derive(Clone)]
struct Spec {
    name: String,
    host: String,
    parent: String,
}

struct Inner {
    /// The `ping` block as of the last `sync`, so a round never re-reads disk.
    cfg: RefCell<Ping>,
    /// name -> rounds, oldest first.
    history: RefCell<HashMap<String, VecDeque<Sample>>>,
    tick: RefCell<Option<glib::SourceId>>,
    /// Interval the running timer was created with, so a config change only
    /// restarts the timer when it actually changed.
    period: Cell<u64>,
    in_flight: Rc<Cell<bool>>,
    tx: glib::Sender<Vec<(String, Sample)>>,
    /// Called on the main loop after every round lands.
    on_update: RefCell<Option<Box<dyn Fn()>>>,
}

/// Clonable handle to the monitor.
#[derive(Clone)]
pub struct Monitor(Rc<Inner>);

impl Monitor {
    // `MainContext::channel` is deprecated in favour of an async channel from
    // the `async-channel` crate. Kept: it is a thread -> main-loop handoff of
    // one small batch, glib 0.18 is the last GTK3 series so this will not be
    // removed under us, and the alternative is a dependency and an executor
    // for a message every thirty seconds.
    #[allow(deprecated)]
    pub fn new() -> Self {
        let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);
        let this = Monitor(Rc::new(Inner {
            cfg: RefCell::new(Ping::default()),
            history: RefCell::new(HashMap::new()),
            tick: RefCell::new(None),
            period: Cell::new(0),
            in_flight: Rc::new(Cell::new(false)),
            tx,
            on_update: RefCell::new(None),
        }));
        let m = this.clone();
        rx.attach(None, move |batch| {
            m.absorb(batch);
            glib::ControlFlow::Continue
        });
        this
    }

    pub fn connect_update(&self, f: impl Fn() + 'static) {
        *self.0.on_update.borrow_mut() = Some(Box::new(f));
    }

    /// Start, stop or re-time the poller to match the config, and drop history
    /// for hosts that no longer exist.
    ///
    /// Cheap to call on every settings change: an unchanged interval keeps the
    /// running timer, so editing a host name doesn't reset everyone's badges.
    pub fn sync(&self, cfg: &Ping) {
        *self.0.cfg.borrow_mut() = cfg.clone();
        let live = cfg.enabled && !cfg.hosts.is_empty();
        if !live {
            self.stop();
            self.0.history.borrow_mut().clear();
            return;
        }
        self.0
            .history
            .borrow_mut()
            .retain(|name, _| cfg.hosts.iter().any(|h| &h.name == name));

        let period = cfg.interval();
        if self.0.tick.borrow().is_some() && self.0.period.get() == period {
            return;
        }
        self.stop();
        self.0.period.set(period);
        let this = self.clone();
        // fire one round straight away so the panel isn't blank for a whole
        // interval after being switched on
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

    /// Every host in draw order, with its badge row.
    pub fn statuses(&self) -> Vec<HostStatus> {
        let cfg = self.0.cfg.borrow();
        let count = cfg.segment_count();
        let per_seg = samples_per_segment(cfg.interval(), count);
        let hist = self.0.history.borrow();
        cfg.tree_order()
            .into_iter()
            .map(|(i, depth)| {
                let h = &cfg.hosts[i];
                let samples = hist.get(&h.name);
                HostStatus {
                    name: h.name.clone(),
                    host: h.host.clone(),
                    depth,
                    last: samples.and_then(|s| s.back().copied()),
                    segments: segments_of(samples, per_seg, count),
                }
            })
            .collect()
    }

    /// Fold a finished round into the history and tell the UI.
    fn absorb(&self, batch: Vec<(String, Sample)>) {
        self.0.in_flight.set(false);
        {
            let cfg = self.0.cfg.borrow();
            let cap = cfg.segment_count() * samples_per_segment(cfg.interval(), cfg.segment_count());
            drop(cfg);
            let mut hist = self.0.history.borrow_mut();
            for (name, sample) in batch {
                let row = hist.entry(name).or_default();
                row.push_back(sample);
                while row.len() > cap {
                    row.pop_front();
                }
            }
        }
        // after the borrow: the callback redraws, which reads the history back
        if let Some(f) = self.0.on_update.borrow().as_ref() {
            f();
        }
    }

    /// Kick off one round on a worker thread.
    fn round(&self) {
        if self.0.in_flight.get() {
            log!(WARN, "ping round still running; skipping this tick");
            return;
        }
        let cfg = self.0.cfg.borrow().clone();
        let specs: Vec<Spec> = cfg
            .hosts
            .iter()
            .filter(|h| !h.host.trim().is_empty())
            .map(|h| Spec {
                name: h.name.clone(),
                host: h.host.trim().to_string(),
                parent: h.parent.clone(),
            })
            .collect();
        if specs.is_empty() {
            return;
        }
        // a host must not be able to hold a round open for longer than the gap
        // between rounds, or the skip above would starve the panel
        let timeout = (cfg.interval() / 2).clamp(1, 5);
        self.0.in_flight.set(true);
        let tx = self.0.tx.clone();
        std::thread::spawn(move || {
            let out = run_round(specs, timeout);
            // a closed channel just means we're shutting down
            let _ = tx.send(out);
        });
    }
}

/// Chunk the newest rounds into badges, newest at the right.
fn segments_of(
    samples: Option<&VecDeque<Sample>>,
    per_seg: usize,
    count: usize,
) -> Vec<Health> {
    let mut out = vec![Health::Unknown; count];
    let Some(samples) = samples else {
        return out;
    };
    let recent: Vec<Sample> = samples.iter().rev().copied().collect();
    for (k, chunk) in recent.chunks(per_seg.max(1)).take(count).enumerate() {
        out[count - 1 - k] = Health::of(chunk);
    }
    out
}

/// One round, on the worker thread: level by level down the tree, every host
/// on a level pinged at once, children skipped under a parent that didn't
/// answer.
fn run_round(specs: Vec<Spec>, timeout: u64) -> Vec<(String, Sample)> {
    let mut done: HashMap<String, Sample> = HashMap::new();
    let mut remaining = specs;

    while !remaining.is_empty() {
        let (ready, rest): (Vec<Spec>, Vec<Spec>) = remaining
            .into_iter()
            .partition(|s| s.parent.is_empty() || done.contains_key(&s.parent));
        if ready.is_empty() {
            // dangling or circular parents: nothing will ever resolve them, so
            // treat what's left as top-level rather than dropping it
            let mut handles = Vec::new();
            for s in rest {
                handles.push(std::thread::spawn(move || {
                    (s.name, ping_once(&s.host, timeout))
                }));
            }
            for h in handles {
                if let Ok((name, sample)) = h.join() {
                    done.insert(name, sample);
                }
            }
            break;
        }

        let mut handles = Vec::new();
        for s in ready {
            let reachable =
                s.parent.is_empty() || matches!(done.get(&s.parent), Some(Sample::Up(_)));
            if !reachable {
                done.insert(s.name, Sample::Blocked);
                continue;
            }
            handles.push(std::thread::spawn(move || {
                (s.name, ping_once(&s.host, timeout))
            }));
        }
        for h in handles {
            if let Ok((name, sample)) = h.join() {
                done.insert(name, sample);
            }
        }
        remaining = rest;
    }

    done.into_iter().collect()
}

/// A single `ping`. Anything other than a clean exit is Down — an unresolvable
/// name is as unreachable, from the desktop's point of view, as a dead router.
fn ping_once(host: &str, timeout: u64) -> Sample {
    let out = std::process::Command::new("ping")
        .args([
            "-n", // no reverse lookups; they can hang far longer than -W
            "-q", // summary only
            "-c",
            "1",
            "-W",
            &timeout.to_string(),
            host,
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            Sample::Up(parse_rtt(&String::from_utf8_lossy(&o.stdout)))
        }
        Ok(_) => Sample::Down,
        Err(e) => {
            log!(WARN, "ping {host} could not run: {e}");
            Sample::Down
        }
    }
}

/// Pull the round-trip time out of `ping -q` output, which ends with a line
/// like `rtt min/avg/max/mdev = 8.921/8.921/8.921/0.000 ms`. The average is
/// the second field and equals the only sample for `-c 1`.
fn parse_rtt(text: &str) -> Option<f64> {
    let line = text
        .lines()
        .find(|l| l.contains("min/avg/max") && l.contains('='))?;
    let stats = line.split('=').nth(1)?;
    stats.split('/').nth(1)?.trim().parse().ok()
}

// -- traceroute -------------------------------------------------------------

/// One hop on the way to a target.
#[derive(Clone, Debug, PartialEq)]
pub struct Hop {
    /// Reverse-DNS name, when the hop had one.
    pub name: Option<String>,
    pub address: String,
}

/// Hops on the way to `target`, in path order.
///
/// Reverse lookups are left *on*: the whole point of watching a route is
/// knowing whose network a delay is in, and `singnet.com.sg` says that where
/// `165.21.138.213` does not. traceroute resolves while it probes, so the
/// names are close to free, and one probe per hop keeps the wait short.
pub fn traceroute(target: &str, max_hops: usize) -> Result<Vec<Hop>, String> {
    let out = std::process::Command::new("traceroute")
        .args(["-q", "1", "-w", "2", "-m", &max_hops.to_string(), target])
        .output()
        .map_err(|e| format!("traceroute could not run: {e}"))?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let hops = parse_traceroute(&String::from_utf8_lossy(&out.stdout), max_hops);
    if hops.is_empty() {
        return Err("no hops answered".to_string());
    }
    Ok(hops)
}

/// Pull one address per hop out of traceroute output.
///
/// Handles both shapes of the tool's output — bare `1  1.2.3.4  0.1 ms` with
/// `-n`, and `1  name (1.2.3.4)  0.1 ms` without it — because a user may well
/// have a `traceroute` aliased to include lookups.
///
/// Two things are deliberately dropped. A hop that answered from more than one
/// address (load balancing, common past the first ISP) keeps only its first:
/// they are alternate paths, not extra hops. A hop that answered from nowhere
/// (`* * *`) is skipped entirely rather than added as a host that can only ever
/// look down — plenty of routers forward happily while refusing to admit it.
fn parse_traceroute(text: &str, max_hops: usize) -> Vec<Hop> {
    let mut hops: Vec<Hop> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // a hop line starts with the hop number
        if fields.first().and_then(|f| f.parse::<u32>().ok()).is_none() {
            continue;
        }
        let found = fields.iter().enumerate().find_map(|(i, f)| {
            let candidate = f.trim_matches(|c| c == '(' || c == ')');
            if !is_address(candidate) {
                return None;
            }
            // `name (1.2.3.4)` puts the name immediately before the bracket;
            // `-n` output has the address bare, with no name to take
            let name = f
                .starts_with('(')
                .then(|| fields.get(i.wrapping_sub(1)).copied())
                .flatten()
                .filter(|n| !is_address(n))
                .map(str::to_string);
            Some(Hop {
                name,
                address: candidate.to_string(),
            })
        });
        if let Some(hop) = found {
            if !hops.iter().any(|h| h.address == hop.address) {
                hops.push(hop);
            }
        }
        if hops.len() >= max_hops {
            break;
        }
    }
    hops
}

/// A short, human label for a hop, and the reason this is worth doing at all:
/// a column of raw addresses tells you a hop is slow but not whose it is.
///
/// The first private address is the box in the hall, the last hop is whatever
/// was asked for, and anything with a reverse name is labelled by the domain
/// that owns it rather than by the interface, so consecutive hops through one
/// network read as that network. Everything else keeps its address.
pub fn hop_label(hop: &Hop, index: usize, last: bool, target: &str) -> String {
    if last && !target.trim().is_empty() {
        return target.trim().to_string();
    }
    if index == 0 && is_private(&hop.address) {
        return "Router".to_string();
    }
    match hop.name.as_deref().map(registrable_domain) {
        Some(domain) if !domain.is_empty() => domain,
        _ => hop.address.clone(),
    }
}

/// The owning domain of a hostname: the last two labels, or three when the
/// second-to-last is one of the country-code second levels (`co.uk`,
/// `com.sg`), which would otherwise reduce every such host to `com.sg`.
fn registrable_domain(host: &str) -> String {
    let labels: Vec<&str> = host.trim_end_matches('.').split('.').collect();
    if labels.len() < 2 {
        return String::new();
    }
    const SECOND_LEVEL: [&str; 6] = ["co", "com", "net", "org", "ac", "gov"];
    let take = match labels.get(labels.len() - 2) {
        Some(l) if SECOND_LEVEL.contains(l) && labels.len() >= 3 => 3,
        _ => 2,
    };
    labels[labels.len() - take..].join(".")
}

/// RFC1918 / loopback / link-local — the addresses that are yours, not the
/// internet's.
fn is_private(address: &str) -> bool {
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local()
        }
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}

/// Whether a token is an IP address rather than a hostname or a timing.
fn is_address(token: &str) -> bool {
    token.parse::<std::net::IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact output shape traceroute produces without `-n`: names with the
    /// address in brackets, three probes a hop, and hops answering from
    /// several different addresses.
    #[test]
    fn traceroute_gives_one_address_per_hop() {
        let text = "\
traceroute to google.com (74.125.68.138), 30 hops max, 60 byte packets
 1  _gateway (192.168.2.254)  0.169 ms  0.155 ms  0.186 ms
 2  192.168.1.254 (192.168.1.254)  0.894 ms  1.516 ms  1.590 ms
 3  bb219-75-37-254.singnet.com.sg (219.75.37.254)  5.228 ms  4.657 ms
 4  * * *
10  192.178.109.115 (192.178.109.115)  3.525 ms 108.170.234.93 (108.170.234.93)  4.034 ms
";
        let hops = parse_traceroute(text, 15);
        let addresses: Vec<&str> = hops.iter().map(|h| h.address.as_str()).collect();
        assert_eq!(
            addresses,
            vec![
                "192.168.2.254",
                "192.168.1.254",
                "219.75.37.254",
                // the unanswered hop is skipped, not recorded as down
                "192.178.109.115",
            ]
        );
        assert_eq!(hops[0].name.as_deref(), Some("_gateway"));
        assert_eq!(hops[2].name.as_deref(), Some("bb219-75-37-254.singnet.com.sg"));
        // an address that is its own "name" is not a name
        assert_eq!(hops[1].name, None);
    }

    #[test]
    fn hops_are_labelled_by_who_owns_them() {
        let hop = |name: Option<&str>, address: &str| Hop {
            name: name.map(str::to_string),
            address: address.to_string(),
        };
        // the box in the hall
        assert_eq!(
            hop_label(&hop(Some("_gateway"), "192.168.2.254"), 0, false, "google.com"),
            "Router"
        );
        // the ISP, named by the domain rather than the interface
        assert_eq!(
            hop_label(
                &hop(Some("SN-SINTP1-BO118-xe-0-1-1.singnet.com.sg"), "165.21.138.213"),
                5,
                false,
                "google.com"
            ),
            "singnet.com.sg"
        );
        // nothing to go on: the address stands
        assert_eq!(
            hop_label(&hop(None, "165.21.193.22"), 3, false, "google.com"),
            "165.21.193.22"
        );
        // the destination is what the user asked for
        assert_eq!(
            hop_label(&hop(None, "74.125.68.138"), 9, true, "google.com"),
            "google.com"
        );
    }

    #[test]
    fn country_code_second_levels_do_not_collapse_to_the_tld() {
        assert_eq!(registrable_domain("a.b.singnet.com.sg"), "singnet.com.sg");
        assert_eq!(registrable_domain("edge.google.com"), "google.com");
        assert_eq!(registrable_domain("host.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(registrable_domain("localhost"), "");
    }

    #[test]
    fn traceroute_parses_the_n_flag_shape_and_honours_the_cap() {
        let text = " 1  192.168.2.254  0.168 ms\n 2  192.168.1.254  0.765 ms\n 3  219.75.37.254  3.073 ms\n";
        let hops = parse_traceroute(text, 2);
        let addresses: Vec<&str> = hops.iter().map(|h| h.address.as_str()).collect();
        assert_eq!(addresses, vec!["192.168.2.254", "192.168.1.254"]);
        assert!(hops.iter().all(|h| h.name.is_none()));
    }

    #[test]
    fn rtt_comes_from_the_average_field() {
        let text = "--- x ping statistics ---\n\
                    1 packets transmitted, 1 received, 0% packet loss, time 0ms\n\
                    rtt min/avg/max/mdev = 8.921/9.5/10.1/0.000 ms\n";
        assert_eq!(parse_rtt(text), Some(9.5));
        assert_eq!(parse_rtt("no stats here"), None);
    }

    #[test]
    fn a_period_is_only_green_when_every_round_answered() {
        assert_eq!(Health::of(&[Sample::Up(Some(1.0)), Sample::Up(None)]), Health::Up);
        assert_eq!(Health::of(&[Sample::Up(None), Sample::Down]), Health::Mixed);
        assert_eq!(Health::of(&[Sample::Down, Sample::Down]), Health::Down);
        assert_eq!(Health::of(&[Sample::Blocked]), Health::Blocked);
        assert_eq!(Health::of(&[Sample::Blocked, Sample::Up(None)]), Health::Mixed);
        assert_eq!(Health::of(&[]), Health::Unknown);
    }

    #[test]
    fn badges_fill_from_the_right() {
        let mut q = VecDeque::new();
        q.push_back(Sample::Down);
        let segs = segments_of(Some(&q), 6, 12);
        assert_eq!(segs.len(), 12);
        assert_eq!(segs[11], Health::Down);
        assert_eq!(segs[0], Health::Unknown);
    }

    #[test]
    fn a_child_is_blocked_not_down_when_its_parent_is_down() {
        // 203.0.113.0/24 is TEST-NET-3: reserved for documentation, never routed
        let specs = vec![
            Spec { name: "r".into(), host: "203.0.113.1".into(), parent: String::new() },
            Spec { name: "c".into(), host: "203.0.113.2".into(), parent: "r".into() },
        ];
        let out: HashMap<String, Sample> = run_round(specs, 1).into_iter().collect();
        assert_eq!(out["r"], Sample::Down);
        assert_eq!(out["c"], Sample::Blocked);
    }
}

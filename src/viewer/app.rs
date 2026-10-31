//! What the viewer knows about each host.

use crate::config::Host;
use crate::conn::State;
use crate::history::{self, History};
use crate::model::{older, AgentMsg, Alert, AlertKind, Inventory, Sample, Workloads};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Agent problems kept per host.
const PROBLEMS: usize = 4;
/// Alerts kept per host, newest last.
const ALERTS: usize = 200;
/// How long a host that was live may be gone before that is an alert.
const UNREACHABLE: Duration = Duration::from_secs(60);

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct HostView {
    pub name: String,
    pub state: State,
    pub inventory: Option<Inventory>,
    pub sample: Option<Sample>,
    pub workloads: Workloads,
    pub last_seen: Option<Instant>,
    pub history: History,
    pub problems: Vec<String>,
    /// The agent is resident: it keeps history and raises alerts on its own.
    pub resident: bool,
    /// The version of a resident agent older than this viewer.
    pub outdated: Option<String>,
    /// That agent is being updated.
    pub updating: bool,
    pub alerts: Vec<Alert>,
    /// Alerts up to this id have been seen in the host view.
    pub read_up_to: u64,
    /// Alerts that happened while the viewer was running, not yet shown as
    /// desktop notifications.
    pub fresh: Vec<Alert>,
    /// When the viewer started, by the wall clock: alerts from before are
    /// old news, not notifications.
    started_ms: u64,
    was_live: bool,
    down_since: Option<Instant>,
    unreachable_raised: bool,
}

impl HostView {
    pub fn new(host: &Host) -> HostView {
        HostView {
            name: host.name.clone(),
            state: State::Connecting,
            inventory: None,
            sample: None,
            workloads: Workloads::default(),
            last_seen: None,
            history: History::default(),
            problems: Vec::new(),
            resident: false,
            outdated: None,
            updating: false,
            alerts: Vec::new(),
            read_up_to: 0,
            fresh: Vec::new(),
            started_ms: now_ms(),
            was_live: false,
            down_since: None,
            unreachable_raised: false,
        }
    }

    pub fn apply(&mut self, msg: AgentMsg) {
        match msg {
            AgentMsg::Hello(h) => {
                self.resident = h.resident;
                let old = h.resident && older(&h.version, env!("CARGO_PKG_VERSION"));
                self.outdated = old.then_some(h.version);
            }
            AgentMsg::OldResident(version) => self.outdated = Some(version),
            AgentMsg::Inventory(inv) => self.inventory = Some(inv),
            AgentMsg::Sample(s) => {
                // A sample that does not match the inventory is from a
                // confused agent; keep the last good one.
                let Some(inv) = &self.inventory else { return };
                if s.gpus.len() != inv.gpus.len() || s.disks.len() != inv.mounts.len() {
                    return;
                }
                self.history.push(s.time_ms, &history::values(&s));
                self.last_seen = Some(Instant::now());
                self.sample = Some(s);
            }
            AgentMsg::Workloads(w) => {
                // GPU references must point into the inventory.
                let gpus = self.inventory.as_ref().map_or(0, |i| i.gpus.len());
                if w.list.iter().all(|wl| wl.gpus.iter().all(|g| (g.gpu as usize) < gpus)) {
                    self.workloads = w;
                }
            }
            // The decoder already refused histories that do not fit.
            AgentMsg::History(h) => self.history = h,
            AgentMsg::Alerts(list) => {
                for a in list {
                    self.add_alert(a);
                }
            }
            AgentMsg::Error(e) => {
                if !self.problems.contains(&e) {
                    if self.problems.len() == PROBLEMS {
                        self.problems.remove(0);
                    }
                    self.problems.push(e);
                }
            }
        }
    }

    fn add_alert(&mut self, a: Alert) {
        if self.alerts.iter().any(|b| b.id == a.id) {
            return;
        }
        if a.time_ms >= self.started_ms && a.id > self.read_up_to {
            self.fresh.push(a.clone());
        }
        let at = self.alerts.partition_point(|b| b.id < a.id);
        self.alerts.insert(at, a);
        if self.alerts.len() > ALERTS {
            self.alerts.remove(0);
        }
    }

    pub fn unread(&self) -> impl Iterator<Item = &Alert> {
        self.alerts.iter().filter(|a| a.id > self.read_up_to)
    }

    /// Everything so far has been seen.
    pub fn mark_read(&mut self) -> bool {
        let newest = self.alerts.last().map_or(0, |a| a.id);
        let changed = newest > self.read_up_to;
        self.read_up_to = self.read_up_to.max(newest);
        changed
    }

    /// The last numbers stay while a host reconnects; the card dims them.
    pub fn set_state(&mut self, state: State) {
        match state {
            State::Live => {
                self.was_live = true;
                self.down_since = None;
                self.unreachable_raised = false;
            }
            State::Down(..) if self.was_live => {
                self.down_since.get_or_insert_with(Instant::now);
            }
            _ => {}
        }
        self.state = state;
    }

    /// Raises "host unreachable" for a host that was live and has been gone
    /// a while. Only the viewer can notice this one.
    pub fn check_reachable(&mut self) {
        let Some(since) = self.down_since else { return };
        if self.unreachable_raised || since.elapsed() < UNREACHABLE || self.live() {
            return;
        }
        self.unreachable_raised = true;
        let t = now_ms();
        let why = match &self.state {
            State::Down(why, _) => format!(": {why}"),
            _ => String::new(),
        };
        let mins = since.elapsed().as_secs() / 60;
        self.add_alert(Alert {
            // Viewer-made ids end in 999, clear of the agent's own.
            id: t * 1000 + 999,
            time_ms: t,
            kind: AlertKind::HostUnreachable,
            subject: self.name.clone(),
            message: format!("{} has been unreachable for {}m{why}", self.name, mins.max(1)),
        });
    }

    pub fn live(&self) -> bool {
        self.state == State::Live
    }

    /// What to say about an out-of-date resident agent, if anything.
    pub fn update_note(&self) -> Option<String> {
        if self.updating {
            return Some("updating the resident agent…".into());
        }
        let version: String = self.outdated.as_ref().filter(|_| self.live())?.chars().take(24).collect();
        Some(format!("older resident agent {version}: u updates it"))
    }
}

// ===== which alerts were seen ===============================================

fn read_path() -> std::path::PathBuf {
    crate::paths::state_dir().join("viewer-read.txt")
}

/// Per host name, the newest alert id seen. Lines of `id<TAB>name`; names
/// cannot hold tabs or newlines (the config refuses control characters).
pub fn load_read() -> Vec<(String, u64)> {
    let Ok(text) = std::fs::read_to_string(read_path()) else { return Vec::new() };
    text.lines()
        .filter_map(|l| {
            let (id, name) = l.split_once('\t')?;
            Some((name.to_string(), id.parse().ok()?))
        })
        .collect()
}

pub fn save_read(hosts: &[HostView]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(dir) = crate::paths::private_dir(&crate::paths::state_dir()) else { return };
    let text: String = hosts.iter().map(|h| format!("{}\t{}\n", h.read_up_to, h.name)).collect();
    let tmp = dir.join(".viewer-read.tmp");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()));
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, read_path());
    }
}

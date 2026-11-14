//! What the viewer knows about each host.

use crate::config::Host;
use crate::conn::State;
use crate::history::{self, History};
use crate::model::{
    older, AgentMsg, Alert, AlertKind, Inventory, QueueInfo, RunInfo, RunStatus, Runs, Sample, Workloads,
};
use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Agent problems kept per host.
const PROBLEMS: usize = 4;
/// Alerts kept per host, newest last.
const ALERTS: usize = 200;
/// How long a host that was live may be gone before that is an alert.
const UNREACHABLE: Duration = Duration::from_secs(60);
/// Alerts dismissed one by one, and runs hidden, remembered per host.
const DISMISSED: usize = 400;
/// The last three digits of the ids of alerts the viewer makes itself.
const VIEWER_MADE: u64 = 999;
const HIDDEN: usize = 400;

/// A line of the runs list: a queue or a run.
#[derive(Clone, Copy, Debug)]
pub enum Item<'a> {
    Queue(&'a QueueInfo),
    Run(&'a RunInfo),
}

impl Item<'_> {
    pub fn key(&self) -> &str {
        match self {
            Item::Queue(q) => &q.key,
            Item::Run(r) => &r.key,
        }
    }

    /// With the key, which time round this is: a resume in the same
    /// directory keeps the key but starts anew.
    pub fn started_ms(&self) -> u64 {
        match self {
            Item::Queue(q) => q.started_ms,
            Item::Run(r) => r.started_ms,
        }
    }

    pub fn status(&self) -> RunStatus {
        match self {
            Item::Queue(q) => q.status,
            Item::Run(r) => r.status,
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct HostView {
    pub name: String,
    pub state: State,
    pub inventory: Option<Inventory>,
    pub sample: Option<Sample>,
    pub workloads: Workloads,
    /// Training runs and queues, from the host's run cards.
    pub runs: Runs,
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
    /// Alerts up to this id were cleared, and these one by one: the host
    /// still has them, the viewer no longer shows them.
    pub cleared_up_to: u64,
    pub dismissed: BTreeSet<u64>,
    /// Runs and queues put away, by key and start; one running is shown
    /// whatever this says.
    pub hidden: Vec<(String, u64)>,
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
            runs: Runs::default(),
            last_seen: None,
            history: History::default(),
            problems: Vec::new(),
            resident: false,
            outdated: None,
            updating: false,
            alerts: Vec::new(),
            read_up_to: 0,
            cleared_up_to: 0,
            dismissed: BTreeSet::new(),
            hidden: Vec::new(),
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
            AgentMsg::Runs(r) => self.runs = r,
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

    /// The run `n` opens nerdtui on: the selected workload's, else the first
    /// running one, else the one that ended last.
    pub fn focus_run(&self, workload: Option<usize>) -> Option<&RunInfo> {
        let id = workload.and_then(|i| self.workloads.list.get(i)).map(|w| w.id);
        let runs: Vec<&RunInfo> =
            self.items().into_iter().filter_map(|i| if let Item::Run(r) = i { Some(r) } else { None }).collect();
        id.and_then(|id| runs.iter().find(|r| r.workload == id && r.status == RunStatus::Running))
            .or_else(|| runs.iter().find(|r| r.status == RunStatus::Running))
            .or_else(|| runs.first())
            .copied()
    }

    fn is_hidden(&self, item: &Item) -> bool {
        item.status() != RunStatus::Running
            && self.hidden.iter().any(|(k, s)| k == item.key() && *s == item.started_ms())
    }

    /// The runs list as shown: queues, then runs, without those put away.
    pub fn items(&self) -> Vec<Item<'_>> {
        let queues = self.runs.queues.iter().map(Item::Queue);
        let runs = self.runs.runs.iter().map(Item::Run);
        queues.chain(runs).filter(|i| !self.is_hidden(i)).collect()
    }

    /// How many of the host's runs and queues are put away.
    pub fn hidden_count(&self) -> usize {
        let all = self.runs.queues.len() + self.runs.runs.len();
        all - self.items().len()
    }

    /// Puts away a run or queue that is not running; says whether it did.
    pub fn hide(&mut self, key: &str, started_ms: u64) -> bool {
        let running = self.items().iter().any(|i| i.key() == key && i.status() == RunStatus::Running);
        if running || self.hidden.iter().any(|(k, s)| k == key && *s == started_ms) {
            return false;
        }
        if self.hidden.len() >= HIDDEN {
            self.hidden.remove(0);
        }
        self.hidden.push((key.to_string(), started_ms));
        true
    }

    /// Puts away every run and queue that is not running; how many.
    pub fn hide_ended(&mut self) -> usize {
        let ended: Vec<(String, u64)> = self
            .items()
            .iter()
            .filter(|i| i.status() != RunStatus::Running)
            .map(|i| (i.key().to_string(), i.started_ms()))
            .collect();
        ended.iter().filter(|(k, s)| self.hide(k, *s)).count()
    }

    /// Shows again everything put away.
    pub fn unhide_all(&mut self) -> bool {
        let any = !self.hidden.is_empty();
        self.hidden.clear();
        any
    }

    /// No longer shows any alert so far, now or when the host sends its
    /// log again.
    ///
    /// The mark is the newest of the host's own alerts: the viewer's own
    /// (ids ending in 999) go by the viewer's clock, which may run ahead of
    /// the host's, and the host never sends them again anyway.
    pub fn clear_alerts(&mut self) -> bool {
        let Some(newest) = self.alerts.last().map(|a| a.id) else { return false };
        if let Some(host) = self.alerts.iter().rev().map(|a| a.id).find(|id| id % 1000 != VIEWER_MADE) {
            self.cleared_up_to = self.cleared_up_to.max(host);
            self.dismissed.retain(|&id| id > host);
        }
        self.read_up_to = self.read_up_to.max(newest);
        self.alerts.clear();
        self.fresh.clear();
        true
    }

    /// No longer shows this alert.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.alerts.len();
        self.alerts.retain(|a| a.id != id);
        self.fresh.retain(|a| a.id != id);
        if self.alerts.len() == before {
            return false;
        }
        self.dismissed.insert(id);
        while self.dismissed.len() > DISMISSED {
            self.dismissed.pop_first();
        }
        true
    }

    fn add_alert(&mut self, a: Alert) {
        if a.id <= self.cleared_up_to || self.dismissed.contains(&a.id) || self.alerts.iter().any(|b| b.id == a.id) {
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
            id: t * 1000 + VIEWER_MADE,
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

fn hidden_path() -> std::path::PathBuf {
    crate::paths::state_dir().join("viewer-hidden.txt")
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
    let text: String = hosts.iter().map(|h| format!("{}\t{}\n", h.read_up_to, h.name)).collect();
    save(&read_path(), &text);
}

/// What each host has cleared, dismissed and put away, as lines of
/// `cleared<TAB>id<TAB>host`, `dismissed<TAB>id<TAB>host` and
/// `hidden<TAB>started_ms<TAB>key<TAB>host`. Keys with control characters
/// (names from a strange host) are not remembered.
pub fn hidden_text(hosts: &[HostView]) -> String {
    let mut text = String::new();
    for h in hosts {
        if h.cleared_up_to > 0 {
            text.push_str(&format!("cleared\t{}\t{}\n", h.cleared_up_to, h.name));
        }
        for id in &h.dismissed {
            text.push_str(&format!("dismissed\t{id}\t{}\n", h.name));
        }
        for (key, started) in h.hidden.iter().filter(|(k, _)| !k.chars().any(char::is_control)) {
            text.push_str(&format!("hidden\t{started}\t{key}\t{}\n", h.name));
        }
    }
    text
}

/// Gives each host back what `hidden_text` wrote about it.
pub fn apply_hidden(hosts: &mut [HostView], text: &str) {
    for line in text.lines() {
        let mut f = line.splitn(4, '\t');
        let (Some(what), Some(n)) = (f.next(), f.next().and_then(|n| n.parse::<u64>().ok())) else { continue };
        let (key, name) = match what {
            "hidden" => match (f.next(), f.next()) {
                (Some(k), Some(name)) => (Some(k), name),
                _ => continue,
            },
            _ => match f.next() {
                Some(name) => (None, name),
                None => continue,
            },
        };
        let Some(h) = hosts.iter_mut().find(|h| h.name == name) else { continue };
        match (what, key) {
            ("cleared", _) => h.cleared_up_to = h.cleared_up_to.max(n),
            ("dismissed", _) => {
                h.dismissed.insert(n);
            }
            ("hidden", Some(k)) if h.hidden.len() < HIDDEN => h.hidden.push((k.to_string(), n)),
            _ => {}
        }
    }
}

pub fn load_hidden(hosts: &mut [HostView]) {
    if let Ok(text) = std::fs::read_to_string(hidden_path()) {
        apply_hidden(hosts, &text);
    }
}

pub fn save_hidden(hosts: &[HostView]) {
    save(&hidden_path(), &hidden_text(hosts));
}

/// Writes `text` to `path` in the private state directory, whole or not at all.
fn save(path: &std::path::Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(dir) = crate::paths::private_dir(&crate::paths::state_dir()) else { return };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return };
    let tmp = dir.join(format!(".{name}.tmp"));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()));
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Access;
    use crate::viewer::host::{Section, Sel};

    fn view() -> HostView {
        HostView::new(&Host { name: "host-a".into(), access: Access::Local })
    }

    fn alert(id: u64) -> Alert {
        Alert {
            id,
            time_ms: now_ms() + id,
            kind: AlertKind::RunEnded,
            subject: "r".into(),
            message: format!("alert {id}"),
        }
    }

    fn run(key: &str, status: RunStatus, started_ms: u64) -> RunInfo {
        RunInfo {
            key: key.into(),
            name: key.into(),
            run_dir: format!("/runs/{key}"),
            status,
            workload: 0,
            start_step: 0,
            max_steps: 10,
            step: 10,
            loss: None,
            eval: None,
            eval_step: 0,
            tokens_per_s: None,
            step_s: None,
            eta_s: None,
            quiet_s: 0,
            nonfinite: 0,
            bad_lines: 0,
            series: vec![],
            evals: vec![],
            started_ms,
            ended_ms: started_ms + 1,
        }
    }

    #[test]
    fn cleared_and_dismissed_alerts_stay_away_when_the_host_sends_its_log_again() {
        let mut v = view();
        v.apply(AgentMsg::Alerts((1..=5).map(|i| alert(i * 1000)).collect()));
        v.fresh.clear();
        assert!(v.dismiss(2000));
        assert!(v.clear_alerts());
        assert!(v.alerts.is_empty() && v.unread().count() == 0);
        // A reconnect resends the whole log, and one new alert.
        v.apply(AgentMsg::Alerts((1..=6).map(|i| alert(i * 1000)).collect()));
        assert_eq!(v.alerts.iter().map(|a| a.id).collect::<Vec<_>>(), [6000]);
        assert_eq!(v.fresh.iter().map(|a| a.id).collect::<Vec<_>>(), [6000], "cleared ones never notify again");
        v.dismiss(6000);
        v.apply(AgentMsg::Alerts(vec![alert(6000)]));
        assert!(v.alerts.is_empty());
        // The viewer's own alert, by a clock far ahead, does not hide the host's next ones.
        v.apply(AgentMsg::Alerts(vec![alert(7000), alert(9_000_000_999)]));
        v.clear_alerts();
        v.apply(AgentMsg::Alerts(vec![alert(8000)]));
        assert_eq!(v.alerts.iter().map(|a| a.id).collect::<Vec<_>>(), [8000]);
    }

    #[test]
    fn a_hidden_run_comes_back_when_it_runs_again() {
        let mut v = view();
        v.apply(AgentMsg::Runs(Runs {
            runs: vec![run("a", RunStatus::Finished, 100), run("b", RunStatus::Running, 5)],
            queues: vec![],
        }));
        assert!(!v.hide("b", 5), "a running run is never hidden");
        assert_eq!(v.hide_ended(), 1);
        assert_eq!(v.items().len(), 1);
        assert_eq!(v.hidden_count(), 1);
        // Resumed in the same directory: same key, a new start.
        v.apply(AgentMsg::Runs(Runs { runs: vec![run("a", RunStatus::Running, 200)], queues: vec![] }));
        assert_eq!(v.items().len(), 1);
        v.apply(AgentMsg::Runs(Runs { runs: vec![run("a", RunStatus::Finished, 200)], queues: vec![] }));
        assert_eq!(v.items().len(), 1, "its new end is news, not hidden");
        assert!(v.unhide_all());
    }

    #[test]
    fn what_was_put_away_is_remembered_per_host() {
        let mut hosts = vec![view()];
        hosts[0].cleared_up_to = 42;
        hosts[0].dismissed.insert(77);
        hosts[0].hidden.push(("k1".into(), 9));
        hosts[0].hidden.push(("bad\tkey".into(), 9));
        let text = hidden_text(&hosts);
        let mut back = vec![view(), HostView::new(&Host { name: "host-b".into(), access: Access::Local })];
        apply_hidden(&mut back, &text);
        apply_hidden(&mut back, "garbage\nhidden\t1\n\tx\tyz\n");
        assert_eq!(back[0].cleared_up_to, 42);
        assert!(back[0].dismissed.contains(&77));
        assert_eq!(back[0].hidden, [("k1".to_string(), 9)]);
        assert_eq!(back[1].cleared_up_to, 0);
    }

    #[test]
    fn the_selection_follows_lists_that_shrink() {
        let mut v = view();
        v.apply(AgentMsg::Runs(Runs {
            runs: (0..4).map(|i| run(&format!("r{i}"), RunStatus::Finished, i)).collect(),
            queues: vec![],
        }));
        v.apply(AgentMsg::Alerts((1..=3).map(|i| alert(i * 1000)).collect()));
        let mut sel = Sel::new(&v);
        assert_eq!(sel.section, Section::Runs);
        for _ in 0..10 {
            sel.step(&v, true);
        }
        assert_eq!(sel.run, 3);
        sel.detail = true;
        v.hide_ended();
        sel.clamp(&v);
        assert_eq!((sel.section, sel.detail), (Section::Runs, false), "every run hidden: still there for H");
        assert!(sel.item(&v).is_none());
        v.unhide_all();
        v.hide_ended();
        v.apply(AgentMsg::Runs(Runs::default()));
        sel.clamp(&v);
        assert_eq!(sel.section, Section::Workloads, "a section with nothing at all lets go");
        sel.cycle(&v, false);
        assert_eq!(sel.section, Section::Alerts, "and is skipped");
        sel.alert = 2;
        assert_eq!(sel.alert(&v).map(|a| a.id), Some(1000), "newest first");
        v.clear_alerts();
        sel.clamp(&v);
        assert_eq!(sel.section, Section::Workloads);
    }
}

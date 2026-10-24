//! Alerts: the moments worth telling someone about, decided on the host so
//! they are caught whether or not a viewer is watching.
//!
//! Every alert fires once, then re-arms only when its condition has clearly
//! gone (below a lower level, or after a quiet spell), so a value hovering
//! at a threshold does not raise one alert after another. The thresholds
//! are fixed for now; the config that tunes them lives on the viewer's
//! machine, and reaching a resident agent with it is later work.

use crate::model::{Alert, AlertKind, Inventory, Kind, Sample, Workload};
use std::collections::{HashMap, VecDeque};

/// How long a training run's GPUs may idle before it counts as stalled.
const STALL_S: u32 = 10 * 60;
/// A workload must have lived this long for its end to be news.
const ENDED_MIN_LIFE_MS: u64 = 2 * 60 * 1000;
/// A server's port must have been up, and then gone, this long.
const LISTEN_MS: u64 = 30 * 1000;
const DISK_FULL: f32 = 90.0;
const DISK_REARM: f32 = 85.0;
/// Thermal or hardware slowdown this long.
const THROTTLE_MS: u64 = 5 * 60 * 1000;
/// GPU memory this full, this long. PyTorch's caching allocator and vLLM's
/// preallocation keep 90% or more on purpose, so the bar is high.
const GPU_MEM_FULL: f32 = 95.0;
const GPU_MEM_REARM: f32 = 90.0;
const GPU_MEM_MS: u64 = 2 * 60 * 1000;
/// Alerts kept, newest last.
pub const LOG: usize = 200;

struct Seen {
    /// When its top process started: with the pid, which process it is.
    started_s: u64,
    kind: Kind,
    label: String,
    model: String,
    first_ms: u64,
    stalled: bool,
    listening_since: Option<u64>,
    silent_since: Option<u64>,
    stopped: bool,
}

#[derive(Default)]
pub struct Engine {
    pub log: VecDeque<Alert>,
    seq: u64,
    workloads: HashMap<u32, Seen>,
    disk_fired: Vec<bool>,
    /// Per GPU: since when it has been throttling, and whether that fired.
    throttle: Vec<(Option<u64>, bool)>,
    /// Per GPU: since when its memory has been full, and whether that fired.
    gpu_mem: Vec<(Option<u64>, bool)>,
    /// The kernel's count of processes the OOM killer took, at the last
    /// workload update.
    oom: Option<u64>,
}

fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h {}m", s / 3600, s / 60 % 60),
        _ => format!("{}d {}h", s / 86400, s / 3600 % 24),
    }
}

impl Engine {
    /// Starts from a saved log, so ids keep growing across restarts.
    pub fn with_log(log: Vec<Alert>) -> Engine {
        Engine { log: log.into_iter().rev().take(LOG).rev().collect(), ..Default::default() }
    }

    fn raise(&mut self, time_ms: u64, kind: AlertKind, subject: &str, message: String, out: &mut Vec<Alert>) {
        self.seq += 1;
        let last = self.log.back().map_or(0, |a| a.id);
        let id = (time_ms * 1000 + self.seq % 1000).max(last + 1);
        let alert = Alert { id, time_ms, kind, subject: subject.to_string(), message };
        if self.log.len() == LOG {
            self.log.pop_front();
        }
        self.log.push_back(alert.clone());
        out.push(alert);
    }

    /// Checks a sample: disks and GPUs.
    pub fn sample(&mut self, inv: &Inventory, s: &Sample) -> Vec<Alert> {
        let mut out = Vec::new();
        let t = s.time_ms;
        self.disk_fired.resize(s.disks.len(), false);
        for (i, (mount, d)) in inv.mounts.iter().zip(&s.disks).enumerate() {
            let pct = d.used as f32 * 100.0 / d.total.max(1) as f32;
            if pct >= DISK_FULL && !self.disk_fired[i] {
                self.disk_fired[i] = true;
                let free = d.total.saturating_sub(d.used) as f64 / (1u64 << 30) as f64;
                self.raise(t, AlertKind::DiskFull, mount, format!("{mount} is {pct:.0}% full, {free:.1} GiB left"), &mut out);
            } else if pct < DISK_REARM {
                self.disk_fired[i] = false;
            }
        }
        self.gpu_mem.resize(s.gpus.len(), (None, false));
        for (i, (g, info)) in s.gpus.iter().zip(&inv.gpus).enumerate() {
            // On unified memory the working set is in shared memory.
            let (used, total) =
                if info.unified { (g.shared_used, info.shared_total) } else { (g.mem_used, info.mem_total) };
            let (Some(used), Some(total)) = (used, total.filter(|t| *t > 0)) else { continue };
            let pct = used as f32 * 100.0 / total as f32;
            let (since, fired) = &mut self.gpu_mem[i];
            if pct >= GPU_MEM_FULL {
                let start = *since.get_or_insert(t);
                if !*fired && t.saturating_sub(start) >= GPU_MEM_MS {
                    *fired = true;
                    let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
                    let msg = format!(
                        "{} gpu{i} memory {pct:.0}% full for {} ({:.1} of {:.1} GiB)",
                        info.vendor.label(),
                        span(t - start),
                        gib(used),
                        gib(total)
                    );
                    self.raise(t, AlertKind::GpuMemoryFull, &format!("gpu{i}"), msg, &mut out);
                }
            } else {
                *since = None;
                if pct < GPU_MEM_REARM {
                    *fired = false;
                }
            }
        }
        self.throttle.resize(s.gpus.len(), (None, false));
        for (i, g) in s.gpus.iter().enumerate() {
            let (since, fired) = &mut self.throttle[i];
            if g.throttled == Some(true) {
                let start = *since.get_or_insert(t);
                if !*fired && t.saturating_sub(start) >= THROTTLE_MS {
                    *fired = true;
                    let temp = g.temp_c.map_or(String::new(), |c| format!(" at {c:.0}°C"));
                    let name = inv.gpus.get(i).map_or("GPU", |g| g.vendor.label());
                    let subject = format!("gpu{i}");
                    let msg = format!("{name} gpu{i} has been slowing itself down for heat or a hardware limit for {}{temp}", span(t - start));
                    self.raise(t, AlertKind::GpuThrottled, &subject, msg, &mut out);
                }
            } else {
                *since = None;
                *fired = false;
            }
        }
        out
    }

    /// Checks a workload update. `own_ports` gives the ports a workload's
    /// top process listens on; `running` says whether a process with this
    /// pid and start time still exists; `oom` is the kernel's OOM kill
    /// count.
    pub fn workloads(
        &mut self,
        time_ms: u64,
        list: &[Workload],
        own_ports: &dyn Fn(u32) -> Vec<u16>,
        running: &dyn Fn(u32, u64) -> bool,
        oom: Option<u64>,
    ) -> Vec<Alert> {
        let mut out = Vec::new();
        let t = time_ms;
        let oom_ran = matches!((self.oom, oom), (Some(a), Some(b)) if b > a);
        self.oom = oom.or(self.oom);

        // Gone since the last update: off the list *and* its top process
        // has exited. Off the list alone is not the end: a runner between
        // its GPU phases idles, drops off, and comes back.
        let listed: Vec<u32> = list.iter().map(|w| w.id).collect();
        let gone: Vec<u32> = self
            .workloads
            .iter()
            .filter(|(id, w)| !listed.contains(id) && !running(**id, w.started_s))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            let w = self.workloads.remove(&id).expect("listed");
            let life = t.saturating_sub(w.first_ms);
            if matches!(w.kind, Kind::Training | Kind::Inference) && life >= ENDED_MIN_LIFE_MS {
                let what = if w.model.is_empty() { w.label.clone() } else { format!("{} ({})", w.label, w.model) };
                // The exit status of a process we did not start is not ours
                // to know; an OOM kill is the one cause the kernel counts.
                let why = if oom_ran { "; the kernel's OOM killer ran around the same time" } else { "" };
                let msg = format!("{} {what} ended after {}{why}", w.kind.label(), span(life));
                self.raise(t, AlertKind::WorkloadEnded, &w.label, msg, &mut out);
            }
        }

        for w in list {
            let seen = self.workloads.entry(w.id).or_insert_with(|| Seen {
                started_s: w.started_s,
                kind: w.kind,
                label: w.label.clone(),
                model: w.model.clone(),
                first_ms: t,
                stalled: false,
                listening_since: None,
                silent_since: None,
                stopped: false,
            });
            // A job can turn from CPU into training once it reaches a GPU.
            seen.kind = w.kind;
            seen.model.clone_from(&w.model);
            if w.kind == Kind::Training {
                if w.gpu_idle_s >= STALL_S && !seen.stalled {
                    seen.stalled = true;
                    let msg = format!(
                        "{}: GPUs idle for {} while its {} processes run; possibly stalled",
                        w.label,
                        span(w.gpu_idle_s as u64 * 1000),
                        w.procs
                    );
                    let label = w.label.clone();
                    self.raise(t, AlertKind::TrainingStalled, &label, msg, &mut out);
                } else if w.gpu_idle_s < 60 {
                    self.workloads.get_mut(&w.id).expect("inserted").stalled = false;
                }
            }
            if w.kind == Kind::Inference {
                let ports = own_ports(w.id);
                let seen = self.workloads.get_mut(&w.id).expect("inserted");
                if !ports.is_empty() {
                    seen.listening_since.get_or_insert(t);
                    seen.silent_since = None;
                    seen.stopped = false;
                } else if seen.listening_since.is_some_and(|s| t.saturating_sub(s) >= LISTEN_MS) {
                    let silent = *seen.silent_since.get_or_insert(t);
                    if !seen.stopped && t.saturating_sub(silent) >= LISTEN_MS {
                        seen.stopped = true;
                        let msg = format!("{} is running but no longer listens for requests", w.label);
                        let label = w.label.clone();
                        self.raise(t, AlertKind::StoppedListening, &label, msg, &mut out);
                    }
                }
            }
        }
        out
    }
}

/// The kernel's running count of OOM kills, from `/proc/vmstat`.
pub fn oom_kills() -> Option<u64> {
    let text = super::read_file("/proc/vmstat").ok()?;
    text.lines().find_map(|l| l.strip_prefix("oom_kill ")?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Disk, Gpu, GpuInfo, Vendor};

    fn work(id: u32, kind: Kind, idle: u32) -> Workload {
        Workload {
            id,
            kind,
            label: "torchrun train.py".into(),
            model: String::new(),
            ports: vec![],
            procs: 3,
            started_s: 0,
            cpu: 1.0,
            rss: 0,
            gpus: vec![],
            gpu_idle_s: idle,
            cmdline: String::new(),
        }
    }

    const NO_PORTS: &dyn Fn(u32) -> Vec<u16> = &|_| Vec::new();
    /// No process outlives its listing.
    const EXITED: &dyn Fn(u32, u64) -> bool = &|_, _| false;

    #[test]
    fn a_stall_fires_once_and_rearms_when_the_gpus_work_again() {
        let mut e = Engine::default();
        assert!(e.workloads(0, &[work(1, Kind::Training, 0)], NO_PORTS, EXITED, None).is_empty());
        let a = e.workloads(1, &[work(1, Kind::Training, 600)], NO_PORTS, EXITED, None);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, AlertKind::TrainingStalled);
        assert!(e.workloads(2, &[work(1, Kind::Training, 900)], NO_PORTS, EXITED, None).is_empty(), "once");
        e.workloads(3, &[work(1, Kind::Training, 0)], NO_PORTS, EXITED, None);
        assert_eq!(e.workloads(4, &[work(1, Kind::Training, 600)], NO_PORTS, EXITED, None).len(), 1, "re-armed");
    }

    #[test]
    fn an_end_is_news_only_for_lasting_gpu_work() {
        let mut e = Engine::default();
        e.workloads(0, &[work(1, Kind::Training, 0), work(2, Kind::Cpu, 0), work(3, Kind::Inference, 0)], NO_PORTS, EXITED, Some(1));
        // A minute later: the server went, too young to matter.
        assert!(e.workloads(60_000, &[work(1, Kind::Training, 0), work(2, Kind::Cpu, 0)], NO_PORTS, EXITED, Some(1)).is_empty());
        // Three hours later the training run ends, with an OOM kill.
        let a = e.workloads(3 * 3600 * 1000, &[], NO_PORTS, EXITED, Some(2));
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a[0].kind, AlertKind::WorkloadEnded);
        assert!(a[0].message.contains("ended after 3h 0m") && a[0].message.contains("OOM"), "{}", a[0].message);
    }

    #[test]
    fn a_runner_between_phases_has_not_ended() {
        // The ranks exit and the runner idles, off the list but alive...
        let mut e = Engine::default();
        let alive: &dyn Fn(u32, u64) -> bool = &|pid, _| pid == 1;
        e.workloads(0, &[work(1, Kind::Training, 0)], NO_PORTS, alive, None);
        assert!(e.workloads(600_000, &[], NO_PORTS, alive, None).is_empty());
        // ...comes back for its next phase, keeping its age...
        e.workloads(700_000, &[work(1, Kind::Training, 0)], NO_PORTS, alive, None);
        // ...and only its own exit is news.
        let a = e.workloads(800_000, &[], NO_PORTS, EXITED, None);
        assert_eq!(a.len(), 1);
        assert!(a[0].message.contains("ended after 13m"), "{}", a[0].message);
    }

    #[test]
    fn a_server_that_stops_listening_is_noticed() {
        let mut e = Engine::default();
        let up: &dyn Fn(u32) -> Vec<u16> = &|_| vec![8080];
        let server = || work(7, Kind::Inference, 0);
        e.workloads(0, &[server()], up, EXITED, None);
        e.workloads(40_000, &[server()], up, EXITED, None);
        assert!(e.workloads(41_000, &[server()], NO_PORTS, EXITED, None).is_empty(), "not yet");
        let a = e.workloads(80_000, &[server()], NO_PORTS, EXITED, None);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, AlertKind::StoppedListening);
        assert!(e.workloads(90_000, &[server()], NO_PORTS, EXITED, None).is_empty(), "once");
    }

    #[test]
    fn disks_fire_above_ninety_and_rearm_below_eighty_five() {
        let inv = Inventory { mounts: vec!["/".into()], ..Default::default() };
        let mut e = Engine::default();
        let at = |pct: u64| Sample { disks: vec![Disk { total: 100, used: pct }], ..Default::default() };
        assert_eq!(e.sample(&inv, &at(91)).len(), 1);
        assert!(e.sample(&inv, &at(92)).is_empty());
        assert!(e.sample(&inv, &at(88)).is_empty(), "between the levels: still fired");
        e.sample(&inv, &at(80));
        assert_eq!(e.sample(&inv, &at(95)).len(), 1);
    }

    #[test]
    fn throttling_needs_five_minutes() {
        let inv = Inventory {
            gpus: vec![GpuInfo {
                vendor: Vendor::Nvidia,
                name: "x".into(),
                bus: String::new(),
                mem_total: None,
                shared_total: None,
                unified: false,
                power_limit_w: None,
                metrics: true,
            }],
            ..Default::default()
        };
        let mut e = Engine::default();
        let at = |t: u64, on: bool| Sample { time_ms: t, gpus: vec![Gpu { throttled: Some(on), ..Default::default() }], ..Default::default() };
        assert!(e.sample(&inv, &at(0, true)).is_empty());
        assert!(e.sample(&inv, &at(100_000, false)).is_empty(), "broken streak");
        assert!(e.sample(&inv, &at(200_000, true)).is_empty());
        assert_eq!(e.sample(&inv, &at(500_000, true)).len(), 1);
    }

    #[test]
    fn gpu_memory_full_needs_two_minutes_and_rearms_below_ninety() {
        let info = GpuInfo {
            vendor: Vendor::Nvidia,
            name: "x".into(),
            bus: String::new(),
            mem_total: Some(100),
            shared_total: None,
            unified: false,
            power_limit_w: None,
            metrics: true,
        };
        let inv = Inventory { gpus: vec![info], ..Default::default() };
        let mut e = Engine::default();
        let at = |t: u64, used: u64| Sample { time_ms: t, gpus: vec![Gpu { mem_used: Some(used), ..Default::default() }], ..Default::default() };
        assert!(e.sample(&inv, &at(0, 97)).is_empty());
        assert!(e.sample(&inv, &at(60_000, 97)).is_empty(), "not yet");
        assert_eq!(e.sample(&inv, &at(120_000, 98)).len(), 1);
        assert!(e.sample(&inv, &at(400_000, 98)).is_empty(), "once");
        e.sample(&inv, &at(410_000, 92));
        e.sample(&inv, &at(420_000, 97));
        assert!(e.sample(&inv, &at(600_000, 97)).is_empty(), "92% did not re-arm it");
        e.sample(&inv, &at(610_000, 50));
        e.sample(&inv, &at(620_000, 97));
        assert_eq!(e.sample(&inv, &at(800_000, 97)).len(), 1, "re-armed below 90%");
    }

    #[test]
    fn ids_keep_growing_across_restarts() {
        let mut e = Engine::default();
        e.workloads(0, &[work(1, Kind::Training, 0)], NO_PORTS, EXITED, None);
        let a = e.workloads(5, &[work(1, Kind::Training, 700)], NO_PORTS, EXITED, None);
        let mut again = Engine::with_log(e.log.iter().cloned().collect());
        // A clock that went back must not reuse an id.
        again.workloads(0, &[work(1, Kind::Training, 0)], NO_PORTS, EXITED, None);
        let b = again.workloads(1, &[work(1, Kind::Training, 700)], NO_PORTS, EXITED, None);
        assert!(b[0].id > a[0].id);
    }
}

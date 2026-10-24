//! What an agent does each tick, whoever it does it for: measure the host,
//! work out its workloads, keep the history, and raise alerts. Watch mode
//! runs one of these for a single viewer; resident mode runs one for as
//! many viewers as connect, and for nobody.

use super::alerts::{self, Engine};
use super::{gpu, linux, procs, workload};
use crate::history::{self, History};
use crate::model::{Alert, AgentMsg, Inventory, Sample, Workloads};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often workloads are worked out. Slightly under two seconds, so that
/// with one-second samples it is every second sample.
const WORK_EVERY: Duration = Duration::from_millis(1900);

pub struct Core {
    gpus: gpu::Gpus,
    mounts: Vec<String>,
    cpu: linux::CpuReader,
    disk_io: linux::DiskIo,
    net: linux::NetIo,
    scanner: procs::Scanner,
    tracker: workload::Tracker,
    pub history: History,
    pub alerts: Engine,
    inventory: Inventory,
    problems: Vec<String>,
    reported: Vec<String>,
    worked: Option<Instant>,
    /// Was the last workload update worked out with detail?
    detailed: bool,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Core {
    pub fn new(history: History, log: Vec<Alert>) -> Core {
        let (gpus, problems) = gpu::Gpus::discover();
        let mounts = linux::mounts();
        let mut core = Core {
            gpus,
            mounts,
            cpu: Default::default(),
            disk_io: Default::default(),
            net: Default::default(),
            scanner: Default::default(),
            tracker: Default::default(),
            history,
            alerts: Engine::with_log(log),
            inventory: Inventory::default(),
            problems,
            reported: Vec::new(),
            worked: None,
            detailed: false,
        };
        core.inventory = core.make_inventory();
        // Prime the counters, so the first sample has rates to report.
        core.cpu.sample();
        core.disk_io.sample();
        core.net.sample();
        core.scanner.scan();
        core
    }

    fn make_inventory(&self) -> Inventory {
        Inventory {
            hostname: linux::hostname(),
            os: linux::os_name(),
            kernel: linux::kernel(),
            cpu_model: linux::cpu_model(),
            threads: linux::CpuReader::threads(),
            mem_total: linux::mem().total,
            mounts: self.mounts.clone(),
            gpus: self.gpus.info(),
        }
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    /// Asks for workloads, with detail, at the next tick rather than in up
    /// to two seconds: someone just started looking closely.
    pub fn work_now(&mut self) {
        self.worked = None;
    }

    pub fn detailed(&self) -> bool {
        self.detailed
    }

    /// One tick: what to tell viewers, in order.
    pub fn tick(&mut self, detail: bool) -> Vec<AgentMsg> {
        let mut out = Vec::new();
        let mut changed = false;
        let gpu_samples = self.gpus.sample(&mut self.problems, &mut changed);
        if changed {
            self.inventory = self.make_inventory();
            out.push(AgentMsg::Inventory(self.inventory.clone()));
        }
        // Each problem is reported once, not every second.
        for p in self.problems.drain(..) {
            if !self.reported.contains(&p) {
                out.push(AgentMsg::Error(p.clone()));
                self.reported.push(p);
            }
        }
        let sample = Sample {
            time_ms: now_ms(),
            cpu: self.cpu.sample(),
            mem: linux::mem(),
            disks: self.mounts.iter().map(|m| linux::disk(m)).collect(),
            disk_io: self.disk_io.sample(),
            net: self.net.sample(),
            gpus: gpu_samples,
        };
        self.history.push(sample.time_ms, &history::values(&sample));
        let new_alerts = self.alerts.sample(&self.inventory, &sample);
        let time_ms = sample.time_ms;
        let gpu_util: Vec<Option<u8>> = sample.gpus.iter().map(|g| g.util).collect();
        out.push(AgentMsg::Sample(sample));
        if !new_alerts.is_empty() {
            out.push(AgentMsg::Alerts(new_alerts));
        }

        // The kernel takes about 10 µs to describe each process, so the
        // process table is read every `WORK_EVERY`, not every sample.
        if self.worked.is_some_and(|t| t.elapsed() < WORK_EVERY) {
            return out;
        }
        self.worked = Some(Instant::now());
        let procs = self.scanner.scan();
        let gpu_procs = self.gpus.processes();
        // Listing listening sockets walks every socket on the host; only
        // do it when something needs ports.
        let listening = std::cell::OnceCell::new();
        let ports = |pid: u32| procs::ports_of(pid, listening.get_or_init(procs::listening));
        let input = workload::Input {
            procs: &procs,
            gpus: &gpu_procs,
            gpu_util: &gpu_util,
            ports: &ports,
            detail,
            me: std::process::id(),
        };
        let (list, top) = self.tracker.update(&input);
        let running = |pid: u32, started_s: u64| procs.iter().any(|p| p.pid == pid && p.started_s == started_s);
        let new_alerts = self.alerts.workloads(time_ms, &list, &ports, &running, alerts::oom_kills());
        self.detailed = detail;
        out.push(AgentMsg::Workloads(Workloads { list, top }));
        if !new_alerts.is_empty() {
            out.push(AgentMsg::Alerts(new_alerts));
        }
        out
    }
}

/// `msg` without what only a viewer looking closely gets: command lines and
/// the busiest processes.
pub fn without_detail(msg: &AgentMsg) -> Option<AgentMsg> {
    match msg {
        AgentMsg::Workloads(w) => {
            let mut w = w.clone();
            w.top.clear();
            for wl in &mut w.list {
                wl.cmdline.clear();
            }
            Some(AgentMsg::Workloads(w))
        }
        _ => None,
    }
}

//! What the viewer knows about each host.

use crate::config::Host;
use crate::conn::State;
use crate::model::{AgentMsg, Inventory, Sample};
use std::collections::VecDeque;
use std::time::Instant;

/// Points kept per graph: 15 minutes at one sample a second.
pub const HISTORY: usize = 900;
/// Agent problems kept per host.
const PROBLEMS: usize = 4;

pub struct HostView {
    pub name: String,
    pub state: State,
    pub inventory: Option<Inventory>,
    pub sample: Option<Sample>,
    pub last_seen: Option<Instant>,
    pub cpu: VecDeque<u64>,
    pub mem: VecDeque<u64>,
    /// Per GPU, utilization in percent.
    pub gpus: Vec<VecDeque<u64>>,
    pub problems: Vec<String>,
}

fn push(series: &mut VecDeque<u64>, v: u64) {
    if series.len() == HISTORY {
        series.pop_front();
    }
    series.push_back(v);
}

impl HostView {
    pub fn new(host: &Host) -> HostView {
        HostView {
            name: host.name.clone(),
            state: State::Connecting,
            inventory: None,
            sample: None,
            last_seen: None,
            cpu: VecDeque::new(),
            mem: VecDeque::new(),
            gpus: Vec::new(),
            problems: Vec::new(),
        }
    }

    pub fn apply(&mut self, msg: AgentMsg) {
        match msg {
            AgentMsg::Hello(_) => {}
            AgentMsg::Inventory(inv) => {
                self.gpus.resize_with(inv.gpus.len(), VecDeque::new);
                self.inventory = Some(inv);
            }
            AgentMsg::Sample(s) => {
                // A sample that does not match the inventory is from a
                // confused agent; keep the last good one.
                let Some(inv) = &self.inventory else { return };
                if s.gpus.len() != inv.gpus.len() || s.disks.len() != inv.mounts.len() {
                    return;
                }
                push(&mut self.cpu, (s.cpu.busy * 100.0).round() as u64);
                let mem = s.mem.total.saturating_sub(s.mem.available) as f64 * 100.0 / s.mem.total.max(1) as f64;
                push(&mut self.mem, mem.clamp(0.0, 100.0) as u64);
                for (series, g) in self.gpus.iter_mut().zip(&s.gpus) {
                    push(series, g.util.unwrap_or(0) as u64);
                }
                self.last_seen = Some(Instant::now());
                self.sample = Some(s);
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

    /// The last numbers stay while a host reconnects; the card dims them.
    pub fn set_state(&mut self, state: State) {
        self.state = state;
    }

    pub fn live(&self) -> bool {
        self.state == State::Live
    }
}

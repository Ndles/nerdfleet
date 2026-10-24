//! The messages agents and the viewer exchange, and the data they carry.

use crate::history::History;
use crate::proto::{bad, Wire};
use std::io;

/// Which agent along a relay a frame is for or from. Empty: the agent at
/// the other end of this connection. Relays (clusters) add a hop per level.
pub type Route = Vec<u16>;

#[derive(Debug, Clone, PartialEq)]
pub struct Frame<M> {
    pub route: Route,
    pub msg: M,
}

impl<M: Wire> Wire for Frame<M> {
    fn put(&self, out: &mut Vec<u8>) {
        self.route.put(out);
        self.msg.put(out);
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(Frame { route: Wire::get(input)?, msg: Wire::get(input)? })
    }
}

/// From an agent to the viewer.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMsg {
    Hello(Hello),
    Inventory(Inventory),
    Sample(Sample),
    /// What the host is doing, after each sample.
    Workloads(Workloads),
    /// Graph history so far, once after the inventory.
    History(History),
    /// Alerts: the log so far when a viewer connects, then each new one.
    Alerts(Vec<Alert>),
    /// A sampler or a detector failed. The agent carries on without it.
    Error(String),
}

/// From the viewer to an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewerMsg {
    /// How often to sample, in milliseconds, which the agent keeps within
    /// `MIN_INTERVAL_MS..=MAX_INTERVAL_MS`; and whether to send detail
    /// (command lines, top processes) for a host the user is looking at.
    Subscribe { interval_ms: u32, detail: bool },
    Bye,
}

pub const MIN_INTERVAL_MS: u32 = 250;
pub const MAX_INTERVAL_MS: u32 = 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub protocol: u32,
    /// The agent's version, `CARGO_PKG_VERSION`.
    pub version: String,
    /// The build target, such as `x86_64-linux`.
    pub target: String,
    /// A resident agent, which keeps sampling while nobody watches.
    pub resident: bool,
}

/// What a host is: sent once after `Hello`, and again if it changes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Inventory {
    pub hostname: String,
    /// `PRETTY_NAME` from os-release, such as "Ubuntu 26.04 LTS".
    pub os: String,
    pub kernel: String,
    pub cpu_model: String,
    pub threads: u16,
    pub mem_total: u64,
    /// Mounts that samples report on, in the order of `Sample::disks`.
    pub mounts: Vec<String>,
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Other,
}

impl Vendor {
    pub fn from_pci(id: u16) -> Vendor {
        match id {
            0x10de => Vendor::Nvidia,
            0x1002 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Apple => "Apple",
            Vendor::Other => "GPU",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuInfo {
    pub vendor: Vendor,
    pub name: String,
    /// PCI address, such as `0000:01:00.0`; empty where there is none.
    pub bus: String,
    /// Dedicated memory (VRAM). On an APU this is the small carve-out.
    pub mem_total: Option<u64>,
    /// Memory shared with the CPU that the GPU can also use (GTT on AMD).
    pub shared_total: Option<u64>,
    /// The GPU's working memory is the host's RAM, not VRAM of its own: an
    /// APU or an Apple Silicon Mac.
    pub unified: bool,
    pub power_limit_w: Option<f32>,
    /// Whether this backend can report anything beyond the name.
    pub metrics: bool,
}

/// One measurement of a host.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sample {
    /// Milliseconds since the Unix epoch, by the agent's clock.
    pub time_ms: u64,
    pub cpu: Cpu,
    pub mem: Mem,
    /// In the order of `Inventory::mounts`.
    pub disks: Vec<Disk>,
    pub disk_io: Io,
    pub net: Io,
    /// In the order of `Inventory::gpus`.
    pub gpus: Vec<Gpu>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cpu {
    /// Busy share of all cores, 0 to 1.
    pub busy: f32,
    /// Busy share of each core, 0 to 100.
    pub cores: Vec<u8>,
    pub load: [f32; 3],
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mem {
    pub total: u64,
    pub available: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Disk {
    pub total: u64,
    pub used: u64,
}

/// A pair of rates in bytes per second.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Io {
    pub read: u64,
    pub write: u64,
}

/// Whether a GPU was measured, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Power {
    #[default]
    Awake,
    /// Powered down (a laptop's dGPU in runtime suspend): not woken to ask.
    Asleep,
    /// Awake but unused, on a GPU that powers down when idle: left alone,
    /// because asking would keep it awake.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gpu {
    pub power: Power,
    /// Busy share, 0 to 100.
    pub util: Option<u8>,
    pub mem_used: Option<u64>,
    pub shared_used: Option<u64>,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub clock_mhz: Option<u32>,
    /// The GPU is slowing itself down for heat or a hardware limit; never
    /// for its power cap, which is how GPUs normally run under load.
    pub throttled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// A training run's GPUs have done almost nothing for a while, though
    /// its processes are alive.
    TrainingStalled,
    /// A training run or model server is gone.
    WorkloadEnded,
    /// A model server's own process no longer listens on its port.
    StoppedListening,
    DiskFull,
    GpuThrottled,
    /// A GPU's memory has stayed nearly full.
    GpuMemoryFull,
    /// The viewer lost the host (made by the viewer, not an agent).
    HostUnreachable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    /// Unique per host, and growing: milliseconds since the epoch times a
    /// thousand, plus a counter.
    pub id: u64,
    pub time_ms: u64,
    pub kind: AlertKind,
    /// What it is about: a workload's label, `gpu0`, a mount.
    pub subject: String,
    pub message: String,
}

/// What a workload is, in the terms of the people who run them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A model server: llama.cpp, vLLM, Ollama, ...
    Inference,
    /// A process tree computing on a GPU that is not a server.
    Training,
    /// A process tree keeping CPUs busy, without a GPU.
    Cpu,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Inference => "inference",
            Kind::Training => "training",
            Kind::Cpu => "cpu",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Workloads {
    pub list: Vec<Workload>,
    /// The busiest processes, in detail mode only.
    pub top: Vec<Proc>,
}

/// A process tree doing one job, named after its top process.
#[derive(Debug, Clone, PartialEq)]
pub struct Workload {
    /// The top process's pid: stable while the job lives, even when the
    /// processes under it come and go (a model server swapping models).
    pub id: u32,
    pub kind: Kind,
    /// Program and script: `torchrun train.py`, `llama-swap`.
    pub label: String,
    /// The model an inference server has loaded; empty when none is, or for
    /// other kinds.
    pub model: String,
    /// TCP ports the tree listens on.
    pub ports: Vec<u16>,
    pub procs: u16,
    /// When the top process started, in seconds since the Unix epoch.
    pub started_s: u64,
    /// CPU cores in use.
    pub cpu: f32,
    pub rss: u64,
    pub gpus: Vec<WorkloadGpu>,
    /// For training: how long its GPUs have been nearly idle, in seconds.
    /// Zero while they are busy.
    pub gpu_idle_s: u32,
    /// The top process's command line, with secrets masked; detail mode
    /// only, empty otherwise.
    pub cmdline: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadGpu {
    /// Index into `Inventory::gpus`.
    pub gpu: u16,
    pub mem: u64,
    /// The workload's share of the GPU's time, 0 to 100, where known.
    pub busy: Option<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub rss: u64,
    pub gpu_mem: u64,
}

// ===== wire =================================================================

impl Wire for AgentMsg {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            AgentMsg::Hello(h) => {
                out.push(0);
                h.put(out);
            }
            AgentMsg::Inventory(i) => {
                out.push(1);
                i.put(out);
            }
            AgentMsg::Sample(s) => {
                out.push(2);
                s.put(out);
            }
            AgentMsg::Error(e) => {
                out.push(3);
                e.put(out);
            }
            AgentMsg::Workloads(w) => {
                out.push(4);
                w.put(out);
            }
            AgentMsg::History(h) => {
                out.push(5);
                h.put(out);
            }
            AgentMsg::Alerts(a) => {
                out.push(6);
                a.put(out);
            }
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => AgentMsg::Hello(Wire::get(input)?),
            1 => AgentMsg::Inventory(Wire::get(input)?),
            2 => AgentMsg::Sample(Wire::get(input)?),
            3 => AgentMsg::Error(Wire::get(input)?),
            4 => AgentMsg::Workloads(Wire::get(input)?),
            5 => AgentMsg::History(Wire::get(input)?),
            6 => AgentMsg::Alerts(Wire::get(input)?),
            _ => return Err(bad("unknown agent message")),
        })
    }
}

impl Wire for ViewerMsg {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            ViewerMsg::Subscribe { interval_ms, detail } => {
                out.push(0);
                interval_ms.put(out);
                detail.put(out);
            }
            ViewerMsg::Bye => out.push(1),
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => ViewerMsg::Subscribe { interval_ms: Wire::get(input)?, detail: Wire::get(input)? },
            1 => ViewerMsg::Bye,
            _ => return Err(bad("unknown viewer message")),
        })
    }
}

/// `Wire` for a struct whose fields are all `Wire`, in declaration order.
macro_rules! wire_struct {
    ($t:ident { $($f:ident),* $(,)? }) => {
        impl Wire for $t {
            fn put(&self, out: &mut Vec<u8>) {
                $(self.$f.put(out);)*
            }
            fn get(input: &mut &[u8]) -> io::Result<Self> {
                Ok($t { $($f: Wire::get(input)?),* })
            }
        }
    };
}

wire_struct!(Hello { protocol, version, target, resident });
wire_struct!(Inventory { hostname, os, kernel, cpu_model, threads, mem_total, mounts, gpus });
wire_struct!(GpuInfo { vendor, name, bus, mem_total, shared_total, unified, power_limit_w, metrics });
wire_struct!(Sample { time_ms, cpu, mem, disks, disk_io, net, gpus });
wire_struct!(Cpu { busy, cores, load });
wire_struct!(Mem { total, available, cached, swap_total, swap_used });
wire_struct!(Disk { total, used });
wire_struct!(Io { read, write });
wire_struct!(Gpu { power, util, mem_used, shared_used, temp_c, power_w, clock_mhz, throttled });
wire_struct!(Alert { id, time_ms, kind, subject, message });

impl Wire for AlertKind {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(match self {
            AlertKind::TrainingStalled => 0,
            AlertKind::WorkloadEnded => 1,
            AlertKind::StoppedListening => 2,
            AlertKind::DiskFull => 3,
            AlertKind::GpuThrottled => 4,
            AlertKind::HostUnreachable => 5,
            AlertKind::GpuMemoryFull => 6,
        });
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => AlertKind::TrainingStalled,
            1 => AlertKind::WorkloadEnded,
            2 => AlertKind::StoppedListening,
            3 => AlertKind::DiskFull,
            4 => AlertKind::GpuThrottled,
            5 => AlertKind::HostUnreachable,
            6 => AlertKind::GpuMemoryFull,
            _ => return Err(bad("unknown alert kind")),
        })
    }
}
wire_struct!(Workloads { list, top });
wire_struct!(Workload { id, kind, label, model, ports, procs, started_s, cpu, rss, gpus, gpu_idle_s, cmdline });
wire_struct!(WorkloadGpu { gpu, mem, busy });
wire_struct!(Proc { pid, name, cpu, rss, gpu_mem });

impl Wire for Kind {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(match self {
            Kind::Inference => 0,
            Kind::Training => 1,
            Kind::Cpu => 2,
        });
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => Kind::Inference,
            1 => Kind::Training,
            2 => Kind::Cpu,
            _ => return Err(bad("unknown workload kind")),
        })
    }
}

impl Wire for Vendor {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(match self {
            Vendor::Nvidia => 0,
            Vendor::Amd => 1,
            Vendor::Intel => 2,
            Vendor::Apple => 3,
            Vendor::Other => 4,
        });
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => Vendor::Nvidia,
            1 => Vendor::Amd,
            2 => Vendor::Intel,
            3 => Vendor::Apple,
            4 => Vendor::Other,
            _ => return Err(bad("unknown vendor")),
        })
    }
}

impl Wire for Power {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(match self {
            Power::Awake => 0,
            Power::Asleep => 1,
            Power::Idle => 2,
        });
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => Power::Awake,
            1 => Power::Asleep,
            2 => Power::Idle,
            _ => return Err(bad("unknown power state")),
        })
    }
}

impl Wire for [f32; 3] {
    fn put(&self, out: &mut Vec<u8>) {
        for v in self {
            v.put(out);
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok([f32::get(input)?, f32::get(input)?, f32::get(input)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{read_frame, write_frame};

    #[test]
    fn messages_survive_a_roundtrip() {
        let msgs = vec![
            AgentMsg::Hello(Hello { protocol: 1, version: "0.1.0".into(), target: "x86_64-linux".into(), resident: true }),
            AgentMsg::Alerts(vec![Alert {
                id: 1,
                time_ms: 2,
                kind: AlertKind::TrainingStalled,
                subject: "torchrun train.py".into(),
                message: "GPUs idle for 10m".into(),
            }]),
            AgentMsg::Inventory(Inventory {
                hostname: "host".into(),
                threads: 32,
                mounts: vec!["/".into()],
                gpus: vec![GpuInfo {
                    vendor: Vendor::Amd,
                    name: "Radeon".into(),
                    bus: "0000:66:00.0".into(),
                    mem_total: Some(8 << 30),
                    shared_total: Some(104 << 30),
                    unified: true,
                    power_limit_w: None,
                    metrics: true,
                }],
                ..Default::default()
            }),
            AgentMsg::Sample(Sample {
                time_ms: 1,
                cpu: Cpu { busy: 0.5, cores: vec![10, 90], load: [1.0, 2.0, 3.0] },
                gpus: vec![Gpu { util: Some(92), temp_c: Some(55.0), ..Default::default() }],
                ..Default::default()
            }),
            AgentMsg::Error("nvml: not found".into()),
            AgentMsg::Workloads(Workloads {
                list: vec![Workload {
                    id: 6291,
                    kind: Kind::Inference,
                    label: "llama-swap".into(),
                    model: "Model-Q3_K_XL".into(),
                    ports: vec![8080, 5810],
                    procs: 2,
                    started_s: 1,
                    cpu: 0.5,
                    rss: 1 << 30,
                    gpus: vec![WorkloadGpu { gpu: 0, mem: 51 << 30, busy: Some(90) }],
                    gpu_idle_s: 0,
                    cmdline: String::new(),
                }],
                top: vec![Proc { pid: 1, name: "x".into(), cpu: 1.0, rss: 2, gpu_mem: 0 }],
            }),
        ];
        for msg in msgs {
            let frame = Frame { route: vec![3], msg };
            let mut buf = Vec::new();
            write_frame(&mut buf, &frame).unwrap();
            let back: Frame<AgentMsg> = read_frame(&mut buf.as_slice()).unwrap().unwrap();
            assert_eq!(back, frame);
        }
    }

    #[test]
    fn random_bytes_never_panic_the_decoder() {
        // A small LCG: deterministic, and no test dependency.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u8
        };
        for len in 0..2000usize {
            let body: Vec<u8> = (0..len % 300).map(|_| next()).collect();
            let _ = Frame::<AgentMsg>::get(&mut body.as_slice());
            let _ = Frame::<ViewerMsg>::get(&mut body.as_slice());
        }
    }

    #[test]
    fn unknown_tags_are_refused() {
        assert!(AgentMsg::get(&mut [9].as_slice()).is_err());
        assert!(ViewerMsg::get(&mut [9].as_slice()).is_err());
        assert!(Vendor::get(&mut [9].as_slice()).is_err());
    }
}

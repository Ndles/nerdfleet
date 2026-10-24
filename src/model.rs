//! The messages agents and the viewer exchange, and the data they carry.

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
    /// A sampler or a detector failed. The agent carries on without it.
    Error(String),
}

/// From the viewer to an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewerMsg {
    /// How often to sample, in milliseconds. The agent keeps it within
    /// `MIN_INTERVAL_MS..=MAX_INTERVAL_MS`.
    Subscribe { interval_ms: u32 },
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
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => AgentMsg::Hello(Wire::get(input)?),
            1 => AgentMsg::Inventory(Wire::get(input)?),
            2 => AgentMsg::Sample(Wire::get(input)?),
            3 => AgentMsg::Error(Wire::get(input)?),
            _ => return Err(bad("unknown agent message")),
        })
    }
}

impl Wire for ViewerMsg {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            ViewerMsg::Subscribe { interval_ms } => {
                out.push(0);
                interval_ms.put(out);
            }
            ViewerMsg::Bye => out.push(1),
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        Ok(match u8::get(input)? {
            0 => ViewerMsg::Subscribe { interval_ms: Wire::get(input)? },
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

wire_struct!(Hello { protocol, version, target });
wire_struct!(Inventory { hostname, os, kernel, cpu_model, threads, mem_total, mounts, gpus });
wire_struct!(GpuInfo { vendor, name, bus, mem_total, shared_total, unified, power_limit_w, metrics });
wire_struct!(Sample { time_ms, cpu, mem, disks, disk_io, net, gpus });
wire_struct!(Cpu { busy, cores, load });
wire_struct!(Mem { total, available, cached, swap_total, swap_used });
wire_struct!(Disk { total, used });
wire_struct!(Io { read, write });
wire_struct!(Gpu { power, util, mem_used, shared_used, temp_c, power_w, clock_mhz });

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
            AgentMsg::Hello(Hello { protocol: 1, version: "0.1.0".into(), target: "x86_64-linux".into() }),
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

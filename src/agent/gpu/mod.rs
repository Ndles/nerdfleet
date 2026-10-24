//! Every GPU on the host, whatever its vendor.
//!
//! GPUs are found on the PCI bus, so each one is listed even when its
//! driver's tools are missing; a backend per vendor then measures it.
//!
//! Some GPUs power off when idle: a laptop's dGPU, mostly. Asking one
//! anything restarts its idle timer, and holding NVML open keeps it awake,
//! so such a GPU is measured every sample only while it is busy. Powered
//! down, it is reported as asleep and not woken. Awake but idle, it is
//! looked at once every `REST` and NVML is let go in between, so its idle
//! timer (about 20 seconds on NVIDIA laptops) can run out. Programs merely
//! holding the device open say nothing: a launcher or a browser does that
//! all day without keeping the GPU awake.

mod amd;
mod drm;
mod nvml;
mod pciids;
mod smi;

use crate::model::{Gpu, GpuInfo, Power, Vendor};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// NVML's process lists are asked every this many NVIDIA queries (about
/// six seconds): with training running they were most of the agent's cost,
/// and a job alone on a GPU is measured by the GPU's own utilization anyway.
const PROCS_EVERY: u8 = 3;

/// GPU memory below which an idle NVIDIA GPU has no processes worth listing.
const IDLE_MEM: u64 = 256 << 20;

/// GPU memory above which a process counts as computing even without
/// compute-engine time: a loaded model waiting for requests.
const COMPUTE_MEM: u64 = 1 << 30;

/// One process's use of one GPU.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcGpu {
    /// Index into the inventory's GPUs.
    pub gpu: u16,
    pub mem: u64,
    pub busy: Option<u8>,
    /// Real compute work (an NVML compute process, AMD compute-engine time,
    /// or a lot of memory), as opposed to drawing a window.
    pub compute: bool,
}

/// How often an awake but idle GPU that could power off is looked at.
const REST: Duration = Duration::from_secs(30);
/// How often NVIDIA GPUs are asked. Each NVML query is a driver call of a
/// millisecond or so, which at every sample was most of the agent's cost on
/// a two-GPU host; in between, the last answer is reused.
const NVIDIA_EVERY: Duration = Duration::from_millis(1900);

enum Backend {
    Amd(amd::Amd),
    /// Measured through NVML, looked up by bus ID among its current handles.
    Nvml,
    /// NVIDIA without NVML: `nvidia-smi`, asked once per sample for all.
    Smi,
    /// Listed, but nothing reads its numbers (yet): Intel, and NVIDIA
    /// before it first wakes up.
    None,
}

struct Entry {
    info: GpuInfo,
    pci: PathBuf,
    backend: Backend,
    /// The GPU powers off when idle.
    can_sleep: bool,
    /// It was awake at the last sample.
    was_awake: bool,
    /// It was busy when last measured.
    busy: bool,
    /// When it was last measured idle.
    rested: Option<Instant>,
    /// What the last sample found.
    last_power: Power,
    /// NVIDIA only: the last answer, reused until `NVIDIA_EVERY` passes.
    cached: Option<Gpu>,
}

pub struct Gpus {
    entries: Vec<Entry>,
    nvml: Option<nvml::Nvml>,
    /// NVML's handles while it is started.
    handles: Vec<nvml::NvGpu>,
    /// NVIDIA is hooked up the first time one of its GPUs is awake, not
    /// before: starting NVML would wake them all.
    nvidia_ready: bool,
    smi: Option<PathBuf>,
    drm: drm::Drm,
    /// When NVIDIA GPUs were last asked, and whether this sample did.
    nvidia_at: Option<Instant>,
    nvidia_fresh: bool,
    /// NVML's compute processes per GPU index, and how many more NVIDIA
    /// queries reuse them.
    nvidia_procs: Vec<(u16, nvml::NvProc)>,
    procs_left: u8,
}

fn read_hex(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

fn suspended(pci: &Path) -> bool {
    super::read_file(pci.join("power/runtime_status")).is_ok_and(|s| s.trim() == "suspended")
}

fn driver(pci: &Path) -> Option<String> {
    let link = std::fs::read_link(pci.join("driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// The driver's `/proc/driver/nvidia/gpus/<bus>/` directory.
fn nvidia_proc(bus: &str) -> PathBuf {
    Path::new("/proc/driver/nvidia/gpus").join(bus)
}

/// Whether the NVIDIA driver powers this GPU off when idle ("Runtime D3").
fn nvidia_can_sleep(bus: &str) -> bool {
    parse_rtd3(&std::fs::read_to_string(nvidia_proc(bus).join("power")).unwrap_or_default())
}

fn parse_rtd3(power: &str) -> bool {
    power.lines().any(|l| {
        l.split_once(':').is_some_and(|(k, v)| k.trim() == "Runtime D3 status" && v.trim().starts_with("Enabled"))
    })
}

impl Gpus {
    /// Finds the GPUs and returns any problem worth telling the viewer.
    pub fn discover() -> (Gpus, Vec<String>) {
        let mut entries = Vec::new();
        let mut dirs: Vec<PathBuf> = std::fs::read_dir("/sys/bus/pci/devices")
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        dirs.sort();
        for pci in dirs {
            // Class 0x03xxxx: VGA, 3D and other display controllers.
            if read_hex(&pci.join("class")).is_none_or(|c| c >> 16 != 0x03) {
                continue;
            }
            let (Some(vendor_id), Some(device_id)) = (read_hex(&pci.join("vendor")), read_hex(&pci.join("device")))
            else {
                continue;
            };
            let vendor = Vendor::from_pci(vendor_id as u16);
            let bus = pci.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let name = pciids::device_name(vendor_id as u16, device_id as u16)
                .unwrap_or_else(|| format!("{} {device_id:04x}", vendor.label()));
            let mut info = GpuInfo {
                vendor,
                name,
                bus: bus.clone(),
                mem_total: None,
                shared_total: None,
                unified: false,
                power_limit_w: None,
                metrics: false,
            };
            let mut can_sleep = false;
            let backend = match vendor {
                Vendor::Amd if driver(&pci).as_deref() == Some("amdgpu") => {
                    let amd = amd::Amd::new(pci.clone());
                    info.mem_total = amd.vram_total();
                    info.shared_total = amd.gtt_total();
                    info.unified = amd::is_unified(info.mem_total, info.shared_total);
                    info.power_limit_w = amd.power_cap_w();
                    info.metrics = true;
                    // amdgpu powers a dGPU off when idle where runtime power
                    // management is on; reading its files wakes it.
                    can_sleep = !info.unified
                        && std::fs::read_to_string(pci.join("power/control")).is_ok_and(|c| c.trim() == "auto");
                    Backend::Amd(amd)
                }
                Vendor::Nvidia => {
                    can_sleep = nvidia_can_sleep(&bus);
                    Backend::None
                }
                _ => Backend::None,
            };
            entries.push(Entry {
                info,
                pci,
                backend,
                can_sleep,
                was_awake: false,
                busy: false,
                rested: None,
                last_power: Power::Asleep,
                cached: None,
            });
        }
        let mut gpus = Gpus {
            entries,
            nvml: None,
            handles: Vec::new(),
            nvidia_ready: false,
            smi: None,
            drm: Default::default(),
            nvidia_at: None,
            nvidia_fresh: false,
            nvidia_procs: Vec::new(),
            procs_left: 0,
        };
        let problems = gpus.start_nvidia().into_iter().collect();
        (gpus, problems)
    }

    pub fn info(&self) -> Vec<GpuInfo> {
        self.entries.iter().map(|e| e.info.clone()).collect()
    }

    /// Hooks the NVIDIA GPUs up to NVML, or to `nvidia-smi` without it,
    /// once one of them is awake. Returns a problem to report, if any.
    fn start_nvidia(&mut self) -> Option<String> {
        let nvidia: Vec<usize> =
            (0..self.entries.len()).filter(|&i| self.entries[i].info.vendor == Vendor::Nvidia).collect();
        if self.nvidia_ready || nvidia.is_empty() || nvidia.iter().all(|&i| suspended(&self.entries[i].pci)) {
            return None;
        }
        self.nvidia_ready = true;
        let started = nvml::Nvml::load().and_then(|mut lib| lib.start().map(|devs| (lib, devs)));
        match started {
            Ok((lib, devs)) => {
                for dev in &devs {
                    let Some(&i) = nvidia.iter().find(|&&i| self.entries[i].info.bus == dev.bus) else { continue };
                    let e = &mut self.entries[i];
                    e.info.name = dev.name.clone();
                    e.info.mem_total = dev.mem_total;
                    e.info.power_limit_w = dev.power_limit_w;
                    e.info.metrics = true;
                    e.backend = Backend::Nvml;
                }
                self.handles = devs;
                self.nvml = Some(lib);
                None
            }
            Err(nvml_err) => match smi::find() {
                Some(path) => {
                    for &i in &nvidia {
                        self.entries[i].info.metrics = true;
                        self.entries[i].backend = Backend::Smi;
                    }
                    self.smi = Some(path);
                    None
                }
                None => Some(format!("{nvml_err}, and nvidia-smi was not found: NVIDIA GPUs show no numbers")),
            },
        }
    }

    /// Measures every GPU. `changed` is set when what the GPUs are (their
    /// names, memory sizes) became known, so the inventory should be sent
    /// again.
    pub fn sample(&mut self, problems: &mut Vec<String>, changed: &mut bool) -> Vec<Gpu> {
        if !self.nvidia_ready {
            problems.extend(self.start_nvidia());
            *changed |= self.nvidia_ready;
        }
        let power: Vec<Power> = self
            .entries
            .iter_mut()
            .map(|e| {
                if suspended(&e.pci) {
                    e.was_awake = false;
                    return Power::Asleep;
                }
                // Just woken (by whatever is about to use it), busy, or
                // rested long enough: measure it.
                let due = !e.can_sleep || !e.was_awake || e.busy || e.rested.is_none_or(|t| t.elapsed() >= REST);
                e.was_awake = true;
                if due {
                    Power::Awake
                } else {
                    Power::Idle
                }
            })
            .collect();
        self.start_nvml(&power, problems);
        let nvidia = |b: &Backend| matches!(b, Backend::Nvml | Backend::Smi);
        // Ask NVIDIA when it is time, or when a GPU just woke and has no
        // answer to reuse.
        self.nvidia_fresh = self.nvidia_at.is_none_or(|t| t.elapsed() >= NVIDIA_EVERY)
            || self.entries.iter().zip(&power).any(|(e, p)| nvidia(&e.backend) && *p == Power::Awake && e.cached.is_none());
        if self.nvidia_fresh {
            self.nvidia_at = Some(Instant::now());
        }
        let smi = match &self.smi {
            Some(path)
                if self.nvidia_fresh
                    && self.entries.iter().zip(&power).any(|(e, p)| matches!(e.backend, Backend::Smi) && *p == Power::Awake) =>
            {
                smi::query(path)
            }
            _ => Vec::new(),
        };
        let (nvml, handles, fresh) = (&self.nvml, &self.handles, self.nvidia_fresh);
        let samples: Vec<Gpu> = self
            .entries
            .iter_mut()
            .zip(power.iter().copied())
            .map(|(e, power)| {
                if power != Power::Awake {
                    e.cached = None;
                    return Gpu { power, ..Default::default() };
                }
                if nvidia(&e.backend) && !fresh {
                    if let Some(g) = &e.cached {
                        return g.clone();
                    }
                }
                let g = match &e.backend {
                    Backend::Amd(amd) => amd.sample(),
                    Backend::Nvml => match (nvml, handles.iter().find(|h| h.bus == e.info.bus)) {
                        (Some(lib), Some(h)) => lib.sample(h),
                        _ => Gpu::default(),
                    },
                    Backend::Smi => {
                        smi.iter().find(|(bus, _)| *bus == e.info.bus).map(|(_, g)| g.clone()).unwrap_or_default()
                    }
                    Backend::None => Gpu::default(),
                };
                if nvidia(&e.backend) {
                    e.cached = Some(g.clone());
                }
                g
            })
            .collect();
        for ((e, g), p) in self.entries.iter_mut().zip(&samples).zip(&power) {
            e.last_power = *p;
            if *p == Power::Awake {
                e.busy = g.util.is_some_and(|u| u > 0);
                if !e.busy {
                    e.rested = Some(Instant::now());
                }
            }
        }
        self.release_nvml();
        samples
    }

    /// Which processes use which GPU, as of the last `sample`. GPUs that
    /// were left alone then are left alone now: NVML is only asked while
    /// it is started, which it is only for GPUs being measured.
    pub fn processes(&mut self) -> HashMap<u32, Vec<ProcGpu>> {
        let mut out: HashMap<u32, Vec<ProcGpu>> = HashMap::new();
        for (pid, uses) in self.drm.sample() {
            for u in uses {
                let Some(i) = self.entries.iter().position(|e| e.info.bus == u.bus) else { continue };
                let mem = u.vram + u.gtt;
                out.entry(pid).or_default().push(ProcGpu {
                    gpu: i as u16,
                    mem,
                    busy: u.busy,
                    compute: u.compute || mem >= COMPUTE_MEM,
                });
            }
        }
        let ask = self.nvidia_fresh && self.procs_left == 0;
        if self.nvidia_fresh {
            self.procs_left = if ask { PROCS_EVERY - 1 } else { self.procs_left - 1 };
        }
        if ask {
            self.nvidia_procs.clear();
            if let Some(lib) = self.nvml.as_ref().filter(|l| l.started()) {
                for (i, e) in self.entries.iter().enumerate() {
                    if !matches!(e.backend, Backend::Nvml) || e.last_power != Power::Awake {
                        continue;
                    }
                    // An idle GPU with no memory in use has nobody on it to
                    // list. A driver keeps a few MiB for itself.
                    let g = e.cached.as_ref();
                    if g.is_some_and(|g| g.util == Some(0) && g.mem_used.is_some_and(|m| m < IDLE_MEM)) {
                        continue;
                    }
                    let Some(h) = self.handles.iter().find(|h| h.bus == e.info.bus) else { continue };
                    self.nvidia_procs.extend(lib.processes(h).into_iter().map(|p| (i as u16, p)));
                }
            }
        }
        for (i, p) in &self.nvidia_procs {
            // A GPU that went to sleep since has no processes.
            if self.entries[*i as usize].last_power != Power::Awake {
                continue;
            }
            out.entry(p.pid).or_default().push(ProcGpu { gpu: *i, mem: p.mem.unwrap_or(0), busy: p.sm, compute: true });
        }
        out
    }

    /// Starts NVML when an NVIDIA GPU it measures is to be measured.
    fn start_nvml(&mut self, power: &[Power], problems: &mut Vec<String>) {
        let Some(lib) = &mut self.nvml else { return };
        let want = self.entries.iter().zip(power).any(|(e, p)| matches!(e.backend, Backend::Nvml) && *p == Power::Awake);
        if want && !lib.started() {
            match lib.start() {
                Ok(devs) => self.handles = devs,
                Err(e) => problems.push(e),
            }
        }
    }

    /// Lets go of NVML when it holds a GPU that could power off and none
    /// of them is busy. On hosts whose GPUs never sleep it stays started.
    fn release_nvml(&mut self) {
        let Some(lib) = &mut self.nvml else { return };
        let nv = || self.entries.iter().filter(|e| matches!(e.backend, Backend::Nvml));
        if lib.started() && nv().any(|e| e.can_sleep) && !nv().any(|e| e.busy) {
            self.handles.clear();
            lib.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_d3_from_the_nvidia_driver() {
        assert!(parse_rtd3("Runtime D3 status:          Enabled (fine-grained)\nVideo Memory: Off\n"));
        assert!(!parse_rtd3("Runtime D3 status:          Disabled by default\n"));
        assert!(!parse_rtd3(""));
    }

}

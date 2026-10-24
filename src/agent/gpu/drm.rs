//! Per-process GPU use from the kernel's DRM fdinfo: memory and engine time
//! for every open DRM client (`/proc/<pid>/fdinfo/<fd>`, `drm-*` keys).
//!
//! It works for amdgpu, i915 and xe, and sees Vulkan work as well as ROCm,
//! which the vendor tools do not. It needs no library and no privileges,
//! but only the user's own processes can be read.
//!
//! Finding which processes have a GPU open means reading every fd of every
//! process, so that is done every few seconds; the fdinfo of the processes
//! found is read every sample.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

/// How often to look for processes newly using a GPU: reading every fd of
/// every process is the costly part.
const RESCAN: Duration = Duration::from_secs(10);
/// NVIDIA is measured through NVML; its DRM clients would count twice.
const DRIVERS: &[&str] = &["amdgpu", "i915", "xe"];

/// One process's use of one GPU, summed over its DRM clients.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Use {
    /// The GPU's PCI address, as in `Inventory::gpus`.
    pub bus: String,
    pub vram: u64,
    pub gtt: u64,
    /// Share of the busiest engine, 0 to 100.
    pub busy: Option<u8>,
    /// Some compute engine ran since the last sample: compute work, not
    /// just a window on the screen.
    pub compute: bool,
}

/// One DRM client's counters, as fdinfo lists them.
#[derive(Debug, Default, PartialEq)]
struct Client {
    bus: String,
    id: u64,
    driver: String,
    vram: u64,
    gtt: u64,
    /// Engine name to busy nanoseconds since the client opened.
    engines: HashMap<String, u64>,
    capacity: HashMap<String, u64>,
}

fn kib(v: &str) -> u64 {
    let mut f = v.split_whitespace();
    let n: u64 = f.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    match f.next() {
        Some("KiB") => n * 1024,
        Some("MiB") => n << 20,
        Some("GiB") => n << 30,
        _ => n,
    }
}

fn parse_fdinfo(text: &str) -> Option<Client> {
    let mut c = Client::default();
    let mut resident: HashMap<&str, u64> = HashMap::new();
    let mut memory: HashMap<&str, u64> = HashMap::new();
    let mut is_drm = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "drm-driver" => {
                is_drm = true;
                c.driver = v.to_string();
            }
            "drm-pdev" => c.bus = v.to_ascii_lowercase(),
            "drm-client-id" => c.id = v.parse().unwrap_or(0),
            k => {
                if let Some(e) = k.strip_prefix("drm-engine-capacity-") {
                    c.capacity.insert(e.to_string(), v.parse().unwrap_or(1));
                } else if let Some(e) = k.strip_prefix("drm-engine-") {
                    c.engines.insert(e.to_string(), v.split_whitespace().next()?.parse().unwrap_or(0));
                } else if let Some(r) = k.strip_prefix("drm-resident-") {
                    resident.insert(r, kib(v));
                } else if let Some(r) = k.strip_prefix("drm-memory-") {
                    memory.insert(r, kib(v));
                }
            }
        }
    }
    if !is_drm {
        return None;
    }
    // Newer kernels say `drm-resident-*`; older ones only `drm-memory-*`,
    // which meant the same.
    let mem = |r: &str| resident.get(r).or_else(|| memory.get(r)).copied().unwrap_or(0);
    c.vram = mem("vram");
    // i915 and xe call system memory "system".
    c.gtt = mem("gtt").max(mem("system"));
    Some(c)
}

#[derive(Default)]
pub struct Drm {
    /// Processes found with a GPU open, and which of their fds.
    users: HashMap<u32, Vec<u32>>,
    scanned: Option<Instant>,
    /// Engine time per (pid, bus, client, engine) at the last sample.
    prev: HashMap<(u32, String, u64, String), u64>,
    last: Option<Instant>,
}

fn drm_fds(pid: u32) -> Vec<u32> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    fds.filter_map(|f| f.ok())
        .filter(|f| std::fs::read_link(f.path()).is_ok_and(|t| t.starts_with("/dev/dri/")))
        .filter_map(|f| f.file_name().to_str()?.parse().ok())
        .collect()
}

impl Drm {
    /// Per process, its use of each GPU.
    pub fn sample(&mut self) -> HashMap<u32, Vec<Use>> {
        let now = Instant::now();
        if self.scanned.is_none_or(|t| now.duration_since(t) >= RESCAN) {
            self.scanned = Some(now);
            let me = std::process::id();
            self.users = std::fs::read_dir("/proc")
                .map(|d| {
                    d.filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                        .filter(|&pid| pid != me)
                        .map(|pid| (pid, drm_fds(pid)))
                        .filter(|(_, fds)| !fds.is_empty())
                        .collect()
                })
                .unwrap_or_default();
        }
        let elapsed = self.last.map(|t| now.duration_since(t).as_nanos() as f64);
        self.last = Some(now);
        let mut prev = HashMap::new();
        let mut out = HashMap::new();
        for (&pid, fds) in &self.users {
            // A client shows up once per fd that refers to it: count it once.
            let mut clients: HashMap<(String, u64), Client> = HashMap::new();
            for fd in fds {
                let path = format!("/proc/{pid}/fdinfo/{fd}");
                let Some(c) = crate::agent::read_file(Path::new(&path)).ok().and_then(|t| parse_fdinfo(&t)) else {
                    continue;
                };
                if DRIVERS.contains(&c.driver.as_str()) {
                    clients.insert((c.bus.clone(), c.id), c);
                }
            }
            let mut by_bus: HashMap<String, Use> = HashMap::new();
            let mut seen = HashSet::new();
            for ((bus, id), c) in clients {
                let u = by_bus.entry(bus.clone()).or_insert_with(|| Use { bus: bus.clone(), ..Default::default() });
                u.vram += c.vram;
                u.gtt += c.gtt;
                for (engine, ns) in &c.engines {
                    let key = (pid, bus.clone(), id, engine.clone());
                    seen.insert(key.clone());
                    let before = self.prev.get(&key).copied();
                    prev.insert(key, *ns);
                    let (Some(before), Some(elapsed)) = (before, elapsed) else { continue };
                    let delta = ns.saturating_sub(before);
                    if engine == "compute" && delta > 0 {
                        u.compute = true;
                    }
                    let cap = c.capacity.get(engine).copied().unwrap_or(1).max(1) as f64;
                    let pct = (delta as f64 / elapsed / cap * 100.0).clamp(0.0, 100.0) as u8;
                    u.busy = Some(u.busy.unwrap_or(0).max(pct));
                }
            }
            if !by_bus.is_empty() {
                out.insert(pid, by_bus.into_values().collect());
            }
        }
        self.prev = prev;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLAMA: &str = "\
pos:\t0
flags:\t02100002
drm-driver:\tamdgpu
drm-client-id:\t342
drm-pdev:\t0000:66:00.0
drm-memory-vram:\t8076276 KiB
drm-memory-gtt: \t53903880 KiB
drm-resident-vram:\t8076276 KiB
drm-resident-gtt:\t53903880 KiB
drm-engine-compute:\t123456789 ns
";

    #[test]
    fn fdinfo_of_a_vulkan_inference_server() {
        let c = parse_fdinfo(LLAMA).unwrap();
        assert_eq!(c.driver, "amdgpu");
        assert_eq!(c.bus, "0000:66:00.0");
        assert_eq!(c.id, 342);
        assert_eq!(c.vram, 8076276 * 1024);
        assert_eq!(c.gtt, 53903880 * 1024);
        assert_eq!(c.engines.get("compute"), Some(&123456789));
    }

    #[test]
    fn non_drm_fds_are_ignored() {
        assert!(parse_fdinfo("pos:\t0\nflags:\t02\nmnt_id:\t25\n").is_none());
    }

    #[test]
    fn intel_system_memory_counts_as_shared() {
        let c = parse_fdinfo("drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t7\ndrm-resident-system:\t2048 KiB\ndrm-engine-render:\t5 ns\ndrm-engine-capacity-render:\t2\n").unwrap();
        assert_eq!(c.gtt, 2048 * 1024);
        assert_eq!(c.capacity.get("render"), Some(&2));
    }
}

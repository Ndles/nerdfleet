//! NVIDIA GPUs through NVML, loaded at run time.
//!
//! The binary does not link against NVML, so one build runs on machines
//! with and without the NVIDIA driver. The library is looked up only by its
//! versioned name through the system loader: never from a path in the
//! environment or the config, and not by the unversioned `libnvidia-ml.so`,
//! a development symlink that is often missing.

use crate::model::{Gpu, Power};
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

type Ret = c_int;
type Device = *mut c_void;
const SUCCESS: Ret = 0;
const ERROR_INSUFFICIENT_SIZE: Ret = 7;
/// The clock is asked every this many samples.
const CLOCK_EVERY: u8 = 5;
/// What NVML reports for a value it may not show (another user's process
/// memory, without privileges).
const VALUE_NOT_AVAILABLE: u64 = u64::MAX;
const TEMPERATURE_GPU: c_int = 0;
const CLOCK_GRAPHICS: c_int = 0;

#[repr(C)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

/// `nvmlPciInfo_t` as `nvmlDeviceGetPciInfo_v3` fills it.
#[repr(C)]
struct PciInfo {
    bus_id_legacy: [c_char; 16],
    domain: c_uint,
    bus: c_uint,
    device: c_uint,
    pci_device_id: c_uint,
    pci_sub_system_id: c_uint,
    bus_id: [c_char; 32],
}

/// `nvmlProcessInfo_t`, as the `_v3` process lists fill it.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessInfo {
    pid: c_uint,
    used_gpu_memory: u64,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

/// `nvmlProcessUtilizationSample_t`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessUtil {
    pid: c_uint,
    time_stamp: u64,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
}

struct Api {
    init: unsafe extern "C" fn() -> Ret,
    shutdown: unsafe extern "C" fn() -> Ret,
    count: unsafe extern "C" fn(*mut c_uint) -> Ret,
    handle: unsafe extern "C" fn(c_uint, *mut Device) -> Ret,
    name: unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Ret,
    pci: unsafe extern "C" fn(Device, *mut PciInfo) -> Ret,
    utilization: unsafe extern "C" fn(Device, *mut Utilization) -> Ret,
    memory: unsafe extern "C" fn(Device, *mut Memory) -> Ret,
    temperature: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
    power: unsafe extern "C" fn(Device, *mut c_uint) -> Ret,
    power_limit: unsafe extern "C" fn(Device, *mut c_uint) -> Ret,
    clock: unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret,
    /// Missing from older drivers; per-process numbers are then unknown.
    compute_procs: Option<unsafe extern "C" fn(Device, *mut c_uint, *mut ProcessInfo) -> Ret>,
    proc_util: Option<unsafe extern "C" fn(Device, *mut ProcessUtil, *mut c_uint, u64) -> Ret>,
}

/// One process computing on an NVIDIA GPU.
#[derive(Debug, Clone, PartialEq)]
pub struct NvProc {
    pub pid: u32,
    pub mem: Option<u64>,
    /// Share of the GPU's streaming multiprocessors, 0 to 100.
    pub sm: Option<u8>,
}

/// The loaded library. Loading does not touch the GPUs; `start` does, and
/// holds them open until `stop`, which keeps a GPU that could power off
/// awake. So hosts whose GPUs can sleep start and stop it as they are used.
pub struct Nvml {
    api: Api,
    started: bool,
}

/// One NVIDIA GPU as NVML sees it.
pub struct NvGpu {
    handle: Device,
    pub name: String,
    /// Lowercase, in sysfs form: `0000:01:00.0`.
    pub bus: String,
    pub mem_total: Option<u64>,
    pub power_limit_w: Option<f32>,
    /// The newest utilization sample seen, so each query returns only
    /// newer ones.
    last_util: std::cell::Cell<u64>,
    /// The clock, and how many more samples reuse it: it changes slowly,
    /// and every NVML call is a driver call.
    clock: std::cell::Cell<(Option<u32>, u8)>,
}

// SAFETY: NVML handles are plain identifiers, and NVML is documented as
// thread-safe.
unsafe impl Send for NvGpu {}
unsafe impl Send for Nvml {}

/// Looks up `name` in `lib` as a function pointer of type `T`.
///
/// SAFETY: the caller must pick `T` to match the C signature of `name`.
unsafe fn sym<T>(lib: *mut c_void, name: &CStr) -> Result<T, String> {
    let p = libc::dlsym(lib, name.as_ptr());
    if p.is_null() {
        return Err(format!("NVML has no {}", name.to_string_lossy()));
    }
    Ok(std::mem::transmute_copy::<*mut c_void, T>(&p))
}

impl Nvml {
    /// Loads NVML. The library stays loaded for the life of the agent.
    pub fn load() -> Result<Nvml, String> {
        // SAFETY: dlopen with a constant name; the handle is never closed,
        // so the function pointers taken from it stay valid.
        unsafe {
            let lib = libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                return Err("NVML (libnvidia-ml.so.1) is not installed".into());
            }
            let api = Api {
                init: sym(lib, c"nvmlInit_v2")?,
                shutdown: sym(lib, c"nvmlShutdown")?,
                count: sym(lib, c"nvmlDeviceGetCount_v2")?,
                handle: sym(lib, c"nvmlDeviceGetHandleByIndex_v2")?,
                name: sym(lib, c"nvmlDeviceGetName")?,
                pci: sym(lib, c"nvmlDeviceGetPciInfo_v3")?,
                utilization: sym(lib, c"nvmlDeviceGetUtilizationRates")?,
                memory: sym(lib, c"nvmlDeviceGetMemoryInfo")?,
                temperature: sym(lib, c"nvmlDeviceGetTemperature")?,
                power: sym(lib, c"nvmlDeviceGetPowerUsage")?,
                power_limit: sym(lib, c"nvmlDeviceGetEnforcedPowerLimit")?,
                clock: sym(lib, c"nvmlDeviceGetClockInfo")?,
                compute_procs: sym(lib, c"nvmlDeviceGetComputeRunningProcesses_v3").ok(),
                proc_util: sym(lib, c"nvmlDeviceGetProcessUtilization").ok(),
            };
            Ok(Nvml { api, started: false })
        }
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Starts NVML and lists its GPUs. Handles from an earlier start are
    /// no longer valid.
    pub fn start(&mut self) -> Result<Vec<NvGpu>, String> {
        if !self.started {
            // SAFETY: nvmlInit takes no arguments.
            match unsafe { (self.api.init)() } {
                SUCCESS => self.started = true,
                code => return Err(format!("NVML failed to start (error {code})")),
            }
        }
        Ok(self.devices())
    }

    /// Lets go of the GPUs, so they can power down.
    pub fn stop(&mut self) {
        if self.started {
            // SAFETY: balanced with the successful nvmlInit in `start`.
            unsafe { (self.api.shutdown)() };
            self.started = false;
        }
    }

    fn devices(&self) -> Vec<NvGpu> {
        let a = &self.api;
        let mut n: c_uint = 0;
        // SAFETY: every call below passes pointers to properly sized,
        // initialized locals, and buffer lengths that match them.
        unsafe {
            if (a.count)(&mut n) != SUCCESS {
                return Vec::new();
            }
            (0..n.min(64))
                .filter_map(|i| {
                    let mut handle: Device = std::ptr::null_mut();
                    if (a.handle)(i, &mut handle) != SUCCESS {
                        return None;
                    }
                    let mut name = [0 as c_char; 96];
                    let name = if (a.name)(handle, name.as_mut_ptr(), name.len() as c_uint) == SUCCESS {
                        name[name.len() - 1] = 0;
                        CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned()
                    } else {
                        "NVIDIA GPU".to_string()
                    };
                    let mut pci: PciInfo = std::mem::zeroed();
                    let bus = if (a.pci)(handle, &mut pci) == SUCCESS {
                        pci.bus_id[pci.bus_id.len() - 1] = 0;
                        sysfs_bus(&CStr::from_ptr(pci.bus_id.as_ptr()).to_string_lossy())
                    } else {
                        String::new()
                    };
                    let mut mem: Memory = std::mem::zeroed();
                    let mem_total = ((a.memory)(handle, &mut mem) == SUCCESS).then_some(mem.total);
                    let mut mw: c_uint = 0;
                    let power_limit_w = ((a.power_limit)(handle, &mut mw) == SUCCESS).then(|| mw as f32 / 1000.0);
                    Some(NvGpu {
                        handle,
                        name,
                        bus,
                        mem_total,
                        power_limit_w,
                        last_util: Default::default(),
                        clock: Default::default(),
                    })
                })
                .collect()
        }
    }

    pub fn sample(&self, d: &NvGpu) -> Gpu {
        if !self.started {
            return Gpu::default();
        }
        let a = &self.api;
        let h = d.handle;
        // SAFETY: as in `devices`; `h` came from this start of NVML, which is
        // still running (checked above), and handles stay valid until `stop`.
        unsafe {
            let mut util: Utilization = std::mem::zeroed();
            let mut mem: Memory = std::mem::zeroed();
            let mut temp: c_uint = 0;
            let mut mw: c_uint = 0;
            let (mut clock, left) = d.clock.get();
            if left == 0 {
                let mut mhz: c_uint = 0;
                clock = ((a.clock)(h, CLOCK_GRAPHICS, &mut mhz) == SUCCESS).then_some(mhz);
                d.clock.set((clock, CLOCK_EVERY - 1));
            } else {
                d.clock.set((clock, left - 1));
            }
            Gpu {
                power: Power::Awake,
                util: ((a.utilization)(h, &mut util) == SUCCESS).then(|| util.gpu.min(100) as u8),
                mem_used: ((a.memory)(h, &mut mem) == SUCCESS).then_some(mem.used),
                shared_used: None,
                temp_c: ((a.temperature)(h, TEMPERATURE_GPU, &mut temp) == SUCCESS).then_some(temp as f32),
                power_w: ((a.power)(h, &mut mw) == SUCCESS).then(|| mw as f32 / 1000.0),
                clock_mhz: clock,
            }
        }
    }
}

impl Nvml {
    /// The processes computing on `d` (not those only drawing on it), with
    /// their memory and recent share of the GPU.
    pub fn processes(&self, d: &NvGpu) -> Vec<NvProc> {
        let Some(list) = self.api.compute_procs.filter(|_| self.started) else { return Vec::new() };
        // SAFETY: the two-call pattern NVML documents: ask for the count,
        // then pass a buffer of that many (plus room for processes starting
        // in between), and retry if it still grew. `n` always holds the
        // buffer's real length when NVML writes into it.
        let procs: Vec<ProcessInfo> = unsafe {
            let mut n: c_uint = 0;
            let mut buf: Vec<ProcessInfo> = Vec::new();
            let mut ok = false;
            for _ in 0..3 {
                buf = vec![std::mem::zeroed(); n as usize + 8];
                n = buf.len() as c_uint;
                match list(d.handle, &mut n, buf.as_mut_ptr()) {
                    SUCCESS => {
                        ok = true;
                        break;
                    }
                    ERROR_INSUFFICIENT_SIZE => continue,
                    _ => break,
                }
            }
            if !ok {
                return Vec::new();
            }
            buf.truncate((n as usize).min(buf.len()));
            buf
        };
        let mut sm: std::collections::HashMap<u32, (u64, u32)> = std::collections::HashMap::new();
        // Whether NVML gave samples this time. Without them a process's share
        // is unknown, not zero: NVML also says "no samples" when its sampling
        // window and ours do not line up.
        let mut sampled = false;
        // The slowest call there is: only with someone to ask about.
        if let Some(util) = self.api.proc_util.filter(|_| !procs.is_empty()) {
            // SAFETY: as above; NVML fills at most `n` samples and sets `n`
            // to how many it wrote.
            unsafe {
                let mut n: c_uint = 0;
                let since = d.last_util.get();
                if util(d.handle, std::ptr::null_mut(), &mut n, since) == ERROR_INSUFFICIENT_SIZE && n > 0 {
                    let mut buf: Vec<ProcessUtil> = vec![std::mem::zeroed(); (n as usize).min(4096)];
                    n = buf.len() as c_uint;
                    if util(d.handle, buf.as_mut_ptr(), &mut n, since) == SUCCESS && n > 0 {
                        sampled = true;
                        for s in &buf[..(n as usize).min(buf.len())] {
                            // Keep each process's newest sample.
                            let e = sm.entry(s.pid).or_insert((0, 0));
                            if s.time_stamp >= e.0 {
                                *e = (s.time_stamp, s.sm_util);
                            }
                            d.last_util.set(d.last_util.get().max(s.time_stamp));
                        }
                    }
                }
            }
        }
        procs
            .iter()
            .map(|p| NvProc {
                pid: p.pid,
                mem: (p.used_gpu_memory != VALUE_NOT_AVAILABLE).then_some(p.used_gpu_memory),
                // With samples, a process without one did not run.
                sm: sampled.then(|| sm.get(&p.pid).map_or(0, |s| s.1.min(100) as u8)),
            })
            .collect()
    }
}

/// NVML writes bus IDs as `00000000:01:00.0`; sysfs as `0000:01:00.0`.
pub fn sysfs_bus(nvml: &str) -> String {
    let id = nvml.trim().to_ascii_lowercase();
    match id.split_once(':') {
        Some((domain, rest)) if domain.len() > 4 => format!("{}:{rest}", &domain[domain.len() - 4..]),
        _ => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_ids_match_sysfs() {
        assert_eq!(sysfs_bus("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(sysfs_bus("0000:03:00.0"), "0000:03:00.0");
        assert_eq!(sysfs_bus("00000000:7A:00.0"), "0000:7a:00.0");
    }

    #[test]
    fn structs_have_nvmls_layout() {
        assert_eq!(std::mem::size_of::<PciInfo>(), 68);
        assert_eq!(std::mem::size_of::<ProcessInfo>(), 24);
        assert_eq!(std::mem::size_of::<ProcessUtil>(), 32);
    }
}

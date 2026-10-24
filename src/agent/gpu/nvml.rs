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
                    Some(NvGpu { handle, name, bus, mem_total, power_limit_w })
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
            let mut mhz: c_uint = 0;
            Gpu {
                power: Power::Awake,
                util: ((a.utilization)(h, &mut util) == SUCCESS).then(|| util.gpu.min(100) as u8),
                mem_used: ((a.memory)(h, &mut mem) == SUCCESS).then_some(mem.used),
                shared_used: None,
                temp_c: ((a.temperature)(h, TEMPERATURE_GPU, &mut temp) == SUCCESS).then_some(temp as f32),
                power_w: ((a.power)(h, &mut mw) == SUCCESS).then(|| mw as f32 / 1000.0),
                clock_mhz: ((a.clock)(h, CLOCK_GRAPHICS, &mut mhz) == SUCCESS).then_some(mhz),
            }
        }
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
    fn pci_info_has_nvmls_layout() {
        assert_eq!(std::mem::size_of::<PciInfo>(), 68);
    }
}

//! AMD GPUs through the amdgpu driver's sysfs files. No library and no
//! ROCm: this works with any ROCm version installed, or none, and sees
//! Vulkan workloads the same as ROCm ones.

use crate::model::{Gpu, Power};
use std::path::{Path, PathBuf};

pub struct Amd {
    /// `/sys/class/drm/cardN/device`.
    dev: PathBuf,
    hwmon: Option<PathBuf>,
}

fn read_u64(path: &Path) -> Option<u64> {
    crate::agent::read_file(path).ok()?.trim().parse().ok()
}

impl Amd {
    pub fn new(dev: PathBuf) -> Amd {
        let hwmon = std::fs::read_dir(dev.join("hwmon"))
            .ok()
            .and_then(|mut d| d.find_map(|e| e.ok()))
            .map(|e| e.path());
        Amd { dev, hwmon }
    }

    pub fn vram_total(&self) -> Option<u64> {
        read_u64(&self.dev.join("mem_info_vram_total"))
    }

    pub fn gtt_total(&self) -> Option<u64> {
        read_u64(&self.dev.join("mem_info_gtt_total"))
    }

    pub fn power_cap_w(&self) -> Option<f32> {
        let hw = self.hwmon.as_ref()?;
        read_u64(&hw.join("power1_cap")).map(|uw| uw as f32 / 1e6).filter(|w| *w > 0.0)
    }

    pub fn sample(&self) -> Gpu {
        let hw = |name: &str| self.hwmon.as_ref().and_then(|h| read_u64(&h.join(name)));
        Gpu {
            power: Power::Awake,
            util: read_u64(&self.dev.join("gpu_busy_percent")).map(|v| v.min(100) as u8),
            mem_used: read_u64(&self.dev.join("mem_info_vram_used")),
            shared_used: read_u64(&self.dev.join("mem_info_gtt_used")),
            // Microwatts; APUs report `power1_input`, dGPUs often only the
            // average.
            power_w: hw("power1_average").or_else(|| hw("power1_input")).map(|uw| uw as f32 / 1e6),
            temp_c: hw("temp1_input").map(|mc| mc as f32 / 1000.0),
            clock_mhz: hw("freq1_input").map(|hz| (hz / 1_000_000) as u32),
            // amdgpu has no clean throttling signal in sysfs; guessing from
            // the temperature would be worse than saying nothing.
            throttled: None,
        }
    }
}

/// An APU uses system memory as its own: its VRAM is a small carve-out and
/// GTT, the system memory it can address, is many times larger.
pub fn is_unified(vram: Option<u64>, gtt: Option<u64>) -> bool {
    match (vram, gtt) {
        (Some(v), Some(g)) => g >= v.saturating_mul(4),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apus_are_told_from_dgpus_by_their_memory() {
        // A Strix Halo APU: 8 GiB carve-out, about 104 GiB GTT.
        assert!(is_unified(Some(8 << 30), Some(104 << 30)));
        // A 24 GiB dGPU in a 64 GiB machine.
        assert!(!is_unified(Some(24 << 30), Some(32 << 30)));
        assert!(!is_unified(None, Some(1 << 30)));
    }

    #[test]
    fn reads_a_fake_device() {
        let dir = std::env::temp_dir().join(format!("nerdfleet-amd-{}", std::process::id()));
        let hw = dir.join("hwmon/hwmon3");
        std::fs::create_dir_all(&hw).unwrap();
        for (f, v) in [
            ("gpu_busy_percent", "92"),
            ("mem_info_vram_used", "1024"),
            ("mem_info_vram_total", "8589934592"),
            ("mem_info_gtt_used", "2048"),
        ] {
            std::fs::write(dir.join(f), format!("{v}\n")).unwrap();
        }
        for (f, v) in [("power1_input", "45000000"), ("temp1_input", "55000"), ("freq1_input", "2900000000")] {
            std::fs::write(hw.join(f), format!("{v}\n")).unwrap();
        }
        let amd = Amd::new(dir.clone());
        let s = amd.sample();
        assert_eq!(s.util, Some(92));
        assert_eq!(s.mem_used, Some(1024));
        assert_eq!(s.shared_used, Some(2048));
        assert_eq!(s.power_w, Some(45.0));
        assert_eq!(s.temp_c, Some(55.0));
        assert_eq!(s.clock_mhz, Some(2900));
        assert_eq!(amd.vram_total(), Some(8 << 30));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

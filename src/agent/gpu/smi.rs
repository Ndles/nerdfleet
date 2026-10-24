//! NVIDIA GPUs through `nvidia-smi`, for hosts where NVML will not load.
//!
//! Run by absolute path from the standard locations only, with fixed
//! arguments and a time limit: a wedged driver can make it hang.

use crate::model::{Gpu, Power};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PATHS: &[&str] = &["/usr/bin/nvidia-smi", "/usr/local/bin/nvidia-smi"];
const QUERY: &str = "--query-gpu=pci.bus_id,utilization.gpu,memory.used,temperature.gpu,power.draw,clocks.gr";
const TIME_LIMIT: Duration = Duration::from_secs(3);

pub fn find() -> Option<PathBuf> {
    PATHS.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Every GPU's numbers, keyed by sysfs bus ID. Empty on any failure.
pub fn query(path: &PathBuf) -> Vec<(String, Gpu)> {
    let Ok(mut child) = Command::new(path)
        .args([QUERY, "--format=csv,noheader,nounits"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < TIME_LIMIT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut out = String::new();
    // The output is a line per GPU; anything much larger is not nvidia-smi.
    let _ = child.stdout.take().map(|s| s.take(64 * 1024).read_to_string(&mut out));
    parse(&out)
}

fn parse(text: &str) -> Vec<(String, Gpu)> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let [bus, util, mem, temp, power, clock] = f[..] else { return None };
            let num = |s: &str| s.parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0);
            Some((
                super::nvml::sysfs_bus(bus),
                Gpu {
                    power: Power::Awake,
                    util: num(util).map(|v| v.min(100.0) as u8),
                    // MiB.
                    mem_used: num(mem).map(|v| (v as u64) << 20),
                    shared_used: None,
                    temp_c: num(temp),
                    power_w: num(power),
                    clock_mhz: num(clock).map(|v| v as u32),
                    throttled: None,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_rows_and_missing_values() {
        let rows = parse("00000000:01:00.0, 87, 30123, 71, 245.31, 2610\n00000000:03:00.0, 0, 3, 40, [N/A], 210\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "0000:01:00.0");
        assert_eq!(rows[0].1.util, Some(87));
        assert_eq!(rows[0].1.mem_used, Some(30123 << 20));
        assert_eq!(rows[1].1.power_w, None);
        assert!(parse("garbage\n").is_empty());
    }
}

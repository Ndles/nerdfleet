//! CPU, memory, disk and network on Linux, from `/proc` and `/sys`.
//!
//! Each reader keeps the previous counters it needs to turn totals into
//! rates. Parsing is kept apart from reading so tests can feed it text.

use crate::model::{Cpu, Disk, Io, Mem};
use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;
use std::time::Instant;

fn read(path: &str) -> String {
    super::read_file(path).unwrap_or_default()
}

// ===== inventory ============================================================

pub fn hostname() -> String {
    read("/proc/sys/kernel/hostname").trim().to_string()
}

pub fn kernel() -> String {
    read("/proc/sys/kernel/osrelease").trim().to_string()
}

pub fn os_name() -> String {
    let text = std::fs::read_to_string("/etc/os-release").or_else(|_| std::fs::read_to_string("/usr/lib/os-release"));
    parse_os_release(&text.unwrap_or_default()).unwrap_or_else(|| "Linux".to_string())
}

fn parse_os_release(text: &str) -> Option<String> {
    let line = text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))?;
    let name = line.trim().trim_matches('"').trim_matches('\'');
    (!name.is_empty()).then(|| name.to_string())
}

pub fn cpu_model() -> String {
    parse_cpu_model(&read("/proc/cpuinfo")).unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

fn parse_cpu_model(text: &str) -> Option<String> {
    // x86 says "model name"; many ARM kernels only say "Hardware" or
    // nothing useful at all.
    ["model name", "Hardware", "cpu model"].iter().find_map(|key| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == *key && !v.trim().is_empty()).then(|| v.split_whitespace().collect::<Vec<_>>().join(" "))
        })
    })
}

// ===== CPU ==================================================================

#[derive(Clone, Copy, Default)]
struct Ticks {
    busy: u64,
    total: u64,
}

fn parse_ticks(fields: &str) -> Option<Ticks> {
    let v: Vec<u64> = fields.split_whitespace().filter_map(|f| f.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal guest guest_nice;
    // guest time is already counted in user and nice.
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some(Ticks { busy: total.saturating_sub(idle), total })
}

/// The `cpu` total line and each `cpuN` line of `/proc/stat`.
fn parse_stat(text: &str) -> (Option<Ticks>, Vec<Ticks>) {
    let mut total = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else { continue };
        let (id, fields) = rest.split_at(rest.find(' ').unwrap_or(rest.len()));
        match id {
            "" => total = parse_ticks(fields),
            id if id.bytes().all(|b| b.is_ascii_digit()) => cores.extend(parse_ticks(fields)),
            _ => {}
        }
    }
    (total, cores)
}

fn share(now: Ticks, before: Ticks) -> f32 {
    let total = now.total.saturating_sub(before.total);
    if total == 0 {
        return 0.0;
    }
    (now.busy.saturating_sub(before.busy) as f32 / total as f32).clamp(0.0, 1.0)
}

#[derive(Default)]
pub struct CpuReader {
    total: Ticks,
    cores: Vec<Ticks>,
}

impl CpuReader {
    pub fn threads() -> u16 {
        parse_stat(&read("/proc/stat")).1.len().min(u16::MAX as usize) as u16
    }

    pub fn sample(&mut self) -> Cpu {
        let (total, cores) = parse_stat(&read("/proc/stat"));
        let total = total.unwrap_or_default();
        let busy = share(total, self.total);
        let per_core = cores
            .iter()
            .enumerate()
            .map(|(i, &now)| (share(now, self.cores.get(i).copied().unwrap_or_default()) * 100.0).round() as u8)
            .collect();
        self.total = total;
        self.cores = cores;
        Cpu { busy, cores: per_core, load: parse_loadavg(&read("/proc/loadavg")) }
    }
}

fn parse_loadavg(text: &str) -> [f32; 3] {
    let mut v = text.split_whitespace().map(|f| f.parse::<f32>().ok().filter(|x| x.is_finite()).unwrap_or(0.0));
    [v.next().unwrap_or(0.0), v.next().unwrap_or(0.0), v.next().unwrap_or(0.0)]
}

// ===== memory ===============================================================

pub fn mem() -> Mem {
    parse_meminfo(&read("/proc/meminfo"))
}

fn parse_meminfo(text: &str) -> Mem {
    let kb: HashMap<&str, u64> = text
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect();
    let get = |k: &str| kb.get(k).copied().unwrap_or(0) * 1024;
    let swap_total = get("SwapTotal");
    Mem {
        total: get("MemTotal"),
        available: get("MemAvailable"),
        cached: get("Cached") + get("Buffers") + get("SReclaimable"),
        swap_total,
        swap_used: swap_total.saturating_sub(get("SwapFree")),
    }
}

// ===== disks ================================================================

/// Filesystems on local storage. Network filesystems are left out for now:
/// `statvfs` on a dead NFS server blocks, and would stall every sample.
const LOCAL_FS: &[&str] =
    &["ext2", "ext3", "ext4", "xfs", "btrfs", "bcachefs", "f2fs", "zfs", "vfat", "exfat", "ntfs", "ntfs3", "jfs"];

/// The mounts worth showing: local filesystems, one mount per device (a
/// btrfs disk is often mounted a dozen times, once per subvolume), and not
/// the small boot partitions.
pub fn mounts() -> Vec<String> {
    parse_mounts(&read("/proc/self/mounts"))
}

fn parse_mounts(text: &str) -> Vec<String> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let [dev, mount, fs, ..] = f[..] else { continue };
        let mount = unescape_mount(mount);
        if !LOCAL_FS.contains(&fs) || mount.starts_with("/boot") || mount.starts_with("/efi") {
            continue;
        }
        if seen.contains(&dev) {
            continue;
        }
        seen.push(dev);
        out.push(mount);
    }
    // The root first, then the rest by path.
    out.sort_by(|a, b| (a != "/", a).cmp(&(b != "/", b)));
    out
}

/// `/proc/mounts` writes spaces and a few other bytes as octal escapes.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = (b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)))
            .then(|| b[i + 1..i + 4].iter().fold(0u16, |n, c| n * 8 + (c - b'0') as u16))
            .and_then(|n| u8::try_from(n).ok());
        if let Some(byte) = octal {
            out.push(byte);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn disk(mount: &str) -> Disk {
    let Ok(path) = CString::new(mount) else { return Disk::default() };
    // SAFETY: statvfs fills the zeroed struct we pass; the path is a valid
    // NUL-terminated string.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return Disk::default();
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    // Space reserved for root counts as used: it is not ours to fill.
    let avail = st.f_bavail as u64 * frsize;
    Disk { total, used: total.saturating_sub(avail) }
}

// ===== rates ================================================================

/// Turns ever-growing byte counters into bytes per second.
#[derive(Default)]
pub struct Rate {
    last: Option<(Instant, u64, u64)>,
}

impl Rate {
    fn update(&mut self, read: u64, write: u64) -> Io {
        let now = Instant::now();
        let io = match self.last {
            Some((t, r, w)) => {
                let secs = now.duration_since(t).as_secs_f64().max(0.001);
                Io {
                    read: (read.saturating_sub(r) as f64 / secs) as u64,
                    write: (write.saturating_sub(w) as f64 / secs) as u64,
                }
            }
            None => Io::default(),
        };
        self.last = Some((now, read, write));
        io
    }
}

/// Whole disks only, so partitions and the device-mapper and RAID devices
/// layered on top are not counted twice. Virtual devices have no `device`
/// link in sysfs.
fn physical_disks() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/sys/block") else { return Vec::new() };
    dir.filter_map(|e| e.ok())
        .filter(|e| e.path().join("device").exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

#[derive(Default)]
pub struct DiskIo {
    rate: Rate,
    disks: Option<Vec<String>>,
}

impl DiskIo {
    pub fn sample(&mut self) -> Io {
        let disks = self.disks.get_or_insert_with(physical_disks);
        let (r, w) = parse_diskstats(&read("/proc/diskstats"), disks);
        self.rate.update(r, w)
    }
}

fn parse_diskstats(text: &str, disks: &[String]) -> (u64, u64) {
    let mut read = 0u64;
    let mut write = 0u64;
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !disks.iter().any(|d| d == f[2]) {
            continue;
        }
        // Sectors here are always 512 bytes, whatever the device's own size.
        read += f[5].parse::<u64>().unwrap_or(0) * 512;
        write += f[9].parse::<u64>().unwrap_or(0) * 512;
    }
    (read, write)
}

#[derive(Default)]
pub struct NetIo {
    rate: Rate,
}

impl NetIo {
    /// Received bytes go in `read`, sent bytes in `write`.
    pub fn sample(&mut self) -> Io {
        let physical = |name: &str| Path::new("/sys/class/net").join(name).join("device").exists();
        let (rx, tx) = parse_net_dev(&read("/proc/net/dev"), physical);
        self.rate.update(rx, tx)
    }
}

/// Totals over physical interfaces: bridges, veths, VPNs and loopback carry
/// traffic that also crossed (or never left for) a real one.
fn parse_net_dev(text: &str, physical: impl Fn(&str) -> bool) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" || !physical(name) {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if f.len() >= 9 {
            rx += f[0];
            tx += f[8];
        }
    }
    (rx, tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release() {
        assert_eq!(parse_os_release("NAME=x\nPRETTY_NAME=\"Arch Linux\"\n").as_deref(), Some("Arch Linux"));
        assert_eq!(parse_os_release("NAME=x\n"), None);
    }

    #[test]
    fn cpu_model_from_cpuinfo() {
        let text = "processor\t: 0\nmodel name\t: AMD Ryzen  9 9950X 16-Core Processor\n";
        assert_eq!(parse_cpu_model(text).as_deref(), Some("AMD Ryzen 9 9950X 16-Core Processor"));
        assert_eq!(parse_cpu_model("processor : 0\nBogoMIPS : 50\n"), None);
    }

    #[test]
    fn cpu_shares_from_two_stat_readings() {
        let a = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\ncpu1 50 0 50 400 0 0 0 0 0 0\nintr 1\n";
        let b = "cpu  200 0 200 800 0 0 0 0 0 0\ncpu0 150 0 150 400 0 0 0 0 0 0\ncpu1 50 0 50 400 0 0 0 0 0 0\n";
        let (ta, ca) = parse_stat(a);
        let (tb, cb) = parse_stat(b);
        assert_eq!(ca.len(), 2);
        assert!((share(tb.unwrap(), ta.unwrap()) - 1.0).abs() < 1e-6);
        assert!((share(cb[0], ca[0]) - 1.0).abs() < 1e-6);
        assert_eq!(share(cb[1], ca[1]), 0.0);
    }

    #[test]
    fn meminfo() {
        let m = parse_meminfo("MemTotal: 1000 kB\nMemAvailable: 400 kB\nCached: 100 kB\nBuffers: 10 kB\nSwapTotal: 50 kB\nSwapFree: 20 kB\n");
        assert_eq!(m.total, 1000 * 1024);
        assert_eq!(m.available, 400 * 1024);
        assert_eq!(m.cached, 110 * 1024);
        assert_eq!(m.swap_used, 30 * 1024);
    }

    #[test]
    fn mounts_keep_one_local_mount_per_device() {
        let text = "\
/dev/nvme0n1p2 / btrfs rw 0 0
/dev/nvme0n1p2 /home btrfs rw 0 0
/dev/nvme0n1p1 /boot vfat rw 0 0
proc /proc proc rw 0 0
server:/x /mnt/nfs nfs4 rw 0 0
/dev/sda1 /mnt/data\\040disk ext4 rw 0 0
";
        assert_eq!(parse_mounts(text), vec!["/".to_string(), "/mnt/data disk".to_string()]);
    }

    #[test]
    fn diskstats_count_whole_disks_only() {
        let text = "\
 259 0 nvme0n1 10 0 100 0 20 0 200 0 0 0 0
 259 1 nvme0n1p1 5 0 50 0 10 0 100 0 0 0 0
 253 0 dm-0 5 0 50 0 10 0 100 0 0 0 0
";
        assert_eq!(parse_diskstats(text, &["nvme0n1".to_string()]), (100 * 512, 200 * 512));
    }

    #[test]
    fn net_dev_counts_physical_interfaces() {
        let text = "\
Inter-|   Receive |  Transmit
 face |bytes packets errs drop fifo frame compressed multicast|bytes
    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0
  eth0: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0
docker0: 50 1 0 0 0 0 0 0 50 1 0 0 0 0 0 0
";
        assert_eq!(parse_net_dev(text, |n| n == "eth0"), (100, 200));
    }

    #[test]
    fn loadavg() {
        assert_eq!(parse_loadavg("2.00 1.50 0.25 3/900 1234\n"), [2.0, 1.5, 0.25]);
        assert_eq!(parse_loadavg(""), [0.0, 0.0, 0.0]);
    }
}

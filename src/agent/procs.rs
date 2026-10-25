//! The process table on Linux: who runs what, under whom, using how much.
//!
//! One pass over `/proc/<pid>/stat` per sample; command lines are read once
//! per process and cached. Kernel threads are left out. Command lines are
//! world-readable on most systems, so other users' processes (a training
//! job in a root container, say) are seen too, but nothing here reads a
//! process's environment.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    /// The kernel's short name (`comm`), at most 15 bytes.
    pub comm: String,
    /// The command line, program first. Empty if it could not be read.
    /// Shared with the scanner's cache, so a scan copies no strings.
    pub args: Arc<[String]>,
    /// Seconds since the Unix epoch.
    pub started_s: u64,
    /// CPU cores used since the last scan.
    pub cpu: f32,
    pub rss: u64,
}

/// A `stat` line's fields after the parenthesized `comm`, which may itself
/// contain spaces and parentheses.
struct Stat {
    comm: String,
    ppid: u32,
    ticks: u64,
    start_ticks: u64,
    rss_pages: u64,
}

fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let f: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // f[0] is the state; the numbers are `proc_pid_stat(5)` fields 4 on.
    let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    Some(Stat { comm, ppid: n(1)? as u32, ticks: n(11)? + n(12)?, start_ticks: n(19)?, rss_pages: n(21)? })
}

fn sysconf(name: libc::c_int, fallback: u64) -> u64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(name) };
    if v > 0 {
        v as u64
    } else {
        fallback
    }
}

fn boot_time() -> u64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()))
        .unwrap_or(0)
}

fn read_args(pid: u32) -> Arc<[String]> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return Arc::from([]) };
    // Programs can rewrite their command line into one long string; cap it.
    raw[..raw.len().min(16 * 1024)]
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .take(256)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

pub struct Scanner {
    tick: u64,
    page: u64,
    boot: u64,
    /// Per pid: its start (to tell a reused pid apart) and CPU ticks then.
    prev: HashMap<u32, (u64, u64)>,
    args: HashMap<(u32, u64), Arc<[String]>>,
    last: Option<Instant>,
    buf: String,
}

impl Default for Scanner {
    fn default() -> Scanner {
        Scanner {
            tick: sysconf(libc::_SC_CLK_TCK, 100),
            page: sysconf(libc::_SC_PAGESIZE, 4096),
            boot: boot_time(),
            prev: HashMap::new(),
            args: HashMap::new(),
            last: None,
            buf: String::with_capacity(4096),
        }
    }
}

impl Scanner {
    pub fn scan(&mut self) -> Vec<ProcInfo> {
        let now = Instant::now();
        let secs = self.last.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.last = Some(now);
        let mut seen = HashMap::new();
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/proc") else { return out };
        for entry in dir.filter_map(|e| e.ok()) {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            // The open stat file says whose process it is, so one open does
            // for both.
            let Ok(mut file) = std::fs::File::open(entry.path().join("stat")) else { continue };
            self.buf.clear();
            if file.read_to_string(&mut self.buf).is_err() {
                continue;
            }
            let Some(st) = parse_stat(&self.buf) else { continue };
            // Kernel threads: kthreadd itself and everything it starts.
            if pid == 2 || st.ppid == 2 {
                continue;
            }
            let uid = file.metadata().map(|m| m.uid()).unwrap_or(u32::MAX);
            let cpu = match self.prev.get(&pid) {
                Some(&(start, ticks)) if start == st.start_ticks && secs > 0.0 => {
                    (st.ticks.saturating_sub(ticks) as f64 / self.tick as f64 / secs) as f32
                }
                _ => 0.0,
            };
            seen.insert(pid, (st.start_ticks, st.ticks));
            let args = self.args.entry((pid, st.start_ticks)).or_insert_with(|| read_args(pid)).clone();
            out.push(ProcInfo {
                pid,
                ppid: st.ppid,
                uid,
                comm: st.comm,
                args,
                started_s: self.boot + st.start_ticks / self.tick,
                cpu,
                rss: st.rss_pages * self.page,
            });
        }
        self.args.retain(|(pid, start), _| seen.get(pid).is_some_and(|(s, _)| s == start));
        self.prev = seen;
        out
    }
}

/// Listening TCP ports by socket inode, from `/proc/net/tcp` and `tcp6`.
pub fn listening() -> HashMap<u64, u16> {
    let mut out = HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        out.extend(parse_listening(&super::read_file(file).unwrap_or_default()));
    }
    out
}

fn parse_listening(text: &str) -> Vec<(u64, u16)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // sl local_address rem_address st ... inode is the tenth field.
            if f.len() < 10 || f[3] != "0A" {
                return None;
            }
            let port = u16::from_str_radix(f[1].rsplit_once(':')?.1, 16).ok()?;
            Some((f[9].parse().ok()?, port))
        })
        .collect()
}

/// The ports `pid` listens on, given the listening sockets by inode.
pub fn ports_of(pid: u32, listening: &HashMap<u64, u16>) -> Vec<u16> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    let mut ports: Vec<u16> = fds
        .filter_map(|f| f.ok())
        .filter_map(|f| {
            let target = std::fs::read_link(f.path()).ok()?;
            let inode = target.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok()?;
            listening.get(&inode).copied()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines_with_odd_names() {
        let line = "312105 (llama (srv) x) S 6291 312105 6291 0 -1 4194560 1 0 0 0 4900 120 0 0 20 0 41 0 98765 99 7219052 0 0";
        let st = parse_stat(line).unwrap();
        assert_eq!(st.comm, "llama (srv) x");
        assert_eq!(st.ppid, 6291);
        assert_eq!(st.ticks, 5020);
        assert_eq!(st.start_ticks, 98765);
        assert_eq!(st.rss_pages, 7219052);
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn listening_sockets() {
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:16B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0
   1: 0100007F:16B2 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0
";
        assert_eq!(parse_listening(text), vec![(4242, 5810)]);
    }

    #[test]
    fn this_process_is_seen_with_its_command_line() {
        let mut s = Scanner::default();
        s.scan();
        let me = std::process::id();
        let procs = s.scan();
        let p = procs.iter().find(|p| p.pid == me).expect("own process");
        assert!(!p.args.is_empty() && p.rss > 0 && p.started_s > 0);
        assert!(procs.iter().all(|p| p.ppid != 2), "kernel threads are left out");
    }
}

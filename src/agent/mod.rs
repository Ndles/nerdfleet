//! The agent: runs on the host being watched, as the user who connected,
//! and writes measurements to stdout until stdin closes.
//!
//! It only reads: `/proc`, `/sys` and the GPU libraries. It never listens
//! on a socket and never reads another process's environment.

#[cfg(target_os = "linux")]
mod gpu;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod procs;
#[cfg(target_os = "linux")]
mod workload;

use crate::model::{AgentMsg, Frame, Hello, ViewerMsg, MAX_INTERVAL_MS, MIN_INTERVAL_MS};
use crate::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often workloads are worked out. Slightly under two seconds, so that
/// with one-second samples it is every second sample.
#[cfg(target_os = "linux")]
const WORK_EVERY: Duration = Duration::from_millis(1900);

/// Reads a small file in one go. `/proc` and `/sys` files report a size of
/// zero, so `fs::read_to_string` probes them with a series of small reads;
/// starting with room for a page makes it one read and an end-of-file.
/// The agent reads thousands of these a second, so it matters.
#[cfg(target_os = "linux")]
fn read_file(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    use std::io::Read;
    let mut s = String::with_capacity(4096);
    std::fs::File::open(path)?.read_to_string(&mut s)?;
    Ok(s)
}

/// The build target, as agents report it and agent files are named:
/// `x86_64-linux`, `aarch64-macos`.
pub fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn send<W: Write>(out: &mut W, msg: AgentMsg) -> std::io::Result<()> {
    write_frame(out, &Frame { route: Vec::new(), msg })
}

/// Reads the viewer's messages on a thread of its own. The channel closes
/// when stdin does, which is the signal to stop.
fn listen() -> mpsc::Receiver<ViewerMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(Some(frame)) = read_frame::<_, Frame<ViewerMsg>>(&mut stdin) {
            if frame.msg == ViewerMsg::Bye || tx.send(frame.msg).is_err() {
                break;
            }
        }
    });
    rx
}

pub fn run_stdio() -> anyhow::Result<()> {
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let hello = Hello { protocol: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION").to_string(), target: target() };
    send(&mut out, AgentMsg::Hello(hello))?;
    let inbox = listen();
    run(&mut out, &inbox)
}

#[cfg(not(target_os = "linux"))]
fn run<W: Write>(out: &mut W, inbox: &mpsc::Receiver<ViewerMsg>) -> anyhow::Result<()> {
    send(out, AgentMsg::Error(format!("the agent does not support {} yet", std::env::consts::OS)))?;
    while inbox.recv().is_ok() {}
    Ok(())
}

#[cfg(target_os = "linux")]
fn run<W: Write>(out: &mut W, inbox: &mpsc::Receiver<ViewerMsg>) -> anyhow::Result<()> {
    use crate::model::{Inventory, Sample, Workloads};

    let (mut gpus, mut problems) = gpu::Gpus::discover();
    let mounts = linux::mounts();
    let inventory = |gpus: &gpu::Gpus| Inventory {
        hostname: linux::hostname(),
        os: linux::os_name(),
        kernel: linux::kernel(),
        cpu_model: linux::cpu_model(),
        threads: linux::CpuReader::threads(),
        mem_total: linux::mem().total,
        mounts: mounts.clone(),
        gpus: gpus.info(),
    };
    send(out, AgentMsg::Inventory(inventory(&gpus)))?;

    let mut cpu = linux::CpuReader::default();
    let mut disk_io = linux::DiskIo::default();
    let mut net = linux::NetIo::default();
    // Prime the counters, so the first sample has rates to report.
    cpu.sample();
    disk_io.sample();
    net.sample();
    let mut scanner = procs::Scanner::default();
    let mut tracker = workload::Tracker::default();
    scanner.scan();
    let mut interval = Duration::from_millis(1000);
    let mut detail = false;
    let mut worked: Option<Instant> = None;
    let mut reported: Vec<String> = Vec::new();

    loop {
        match inbox.recv_timeout(interval) {
            Ok(ViewerMsg::Subscribe { interval_ms, detail: d }) => {
                interval = Duration::from_millis(interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS) as u64);
                // Asked for detail: send it with the next sample.
                if d && !detail {
                    worked = None;
                }
                detail = d;
                continue;
            }
            Ok(ViewerMsg::Bye) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let mut changed = false;
        let gpu_samples = gpus.sample(&mut problems, &mut changed);
        if changed {
            send(out, AgentMsg::Inventory(inventory(&gpus)))?;
        }
        // Each problem is reported once, not every second.
        for p in problems.drain(..) {
            if !reported.contains(&p) {
                send(out, AgentMsg::Error(p.clone()))?;
                reported.push(p);
            }
        }
        let time_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let sample = Sample {
            time_ms,
            cpu: cpu.sample(),
            mem: linux::mem(),
            disks: mounts.iter().map(|m| linux::disk(m)).collect(),
            disk_io: disk_io.sample(),
            net: net.sample(),
            gpus: gpu_samples,
        };
        let gpu_util: Vec<Option<u8>> = sample.gpus.iter().map(|g| g.util).collect();
        send(out, AgentMsg::Sample(sample))?;

        // The kernel takes about 10 µs to describe each process, so the
        // process table is read every `WORK_EVERY`, not every sample.
        if worked.is_some_and(|t: Instant| t.elapsed() < WORK_EVERY) {
            continue;
        }
        worked = Some(Instant::now());
        let procs = scanner.scan();
        let gpu_procs = gpus.processes();
        // Listing listening sockets walks every socket on the host; only
        // do it when some job needs its ports.
        let listening = std::cell::OnceCell::new();
        let ports = |pid: u32| procs::ports_of(pid, listening.get_or_init(procs::listening));
        let input = workload::Input {
            procs: &procs,
            gpus: &gpu_procs,
            gpu_util: &gpu_util,
            ports: &ports,
            detail,
            me: std::process::id(),
        };
        let (list, top) = tracker.update(&input);
        send(out, AgentMsg::Workloads(Workloads { list, top }))?;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod bench {
    use std::time::Instant;

    /// Where a sample's time goes. `cargo test --release bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sample_costs() {
        let n = 50;
        let time = |name: &str, f: &mut dyn FnMut()| {
            f();
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            eprintln!("{name:<14} {:>8.0} µs", t.elapsed().as_micros() as f64 / n as f64);
        };
        let mut cpu = super::linux::CpuReader::default();
        use std::hint::black_box;
        time("cpu", &mut || drop(black_box(cpu.sample())));
        time("mem", &mut || _ = black_box(super::linux::mem()));
        let mounts = super::linux::mounts();
        time("disks", &mut || _ = black_box(mounts.iter().map(|m| super::linux::disk(m)).collect::<Vec<_>>()));
        let mut dio = super::linux::DiskIo::default();
        time("disk io", &mut || _ = black_box(dio.sample()));
        let mut net = super::linux::NetIo::default();
        time("net", &mut || _ = black_box(net.sample()));
        let (mut gpus, _) = super::gpu::Gpus::discover();
        time("gpus", &mut || drop(black_box(gpus.sample(&mut Vec::new(), &mut false))));
        time("gpu procs", &mut || drop(black_box(gpus.processes())));
        let mut scanner = super::procs::Scanner::default();
        time("procs", &mut || drop(black_box(scanner.scan())));
        time("listening", &mut || drop(black_box(super::procs::listening())));
        let procs = scanner.scan();
        let gp = gpus.processes();
        let listening = super::procs::listening();
        let ports = |pid: u32| super::procs::ports_of(pid, &listening);
        let mut tracker = super::workload::Tracker::default();
        time("workloads", &mut || {
            let input = super::workload::Input {
                procs: &procs,
                gpus: &gp,
                gpu_util: &[],
                ports: &ports,
                detail: false,
                me: std::process::id(),
            };
            drop(tracker.update(&input));
        });
        eprintln!("{} processes", procs.len());
    }
}

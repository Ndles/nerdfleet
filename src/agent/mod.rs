//! The agent: runs on the host being watched, as the user who connected,
//! and writes measurements to stdout until stdin closes.
//!
//! It only reads: `/proc`, `/sys` and the GPU libraries. It never listens
//! on a socket and never reads another process's environment.

#[cfg(target_os = "linux")]
mod gpu;
#[cfg(target_os = "linux")]
mod linux;

use crate::model::{AgentMsg, Frame, Hello, ViewerMsg, MAX_INTERVAL_MS, MIN_INTERVAL_MS};
use crate::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    use crate::model::{Inventory, Sample};

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
    let mut interval = Duration::from_millis(1000);
    let mut reported: Vec<String> = Vec::new();

    loop {
        match inbox.recv_timeout(interval) {
            Ok(ViewerMsg::Subscribe { interval_ms }) => {
                interval = Duration::from_millis(interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS) as u64);
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
        send(out, AgentMsg::Sample(sample))?;
    }
}

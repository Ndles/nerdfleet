//! The agent: runs on the host being watched, as the user who connected.
//!
//! In watch mode (`agent --stdio`) it writes measurements to stdout until
//! stdin closes, or, when a resident agent runs on the host, connects the
//! viewer to that one instead. In resident mode (`agent --resident`) it
//! keeps sampling as a user service; see `resident`.
//!
//! It only reads: `/proc`, `/sys` and the GPU libraries. It never listens
//! on a network socket and never reads another process's environment.

#[cfg(target_os = "linux")]
mod alerts;
#[cfg(target_os = "linux")]
mod core;
#[cfg(target_os = "linux")]
mod gpu;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod procs;
#[cfg(target_os = "linux")]
pub mod resident;
#[cfg(target_os = "linux")]
mod workload;

use crate::model::{AgentMsg, Frame, Hello, ViewerMsg};
use crate::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use std::io::Write;
use std::sync::mpsc;

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

fn hello(resident: bool) -> Hello {
    Hello { protocol: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION").to_string(), target: target(), resident }
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

#[cfg(not(target_os = "linux"))]
pub fn run_stdio() -> anyhow::Result<()> {
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    send(&mut out, AgentMsg::Hello(hello(false)))?;
    send(&mut out, AgentMsg::Error(format!("the agent does not support {} yet", std::env::consts::OS)))?;
    let inbox = listen();
    while inbox.recv().is_ok() {}
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run_resident() -> anyhow::Result<()> {
    anyhow::bail!("resident mode does not support {} yet", std::env::consts::OS)
}

#[cfg(target_os = "linux")]
pub fn run_resident() -> anyhow::Result<()> {
    resident::run()
}

#[cfg(target_os = "linux")]
pub fn run_stdio() -> anyhow::Result<()> {
    use crate::model::{MAX_INTERVAL_MS, MIN_INTERVAL_MS};
    use std::time::Duration;

    // A resident agent has the history and the alerts: hand the viewer to
    // it. One of another protocol (an older build) is left alone.
    let mismatch = match resident::attach()? {
        resident::Attach::Done => return Ok(()),
        resident::Attach::None => None,
        resident::Attach::Mismatch(h) => Some(h),
    };

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    send(&mut out, AgentMsg::Hello(hello(false)))?;
    if let Some(h) = mismatch {
        send(
            &mut out,
            AgentMsg::Error(format!(
                "the resident agent here is an older build (version {}, protocol {}); \
                 run `nerdfleet resident enable` for this host to update it",
                h.version, h.protocol
            )),
        )?;
    }
    let inbox = listen();
    let mut core = core::Core::new(Default::default(), Vec::new());
    send(&mut out, AgentMsg::Inventory(core.inventory().clone()))?;
    // No history to send: this agent has just started, and the viewer
    // keeps what it collected before a reconnect. A resident agent sends
    // its own.
    let mut interval = Duration::from_millis(1000);
    let mut detail = false;
    loop {
        match inbox.recv_timeout(interval) {
            Ok(ViewerMsg::Subscribe { interval_ms, detail: d }) => {
                interval = Duration::from_millis(interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS) as u64);
                // Asked for detail: send it with the next sample.
                if d && !detail {
                    core.work_now();
                }
                detail = d;
                continue;
            }
            Ok(ViewerMsg::Bye) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        for msg in core.tick(detail) {
            send(&mut out, msg)?;
        }
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

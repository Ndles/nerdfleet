//! A pretend agent for the demo recording (see docs/demo/README.md): speaks
//! the agent's side of the protocol with made-up numbers for one made-up
//! host, so the recording shows a whole fleet and nobody's real machines.

use nerdfleet::history::{self, History};
use nerdfleet::model::*;
use nerdfleet::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GIB: u64 = 1 << 30;
const HOUR: u64 = 3_600_000;

/// Set once from the host's name, so no two hosts move alike.
static HOST_SEED: AtomicU32 = AtomicU32::new(0);

/// A number that wanders smoothly around `mid`, the same for the same time.
fn wander(t_ms: u64, seed: u32, mid: f32, by: f32) -> f32 {
    let t = t_ms as f64 / 1000.0;
    let s = (seed + HOST_SEED.load(Ordering::Relaxed)) as f64 * 1.37;
    let tau = std::f64::consts::TAU;
    let w = (t * tau / 1800.0 + s).sin() * 0.3
        + (t * tau / 47.0 + s).sin() * 0.3
        + (t * tau / 13.0 + s * 2.0).sin() * 0.25
        + (t * tau / 3.1 + s * 3.0).sin() * 0.15;
    mid + by * w as f32
}

struct Job {
    kind: Kind,
    label: &'static str,
    model: &'static str,
    port: Option<u16>,
    procs: u16,
    /// How long it has been running when the recording starts, in hours.
    hours: u64,
    cores: f32,
    rss: u64,
    /// GPU memory on each GPU it uses, and how busy it keeps them.
    gpus: &'static [u16],
    gpu_mem: u64,
    busy: f32,
    cmdline: &'static str,
}

struct Machine {
    os: &'static str,
    kernel: &'static str,
    cpu: &'static str,
    threads: u16,
    mem: u64,
    gpu_name: &'static str,
    vendor: Vendor,
    gpu_count: usize,
    vram: u64,
    power_limit: f32,
    unified: bool,
    disk: (u64, u64),
    jobs: Vec<Job>,
}

fn machine(host: &str) -> Machine {
    match host {
        "gpu-box" => Machine {
            os: "Ubuntu 24.04 LTS",
            kernel: "6.8.0-generic",
            cpu: "AMD Ryzen Threadripper PRO 7975WX",
            threads: 64,
            mem: 512 * GIB,
            gpu_name: "NVIDIA GeForce RTX 4090",
            vendor: Vendor::Nvidia,
            gpu_count: 4,
            vram: 24 * GIB,
            power_limit: 450.0,
            unified: false,
            disk: (3600, 2100),
            jobs: vec![Job {
                kind: Kind::Training,
                label: "torchrun train.py",
                model: "",
                port: None,
                procs: 21,
                hours: 5,
                cores: 26.0,
                rss: 184 * GIB,
                gpus: &[0, 1, 2, 3],
                gpu_mem: 21 * GIB,
                busy: 93.0,
                cmdline: "torchrun --nproc_per_node=4 train.py --config configs/vit-large.yaml --epochs 90",
            }],
        },
        "llm-server" => Machine {
            os: "Debian GNU/Linux 12",
            kernel: "6.1.0-amd64",
            cpu: "AMD EPYC 9354",
            threads: 64,
            mem: 384 * GIB,
            gpu_name: "NVIDIA A100 80GB",
            vendor: Vendor::Nvidia,
            gpu_count: 2,
            vram: 80 * GIB,
            power_limit: 300.0,
            unified: false,
            disk: (1800, 1120),
            jobs: vec![Job {
                kind: Kind::Inference,
                label: "vllm serve",
                model: "Llama-3.3-70B-Instruct",
                port: Some(8000),
                procs: 6,
                hours: 96,
                cores: 5.5,
                rss: 38 * GIB,
                gpus: &[0, 1],
                gpu_mem: 71 * GIB,
                busy: 58.0,
                cmdline: "vllm serve meta-llama/Llama-3.3-70B-Instruct --port 8000 --tensor-parallel-size 2",
            }],
        },
        "workstation" => Machine {
            os: "Fedora Linux 42",
            kernel: "6.14.0-fc42",
            cpu: "Intel Core Ultra 9 285K",
            threads: 24,
            mem: 128 * GIB,
            gpu_name: "NVIDIA GeForce RTX 5080",
            vendor: Vendor::Nvidia,
            gpu_count: 1,
            vram: 16 * GIB,
            power_limit: 360.0,
            unified: false,
            disk: (1800, 760),
            jobs: vec![Job {
                kind: Kind::Training,
                label: "python finetune.py",
                model: "",
                port: None,
                procs: 5,
                hours: 3,
                cores: 4.0,
                rss: 22 * GIB,
                gpus: &[0],
                gpu_mem: 13 * GIB,
                busy: 88.0,
                cmdline: "python finetune.py --base-model bert-base --lr 3e-5 --batch-size 32",
            }],
        },
        "apu-mini" => Machine {
            os: "Ubuntu 25.04",
            kernel: "6.14.0-generic",
            cpu: "AMD Ryzen 9 8945HS",
            threads: 16,
            mem: 96 * GIB,
            gpu_name: "AMD Radeon 780M",
            vendor: Vendor::Amd,
            gpu_count: 1,
            vram: GIB / 2,
            power_limit: 54.0,
            unified: true,
            disk: (1800, 900),
            jobs: vec![Job {
                kind: Kind::Inference,
                label: "llama-server",
                model: "Llama-3.3-8B-Q4_K_M",
                port: Some(8080),
                procs: 1,
                hours: 28,
                cores: 2.5,
                rss: 3 * GIB,
                gpus: &[0],
                gpu_mem: 9 * GIB,
                busy: 34.0,
                cmdline: "llama-server -m models/Llama-3.3-8B-Q4_K_M.gguf --port 8080 -ngl 99",
            }],
        },
        _ => Machine {
            os: "Rocky Linux 9.5",
            kernel: "5.14.0-el9",
            cpu: "Intel Xeon Gold 6338",
            threads: 64,
            mem: 256 * GIB,
            gpu_name: "",
            vendor: Vendor::Other,
            gpu_count: 0,
            vram: 0,
            power_limit: 0.0,
            unified: false,
            disk: (7200, 6100),
            jobs: vec![Job {
                kind: Kind::Cpu,
                label: "python preprocess.py",
                model: "",
                port: None,
                procs: 41,
                hours: 2,
                cores: 40.0,
                rss: 96 * GIB,
                gpus: &[],
                gpu_mem: 0,
                busy: 0.0,
                cmdline: "python preprocess.py --workers 40 --input shards/ --output tokens/",
            }],
        },
    }
}

fn inventory(host: &str, m: &Machine) -> Inventory {
    Inventory {
        hostname: host.to_string(),
        os: m.os.to_string(),
        kernel: m.kernel.to_string(),
        cpu_model: m.cpu.to_string(),
        threads: m.threads,
        mem_total: m.mem,
        mounts: vec!["/".to_string()],
        gpus: (0..m.gpu_count)
            .map(|i| GpuInfo {
                vendor: m.vendor,
                name: m.gpu_name.to_string(),
                bus: format!("0000:{:02x}:00.0", i + 1),
                mem_total: Some(m.vram),
                shared_total: m.unified.then_some(m.mem / 2),
                unified: m.unified,
                power_limit_w: Some(m.power_limit),
                metrics: true,
            })
            .collect(),
    }
}

/// How busy job `j` keeps its GPUs at `t`: nothing before it started, or
/// once it stalled.
fn job_busy(j: &Job, t: u64, now: u64, stalled: bool) -> f32 {
    let started = now.saturating_sub(j.hours * HOUR);
    if t < started || stalled {
        0.0
    } else {
        j.busy
    }
}

fn sample(m: &Machine, t: u64, now: u64, stalled: bool) -> Sample {
    let job = &m.jobs[0];
    let running = t >= now.saturating_sub(job.hours * HOUR);
    let cores = if running { job.cores } else { 0.4 };
    let busy = (wander(t, 1, cores / m.threads as f32, cores / m.threads as f32 * 0.15)).clamp(0.005, 1.0);
    let hot = (busy * m.threads as f32).round() as usize;
    let used = if running { job.rss + 12 * GIB } else { 9 * GIB };
    let gpus = (0..m.gpu_count)
        .map(|i| {
            let on = job.gpus.contains(&(i as u16));
            let b = if on { job_busy(job, t, now, stalled) } else { 0.0 };
            let util = if b > 0.0 { wander(t, i as u32 + 2, b, 7.0).clamp(0.0, 100.0) } else { 0.0 };
            let vram_used = if on && running { job.gpu_mem } else { GIB / 4 };
            let (mem_used, shared_used) = if m.unified { (GIB / 4, Some(vram_used)) } else { (vram_used, None) };
            Gpu {
                power: Power::Awake,
                util: Some(util as u8),
                mem_used: Some(mem_used),
                shared_used,
                temp_c: Some(38.0 + util * 0.42),
                power_w: Some(m.power_limit * (0.08 + util / 100.0 * 0.85)),
                clock_mhz: Some(if util > 1.0 { 2100 } else { 300 }),
                throttled: Some(false),
            }
        })
        .collect();
    Sample {
        time_ms: t,
        cpu: Cpu {
            busy,
            cores: (0..m.threads as usize)
                .map(
                    |c| {
                        if c < hot {
                            wander(t, c as u32, 85.0, 12.0) as u8
                        } else {
                            wander(t, c as u32, 3.0, 2.0) as u8
                        }
                    },
                )
                .collect(),
            load: [busy * m.threads as f32, busy * m.threads as f32 * 0.97, busy * m.threads as f32 * 0.9],
        },
        mem: Mem {
            total: m.mem,
            available: m.mem - used.min(m.mem),
            cached: m.mem / 8,
            swap_total: 8 * GIB,
            swap_used: 0,
        },
        disks: vec![Disk { total: m.disk.0 * GIB, used: m.disk.1 * GIB }],
        disk_io: Io { read: wander(t, 7, 120e6, 90e6) as u64, write: wander(t, 8, 60e6, 40e6) as u64 },
        net: Io { read: wander(t, 9, 30e6, 20e6) as u64, write: wander(t, 10, 45e6, 30e6) as u64 },
        gpus,
    }
}

fn workloads(m: &Machine, t: u64, now: u64, stalled_for: Option<u64>, detail: bool) -> Workloads {
    let list: Vec<Workload> = m
        .jobs
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let stalled = stalled_for.is_some();
            let busy = job_busy(j, t, now, stalled);
            Workload {
                id: 41_000 + i as u32,
                kind: j.kind,
                label: j.label.to_string(),
                model: j.model.to_string(),
                ports: j.port.into_iter().collect(),
                procs: j.procs,
                started_s: (now - j.hours * HOUR) / 1000,
                cpu: wander(t, 11, if stalled { 0.3 } else { j.cores }, j.cores * 0.1).max(0.1),
                rss: j.rss,
                gpus: j
                    .gpus
                    .iter()
                    .map(|&g| WorkloadGpu {
                        gpu: g,
                        mem: j.gpu_mem,
                        busy: Some(
                            wander(t, g as u32 + 2, busy, if busy > 0.0 { 7.0 } else { 0.0 }).clamp(0.0, 100.0) as u8
                        ),
                    })
                    .collect(),
                gpu_idle_s: stalled_for.unwrap_or(0) as u32,
                cmdline: if detail { j.cmdline.to_string() } else { String::new() },
            }
        })
        .collect();
    let top = if detail {
        m.jobs
            .iter()
            .flat_map(|j| {
                let n = j.gpus.len().max(4) as u32;
                (0..n).map(move |p| Proc {
                    pid: 41_001 + p,
                    name: j.label.split(' ').next().unwrap_or("python").to_string(),
                    cpu: j.cores / n as f32,
                    rss: j.rss / n as u64,
                    gpu_mem: if (p as usize) < j.gpus.len() { j.gpu_mem } else { 0 },
                    job: 41_000,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Workloads { list, top }
}

fn past(m: &Machine, now: u64) -> History {
    let mut h = History::default();
    let quarter = 15 * 60 * 1000;
    let mut t = now - 24 * HOUR;
    while t < now - quarter {
        h.push(t, &history::values(&sample(m, t, now, false)));
        t += 10_000;
    }
    while t < now {
        h.push(t, &history::values(&sample(m, t, now, false)));
        t += 1000;
    }
    h
}

fn main() -> std::io::Result<()> {
    let host = std::env::args().nth(1).unwrap_or_default();
    // The workstation's run stalls this many seconds into the recording.
    let stall_after: Option<u64> = std::env::var("DEMO_STALL_S").ok().and_then(|s| s.parse().ok());
    HOST_SEED.store(host.bytes().map(u32::from).sum(), Ordering::Relaxed);
    let m = machine(&host);
    let now = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let start = now();

    let detail = Arc::new(AtomicBool::new(false));
    let interval = Arc::new(AtomicU32::new(1000));
    let (d, i) = (detail.clone(), interval.clone());
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(Some(f)) = read_frame::<_, Frame<ViewerMsg>>(&mut stdin) {
            match f.msg {
                ViewerMsg::Subscribe { interval_ms, detail } => {
                    i.store(interval_ms, Ordering::SeqCst);
                    d.store(detail, Ordering::SeqCst);
                }
                ViewerMsg::Bye => break,
            }
        }
        std::process::exit(0);
    });

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut send = |msg: AgentMsg| write_frame(&mut out, &Frame { route: Vec::new(), msg });
    let hello = Hello {
        protocol: PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: nerdfleet::agent::target(),
        resident: true,
    };
    send(AgentMsg::Hello(hello))?;
    send(AgentMsg::Inventory(inventory(&host, &m)))?;
    send(AgentMsg::History(past(&m, start)))?;
    let mut alerted = false;
    loop {
        let t = now();
        // Reported as idle for longer than the stall threshold, as a real
        // agent would by the time it says so.
        let stalled_for = stall_after
            .filter(|&s| host == "workstation" && t >= start + s * 1000)
            .map(|s| 660 + (t - start - s * 1000) / 1000);
        send(AgentMsg::Sample(sample(&m, t, start, stalled_for.is_some())))?;
        send(AgentMsg::Workloads(workloads(&m, t, start, stalled_for, detail.load(Ordering::SeqCst))))?;
        if stalled_for.is_some() && !alerted {
            alerted = true;
            send(AgentMsg::Alerts(vec![Alert {
                id: t * 1000,
                time_ms: t,
                kind: AlertKind::TrainingStalled,
                subject: m.jobs[0].label.to_string(),
                message: format!(
                    "{}: GPUs idle for 11m while its {} processes run; possibly stalled",
                    m.jobs[0].label, m.jobs[0].procs
                ),
            }]))?;
        }
        std::thread::sleep(Duration::from_millis(interval.load(Ordering::SeqCst) as u64));
    }
}

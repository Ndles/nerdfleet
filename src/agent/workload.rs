//! Turning a process table into workloads: what the machine is doing, in
//! the terms of the people who use it.
//!
//! Processes are grouped into jobs under their top process: the highest
//! ancestor that is not a shell, a terminal, a multiplexer, a session
//! manager or another user's. A job is then
//!
//! - **inference** if one of its programs is a known model server;
//! - **training** if it computes on a GPU otherwise;
//! - **cpu** if it has kept `CPU_CORES` busy for `CPU_HOLD`.
//!
//! Only real compute counts as GPU use (see `ProcGpu::compute`), and known
//! desktop programs (compositors, browsers, launchers) never become
//! workloads, however much they draw: they hold GPUs open all day.
//!
//! Listening on a port does not make a GPU job a server: training listens
//! too (PyTorch's rendezvous store, NCCL's bootstrap, W&B's service,
//! Jupyter). Servers nerdfleet does not know are named in the config.

use super::gpu::ProcGpu;
use super::procs::ProcInfo;
use crate::model::{Kind, Proc, Workload, WorkloadGpu};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A job using this many cores for `CPU_HOLD` is a CPU workload...
const CPU_CORES: f32 = 2.0;
const CPU_HOLD: Duration = Duration::from_secs(30);
/// ...until it drops below this.
const CPU_KEEP: f32 = 1.0;
/// A GPU below this share counts as idle for the stall clock.
const GPU_IDLE: u8 = 5;
/// Processes listed in detail mode.
const TOP: usize = 8;

/// Programs that run other programs for the user. A job's top process is
/// the one just below them.
const BOUNDARY: &[&str] = &[
    "systemd",
    "init",
    "sshd",
    "sshd-session",
    "login",
    "sudo",
    "su",
    "doas",
    "cron",
    "crond",
    "bash",
    "zsh",
    "fish",
    "sh",
    "dash",
    "ksh",
    "tcsh",
    "csh",
    "nu",
    "xonsh",
    "tmux",
    "tmux: server",
    "screen",
    "SCREEN",
    "nerdmux",
    "zellij",
    "gnome-terminal-",
    "konsole",
    "kitty",
    "alacritty",
    "foot",
    "ghostty",
    "wezterm-gui",
    "xterm",
    "claude",
    "code",
    "containerd-shim",
    "containerd-shim-runc-v2",
    "conmon",
    "tini",
    "dumb-init",
    "docker-init",
    "uwsm",
    "hyprland",
    "Hyprland",
];

/// Desktop programs: they draw on GPUs, and are never workloads.
const DESKTOP: &[&str] = &[
    "Xorg",
    "Xwayland",
    "gnome-shell",
    "kwin_wayland",
    "kwin_x11",
    "mutter",
    "Hyprland",
    "sway",
    "river",
    "weston",
    "gamescope",
    "plasmashell",
    "chrome",
    "chromium",
    "firefox",
    "firefox-bin",
    "brave",
    "electron",
    "walker",
    "swayosd-server",
    "waybar",
    "mako",
    "hyprpaper",
    "swaybg",
    "mpv",
    "vlc",
    "obs",
    "gjs",
    "steam",
    "Discord",
    "slack",
    "code",
    "kitty",
    "alacritty",
    "foot",
    "ghostty",
    "wezterm-gui",
];

/// Model servers, by program name.
const SERVERS: &[&str] = &[
    "llama-server",
    "llama-swap",
    "ollama",
    "vllm",
    "text-generation-launcher",
    "text-generation-server",
    "lms",
    "llamafile",
    "koboldcpp",
    "tabby",
    "localai",
    "local-ai",
];

/// Model servers started as Python modules or subcommands.
const SERVER_MODULES: &[&str] = &["vllm", "sglang", "mlx_lm.server", "text_generation_server", "tgi", "lmdeploy"];

/// Programs whose first `.py` argument (or `-m` module) names the job.
const LAUNCHERS: &[&str] = &["torchrun", "accelerate", "deepspeed", "uv", "uvx", "poetry", "pipenv", "conda", "pixi"];

fn basename(a: &str) -> &str {
    a.rsplit('/').next().unwrap_or(a)
}

/// The program's own name: the basename of `args[0]`, else `comm`. Tools a
/// virtualenv installs run as `python /venv/bin/torchrun ...`; for those it
/// is the tool, not the interpreter.
fn program(p: &ProcInfo) -> String {
    // Some programs rewrite their command line into one string (Chrome,
    // anything using setproctitle): the program is its first word.
    let first = p.args.first().map(|a| a.split(' ').next().unwrap_or(a));
    let Some(first) = first.map(basename).filter(|n| !n.is_empty()) else { return p.comm.clone() };
    if is_python(first) {
        if let Some(tool) = p.args.get(1).filter(|a| a.contains('/') && !a.ends_with(".py") && !a.starts_with('-')) {
            return basename(tool).to_string();
        }
    }
    first.to_string()
}

fn is_python(prog: &str) -> bool {
    prog == "python" || prog.starts_with("python3") || prog == "python2"
}

fn listed(list: &[&str], p: &ProcInfo) -> bool {
    let prog = program(p);
    list.contains(&prog.as_str()) || list.contains(&p.comm.as_str())
}

/// A model server by name, or a Python one by module (`python -m vllm...`,
/// `vllm serve`).
fn is_server(p: &ProcInfo) -> bool {
    if listed(SERVERS, p) {
        return true;
    }
    let prog = program(p);
    if is_python(&prog) || LAUNCHERS.contains(&prog.as_str()) {
        return p.args.iter().skip(1).any(|a| SERVER_MODULES.iter().any(|m| a == m || a.starts_with(&format!("{m}."))));
    }
    false
}

/// Programs that run a command with a tweak (timing, priority, CPU pinning,
/// settings), and the options of theirs that take a value.
const WRAPPERS: &[(&str, &[&str])] = &[
    ("time", &["-f", "--format", "-o", "--output"]),
    ("nice", &["-n", "--adjustment"]),
    ("ionice", &["-c", "-n", "--class", "--classdata"]),
    ("taskset", &[]),
    ("chrt", &[]),
    (
        "numactl",
        &["-C", "-N", "-m", "-p", "-i", "--physcpubind", "--cpunodebind", "--membind", "--preferred", "--interleave"],
    ),
    ("timeout", &["-s", "-k", "--signal", "--kill-after"]),
    ("env", &["-u", "--unset", "-C", "--chdir"]),
    ("stdbuf", &[]),
    ("nohup", &[]),
];

/// `args` without leading wrappers: `time -v nice -n 5 python x.py` is
/// `python x.py`. Wrappers whose first word is a value of theirs (the CPU
/// mask of `taskset`, the duration of `timeout`, the priority of `chrt`)
/// lose that too, as do `NAME=VALUE` settings for `env`.
fn unwrapped(args: &[String]) -> &[String] {
    let mut rest = args;
    'wrappers: while let Some((name, takes)) =
        rest.first().and_then(|a| WRAPPERS.iter().find(|(w, _)| *w == basename(a)))
    {
        let mut i = 1;
        let positional = matches!(*name, "taskset" | "chrt" | "timeout");
        let mut skipped_positional = false;
        while let Some(a) = rest.get(i) {
            if a.starts_with('-') {
                i += if takes.contains(&a.as_str()) { 2 } else { 1 };
            } else if *name == "env" && a.contains('=') {
                i += 1;
            } else if positional && !skipped_positional {
                skipped_positional = true;
                i += 1;
            } else {
                rest = &rest[i..];
                continue 'wrappers;
            }
        }
        // A wrapper with nothing after it: keep it as it is.
        break;
    }
    rest
}

/// `torchrun train.py`, `python -m sft.train`, `uv train.py`: the program
/// and the script or module it runs, without paths or wrappers.
pub fn label(p: &ProcInfo) -> String {
    let inner = unwrapped(&p.args);
    if inner.len() != p.args.len() {
        let inner = ProcInfo { args: inner.into(), ..p.clone() };
        return label(&inner);
    }
    let prog = program(p);
    let mut prog_label = if is_python(&prog) { "python".to_string() } else { prog.clone() };
    if !(is_python(&prog) || LAUNCHERS.contains(&prog.as_str())) {
        return prog_label;
    }
    let mut args = p.args.iter().skip(1);
    while let Some(a) = args.next() {
        if a == "-m" {
            if let Some(m) = args.next() {
                prog_label.push_str(&format!(" -m {m}"));
                return prog_label;
            }
        } else if a.ends_with(".py") {
            prog_label.push(' ');
            prog_label.push_str(a.rsplit('/').next().unwrap_or(a));
            return prog_label;
        }
    }
    prog_label
}

/// The model a server was started with, from its command line: a file's
/// stem without shard numbers (`Qwen-Q4_K_M` from
/// `/models/Qwen-Q4_K_M-00001-of-00003.gguf`), or a hub name as given.
pub fn model(p: &ProcInfo) -> Option<String> {
    let args = &p.args;
    let prog = program(p);
    let server = prog == "llama-server";
    // To Python, `-m` names a module, not a model.
    let python = is_python(&prog);
    let mut found: Option<String> = None;
    for i in 1..args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v)),
            _ => (args[i].as_str(), None),
        };
        let value = inline.or_else(|| args.get(i + 1).map(String::as_str)).map(str::to_string);
        match flag {
            // An alias is what clients ask for: the best name there is.
            "--alias" | "-a" if server => return value,
            "--served-model-name" => return value,
            "-m" if python => {}
            "--model" | "-m" | "--hf-repo" | "-hf" | "--model-id" | "--model-path" if found.is_none() => found = value,
            // `vllm serve <model>`, `ollama run <model>`.
            "serve" | "run" if found.is_none() => found = value.filter(|v| !v.starts_with('-')),
            _ => {}
        }
    }
    found.map(|m| model_name(&m)).filter(|m| !m.is_empty())
}

fn model_name(m: &str) -> String {
    let is_path = m.starts_with('/') || m.starts_with('.') || m.starts_with('~') || m.ends_with(".gguf");
    if !is_path {
        return m.to_string();
    }
    let base = m.trim_end_matches('/').rsplit('/').next().unwrap_or(m);
    let stem = base.strip_suffix(".gguf").unwrap_or(base);
    // `-00001-of-00003`: one shard of a split model.
    match stem.rfind("-of-") {
        Some(i) if i >= 6 && stem[i + 4..].bytes().all(|b| b.is_ascii_digit()) && stem.as_bytes()[i - 6] == b'-' => {
            stem[..i - 6].to_string()
        }
        _ => stem.to_string(),
    }
}

/// Flag names whose values are secrets.
fn secret_flag(flag: &str) -> bool {
    let f = flag.trim_start_matches('-').to_ascii_lowercase();
    ["key", "token", "secret", "passw", "auth", "credential", "cookie", "bearer"].iter().any(|w| f.contains(w))
}

/// The command line with secrets masked: values of flags like `--api-key`,
/// `NAME=VALUE` settings, and passwords in URLs. Done on the host, before
/// anything leaves it.
pub fn redact(args: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut hide_next = false;
    // Word by word: a program that rewrote its command line into one string
    // must not hide `--api-key x` inside a single argument.
    for a in args.iter().flat_map(|a| a.split_whitespace()) {
        if hide_next {
            hide_next = false;
            out.push("***".into());
            continue;
        }
        let masked = if let Some((k, _)) = a.split_once('=').filter(|(k, _)| k.starts_with('-')) {
            if secret_flag(k) {
                format!("{k}=***")
            } else {
                mask_url(a)
            }
        } else if a.starts_with('-') {
            hide_next = secret_flag(a);
            a.to_string()
        } else if let Some((k, _)) = a.split_once('=').filter(|(k, _)| {
            let env_style = !k.is_empty()
                && k.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                && !k.as_bytes()[0].is_ascii_digit();
            env_style || secret_flag(k)
        }) {
            // `HF_TOKEN=...` as `env` takes it, or `api_key=...`. Settings
            // like Hydra's `lr=3e-4` stay readable.
            format!("{k}=***")
        } else {
            mask_url(a)
        };
        out.push(masked.to_string());
    }
    out.join(" ")
}

/// `scheme://user:pass@host` with the user and password masked.
fn mask_url(a: &str) -> String {
    let Some(scheme_end) = a.find("://") else { return a.to_string() };
    let rest = &a[scheme_end + 3..];
    let host_start = rest.find('/').unwrap_or(rest.len());
    match rest[..host_start].rfind('@') {
        Some(at) => format!("{}://***@{}", &a[..scheme_end], &rest[at + 1..]),
        None => a.to_string(),
    }
}

/// What the tracker remembers about a job between samples.
struct Seen {
    /// When its CPU use crossed `CPU_CORES`, while it stays above `CPU_KEEP`.
    hot_since: Option<Instant>,
    /// When its GPUs were last busy (or when it was first seen).
    gpu_busy_at: Instant,
}

#[derive(Default)]
pub struct Tracker {
    seen: HashMap<u32, Seen>,
}

/// One sample's facts, as the tracker needs them.
pub struct Input<'a> {
    pub procs: &'a [ProcInfo],
    pub gpus: &'a HashMap<u32, Vec<ProcGpu>>,
    /// Each GPU's own utilization, for jobs that are its only user.
    pub gpu_util: &'a [Option<u8>],
    /// The ports a process listens on.
    pub ports: &'a dyn Fn(u32) -> Vec<u16>,
    pub detail: bool,
    pub me: u32,
}

/// A job being assembled.
#[derive(Default)]
struct Job {
    members: Vec<usize>,
    cpu: f32,
    rss: u64,
    gpu: HashMap<u16, (u64, Option<u8>)>,
    computes: bool,
    server: Option<usize>,
}

impl Tracker {
    pub fn update(&mut self, input: &Input) -> (Vec<Workload>, Vec<Proc>) {
        let now = Instant::now();
        let procs = input.procs;
        let by_pid: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();

        // Each process's job: walk up while the parent is ours and not a
        // boundary. Desktop programs and this agent stay out of jobs.
        let root_of = |mut i: usize| loop {
            let p = &procs[i];
            let Some(&pi) = by_pid.get(&p.ppid) else { return i };
            let parent = &procs[pi];
            if parent.pid <= 1 || parent.uid != p.uid || listed(BOUNDARY, parent) || listed(DESKTOP, parent) {
                return i;
            }
            i = pi;
        };
        let mut jobs: HashMap<usize, Job> = HashMap::new();
        for (i, p) in procs.iter().enumerate() {
            if p.pid == input.me || listed(DESKTOP, p) {
                continue;
            }
            let root = root_of(i);
            if listed(DESKTOP, &procs[root]) {
                continue;
            }
            let job = jobs.entry(root).or_default();
            job.members.push(i);
            job.cpu += p.cpu;
            job.rss += p.rss;
            if job.server.is_none() && is_server(p) {
                job.server = Some(i);
            }
            for g in input.gpus.get(&p.pid).into_iter().flatten().filter(|g| g.compute) {
                job.computes = true;
                let e = job.gpu.entry(g.gpu).or_insert((0, None));
                e.0 += g.mem;
                e.1 = match (e.1, g.busy) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
        }

        // How many jobs compute on each GPU.
        let mut users: HashMap<u16, usize> = HashMap::new();
        for job in jobs.values() {
            for g in job.gpu.keys() {
                *users.entry(*g).or_default() += 1;
            }
        }

        let mut list = Vec::new();
        let mut alive = Vec::new();
        for (root, mut job) in jobs {
            let top = &procs[root];
            alive.push(top.pid);
            let seen = self.seen.entry(top.pid).or_insert(Seen { hot_since: None, gpu_busy_at: now });
            // A job alone on a GPU is busy at least as much as the GPU is:
            // per-process figures lag, and can be missing.
            for (g, (_, busy)) in job.gpu.iter_mut() {
                if users.get(g) == Some(&1) {
                    let device = input.gpu_util.get(*g as usize).copied().flatten();
                    *busy = (*busy).max(device);
                }
            }
            // The CPU clock: hot after `CPU_HOLD` above `CPU_CORES`, until it
            // falls below `CPU_KEEP`.
            if job.cpu >= CPU_CORES {
                seen.hot_since.get_or_insert(now);
            } else if job.cpu < CPU_KEEP {
                seen.hot_since = None;
            }
            let hot = seen.hot_since.is_some_and(|t| now.duration_since(t) >= CPU_HOLD);
            // The stall clock runs only on known, low numbers: unknown resets
            // it, so a job is never called stalled for lack of data.
            if job.gpu.values().any(|(_, b)| b.is_none_or(|b| b >= GPU_IDLE)) {
                seen.gpu_busy_at = now;
            }
            let ports: Vec<u16> = if job.server.is_some() {
                // The top process's ports first: a proxy's public port before
                // its servers' private ones.
                let mut ports: Vec<u16> = Vec::new();
                for port in job.members.iter().flat_map(|&i| (input.ports)(procs[i].pid)) {
                    if !ports.contains(&port) {
                        ports.push(port);
                    }
                }
                ports
            } else {
                Vec::new()
            };
            let kind = if job.server.is_some() {
                Kind::Inference
            } else if job.computes {
                Kind::Training
            } else if hot {
                Kind::Cpu
            } else {
                continue;
            };
            let model = if kind == Kind::Inference {
                // Models are on whichever server processes have one: a proxy
                // like llama-swap names them on the children it started, and
                // may run several at once.
                let mut models: Vec<String> = Vec::new();
                for m in job.members.iter().filter(|&&i| is_server(&procs[i])).filter_map(|&i| model(&procs[i])) {
                    if !models.contains(&m) {
                        models.push(m);
                    }
                }
                models.join(", ")
            } else {
                String::new()
            };
            let mut gpus: Vec<WorkloadGpu> =
                job.gpu.iter().map(|(g, (mem, busy))| WorkloadGpu { gpu: *g, mem: *mem, busy: *busy }).collect();
            gpus.sort_by_key(|g| g.gpu);
            // A server started by a wrapper or a script is named after the
            // server, not the shell that runs it.
            let named = match job.server {
                Some(s) if kind == Kind::Inference && !is_server(top) => &procs[s],
                _ => top,
            };
            list.push(Workload {
                id: top.pid,
                kind,
                label: label(named),
                model,
                ports,
                procs: job.members.len().min(u16::MAX as usize) as u16,
                started_s: top.started_s,
                cpu: job.cpu,
                rss: job.rss,
                gpus,
                gpu_idle_s: if kind == Kind::Training {
                    now.duration_since(seen.gpu_busy_at).as_secs() as u32
                } else {
                    0
                },
                cmdline: if input.detail { redact(&top.args) } else { String::new() },
            });
        }
        self.seen.retain(|pid, _| alive.contains(pid));
        list.sort_by(|a, b| {
            let gpu_mem = |w: &Workload| w.gpus.iter().map(|g| g.mem).sum::<u64>();
            (a.kind as u8, std::cmp::Reverse(gpu_mem(a)), std::cmp::Reverse((a.cpu * 100.0) as u64)).cmp(&(
                b.kind as u8,
                std::cmp::Reverse(gpu_mem(b)),
                std::cmp::Reverse((b.cpu * 100.0) as u64),
            ))
        });

        let top = if input.detail {
            let mut all: Vec<&ProcInfo> = procs.iter().filter(|p| p.pid != input.me).collect();
            let gpu_mem = |p: &ProcInfo| input.gpus.get(&p.pid).map_or(0, |g| g.iter().map(|g| g.mem).sum::<u64>());
            all.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(gpu_mem(b).cmp(&gpu_mem(a))));
            all.iter()
                .take(TOP)
                .map(|p| Proc { pid: p.pid, name: program(p), cpu: p.cpu, rss: p.rss, gpu_mem: gpu_mem(p) })
                .collect()
        } else {
            Vec::new()
        };
        (list, top)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, args: &str, cpu: f32) -> ProcInfo {
        let args: Vec<String> = args.split(' ').map(String::from).collect();
        let comm: String = args[0].rsplit('/').next().unwrap().chars().take(15).collect();
        ProcInfo { pid, ppid, uid: 1000, comm, args: args.into(), started_s: 100, cpu, rss: 1 << 20 }
    }

    fn gpu(gpu: u16, mem: u64, busy: Option<u8>, compute: bool) -> ProcGpu {
        ProcGpu { gpu, mem, busy, compute }
    }

    fn run(procs: &[ProcInfo], gpus: &[(u32, ProcGpu)], ports: &[(u32, u16)]) -> Vec<Workload> {
        let mut map: HashMap<u32, Vec<ProcGpu>> = HashMap::new();
        for (pid, g) in gpus {
            map.entry(*pid).or_default().push(g.clone());
        }
        let ports = ports.to_vec();
        let ports_of = move |pid: u32| ports.iter().filter(|(p, _)| *p == pid).map(|(_, port)| *port).collect();
        let input = Input { procs, gpus: &map, gpu_util: &[Some(90), Some(0)], ports: &ports_of, detail: true, me: 0 };
        Tracker::default().update(&input).0
    }

    #[test]
    fn a_model_proxy_and_its_server_are_one_inference_workload() {
        // The shape on an APU box: a proxy under systemd starts a server.
        let procs = vec![
            proc(1, 0, "/usr/lib/systemd/systemd", 0.0),
            proc(10, 1, "/opt/llama-swap/llama-swap -config /srv/x.yaml -listen 0.0.0.0:8080", 0.0),
            proc(
                11,
                10,
                "/opt/llama/bin/llama-server --host 127.0.0.1 --port 5810 --model /srv/models/Big-Model-UD-Q3_K_XL-00001-of-00003.gguf",
                0.5,
            ),
            proc(20, 1, "node daemon.js --port 7171", 0.1),
        ];
        let w = run(&procs, &[(11, gpu(0, 59 << 30, Some(93), true))], &[(10, 8080), (11, 5810), (20, 7171)]);
        assert_eq!(w.len(), 1, "{w:#?}");
        assert_eq!(w[0].kind, Kind::Inference);
        assert_eq!(w[0].id, 10, "keyed by the proxy, which outlives model swaps");
        assert_eq!(w[0].label, "llama-swap");
        assert_eq!(w[0].model, "Big-Model-UD-Q3_K_XL");
        assert_eq!(w[0].ports, vec![8080, 5810]);
        assert_eq!(w[0].gpus[0].mem, 59 << 30);
    }

    #[test]
    fn a_proxy_running_two_models_names_both() {
        let procs = vec![
            proc(1, 0, "systemd", 0.0),
            proc(10, 1, "llama-swap -listen :8080", 0.0),
            proc(11, 10, "llama-server --port 5807 -m /m/Big-Q4_K_XL.gguf", 0.1),
            proc(12, 10, "llama-server --port 5801 -m /m/Small-Q8_0.gguf", 0.1),
        ];
        let w = run(&procs, &[(11, gpu(0, 30 << 30, None, true)), (12, gpu(0, 18 << 30, None, true))], &[]);
        assert_eq!(w[0].model, "Big-Q4_K_XL, Small-Q8_0");
        assert_eq!(w[0].gpus[0].mem, 48 << 30);
    }

    #[test]
    fn a_server_under_a_script_is_named_after_the_server() {
        let procs = vec![
            proc(1, 0, "systemd", 0.0),
            proc(10, 1, "timeout 2h sh serve.sh", 0.0),
            proc(11, 10, "/opt/llama/llama-server -m /m/Qwen3-8B-Q4_K_M.gguf --port 8080", 0.1),
        ];
        let w = run(&procs, &[], &[]);
        assert_eq!((w[0].label.as_str(), w[0].id), ("llama-server", 10));
    }

    #[test]
    fn a_proxy_without_a_model_is_still_a_server() {
        let procs = vec![proc(1, 0, "systemd", 0.0), proc(10, 1, "/opt/llama-swap/llama-swap -listen :8080", 0.0)];
        let w = run(&procs, &[], &[(10, 8080)]);
        assert_eq!((w[0].kind, w[0].model.as_str()), (Kind::Inference, ""));
    }

    #[test]
    fn torchrun_and_its_ranks_are_one_training_run() {
        let procs = vec![
            proc(1, 0, "systemd", 0.0),
            proc(5, 1, "bash", 0.0),
            proc(
                30,
                5,
                "/home/u/.venv/bin/python /home/u/.venv/bin/torchrun --nproc-per-node 2 train.py --lr 3e-4",
                0.1,
            ),
            proc(31, 30, "/home/u/.venv/bin/python -u train.py --lr 3e-4", 1.0),
            proc(32, 30, "/home/u/.venv/bin/python -u train.py --lr 3e-4", 1.0),
        ];
        let w = run(&procs, &[(31, gpu(0, 25 << 30, Some(97), true)), (32, gpu(1, 25 << 30, Some(98), true))], &[]);
        assert_eq!(w.len(), 1, "{w:#?}");
        assert_eq!(w[0].kind, Kind::Training);
        assert_eq!(w[0].id, 30);
        assert_eq!(w[0].label, "torchrun train.py");
        assert_eq!(w[0].procs, 3);
        assert_eq!(w[0].gpus.len(), 2);
        assert_eq!(w[0].gpu_idle_s, 0);
    }

    #[test]
    fn desktop_programs_drawing_on_a_gpu_are_not_workloads() {
        let procs = vec![
            proc(1, 0, "systemd", 0.0),
            proc(40, 1, "/usr/bin/gnome-shell --mode=ubuntu", 3.0),
            proc(41, 1, "/opt/google/chrome/chrome", 0.0),
            proc(
                42,
                41,
                "/opt/google/chrome/chrome --type=gpu-process --render-node-override=/dev/dri/renderD129",
                0.5,
            ),
            proc(43, 1, "walker --gapplication-service", 0.0),
        ];
        let w = run(
            &procs,
            &[
                (40, gpu(1, 300 << 20, Some(10), false)),
                (42, gpu(1, 2 << 30, Some(5), true)),
                (43, gpu(0, 0, None, true)),
            ],
            &[],
        );
        assert!(w.is_empty(), "{w:#?}");
    }

    #[test]
    fn a_job_that_only_draws_is_not_training() {
        let procs = vec![proc(1, 0, "systemd", 0.0), proc(50, 1, "/usr/bin/some-viewer", 0.1)];
        assert!(run(&procs, &[(50, gpu(0, 200 << 20, Some(3), false))], &[]).is_empty());
    }

    #[test]
    fn python_labels() {
        assert_eq!(label(&proc(1, 0, "/usr/bin/python3.12 -u /x/y/prep.py --n 4", 0.0)), "python prep.py");
        assert_eq!(label(&proc(1, 0, "python -m sft.train --cfg a", 0.0)), "python -m sft.train");
        assert_eq!(label(&proc(1, 0, "/home/u/.local/bin/uv run train.py", 0.0)), "uv train.py");
        assert_eq!(label(&proc(1, 0, "/usr/bin/make -j32", 0.0)), "make");
        assert_eq!(label(&proc(1, 0, "/v/bin/python3 /v/bin/accelerate launch fit.py", 0.0)), "accelerate fit.py");
        assert_eq!(
            label(&proc(1, 0, "/usr/bin/time -v uv run --no-sync python scripts/export.py --jobs 12", 0.0)),
            "uv export.py"
        );
        assert_eq!(label(&proc(1, 0, "nice -n 10 taskset -c 0-7 python3 prep.py", 0.0)), "python prep.py");
        assert_eq!(
            label(&proc(1, 0, "env CUDA_VISIBLE_DEVICES=1 timeout 2h torchrun train.py", 0.0)),
            "torchrun train.py"
        );
        assert_eq!(label(&proc(1, 0, "/usr/bin/time", 0.0)), "time");
    }

    #[test]
    fn model_names() {
        let m = |args: &str| model(&proc(1, 0, args, 0.0));
        assert_eq!(m("llama-server -m /m/Qwen3-8B-Q4_K_M.gguf").as_deref(), Some("Qwen3-8B-Q4_K_M"));
        assert_eq!(m("llama-server --model=/m/A-00002-of-00004.gguf").as_deref(), Some("A"));
        assert_eq!(m("llama-server -m /m/x.gguf --alias coder").as_deref(), Some("coder"));
        assert_eq!(m("vllm serve Qwen/Qwen3-8B --port 8000").as_deref(), Some("Qwen/Qwen3-8B"));
        assert_eq!(m("python -m vllm.entrypoints.openai.api_server --model /ckpt/run7/").as_deref(), Some("run7"));
        assert_eq!(m("ollama serve"), None);
        assert!(is_server(&proc(1, 0, "/v/bin/python /v/bin/vllm serve Qwen/Qwen3-8B", 0.0)));
        assert_eq!(m("/v/bin/python /v/bin/vllm serve Qwen/Qwen3-8B").as_deref(), Some("Qwen/Qwen3-8B"));
    }

    #[test]
    fn secrets_never_leave_the_host() {
        let args: Vec<String> = "llama-server --api-key sk-123 --port 8080 --hf-token=hf_abc HF_TOKEN=hf_x \
             --url https://user:pa55@host/x -v --password hunter2"
            .split(' ')
            .map(String::from)
            .collect();
        let r = redact(&args);
        for secret in ["sk-123", "hf_abc", "hf_x", "pa55", "hunter2", "user:"] {
            assert!(!r.contains(secret), "{secret} leaked in {r}");
        }
        assert!(r.contains("--port 8080") && r.contains("--api-key ***") && r.contains("https://***@host/x"));
    }

    #[test]
    fn rewritten_command_lines_are_split_before_masking() {
        let args = vec!["/opt/app/server --type=x --api-key sk-999 --token=abc".to_string()];
        let r = redact(&args);
        assert!(!r.contains("sk-999") && !r.contains("abc"), "{r}");
        assert_eq!(program(&proc(1, 0, "x", 0.0)), "x");
        let chrome = ProcInfo {
            args: vec!["/opt/google/chrome/chrome --type=renderer --lang=en".to_string()].into(),
            ..proc(2, 0, "chrome", 0.0)
        };
        assert_eq!(program(&chrome), "chrome");
    }

    #[test]
    fn training_overrides_stay_readable() {
        let args: Vec<String> =
            "python train.py lr=3e-4 model=x HF_TOKEN=y api_key=z".split(' ').map(String::from).collect();
        let r = redact(&args);
        assert!(r.contains("lr=3e-4") && r.contains("model=x"), "{r}");
        assert!(!r.contains("=y") && !r.contains("=z"), "{r}");
    }

    #[test]
    fn training_that_listens_is_still_training() {
        // PyTorch's store on the launcher, NCCL bootstrap ports on the ranks,
        // W&B's service under rank 0, and a notebook kernel on its own.
        let procs = vec![
            proc(1, 0, "systemd", 0.0),
            proc(5, 1, "bash", 0.0),
            proc(30, 5, "/v/bin/python /v/bin/torchrun --nproc-per-node 2 train.py", 0.1),
            proc(31, 30, "/v/bin/python -u train.py", 1.0),
            proc(32, 30, "/v/bin/python -u train.py", 1.0),
            proc(33, 31, "/v/lib/wandb-core --port-filename /tmp/x", 0.1),
            proc(40, 1, "/v/bin/python -m ipykernel_launcher -f /run/kernel.json", 0.5),
        ];
        let w = run(
            &procs,
            &[
                (31, gpu(0, 20 << 30, Some(95), true)),
                (32, gpu(1, 20 << 30, Some(95), true)),
                (40, gpu(0, 3 << 30, Some(40), true)),
            ],
            &[(30, 29500), (31, 45123), (32, 45124), (33, 38211), (40, 8888)],
        );
        assert_eq!(w.len(), 2, "{w:#?}");
        assert!(w.iter().all(|w| w.kind == Kind::Training), "{w:#?}");
        assert!(w.iter().all(|w| w.ports.is_empty()));
    }

    #[test]
    fn a_busy_gpu_never_reads_as_stalled() {
        // Per-process numbers say 0 (stale) or nothing; the GPU says 90%.
        let procs = vec![proc(1, 0, "systemd", 0.0), proc(5, 1, "bash", 0.0), proc(30, 5, "python train.py", 1.0)];
        let mut map: HashMap<u32, Vec<ProcGpu>> = HashMap::new();
        map.insert(30, vec![gpu(0, 20 << 30, Some(0), true)]);
        let ports = |_: u32| Vec::new();
        let mut tracker = Tracker::default();
        let input = Input { procs: &procs, gpus: &map, gpu_util: &[Some(90)], ports: &ports, detail: false, me: 0 };
        let w = tracker.update(&input).0;
        assert_eq!(w[0].gpus[0].busy, Some(90));
        // Unknown everywhere: the clock does not run either.
        tracker.seen.get_mut(&30).unwrap().gpu_busy_at -= Duration::from_secs(3600);
        map.insert(30, vec![gpu(0, 20 << 30, None, true)]);
        let input = Input { procs: &procs, gpus: &map, gpu_util: &[None], ports: &ports, detail: false, me: 0 };
        assert_eq!(tracker.update(&input).0[0].gpu_idle_s, 0);
    }

    #[test]
    fn heavy_cpu_needs_to_last() {
        let procs =
            vec![proc(1, 0, "systemd", 0.0), proc(5, 1, "bash", 0.0), proc(60, 5, "/usr/bin/python3 tokenize.py", 6.0)];
        // Hot, but not for long enough yet.
        assert!(run(&procs, &[], &[]).is_empty());
    }
}

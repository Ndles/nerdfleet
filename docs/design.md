# nerdfleet: design

Status: draft for review, 2026-10-24.

nerdfleet watches a fleet of machines from one terminal: what each one is training or serving, how hard its CPUs,
memory and GPUs are working, and a shell on any of them a keystroke away. It is aimed at AI/ML engineers and
researchers whose work is spread over a few GPU boxes, Macs serving models, and Slurm-style clusters reached through
a login node.

It is a TUI, not a web app. It runs natively on Linux and macOS, in any terminal, and over SSH from anywhere. It is
built on top of [nerdmux](../../nerdmux) and shares its look, keys and terminal engine, but it is a separate program:
nerdmux stays a multiplexer.

## Goals

- **One screen for the whole fleet:** every host's CPU, memory, GPU and workload at a glance, live.
- **Workload as ML people think of it:** training runs, inference servers, and heavy CPU jobs, not a process table.
- **Drill down:** a host's devices and workloads, with recent history as graphs.
- **Terminals:** a shell on any host, or inside a running cluster job, without leaving the tool, that survives the
  laptop sleeping or the network dropping.
- **NVIDIA, AMD and Apple as equals,** including mixed hosts (host-a has NVIDIA and AMD) and unified-memory machines
  (host-b, every Apple Silicon Mac).
- **Linux and macOS hosts both monitored.** Macs are a first-class kind of host, not just where the viewer runs.
- **Clusters:** nodes behind a login node, jobs as the unit of workload, Slurm first and other schedulers behind the
  same interface.
- **Security first.** See [Security](#security). In short: no ports, no daemons by default, no root, no new
  credentials, nothing hardcoded, and every byte from a remote host treated as untrusted.

## Non-goals (for now)

- A web UI or a native windowed app. The protocol is front-end-agnostic, so a GUI could be added later without
  touching the agents, but it is not planned.
- Changing anything on a host: killing processes, `scancel`, restarting services. Read-only at first. Actions may
  come later, each behind a confirmation.
- Long-term metrics storage and dashboards à la Prometheus/Grafana. nerdfleet shows the present and the recent past.
- Experiment tracking. nerdfleet is not W&B or TensorBoard. It may link to a run's dashboard later, but it does not
  replace one.
- Kubernetes. It would be a separate access method (`kubectl exec`, metrics API, a DaemonSet), and is noted under
  [Later](#later).

## What the hosts look like today

Measured on 2026-10-24. These are the development machines, not defaults: nothing in the code knows their names,
addresses or hardware.

| Host  | OS / glibc                      | GPUs                                                                                    | Notes                                    |
|-------|---------------------------------|-----------------------------------------------------------------------------------------|------------------------------------------|
| host-a | Ubuntu, kernel 7.0, glibc 2.43  | 2× NVIDIA RTX PRO 4500 Blackwell, 32 GB each, driver 580.178.04; AMD Granite Ridge iGPU | 32 threads, 89 GiB RAM, Docker present   |
| host-b   | Ubuntu, kernel 7.0              | AMD Strix Halo (Radeon 8060S), 8 GiB VRAM carve-out, ~104 GiB GTT                         | No Docker or Podman; llama runs on Vulkan; `amd-smi` and `rocm-smi` installed |

Linux comes first, on systemd distributions: Arch (including Omarchy) and Ubuntu are tested, and others with
glibc ≥ 2.28 and systemd should work. Linux without systemd still gets watch mode but not resident mode.

Assumed, not yet checked:

- The clusters we target run RHEL/Rocky 8 or newer (glibc ≥ 2.28), have shared home directories, and may mix x86_64
  and aarch64 nodes.
- No Mac is in the fleet yet; macOS support is written against Apple Silicon, macOS 14 or newer, once one is
  available to test on.

## Architecture

```
 viewer (laptop: Linux or macOS)                      host or login node (Linux or macOS)
┌────────────────────────────────┐   ssh    ┌───────────────────────────────────┐
│ nerdfleet                      │ ──────── │ nerdfleet agent --stdio           │
│  overview · host · cluster     │ (stdio)  │  samplers: cpu mem disk net gpu   │
│  terminal panes (local ptys    │          │  workload detectors               │
│  running ssh / srun)           │          │  scheduler adapter (login node)   │
└────────────────────────────────┘          │  relay ─────────► node agents     │
                                            └───────────────────────────────────┘
```

Two programs in one binary:

- **The viewer** (`nerdfleet`) is the TUI. It reads the fleet config, opens one connection per host or cluster, and
  draws what the agents send.
- **The agent** (`nerdfleet agent --stdio`) runs on the host being watched, as the user, started by the viewer over
  SSH. It samples metrics, detects workload, and writes protocol messages to stdout. It never listens on a network
  socket.

The viewer runs the system `ssh` with the host's alias from the fleet config. Everything about reaching and
authenticating to the host (address, user, key, `ProxyJump`, Tailscale names, host keys) lives in the user's
`~/.ssh/config`, where it already is, and nerdfleet never duplicates it.

### Agent lifetime and history

Graphs need past data points. Where those points come from depends on how long the agent lives.

- **Watch mode (default, everywhere).** The agent starts when you connect and exits when you disconnect. It keeps the
  last 15 minutes in memory, so graphs fill up while you watch. Before you connected, there is nothing to show.
- **Resident mode (opt-in per host, personal hosts and Macs only).** The agent runs as a user service (systemd user
  unit on Linux, launchd agent on macOS) and keeps sampling while nobody watches. It holds 24 hours of history at
  reduced resolution, so in the morning you can see that the training run's GPU utilization dropped to zero at 03:12,
  or that an inference server ran out of memory overnight. Alerts run here (see [Alerts](#alerts)).

Resident mode is never used on cluster login or compute nodes, where long-running user processes are usually against
the rules.

When a resident agent exists, the viewer's SSH session runs `nerdfleet agent --attach`. That command connects to the
resident agent through a Unix socket in the user's private runtime directory instead of sampling itself.

### Resident mode in detail

As built in phase 3 (2026-10-24), and checked on an Arch laptop and an Ubuntu 26.04 APU box.

- **Enabling it.** `nerdfleet resident enable <host>`:
  - copies and verifies the agent (see [Agent deployment](#agent-deployment));
  - writes `~/.config/systemd/user/nerdfleet-agent.service` and starts it with `systemctl --user`;
  - removes older agent builds;
  - reports whether lingering is on.

  `nerdfleet resident status <host>` says whether it runs. `disable` stops it and removes the unit; the saved state
  stays until `disable --purge`. The same commands work for `local = true` hosts, without SSH.
- **Surviving logout.** A user service stops when the user's last session ends, unless lingering is on for that user.
  That is an admin setting, so nerdfleet says whether it is on and prints `sudo loginctl enable-linger <user>`, but
  never runs it or anything else with elevated rights.
- **Integrity at every start.** The unit's `ExecStart` is the same fixed `sh -c` script as every other start:
  - It checks that the agent directory is private and the file's SHA-256 matches, then runs `agent --resident`.
  - `$` and `%` are doubled for systemd.
  - Tested: a replaced agent file makes the unit refuse to start (exit 98, retried every 10 s), and `enable` copies a
    good build back. A viewer connecting at that point repairs it too, if its own build is the one the unit runs;
    otherwise it copies its own build, and the unit keeps failing until `resident enable` is run.
  - A running binary cannot be written in place ("Text file busy"); only a replaced one is a threat.
- **Hardening.** The unit sets:
  - `NoNewPrivileges`, `RestrictAddressFamilies=AF_UNIX` (the agent needs no network);
  - `RestrictNamespaces`, `LockPersonality`, `RestrictRealtime`, `SystemCallArchitectures=native`;
  - `CPUQuota=10%`, `MemoryMax=128M`, `TasksMax=64`, `Nice=10`, `UMask=0077`.

  All of these are enforced on both Arch and Ubuntu, checked with throwaway `systemd-run` units (for example, a TCP
  socket is refused).

  **Filesystem protections are left out on purpose:**
  - `ProtectSystem`, `ProtectHome` and `PrivateTmp` in a user unit put the service in a user namespace of its own. From
    there the kernel refuses to show other processes' fdinfo, even the user's own, which breaks ports and per-process
    GPU use. That holds on both distributions.
  - On Ubuntu 26.04 (`apparmor_restrict_unprivileged_userns=1`) they don't even protect anything: they are accepted and
    not enforced.

  The agent's real protection is that it only reads.
- **No `StateDirectory=`.** With a config directory of the same name present (`~/.config/nerdfleet`, the viewer's),
  systemd 259 takes `StateDirectory=nerdfleet` for a layout from before systemd 253, and links
  `~/.local/state/nerdfleet` to the config directory. The enable script creates the state directory itself (mode
  0700, never a symlink); `RuntimeDirectory=nerdfleet` (0700, preserved across stops) is fine.
- **The socket.** `$XDG_RUNTIME_DIR/nerdfleet/agent.sock`, mode 0600, in a 0700 directory.
  - The resident and anyone connecting both check the other end's user id (`SO_PEERCRED`).
  - A second resident refuses to start.
  - A stale socket is removed only if it is a socket owned by the user and nothing answers on it.
- **Viewers.**
  - Each viewer's connection gets the hello, the inventory, the history and the alert log, then live messages.
  - The resident always samples every second, whatever viewers ask.
  - It works out detail if any viewer wants it, and strips it for the others.
  - Every viewer has a queue of 64 frames; a full queue drops that viewer, so sampling never waits for one.
- **Attaching.** The viewer still runs `agent --stdio`. That command connects to the resident if the protocol versions
  match, then passes bytes both ways.
  - If the resident is another protocol version (an older build), it samples by itself and tells the viewer to run
    `resident enable` again.
  - With no resident, it samples by itself, as before.
- **History.** Three rings, kept by the same code (`history.rs`) in agents and the viewer:
  - one point per sample for 15 minutes;
  - the highest value per 10 seconds for 6 hours;
  - per minute for 24 hours.

  Series: CPU, memory, and each GPU's utilization, one byte per point. The coarser rings are cut at fixed multiples of
  their step, so a stretch with the agent off shows as a gap rather than being bridged. The host view's `t` switches
  between the rings.
- **Persistence.**
  - The history and the alert log (the last 200 alerts) go to `$XDG_STATE_HOME/nerdfleet/agent-state.bin` every 5
    minutes and on SIGTERM.
  - The file is mode 0600, written to a temporary file and renamed, with a magic number and a layout version.
  - It is read back with the protocol's limits. A damaged or older file means starting fresh.
  - It holds numbers, alert messages and timestamps: no command lines.
- **Cost.** 0.59% of a core over 3 minutes with no viewer attached, and 4 MiB of memory, on the APU box
  (2026-10-24).
- **Upgrades.**
  - Deploying a new build never deletes the build the unit runs: the upload script keeps the file the unit names.
  - The resident keeps running its build until `resident enable` is run again, which installs the new unit and
    restarts it. The new agent loads the saved state if its layout matches.

### Relay

On a cluster, the viewer connects only to the login node. That agent opens child agents on compute nodes and
forwards their messages over the single connection: one SSH session, one MFA prompt, and no per-node traffic from the
laptop. Every message carries a route, so a node behind a login node is addressed the same way as a host connected
directly. The relay is part of the protocol from day one, even though phase 1 only has direct hosts.

## Protocol

It follows nerdmux's `proto.rs`: a little-endian `u32` length, then a hand-written body (integers little-endian,
strings and lists length-prefixed, enums as a one-byte tag). There is no serialization library, and a version
constant is checked when connecting.

Envelope:

```
Frame { route: Vec<NodeId>, msg: Msg }      // route is empty for the agent you are talking to
```

Messages, agent → viewer:

| Message     | When                         | Contents                                                       |
|-------------|------------------------------|----------------------------------------------------------------|
| `Hello`     | first                        | protocol version, agent version, target                        |
| `Inventory` | after Hello, on change       | hostname, OS, CPU model and threads, RAM, disks, NICs, GPU list |
| `Sample`    | every tick                   | timestamp and every measured value                              |
| `History`   | on request                   | ring-buffer contents for the requested series                   |
| `Workload`  | every few seconds, on change | detected workloads and top processes                            |
| `Jobs`      | login nodes, every ~15 s     | scheduler view: jobs, partitions, node states                   |
| `NodeUp` / `NodeDown` | relay              | a child agent connected or went away                            |
| `Error`     | any time                     | a sampler or detector failed, with a reason                     |

Messages, viewer → agent: `Subscribe { detail }` (the focused host gets per-process and per-device detail, others
only the summary), `RequestHistory`, `OpenNode` / `CloseNode` (relay), `Bye`.

When the versions do not match, the viewer copies its own agent build to the host and reconnects.

Both sides decode with limits: a maximum frame size (4 MiB), and maximum list and string lengths (4096 each).
Numbers that are not finite are refused. A malformed frame ends the connection; it never panics the viewer. Each host
has its own connection thread, so a host that stops halfway through a frame stalls only its own card, and SSH
keepalives end a connection that has gone silent. See [Security](#security) for why.

## Metrics

| Group   | Values                                                                 | Linux source                            | macOS source                         |
|---------|------------------------------------------------------------------------|-----------------------------------------|--------------------------------------|
| CPU     | total and per-core use, load, frequency, temperature where available   | `/proc/stat`, `/proc/loadavg`, hwmon    | `host_processor_info`, sysctl        |
| Memory  | used, available, cache, swap, pressure                                 | `/proc/meminfo`, `/proc/pressure`       | `host_statistics64`, `vm.memory_pressure` |
| Disk    | per-mount use, per-device throughput                                   | `statvfs`, `/proc/diskstats`            | `statfs`, IOKit                      |
| Network | per-interface throughput                                               | `/proc/net/dev`                         | `getifaddrs`                         |
| GPU     | see [GPUs](#gpus)                                                      |                                         |                                      |

Everything is sampled every tick, one second by default (`sampling.interval_ms`). In watch mode each series keeps 15
minutes at full rate in a ring buffer (about 900 points). Samples are sent whole: a few hundred bytes a second per
host is not worth the complexity of deltas. Intervals are settings, not constants.

## GPUs

A host has a **list of GPU devices, each with a vendor**, never a single "GPU". host-a reports three devices: two
NVIDIA and one AMD.

GPUs are found on the PCI bus (display-class devices in `/sys/bus/pci/devices`), not through vendor tools, so every
GPU is listed even when its driver's tools are missing. Names come from the system's PCI ID database when the driver
cannot give one.

**GPUs that power off when idle are left to sleep.** These are mostly laptop dGPUs. The agent knows them from the
NVIDIA driver's "Runtime D3 status: Enabled" in `/proc/driver/nvidia/gpus/<bus>/power`, or from runtime power
management (`power/control` is `auto`) on an AMD dGPU. Any query restarts such a GPU's idle timer, and an open NVML
session keeps it awake, so:

- **Suspended** (`power/runtime_status`): reported as asleep, and not asked anything.
- **Just woken:** something started using it, so it is measured at once. That doesn't wake anything.
- **Busy** when last measured (utilization above zero): measured every sample.
- **Idle** when last measured: looked at only once every 30 seconds, and NVML is let go in between, so the GPU's own
  idle timer (about 20 seconds on an NVIDIA laptop) can run out. In between it is reported as idle.

Programs that merely hold the device open are no sign of use. A launcher, an OSD and a browser each held
`/dev/nvidia0` open on the development laptop all day, and the GPU still suspended.

Measured on an Arch laptop with an RTX 4060 (2026-10-24): woken by `nvidia-smi`, the dGPU suspended again after about
22 seconds, with and without the agent running.

Every backend fills the same record: name, vendor, bus ID, utilization, memory (see below), power and its limit,
temperature, clocks, fan, and per-process usage where the backend can tell. A field the backend cannot provide stays
empty and the UI leaves it out.

### NVIDIA

- Loads NVML at runtime with `dlopen("libnvidia-ml.so.1")`, the versioned name. The unversioned `libnvidia-ml.so`
  is a development symlink and is often missing on clusters.
- The library is looked up by that name through the system loader only, never from a path in the environment or the
  config.
- The binary does not link against NVML, so the same build runs on hosts without NVIDIA hardware.
- If NVML will not load, it falls back to `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`. That is run by
  absolute path when found in the standard locations, with fixed arguments.
- Per-process usage comes from NVML's process lists.

### AMD

- Reads the amdgpu driver's sysfs files directly: `gpu_busy_percent`, `mem_info_vram_{used,total}`,
  `mem_info_gtt_{used,total}`, and hwmon (`power1_average` or `power1_input`, `temp1_input`, `freq1_input`).
- Needs no library and no ROCm, so it works whatever ROCm version is installed, or none.
- Per-process usage comes from `/proc/<pid>/fdinfo/*` (DRM fdinfo: `drm-engine-*` busy time, `drm-memory-*`). This is
  the only per-process source that also sees Vulkan workloads, which is what llama on host-b uses. ROCm tools do not
  attribute Vulkan work.
- `amdsmi` is an optional extra where installed, for fields sysfs does not have. It is never required.

### Apple Silicon

- Device-wide utilization and GPU memory in use come from IOKit (`IOAccelerator` performance statistics). This needs
  no root.
- GPU power and frequency are available without root only through IOReport, a private framework that tools like
  `macmon` use. It may change between macOS releases, so it is optional: if it fails, those fields stay empty.
- `powermetrics` is never used, because it needs sudo.
- Per-process GPU use is not available without root. The macOS agent attributes workloads by process instead (see
  [Workload](#workload)).

### Unified memory (APUs and Macs)

On host-b the VRAM figure is an 8 GiB carve-out, and models live in GTT: system RAM the GPU can address, about 104 GiB.
A Mac has no VRAM at all; the GPU uses the same RAM as everything else, up to a wired limit
(`iogpu.wired_limit_mb`).

Showing only VRAM would make a loaded 70B model look like an empty GPU. On unified-memory devices, the UI:

- shows GPU-addressable memory in use against its limit, with VRAM and GTT side by side on AMD APUs;
- marks it as shared with the CPU, so it is not counted twice in the host's memory bar.

## Workload

Workload is what the machine is doing for its owner. nerdfleet recognizes three kinds, in this order of precedence:

### Inference

Model servers, recognized by program and command line:

- llama.cpp (`llama-server`), vLLM, SGLang, TGI, Ollama, LM Studio, MLX (`mlx_lm.server`).
- Any process that uses a GPU and listens on a TCP port, as a fallback.

Shows: model (from the command line, or the server's model list), port, GPU devices and memory, uptime. Where the
server exposes a Prometheus `/metrics` endpoint (vLLM, SGLang, llama-server with `--metrics`), it also shows
throughput, running and queued requests, and KV-cache use.

Metrics endpoints are fetched under these rules:

- only on `127.0.0.1`/`::1`;
- only on ports the server's own process is listening on, which the agent looks up itself;
- with a short timeout and a response size limit;
- with no credentials, ever.

A server that needs an API key for `/metrics` simply goes without those numbers. On the development APU box every
llama endpoint (`/metrics`, `/slots`, `/props`, llama-swap's `/running`) needs a key, so fetching them is not built
yet; nerdfleet would show nothing more there.

### Training

GPU processes that are not servers: PyTorch (`torchrun`, `accelerate`, `deepspeed`, Lightning), JAX and similar, and
any process tree that holds a GPU and does not listen on a port.

- Ranks are grouped into one run by their process tree: one `torchrun` with eight workers is one workload, not eight.
- Shows GPUs and memory per rank, CPU and data-loader load, uptime, and the script and its arguments.
- **Stall check:** a run whose GPUs have stayed near 0% for several minutes while its processes are alive is flagged
  as possibly stalled. This is the most common silent failure in training: a deadlocked NCCL collective, a hung data
  loader, a dead node in a multi-node job.
- Progress (step, loss) is out of scope for now. A later option is to follow a log file the user names in the config.

### High CPU

Any process tree above a CPU threshold for a sustained period (by default 2 cores for 30 seconds): data preprocessing,
tokenization, evaluation, builds. The tree is shown as one workload under its top process.

### Rules and context

- Config rules can name a workload: a command-line pattern gets a label and a kind. Rules take precedence over
  detection.
- **Containers are context, not a kind.** A training run inside Docker is still a training run; the container name and
  image are shown with it. This needs the container runtime's socket to be reachable; the agent only reads from it.
  On a host without Docker (host-b), the context is simply absent.
- Everything else appears in a "top processes" list on the host view, by CPU, memory and GPU.

### How detection works per OS

| OS    | Signals                                                                                              |
|-------|------------------------------------------------------------------------------------------------------|
| Linux | command line; process tree; GPU use (NVML process list, DRM fdinfo); listening ports (`/proc/net/tcp*` matched to the process's sockets) |
| macOS | command line and process tree (`libproc`); listening ports (`proc_pidfdinfo`); GPU use device-wide only, so GPU-ness comes from the server and framework lists above |

The agent only looks at the user's own processes unless the OS lets it see others read-only (Linux usually does for
command lines, not for fdinfo). It never reads process environments.

### As built (phase 2, Linux)

**Jobs.**
- A job is a process and everything under it, named after its top process: the highest ancestor that is not a
  boundary (shells, terminals, multiplexers, `sshd`, `sudo`, `systemd`, container shims, Claude Code) and has the same
  owner.
- A job keeps its id, the top process's pid, while processes under it come and go. llama-swap swapping models stays
  one workload.

**What counts as GPU use.** Only real compute, because desktop software holds GPUs open all day:
- an NVML *compute* process (not the graphics list);
- AMD compute-engine time since the last sample;
- more than 1 GiB of GPU memory, which covers a loaded model waiting for requests.

Compositors, Xwayland, browsers, launchers, terminals and similar programs never become workloads. A process started
under one becomes a job of its own, so a Jupyter kernel started from VS Code is still a workload.

**Kinds.**
- **Inference:** a job with a known model server (by program, by Python module, or a virtualenv tool run as
  `python /venv/bin/vllm`).
  - Listening on a port is not enough. Training listens too: PyTorch's rendezvous store, NCCL's bootstrap sockets,
    W&B's local service, Jupyter.
  - A server nerdfleet does not know shows as training until the config names it (`[[workload]]` rules, still to be
    built).
- **Training:** a compute job otherwise.
- **CPU:** a job above 2 cores for 30 seconds, until it drops below 1 core.

**Names.**
- Labels look through interpreters and wrappers (`time`, `nice`, `taskset`, `numactl`, `timeout`, `env`, `stdbuf`,
  `nohup`). `/usr/bin/time -v uv run python scripts/export.py --jobs 12` reads `uv export.py`.
- Model names come from `--alias`, `--served-model-name`, `--model`/`-m` (not Python's `-m`), `--hf-repo`, and `serve`
  or `run` arguments. A file path becomes its stem without `.gguf` or shard numbers.
- A model server with no model running is "no model loaded".

**Per-process GPU use.**
- **DRM fdinfo** (amdgpu, i915, xe): counted once per `(drm-pdev, drm-client-id)`. Memory is `drm-resident-*`, or
  `drm-memory-*` on older kernels. Busy is the busiest engine's time delta divided by its capacity.
- Processes using a GPU are found by reading every fd every 10 seconds; their fdinfo is read every pass.
- **NVML:** `nvmlDeviceGetComputeRunningProcesses_v3` with the two-call pattern, plus per-process SM utilization.
  Asked only when a GPU has work (utilization above zero or more than 256 MiB in use).
- A job alone on a GPU without per-process numbers gets the GPU's own utilization.

**The stall clock.** A training job's `gpu_idle_s` counts how long all its GPUs have stayed under 5%.
- It runs only on known numbers. An unknown share resets it, so a job is never called stalled for lack of data.
- A job alone on a GPU counts as at least as busy as the GPU itself.
- NVML's per-process share is unknown when NVML returns no samples, not zero.
- The viewer calls out 10 minutes as possibly stalled; the alert in phase 3 will use the same clock.

**Secrets.**
- Command lines go to the viewer only in detail mode (the host view), with these masked on the host:
  - the values after flags whose names contain key, token, secret, passw, auth, credential, cookie or bearer;
  - `NAME=VALUE` settings whose name looks secret or is all caps (`HF_TOKEN=...`, `api_key=...`);
  - `user:pass@` in URLs.
- Settings such as Hydra's `lr=3e-4 model=llama` stay readable.
- Cards show only the derived label and model.

**Detail mode.** The viewer sends `Subscribe { detail: true }` for the host being viewed, and then the agent also
sends command lines and the eight busiest processes.

### Agent cost

Measured on 2026-10-24 over 60 seconds of CPU ticks, with nothing else changed:

| Host                                       | Before tuning | After      |
|--------------------------------------------|---------------|------------|
| Two NVIDIA GPUs plus an iGPU, ~600 processes | 2.25% of a core | 0.42%    |
| APU, ~440 processes                        | 1.13%         | 0.47%      |

What it took:
- **Reading `/proc` files into a buffer with room for a page.** `fs::read_to_string` probes zero-size files with a
  series of small reads: 43k reads per 12 s, down to 10k.
- **Reading the process table and working out workloads every 2 s** rather than every sample. The kernel spends about
  10 µs describing each process.
- **Listing listening sockets only when a job needs its ports.** Walking every socket took about 1.8 ms.
- **Fewer NVML driver calls**, each about a millisecond:
  - GPUs asked every 2 s, with the answer reused in between;
  - the clock asked every 10 s;
  - process lists only for GPUs with work;
  - per-process utilization only when there are processes.

With two-GPU training running, the process-list calls came back and the cost rose to 0.72%. NVML's process lists are
now asked every 6 seconds, which brought it to 0.48%. A job alone on a GPU is measured by the GPU's own utilization
anyway.

## Alerts

Alerts are worked out by the agent on the host, so a resident agent raises them whether or not a viewer is connected.
A watch-mode agent raises them too, but only while someone is watching.

Each alert fires once, then re-arms only when its condition has clearly gone: below a lower level, or after a quiet
spell. A value hovering at a threshold therefore doesn't raise one alert after another.

The thresholds are fixed for now. The config that would tune them lives on the viewer's machine, while a resident
agent runs on its own; getting settings to it is later work. The set grows with use.

**Built (phase 3):**

| Alert              | Fires when                                                                                   | Re-arms when                 |
|--------------------|----------------------------------------------------------------------------------------------|------------------------------|
| Training stalled   | a training run's GPUs have stayed under 5% for 10 minutes while its processes run (the [stall clock](#as-built-phase-2-linux)) | its GPUs are busy again |
| Workload ended     | a training run or model server that lived at least 2 minutes is gone: off the list *and* its top process has exited (by pid and start time). A runner idling between its GPU phases has not ended. If the kernel's OOM kill count (`/proc/vmstat`) rose meanwhile, it says the OOM killer ran around the same time. | — |
| Stopped listening  | a model server's own top process (llama-swap's `:8080`, not the ports of the models it swaps) listened for 30 s and then has not for 30 s, while running | it listens again |
| Disk nearly full   | a shown mount passes 90%                                                                     | it is below 85%              |
| GPU throttling     | an NVIDIA GPU under load has slowed itself for heat or a hardware limit for 5 minutes        | it stops                     |
| GPU memory full    | a GPU's memory (shared memory, on unified-memory GPUs) stays at 95% or more for 2 minutes. The bar is high because PyTorch's caching allocator and vLLM's preallocation hold 90% or more on purpose. | it is below 90% |
| Host unreachable   | the viewer has lost a host that was live for more than a minute (raised by the viewer)        | the host is back             |

**Notes on the rules:**
- **No exit status:** the agent didn't start the processes it watches, so "ended" can't say whether they exited
  cleanly. The OOM count is the one cause the kernel records, and it is host-wide, hence "around the same time".
- **Throttling** uses NVML's clock-event reasons: hardware slowdown, software and hardware thermal slowdown, and
  hardware power brake (`0x8 | 0x20 | 0x40 | 0x80`, from `nvml.h`).
  - The software power cap (`0x4`) is left out. The GPU box's cards report it the whole time they train at their
    200 W limit, which is normal.
  - It is asked every 10 s, and only above 10% utilization.
  - AMD has no clean signal in sysfs, so AMD GPUs report none; guessing from temperature would be worse.
- **"Stopped listening"** is the honest version of "server down". Checking whether a server answers would mean HTTP
  requests, and the development box's servers all require a key.

**Not built:**
- **High-CPU job done:** off by default, as planned.

**Delivery:**
- Every alert goes into the host's alert log (the last 200), saved with the history. The viewer receives the whole log
  when it connects and keeps it.
- **Unread alerts:**
  - A card shows "⚠ N new" with the newest message.
  - Opening the host view marks them read.
  - Which alerts are read is kept per host in `$XDG_STATE_HOME/nerdfleet/viewer-read.txt` (mode 0600).
- **Notifications:** alerts that happen while the viewer runs also go to `notify-send`, as arguments and never through
  a shell, unless the user is looking at that host. Old alerts from the log never become notifications.
- **Ids** are milliseconds times a thousand plus a counter, and always grow past the last saved id. So they stay unique
  across restarts, and a clock that went back can't reuse one.
- **Off the machines** (ntfy, email, Slack) is later work. It would need credentials, which would be read from a
  separate file with mode 0600 or the system keyring, never from the main config.

## Access methods

How the agent reaches a node:

| Method          | Use                                                               |
|-----------------|-------------------------------------------------------------------|
| `ssh`           | host-a, host-b, any reachable host (OpenSSH config applies)           |
| `ssh` with jump | compute nodes reachable through the login node                    |
| `relay`         | many nodes over one connection to the login node                  |
| `job-step`      | nodes you cannot SSH into: `srun --jobid=<id> --overlap` starts the agent inside your job (PBS `pbsdsh`, LSF `blaunch` later) |

For each connection it opens, the viewer starts an SSH `ControlMaster` with its socket in a private directory (see
[Security](#security)). A cluster that prompts for MFA then prompts once.

## Clusters

A scheduler adapter runs on the login node's agent and produces the `Jobs` message: your jobs (state, nodes, GPUs,
elapsed time against the limit, output log path), partitions, and node states.

**Slurm: 24.05.2 or newer, JSON only.**

- Uses `squeue`, `sinfo` and `sacct` with `--json=<data_parser version>`, always naming the version explicitly. The
  default parser crashed `squeue --json` in 24.05.0 and 24.05.1, fixed in 24.05.2, and the schema changes between
  parser versions.
- The adapter probes once, at connect, for the newest parser version it understands and the cluster offers, and keeps
  using that one.
- `slurmrestd` is used where a site enables it and the user already has a token. The token comes from Slurm's own
  `SLURM_JWT` mechanism on the login node, not from nerdfleet's config.
- No `-o` format-string fallback. Older clusters show "Slurm older than 24.05.2 is not supported", and nodes on them
  can still be watched as plain hosts.
- Agents on the nodes of running jobs give per-node detail through `relay` or `job-step`. Nodes without your jobs
  show scheduler data only.
- Polling is gentle: every 15 seconds or more, never in a tight loop against `slurmctld`.

PBS and LSF use the same adapter interface later.

## Terminals

A terminal in nerdfleet is a pane, nerdmux's terminal engine (alacritty_terminal and portable-pty), running a local
pty with one of:

- **Host:** `ssh -t <alias> nerdmux attach --plain -t fleet`.
- **Cluster job:** `ssh -t <login alias> srun --jobid=<id> --overlap --pty $SHELL`. The shell runs inside the job's
  allocation and ends with the job; the pane says so instead of offering to reconnect.

**Persistence without nesting.** Running a full remote nerdmux inside a nerdfleet pane would put two multiplexers on
the same keys (`Alt+Space`, `Alt+arrows`) and show two status bars. Instead, nerdmux gets a `--plain` attach mode: no
status bar, no key bindings, every key passed through, so it only keeps the session alive. If nerdmux is not on the
host, the pane falls back to plain `ssh -t` and says that the shell will not survive a disconnect.

**As built (phase 4, 2026-10-25).** Checked on the Arch laptop (local, with nerdmux) and on host-a (over SSH; its
nerdmux in `~/.cargo/bin` predates `--plain`, so it got the fallback).

- **Keys.** `s` opens a shell on the selected host from the overview, or on the host in view. It is full screen, under
  a one-line bar naming the host and the way back. Every key goes to the shell except `Ctrl+]`, which goes back to the
  dashboard (terminals send it as byte 0x1d, which crossterm reads as `Ctrl+5`; both spellings count). `s` again
  returns to the same shell, which kept running meanwhile. Pastes arrive bracketed.
- **One shell per host.** It lives as long as the viewer. When it ends (the user typed `exit`, the connection dropped)
  the bar says so and `Enter` starts a new one; with nerdmux on the host, that one attaches to the same session.
- **What runs.** A fixed script, the same on every host, that looks for a nerdmux whose `--help` mentions `--plain`
  on PATH, in `~/.local/bin` and in `~/.cargo/bin` (non-interactive SSH rarely has the last two on PATH). Paths with
  anything but plain characters are skipped, checked with `grep`: dash, Ubuntu's `sh`, reads `[^...]` in a `case`
  pattern literally. It starts
  `nerdmux attach --plain -t fleet` through the user's login shell, so the session's shells get a login PATH. Without
  one it prints why and starts a login shell. On this machine nerdfleet runs nerdmux, or `$SHELL`, directly.
- **A server of its own.** `attach --plain` uses `plain.sock` beside nerdmux's default socket, so the fleet session
  stays out of the user's session list, `kill-server` and restore, and an older nerdmux server already running
  cannot refuse it.
- **SSH.** The same options as the agent's connections (shared sign-in through the control socket, no agent, X11 or
  port forwarding, no local commands), with `-t` and without `BatchMode`: a terminal is where a password or second
  factor prompt can be answered. The remote command skips the agent's preamble, whose `umask 077` would otherwise be
  the user's.
- **Environment.** The viewer's own, so `SSH_AUTH_SOCK` reaches `ssh`, minus nerdmux's variables, with
  `TERM=xterm-256color`, which every host has terminfo for.
- **Untrusted output.** The pane is a real terminal emulator, as safe as `ssh` in any terminal. The window title a
  host sets is cleaned before it is drawn in the bar. Clipboard writes (OSC 52) and bells are dropped, so a host
  cannot set this machine's clipboard.
- **Latency.** Input is read on a thread and wakes the loop together with pane output; frames are at most every 12 ms
  while a shell is busy.
- **Not built yet:** the split with the dashboard, mouse in panes, cluster-job terminals (phase 5).

## UI

- **Overview:** a grid of host cards, one per host or cluster.
  - Each card shows CPU, memory and one bar per GPU device, sparklines for the last few minutes, and its workloads
    with their kind (inference, training, CPU).
  - A stalled training run is highlighted.
  - A card goes dim when its agent is unreachable.
- **Host:** graphs for each metric group, a GPU panel per device, and the workload list. Enter on a workload shows its
  processes and details; `s` opens a terminal (`t` already switches the graphs' time window).
- **Cluster:** jobs, partitions, and a node heatmap. Enter on a job shows its nodes with live detail.
- **Terminal:** full-screen or split with the dashboard.

Keys: arrows (or `Alt+arrows`, as in nerdmux) move between cards, and plain letters act on the dashboard: `q` quits.
There are no vi-style letter keys. Run inside nerdmux, nerdfleet never sees nerdmux's leader (`Alt+Space`), so
nerdfleet does not use a leader of its own on the dashboard. In a terminal every key goes to the shell except
`Ctrl+]`, which goes back to the dashboard (see [Terminals](#terminals)). The theme is read from nerdmux's config, so
both tools look the same.

Every string that came from a host (hostnames, process names, command lines, model names, container names) is
stripped of control characters and escape sequences before it is drawn. See [Security](#security).

## Configuration

`~/.config/nerdfleet/config.toml` (`$XDG_CONFIG_HOME` respected; `~/Library/Application Support/nerdfleet/` is not
used on macOS so dotfiles stay portable):

```toml
[[host]]
name = "gpu-box"           # label shown in the UI
ssh = "gpu-box"            # a Host alias from ~/.ssh/config; nothing else about the connection goes here

[[host]]
name = "laptop"
local = true               # the machine nerdfleet runs on, without SSH

[sampling]
interval_ms = 1000         # 250 to 60000

[[cluster]]
name = "hpc"
login = "hpc-login"        # also an ~/.ssh/config alias
scheduler = "slurm"
nodes = "relay"            # or "job-step"

[[workload]]
label = "pretraining"
kind = "training"
match = "python .*pretrain\\.py"
```

- The config holds no secrets and has no place for one: no passwords, tokens, keys or key paths. Connection details
  belong in `~/.ssh/config`, and server credentials are not used at all.
- An `ssh` value must be a plain alias or `user@host`. Anything starting with `-`, or containing whitespace or shell
  metacharacters, is rejected when the config is loaded, so a config value can never become an SSH option or a shell
  command.
- Unknown keys are errors, so a misspelled option, or an attempt to add a `password`, is reported instead of
  ignored.
- nerdfleet ships no default hosts. A fresh install shows an empty fleet and tells you how to add one.
- The config file and its directory must be owned by the user and not writable by group or others; otherwise
  nerdfleet refuses to start, the way nerdmux checks its own private directory.

## Security

nerdfleet logs into machines that run expensive, often sensitive work. A mistake in it can give someone else that
access, so the rules below apply to the design, the code and the docs.

### Nothing hardcoded

- No hostnames, addresses, usernames, ports, key paths or tokens in code, tests or examples. Examples use placeholder
  names, as in [Configuration](#configuration).
- Paths come from the XDG base directories (or their macOS equivalents) and `$HOME`, never from literal user paths.
- Thresholds and intervals are settings with defaults, not constants scattered in code.

### SSH

- nerdfleet always uses the system OpenSSH, and its authentication and host-key settings come from the user's own SSH
  config.
- Connections the viewer starts on its own (agents, the control master) add `BatchMode=yes`, so they fail instead of
  prompting where nobody can see the prompt. The first interactive sign-in, for MFA, happens in a terminal pane the
  user is looking at.
- nerdfleet never sets `StrictHostKeyChecking=no`, never writes to `known_hosts`, and never uses agent forwarding
  (`-A`) or X11 forwarding.
- Arguments go to `ssh` as an argument vector, never through a local shell.
- The remote command is fixed by nerdfleet, not taken from config: `sh -c '<script>' nerdfleet <name> <hash>`, where
  the scripts are constants in the source and the only values passed in are the agent's file name and hash. They are
  passed as positional arguments, not pasted into the script, and checked to be plain words.
- The scripts avoid `!`, backslashes and newlines, the constructs that fish and csh-family login shells treat
  differently inside single quotes. They have been tested with bash as the login shell, and with bash and dash as
  `sh`; zsh, fish and csh are untested so far.
- `ForwardAgent=no`, `ForwardX11=no`, `ClearAllForwardings=yes` and `PermitLocalCommand=no` are always set, so a
  user's interactive SSH settings never hand anything to a host through nerdfleet.
- Control-master sockets live in a directory with mode 0700 inside the user's runtime directory, and are removed on
  exit.

### Agent deployment

- The viewer deploys only its own agent builds, which it carries inside its binary, never anything downloaded.
- The agent goes to `$XDG_CACHE_HOME/nerdfleet/` (`~/.cache/nerdfleet/` by default) as
  `agent-<version>-<target>-<hash prefix>`, mode 0700.
- It is written to a temporary file in that directory, its SHA-256 is checked, and only then is it renamed into place,
  so a half-written or corrupted agent never runs. Older agents in the directory are removed.
- Before every start, the remote command checks the file's SHA-256 against the hash the viewer expects, and refuses
  to run on a mismatch. The viewer then copies its own build again, which replaces the file.
- Every script first checks that the directory is a real directory (not a symlink), owned by the user, with mode
  exactly 0700, and stops with an error otherwise; this matters on shared cluster filesystems. The checks fail
  closed: if the mode cannot be read, that is a refusal.

### Least privilege on the host

- Agents run as the user, never with sudo, setuid or capabilities.
- Agents read only: `/proc`, `/sys`, IOKit and IOReport, scheduler commands, and container sockets the user already
  has access to.
- They never read process environments, never store command lines, and never send command lines anywhere but the
  viewer.
- HTTP fetches stay on loopback, to the server's own listening ports, without credentials (see [Inference](#inference)).
- Resident mode, if built, listens only on a Unix socket with mode 0600 in the user's runtime directory. It checks
  the peer's user ID on every connection (`SO_PEERCRED` on Linux, `getpeereid` on macOS).

### Untrusted input

The viewer treats everything that comes from a host as untrusted, because a compromised machine in the fleet must not
be able to take over the laptop that watches it.

- The protocol decoder has hard limits on frame size, list and string lengths, and timing, and fails closed.
- All remote strings are stripped of control characters and terminal escape sequences before drawing. A process
  named with an escape sequence could otherwise rewrite the screen or, in some terminals, run commands.
- Terminal panes are the exception: they run a real terminal and are as safe as `ssh` in any terminal emulator.
- The same rules apply to what the login node's relay forwards from compute nodes.

### Logging

- Logs go to the user's state directory with mode 0600, contain no command lines, and never contain anything read
  from a server's metrics endpoint.

### Dependencies

- New third-party crates are vetted before they are added: maintainers, activity, `unsafe` use, transitive
  dependencies. `cargo audit` runs in CI. See [Dependencies](#dependencies).

## Build and distribution

- One binary, `nerdfleet`, that is both viewer and agent.
- Viewer targets: x86_64 and aarch64 Linux, Apple Silicon macOS, matching nerdmux's release workflow.
- The Linux agent is built against a **glibc 2.28 baseline** (a Rocky 8 container) so it runs on older clusters and
  can still load NVML. Fully static musl builds cannot `dlopen` the NVIDIA driver library, so they are out.
- `scripts/release.sh` does this build:
  - in `build/rocky8.Dockerfile`, based on Rocky's own `rockylinux/rockylinux:8` image pinned by digest, with a pinned
    Rust toolchain;
  - as the calling user, with nerdmux mounted read-only;
  - it refuses a binary that needs a glibc newer than 2.28, and writes the binary and its SHA-256 to `dist/`.
- The `rustup-init` download is checked against the SHA-256 published beside it. That proves an intact download, not
  who built it.
- Checked on 2026-10-24: the x86_64 build runs in plain Rocky 8 and deploys to Ubuntu 26.04 hosts.
- The macOS agent is the same binary as the macOS viewer.
- The viewer carries the agent builds for the targets it supports, with their SHA-256 hashes, and copies the right
  one on first connect (see [Agent deployment](#agent-deployment)). Because the version and target are in the name,
  x86_64 and aarch64 nodes can share one home directory.
- Since the agent is the viewer's own program, a release is built in two stages: plain builds for every target first
  (the agents), then each viewer again with those agents inside. `build.rs` embeds what `NERDFLEET_AGENTS` names,
  refusing files of another version or whose ELF machine does not match the target in their name.
- A release viewer deploys its embedded agent even to hosts like the machine it runs on, so every viewer of a version
  puts byte-identical files on hosts. A development build carries no agents and deploys itself, so hosts of another
  target are reported, with the targets it does carry.
- The viewer checks that the agent's `Hello` names the target the host reported.
- **Today (2026-10-25):** release viewers for x86_64 and aarch64 Linux, each carrying agents for both. aarch64 is
  cross-built with `cargo-zigbuild` against glibc 2.28 (no dependency compiles C, so zig is only the linker). The
  aarch64 agent has passed the agent protocol test under qemu user emulation with a Rocky 8 aarch64 userland, not on
  aarch64 hardware yet. The x86_64 release viewer deployed its embedded agent to host-b. macOS waits for phase 6 and a
  Mac: there is no macOS agent, and the macOS viewer needs a Mac (or a CI runner) to build.
- A development build on a rolling distribution (Arch) may need a newer glibc than an older host has; the Arch dev
  build needed 2.39. Use the release build as the viewer when the fleet includes older hosts, since the viewer
  deploys itself.

## Changes to nerdmux

These are made in nerdmux, as separate commits there, with its tests passing on Linux and macOS.

1. **Reusable panes.** `pane.rs` currently sends `server::Event` and uses `status`, and `render.rs` draws a whole
   `Server`.
   - `Pane` sends its events through a small type it owns, which nerdmux's server and nerdfleet each adapt.
   - Drawing a pane is split out of drawing the whole server.
   - nerdmux's behavior does not change.
2. **`nerdmux attach --plain`.** It attaches without a status bar or key bindings and passes every key through, so a
   session can be kept alive inside another program that owns the keys. It detaches when the connection closes.

Done (2026-10-25), in nerdmux `72b61aa` and `ac14b01`:

- `Pane::spawn` takes any sender whose events convert from `PaneEvent`; the server converts them to its own. A pane
  can be started without a server socket, and then sets no `NERDMUX` variables.
- `render::draw_pane`, `client::convert_key` and `client::convert_mouse` are public.
- `attach --plain -t NAME` is a new request, so the protocol version stays at 6 and running servers stay compatible.
  It takes the session by exact name and makes it if needed, on `plain.sock` unless `-S` or `NERDMUX_SOCKET` says
  otherwise. A session driven by a plain client has no status bar. Its keys skip the leader, chords and copy mode.
- Tested end to end in nerdmux's suite on Linux. macOS waits for the Mac.

nerdfleet depends on nerdmux as a crate, by path while both are in development and by version once nerdmux
publishes.

## Dependencies

Already in nerdmux: ratatui, crossterm, alacritty_terminal, portable-pty, serde, toml_edit, libc, anyhow.

Decided (2026-10-24):

- **NVML:** plain `libc::dlopen`, with our own minimal bindings for the ten NVML calls we use. No new crate.
- **Linux metrics:** by hand from `/proc` and `/sys`, like `procinfo.rs`. No new crate.
- **Hashing:** `sha2` (RustCrypto), for agent checks.

Candidates to vet, none decided:

- **macOS metrics:** hand-written `libc`/IOKit calls (`core-foundation` and `io-kit-sys` or similar) against a
  system-metrics crate such as `sysinfo`. IOReport has no maintained crate and would be our own bindings.
- **Slurm:** subprocess plus `serde_json`, the one new crate it needs.
- **Prometheus `/metrics`:** a minimal HTTP/1.1 GET over a loopback `TcpStream` and a small text-format parser,
  instead of an HTTP client crate.

## Phases

1. **Metrics agent and overview.**
   - Agent with Linux samplers (CPU, memory, disk, network, NVIDIA, AMD) and the protocol with routes, limits and the
     version handshake.
   - The glibc 2.28 build and secure agent deployment.
   - The config with its checks, and the overview screen.
   - Done when host-a and host-b are live, host-a shows three GPU devices with the right vendors, host-b shows GTT, and a
     malformed-frame test and an escape-sequence test pass.
   - **Status (2026-10-24):** working on host-a and host-b (Ubuntu) and on an Arch laptop, including fresh deploys,
     refusal of an open agent directory, clean shutdown, laptop dGPUs left to sleep, and the glibc 2.28 release
     build. Not done yet: CI, and building the viewer for macOS (nothing has been compiled for macOS yet).
2. **Workload and host view.**
   - Inference, training and high-CPU detectors, with per-process GPU usage (NVML, DRM fdinfo) and the stall check.
   - Graphs from history and the GPU panel.
   - **Status (2026-10-24):**
     - Inference checked live: llama-swap with a Vulkan llama-server on the APU box, and an idle Ollama on the laptop.
     - CPU checked live: a 12-process data export on the GPU box.
     - Desktop GPU users stay out.
     - The host view opens with `Enter`.
     - Checked against a real training run on the GPU box (2026-10-24):
       - NVML's process structs decode correctly: 24.9 GiB per rank, the same as `nvidia-smi --query-compute-apps`.
       - The experiment runner, `torchrun` and two ranks group into one training workload (8 processes).
       - Per-GPU busy matches, at 94–100%.
       - The run listens on about 16 ports (PyTorch's elastic agent and every rank), which confirms that ports must
         not decide inference.
       - Both cards report the software power cap (`0x4`) throughout, which is normal under load.
     - The stall clock hasn't fired for real yet; it needs a run that actually stalls.
3. **Resident mode and alerts.** The systemd user unit and its hardening, the three-resolution history and its file,
   the starter alerts, the alert log, and desktop notifications.
   - **Status (2026-10-24):** built, and running as user services on the laptop and the APU box.
     - Checked: attaching, history from before the viewer connected, refusal of a tampered agent, kept builds,
       saving on stop, and the cost.
     - Not yet seen for real: an alert firing on a live host. The rules are tested with synthetic sequences.
     - The GPU box is left alone while its experiments run.
4. **Terminals.** The two nerdmux changes, terminal panes, and the split layout.
   - **Status (2026-10-25):** full-screen terminals built (see [Terminals](#terminals)). Checked live: on the laptop a
     shell in the fleet session, back and forth with `Ctrl+]`, the session surviving the viewer and reattached by
     the next one; on host-a the fallback login shell, with the host's own umask and no agent forwarding. Not built:
     the split layout. host-a's nerdmux predates `--plain` and host-b has none, so their shells do not survive a
     disconnect until a current nerdmux is installed there.
5. **Clusters.**
   - Relay, `job-step` access, the Slurm adapter and the cluster view.
   - Tested later, against a local Slurm in containers first and a real cluster when one is available.
6. **macOS agent,** once a Mac is available. CPU, memory, disk, network, the Apple GPU through IOKit (IOReport
   optional), the unified-memory display, the inference detectors for Ollama, LM Studio, MLX and llama.cpp, and
   resident mode as a launchd agent. The macOS viewer should be built and tested in CI before then, once CI exists.

### Later

PBS and LSF, Kubernetes, sending alerts off the machines, training progress from logs, safe actions with
confirmation, and a GUI front end.

## Decisions

- **TUI first,** named nerdfleet, built on nerdmux (2026-10-24).
- **Workload is inference, training and high CPU,** for an AI/ML audience (2026-10-24).
- **Macs are monitored hosts,** not only viewers (2026-10-24).
- **nerdmux changes are approved** as described in [Changes to nerdmux](#changes-to-nerdmux) (2026-10-24).
- **Slurm 24.05.2 or newer, JSON only.** Older versions show an unsupported message (2026-10-24).
- **Security rules** as in [Security](#security) apply to all host access (2026-10-24).
- **Resident mode is in,** with 24-hour history; alerts start with the starter set and grow with use (2026-10-24).
- **Linux first** (Arch/Omarchy, Ubuntu, other systemd distributions); the macOS agent waits until a Mac is available
  (2026-10-24).

## Open questions

1. **Alerts beyond the starter set,** decided as the tool gets used.
2. **A Mac to test on,** for phase 6.

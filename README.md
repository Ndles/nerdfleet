# nerdfleet

Watch a fleet of machines from one terminal: what each one is training or serving, how hard its CPUs, memory and
GPUs are working, and (soon) a shell on any of them a keystroke away. For AI/ML engineers and researchers whose work
is spread over GPU boxes, Macs serving models and clusters.

A TUI that runs on Linux and macOS, built on [nerdmux](../nerdmux). NVIDIA, AMD and Intel GPUs are found on any host,
including mixed ones, and unified-memory machines (AMD APUs) show the shared memory their models actually live in.

Early days: phases 1 and 2 of [the design](docs/design.md), live hosts and what they are doing, are working.

## Build

Needs Rust 1.85 or newer, and a nerdmux checkout next to this one.

```sh
cargo install --path .      # puts `nerdfleet` in ~/.cargo/bin
scripts/release.sh          # or: a build for glibc 2.28 and newer, in dist/ (needs Docker)
```

The viewer copies itself to each host as the agent, so if the fleet has hosts older than your machine (RHEL 8-era
clusters), run the release build.

## Use

List the hosts in `~/.config/nerdfleet/config.toml` (`nerdfleet config` prints an example):

```toml
[[host]]
name = "gpu-box"
ssh = "gpu-box"     # an alias from ~/.ssh/config

[[host]]
name = "laptop"
local = true        # this machine
```

```sh
nerdfleet                 # the overview
nerdfleet check           # what is wrong with the config, if anything
nerdfleet probe gpu-box   # connect to one host and print what it reports
```

Arrows (or `Alt+arrows`, as in nerdmux) move between hosts, `Enter` opens one, `Esc` goes back, and `q` or `Ctrl+C`
quits.

Each card lists what the host is doing: model servers (`INFR`), GPU jobs (`TRAIN`) and jobs keeping the CPUs busy
(`CPU`), named the way you would name them (`torchrun train.py`, `llama-swap · Qwen3-8B-Q4_K_M`). The host view adds
graphs, every GPU in full, each workload's processes and command line (with keys and passwords masked on the host),
and the busiest processes.

## Resident agents

By default the agent on a host runs only while you watch. To keep 24 hours of history and get alerts while you are
away (a training run that stalls, a model server that stops, a disk filling up), make it resident:

```sh
nerdfleet resident enable gpu-box     # a systemd user service; no root needed
nerdfleet resident status gpu-box
nerdfleet resident disable gpu-box    # --purge also removes its saved history
```

A user service stops at your last logout unless lingering is on for your user; `enable` tells you, and prints the
command an admin would run. In the host view, `t` switches the graphs between 15 minutes, 6 hours and 24 hours.

## How it reaches hosts

- **Your own SSH setup.** nerdfleet runs the system `ssh` with the host's alias, so the address, user, key, jump hosts
  and host keys all come from `~/.ssh/config`. The config file has no place for passwords, tokens or keys.
- **No ports, no daemons, no root.** On first contact nerdfleet copies its own binary to `~/.cache/nerdfleet/` on the
  host (mode 700, checked against its SHA-256 before every start). It runs as you, only reads `/proc`, `/sys` and the
  GPU drivers, and exits when you quit.
- **Hosts are not trusted.** Everything an agent sends is size-limited and cleaned of escape sequences before it is
  drawn, so a compromised machine cannot take over the terminal that watches it.

See [Security](docs/design.md#security) in the design for the full list.

## License

MIT.

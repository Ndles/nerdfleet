//! nerdfleet: the viewer, and the agent it starts on each host.

use nerdfleet::{agent, config, paths, resident_ctl, viewer};

const USAGE: &str = "\
nerdfleet - watch a fleet of machines from one terminal

USAGE
  nerdfleet                   open the overview of every host in the config
  nerdfleet config            print an example config file
  nerdfleet config --path     print where the config file lives
  nerdfleet check             check the config file and say what is wrong
  nerdfleet probe NAME        connect to one host and print what its agent
                              reports, without the dashboard
  nerdfleet resident enable NAME
                              keep an agent running on NAME as a user
                              service: 24 hours of history, and alerts
  nerdfleet resident disable NAME [--purge]
                              stop and remove it (--purge: its saved history
                              and alerts too)
  nerdfleet resident status NAME

OPTIONS
  -h, --help                  this text
  -V, --version               version

Hosts are aliases from ~/.ssh/config: nerdfleet connects with your own SSH
setup and copies its agent to ~/.cache/nerdfleet on each host. Nothing runs
there after you quit, unless you make it resident.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        [] => viewer::run(&paths::config_path()),
        // Started by the viewer on each host.
        ["agent", "--stdio"] => agent::run_stdio(),
        // Started by systemd on hosts with a resident agent.
        ["agent", "--resident"] => agent::run_resident(),
        ["resident", "enable", name] => with_host(name, resident_ctl::enable),
        ["resident", "disable", name] => with_host(name, |h, dir| resident_ctl::disable(h, dir, false)),
        ["resident", "disable", name, "--purge"] => with_host(name, |h, dir| resident_ctl::disable(h, dir, true)),
        ["resident", "status", name] => with_host(name, resident_ctl::status),
        ["config"] => {
            print!("{}", config::TEMPLATE);
            Ok(())
        }
        ["config", "--path"] => {
            println!("{}", paths::config_path().display());
            Ok(())
        }
        ["check"] => check(),
        ["probe", name] => viewer::probe(&paths::config_path(), name),
        ["-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        ["-V" | "--version"] => {
            println!("nerdfleet {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("nerdfleet: unknown arguments\nTry `nerdfleet --help`.");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("nerdfleet: {e:#}");
        std::process::exit(1);
    }
}

/// Runs `f` for the host called `name` in the config.
fn with_host(name: &str, f: impl FnOnce(&config::Host, &std::path::Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let path = paths::config_path();
    let c = config::Config::load(&path)?;
    let Some(host) = c.hosts.iter().find(|h| h.name == name) else {
        anyhow::bail!("no host named {name:?} in {}", path.display());
    };
    let control_dir = paths::private_dir(&paths::runtime_dir().join("ssh"))?;
    let result = f(host, &control_dir);
    if let config::Access::Ssh(alias) = &host.access {
        if let Ok(ssh) = nerdfleet::remote::Ssh::new(alias, &control_dir) {
            ssh.close();
        }
    }
    result
}

fn check() -> anyhow::Result<()> {
    let theme = nerdmux::embed::theme_path();
    if theme.exists() {
        let problems = nerdmux::embed::check_theme();
        println!("{}: {}", theme.display(), if problems.is_empty() { "fine" } else { "used, except" });
        for p in problems {
            println!("  {p}");
        }
    }
    let path = paths::config_path();
    let c = config::Config::load(&path)?;
    if !path.exists() {
        println!("{} does not exist yet: the fleet is empty. `nerdfleet config` prints an example.", path.display());
        return Ok(());
    }
    println!("{}: {} host(s), sampling every {} ms", path.display(), c.hosts.len(), c.interval_ms);
    for h in &c.hosts {
        match &h.access {
            config::Access::Ssh(alias) => println!("  {:<16} ssh {alias}", h.name),
            config::Access::Local => println!("  {:<16} this machine", h.name),
        }
    }
    Ok(())
}

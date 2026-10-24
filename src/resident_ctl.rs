//! `nerdfleet resident enable|disable|status <host>`: putting a resident
//! agent on a host as a systemd user service, and taking it off again.
//!
//! The unit runs the same verified agent file the viewer deploys: its start
//! command checks the agent directory and the file's SHA-256 before it
//! executes anything, every time it starts. Nothing here needs root.
//! Lingering (keeping user services running after the last logout) is an
//! admin setting, so nerdfleet only says whether it is on, and what to run.

use crate::config::{Access, Host};
use crate::remote::{self, AgentBuild, Ssh, PREPARE};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Where the unit goes, as a shell expression on the host.
/// Succeeds if agent `$1` is there with SHA-256 `$2`.
pub const VERIFY: &str = "f=\"$d/$1\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$2\" || exit 98; echo ok";

/// Installs the unit from stdin, starts it, and reports lingering. The
/// state directory is made here, not by systemd's `StateDirectory=`: with
/// a config directory of the same name present, systemd takes the state
/// directory for a pre-253 layout and links it to the config directory.
pub const ENABLE: &str = "g=\"${XDG_CONFIG_HOME:-}\"; case \"$g\" in /*) ;; *) g=\"$HOME/.config\";; esac; \
    u=\"$g/systemd/user\"; mkdir -p \"$u\" || exit 99; \
    s=\"${XDG_STATE_HOME:-}\"; case \"$s\" in /*) ;; *) s=\"$HOME/.local/state\";; esac; \
    mkdir -p \"$s/nerdfleet\" || exit 99; test -L \"$s/nerdfleet\" && exit 99; chmod 700 \"$s/nerdfleet\"; \
    t=\"$(mktemp \"$u/.nerdfleet.XXXXXX\")\" || exit 99; cat > \"$t\" || { rm -f \"$t\"; exit 99; }; \
    mv -f \"$t\" \"$u/nerdfleet-agent.service\" || exit 99; \
    command -v systemctl > /dev/null || exit 95; \
    systemctl --user daemon-reload || exit 94; \
    systemctl --user enable nerdfleet-agent.service > /dev/null 2>&1 || exit 94; \
    systemctl --user restart nerdfleet-agent.service || exit 93; \
    sleep 2; systemctl --user is-active --quiet nerdfleet-agent.service || exit 93; \
    for o in \"$d\"/agent-*; do test \"${o##*/}\" = \"$1\" || rm -f \"$o\"; done; \
    echo \"linger=$(loginctl show-user \"$(id -un)\" -p Linger --value 2>/dev/null) user=$(id -un)\"";

/// Stops and removes the unit; with `$1` = `purge`, the saved state too.
pub const DISABLE: &str = "g=\"${XDG_CONFIG_HOME:-}\"; case \"$g\" in /*) ;; *) g=\"$HOME/.config\";; esac; \
    u=\"$g/systemd/user\"; \
    systemctl --user disable --now nerdfleet-agent.service > /dev/null 2>&1; \
    rm -f \"$u/nerdfleet-agent.service\"; systemctl --user daemon-reload > /dev/null 2>&1; \
    if test \"$1\" = purge; then s=\"${XDG_STATE_HOME:-}\"; case \"$s\" in /*) ;; *) s=\"$HOME/.local/state\";; esac; \
    rm -f \"$s/nerdfleet/agent-state.bin\" \"$s/nerdfleet/.agent-state.tmp\"; fi; echo ok";

/// Whether the unit is there, enabled and running, and lingering.
pub const STATUS: &str = "echo \"active=$(systemctl --user is-active nerdfleet-agent.service 2>/dev/null) \
    enabled=$(systemctl --user is-enabled nerdfleet-agent.service 2>/dev/null) \
    linger=$(loginctl show-user \"$(id -un)\" -p Linger --value 2>/dev/null) user=$(id -un)\"";

/// The script the unit starts: the same checks as every other start, then
/// the agent in resident mode.
const RUN_RESIDENT: &str = "f=\"$d/$1\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$2\" || exit 98; exec \"$f\" agent --resident";

/// systemd expands `$` and `%` in command lines; the script means them
/// literally.
fn systemd_escape(s: &str) -> String {
    s.replace('%', "%%").replace('$', "$$")
}

/// The unit file for `agent`.
///
/// Sandboxing: no new privileges, Unix sockets only, the syscall
/// restrictions, and limits on memory, CPU and tasks; all enforced on both
/// Arch and Ubuntu. Filesystem protections (`ProtectSystem`, `ProtectHome`,
/// `PrivateTmp`) are left out: in a user unit systemd gets them by putting
/// the service in a user namespace of its own, and from there the kernel
/// no longer lets it read other processes' fd tables and fdinfo, which is
/// how the agent sees ports and per-process GPU use. (On Ubuntu 26.04 they
/// do not even protect the filesystem: they are accepted and not enforced.)
/// What keeps the agent harmless is that it only reads.
pub fn unit(agent: &AgentBuild) -> String {
    let script = systemd_escape(&format!("{PREPARE}{RUN_RESIDENT}"));
    format!(
        "# Written by nerdfleet: the resident agent. `nerdfleet resident disable` removes it.
[Unit]
Description=nerdfleet resident agent

[Service]
Type=simple
ExecStart=/bin/sh -c '{script}' nerdfleet {name} {hash}
Restart=on-failure
RestartSec=10
Nice=10
UMask=0077
RuntimeDirectory=nerdfleet
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=yes
NoNewPrivileges=yes
LockPersonality=yes
RestrictRealtime=yes
RestrictNamespaces=yes
SystemCallArchitectures=native
RestrictAddressFamilies=AF_UNIX
CPUQuota=10%
MemoryMax=128M
TasksMax=64

[Install]
WantedBy=default.target
",
        name = agent.name,
        hash = agent.hash,
    )
}

/// Where to run a host's scripts: over SSH, or on this machine.
enum Target {
    Ssh(Ssh),
    Local,
}

impl Target {
    fn of(host: &Host, control_dir: &Path) -> anyhow::Result<Target> {
        Ok(match &host.access {
            Access::Ssh(alias) => Target::Ssh(Ssh::new(alias, control_dir)?),
            Access::Local => Target::Local,
        })
    }

    fn script(&self, script: &str, args: &[&str]) -> Command {
        match self {
            Target::Ssh(ssh) => ssh.script(script, args),
            Target::Local => {
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(format!("{PREPARE}{script}")).arg("nerdfleet").args(args);
                cmd
            }
        }
    }

    /// Runs `script`, feeding it `input`, and returns its output or a
    /// failure in words.
    fn run(&self, script: &str, args: &[&str], input: &[u8], what: &str) -> anyhow::Result<String> {
        let mut child =
            self.script(script, args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let written = child.stdin.take().expect("piped").write_all(input);
        let out = child.wait_with_output()?;
        let stdout = crate::text::clean(String::from_utf8_lossy(&out.stdout).trim());
        if out.status.success() {
            written?;
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let why = match out.status.code() {
            Some(93) => "the service did not start; `journalctl --user -u nerdfleet-agent` on the host says why".into(),
            Some(94) => "systemd refused the unit (is `systemctl --user` usable in an SSH session there?)".into(),
            Some(95) => "the host has no systemd, which resident mode needs".into(),
            code => remote::explain(code, &stderr, what),
        };
        anyhow::bail!("{why}")
    }
}

/// `key=value` pairs from a script's report.
fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')).unwrap_or("")
}

fn lingering_hint(host: &str, report: &str) {
    let user = field(report, "user");
    if field(report, "linger") == "yes" {
        println!("Lingering is on for {user}, so it keeps running after you log out of {host}.");
    } else if user.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) && !user.is_empty() {
        println!(
            "Lingering is off for {user}: the agent stops when your last session on {host} ends.\n\
             To keep it running, an admin runs this on {host}:  sudo loginctl enable-linger {user}"
        );
    }
}

pub fn enable(host: &Host, control_dir: &Path) -> anyhow::Result<()> {
    let target = Target::of(host, control_dir)?;
    let agent = AgentBuild::current()?;
    if let Target::Ssh(ssh) = &target {
        let t = ssh.probe()?;
        if t != agent.target {
            anyhow::bail!("{} is {t}, and this nerdfleet only carries an agent for {}", host.name, agent.target);
        }
    }
    let args = [agent.name.as_str(), agent.hash.as_str()];
    if target.run(VERIFY, &args, b"", &host.name).is_err() {
        println!("Copying the agent to {}…", host.name);
        target.run(remote::UPLOAD_SCRIPT, &args, &agent.bytes, &host.name)?;
        target.run(VERIFY, &args, b"", &host.name)?;
    }
    let report = target.run(ENABLE, &args, unit(&agent).as_bytes(), &host.name)?;
    println!("The resident agent runs on {}: it keeps 24 hours of history and raises alerts on its own.", host.name);
    lingering_hint(&host.name, &report);
    Ok(())
}

pub fn disable(host: &Host, control_dir: &Path, purge: bool) -> anyhow::Result<()> {
    let target = Target::of(host, control_dir)?;
    target.run(DISABLE, &[if purge { "purge" } else { "keep" }], b"", &host.name)?;
    if purge {
        println!("The resident agent on {} is stopped and removed, with its saved history and alerts.", host.name);
    } else {
        println!(
            "The resident agent on {} is stopped and removed. Its saved history stays; --purge removes it.",
            host.name
        );
    }
    Ok(())
}

pub fn status(host: &Host, control_dir: &Path) -> anyhow::Result<()> {
    let target = Target::of(host, control_dir)?;
    let report = target.run(STATUS, &[], b"", &host.name)?;
    match (field(&report, "active"), field(&report, "enabled")) {
        ("active", _) => println!("The resident agent on {} is running.", host.name),
        (_, "enabled") => println!("The resident agent on {} is enabled but not running.", host.name),
        _ => println!("No resident agent on {}. `nerdfleet resident enable {}` starts one.", host.name, host.name),
    }
    lingering_hint(&host.name, &report);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentBuild {
        AgentBuild {
            bytes: Vec::new(),
            hash: "ab".repeat(32),
            name: "agent-0.1.0-x86_64-linux-abababababab".into(),
            target: "x86_64-linux".into(),
        }
    }

    #[test]
    fn the_unit_escapes_what_systemd_would_expand() {
        let u = unit(&agent());
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        // Every `$` and `%` doubled; none left single.
        assert!(!exec.replace("$$", "").contains('$'), "{exec}");
        assert!(!exec.replace("%%", "").contains('%'), "{exec}");
        assert!(exec.ends_with(&format!("' nerdfleet {} {}", agent().name, agent().hash)));
        assert!(u.contains("NoNewPrivileges=yes") && u.contains("RestrictAddressFamilies=AF_UNIX"));
    }

    #[test]
    fn reports_are_read_by_key() {
        let r = "active=active enabled=enabled linger=no user=alice";
        assert_eq!(field(r, "linger"), "no");
        assert_eq!(field(r, "user"), "alice");
        assert_eq!(field(r, "missing"), "");
    }
}

//! `nerdfleet resident enable|disable|status <host>`: putting a resident
//! agent on a host as a systemd user service, and taking it off again.
//!
//! The unit runs the same verified agent file the viewer deploys: its start
//! command checks the agent directory and the file's SHA-256 before it
//! executes anything, every time it starts. Nothing here needs root.
//! Lingering (keeping user services running after the last logout) is an
//! admin setting, so nerdfleet only says whether it is on, and what to run.
//!
//! Beside the agent's unit go two for the alert hook (see `agent::notify`):
//! a path unit watching the outbox, and the service it starts. The agent
//! stays without network access; only the hook's service has it.

use crate::config::{Access, Host};
use crate::remote::{self, AgentBuild, Agents, Ssh, PREPARE};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// `concat!` for `&str` constants.
macro_rules! concatcp {
    ($($s:expr),+ $(,)?) => {{
        const PARTS: &[&str] = &[$($s),+];
        const LEN: usize = {
            let mut n = 0;
            let mut i = 0;
            while i < PARTS.len() {
                n += PARTS[i].len();
                i += 1;
            }
            n
        };
        const BYTES: [u8; LEN] = {
            let mut out = [0u8; LEN];
            let mut n = 0;
            let mut i = 0;
            while i < PARTS.len() {
                let b = PARTS[i].as_bytes();
                let mut j = 0;
                while j < b.len() {
                    out[n] = b[j];
                    n += 1;
                    j += 1;
                }
                i += 1;
            }
            out
        };
        match std::str::from_utf8(&BYTES) {
            Ok(s) => s,
            Err(_) => panic!("not UTF-8"),
        }
    }};
}

/// Where the unit goes, as a shell expression on the host.
/// Succeeds if agent `$1` is there with SHA-256 `$2`.
pub const VERIFY: &str = "f=\"$d/$1\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$2\" || exit 98; echo ok";

/// The user's config and state directories, as the agent works them out.
const DIRS: &str = "g=\"${XDG_CONFIG_HOME:-}\"; case \"$g\" in /*) ;; *) g=\"$HOME/.config\";; esac; \
    u=\"$g/systemd/user\"; \
    s=\"${XDG_STATE_HOME:-}\"; case \"$s\" in /*) ;; *) s=\"$HOME/.local/state\";; esac; ";

/// Whether the alert hook is there and executable.
const HOOK: &str = "hook=$(test -x \"$g/nerdfleet/on-alert\" && echo yes || echo no)";

/// Writes stdin as unit `$1`, one of the services nerdfleet makes.
pub const WRITE_UNIT: &str = concatcp!(
    "case \"$1\" in nerdfleet-agent.service|nerdfleet-notify.service) ;; *) exit 99;; esac; ",
    DIRS,
    "mkdir -p \"$u\" || exit 99; \
    t=\"$(mktemp \"$u/.nerdfleet.XXXXXX\")\" || exit 99; cat > \"$t\" || { rm -f \"$t\"; exit 99; }; \
    mv -f \"$t\" \"$u/$1\" || exit 99; echo ok"
);

/// Writes the path unit, starts the units, and reports lingering and the
/// hook. The path unit is written here because it names the outbox by its
/// full path: `%S` meant `~/.config` in user units before systemd 253. The
/// state directory is made here too, not by systemd's `StateDirectory=`:
/// with a config directory of the same name present, systemd takes the
/// state directory for a pre-253 layout and links it to the config
/// directory.
pub const ENABLE: &str = concatcp!(
    DIRS,
    "mkdir -p \"$s/nerdfleet\" || exit 99; test -L \"$s/nerdfleet\" && exit 99; chmod 700 \"$s/nerdfleet\"; \
    o=\"$s/nerdfleet/outbox\"; case \"$o\" in *%*) exit 99;; esac; \
    mkdir -p \"$o\" || exit 99; test -L \"$o\" && exit 99; chmod 700 \"$o\"; \
    t=\"$(mktemp \"$u/.nerdfleet.XXXXXX\")\" || exit 99; \
    { echo \"# Written by nerdfleet: starts the alert hook. nerdfleet resident disable removes it.\"; \
    echo \"[Unit]\"; echo \"Description=nerdfleet alert hook: the outbox\"; \
    echo \"[Path]\"; echo \"DirectoryNotEmpty=$o\"; \
    echo \"[Install]\"; echo \"WantedBy=default.target\"; } > \"$t\" || { rm -f \"$t\"; exit 99; }; \
    mv -f \"$t\" \"$u/nerdfleet-notify.path\" || exit 99; \
    command -v systemctl > /dev/null || exit 95; \
    systemctl --user daemon-reload || exit 94; \
    systemctl --user enable nerdfleet-agent.service nerdfleet-notify.path > /dev/null 2>&1 || exit 94; \
    systemctl --user restart nerdfleet-notify.path || exit 93; \
    systemctl --user restart nerdfleet-agent.service || exit 93; \
    sleep 2; systemctl --user is-active --quiet nerdfleet-agent.service || exit 93; \
    for o in \"$d\"/agent-*; do test \"${o##*/}\" = \"$1\" || rm -f \"$o\"; done; ",
    HOOK,
    "; echo \"linger=$(loginctl show-user \"$(id -un)\" -p Linger --value 2>/dev/null) user=$(id -un) hook=$hook\""
);

/// Stops and removes the units; with `$1` = `purge`, the saved state and
/// the alerts waiting for the hook too. The hook itself is the user's, and
/// stays.
pub const DISABLE: &str = concatcp!(
    DIRS,
    "systemctl --user disable --now nerdfleet-notify.path nerdfleet-agent.service > /dev/null 2>&1; \
    systemctl --user stop nerdfleet-notify.service > /dev/null 2>&1; \
    rm -f \"$u/nerdfleet-agent.service\" \"$u/nerdfleet-notify.service\" \"$u/nerdfleet-notify.path\"; \
    systemctl --user daemon-reload > /dev/null 2>&1; \
    if test \"$1\" = purge; then \
    rm -f \"$s/nerdfleet/agent-state.bin\" \"$s/nerdfleet/.agent-state.tmp\"; rm -rf \"$s/nerdfleet/outbox\"; fi; echo ok"
);

/// Whether the unit is there, enabled and running, lingering, and the
/// alert hook.
pub const STATUS: &str = concatcp!(
    DIRS,
    HOOK,
    "; echo \"active=$(systemctl --user is-active nerdfleet-agent.service 2>/dev/null) \
    enabled=$(systemctl --user is-enabled nerdfleet-agent.service 2>/dev/null) \
    notify=$(systemctl --user is-active nerdfleet-notify.path 2>/dev/null) \
    linger=$(loginctl show-user \"$(id -un)\" -p Linger --value 2>/dev/null) user=$(id -un) hook=$hook\""
);

/// Has the resident's agent put a test alert in the outbox, waits for the
/// hook's service to take it and finish, and reports how that went. The
/// agent and its hash come from the hook's unit, so it is the resident's
/// own agent, whatever version this viewer is.
pub const TEST_ALERT: &str = concatcp!(
    DIRS,
    "l=\"$(grep -o \"nerdfleet agent-[A-Za-z0-9._-]* [0-9a-f]*\" \"$u/nerdfleet-notify.service\" 2>/dev/null | head -n 1)\"; \
    test -n \"$l\" || exit 91; set -- $l; \
    f=\"$d/$2\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$3\" || exit 98; \
    \"$f\" agent --test-alert || exit 90; \
    n=0; while test -n \"$(ls -A \"$s/nerdfleet/outbox\" 2>/dev/null)\"; do \
    n=$((n+1)); test $n -gt 10 && exit 89; sleep 1; done; \
    n=0; while systemctl --user is-active --quiet nerdfleet-notify.service; do \
    n=$((n+1)); test $n -gt 30 && break; sleep 1; done; \
    echo \"result=$(systemctl --user show nerdfleet-notify.service -p Result --value 2>/dev/null)\""
);

/// The script the unit starts: the same checks as every other start, then
/// the agent in resident mode.
const RUN_RESIDENT: &str = "f=\"$d/$1\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$2\" || exit 98; exec \"$f\" agent --resident";

/// The same, for the service that runs the alert hook.
const RUN_NOTIFY: &str = "f=\"$d/$1\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$2\" || exit 98; exec \"$f\" agent --notify";

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

/// The unit that runs the alert hook, started by the path unit whenever the
/// outbox has something in it.
///
/// It keeps the agent's protections except the one that matters to a hook:
/// it may open network sockets, since sending the alert somewhere is the
/// hook's whole job. No resource limits beyond tasks and memory, and a
/// generous timeout: the hook is the user's own program, and each run of it
/// is cut off after 20 seconds by the agent anyway.
///
/// `host` is what the config calls the host, which the hook gets as
/// `NERDFLEET_HOST`: the name the user knows it by in nerdfleet.
pub fn notify_unit(agent: &AgentBuild, host: &str) -> String {
    let script = systemd_escape(&format!("{PREPARE}{RUN_NOTIFY}"));
    // A quoted `Environment=` value: backslashes and quotes escaped, `%`
    // doubled. Config names have no control characters.
    let host = crate::text::clean(host).replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    let var = crate::agent::NOTIFY_NAME_VAR;
    format!(
        "# Written by nerdfleet: runs ~/.config/nerdfleet/on-alert for each new alert. `nerdfleet resident disable` removes it.
[Unit]
Description=nerdfleet alert hook

[Service]
Type=oneshot
Environment=\"{var}={host}\"
ExecStart=/bin/sh -c '{script}' nerdfleet {name} {hash}
TimeoutStartSec=15min
UMask=0077
NoNewPrivileges=yes
LockPersonality=yes
RestrictRealtime=yes
SystemCallArchitectures=native
MemoryMax=256M
TasksMax=64
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
            Some(91) => format!(
                "no alert hook service on {what}: `nerdfleet resident enable {what}` puts it there \
                 (a resident agent from before nerdfleet 0.2.6 has none)"
            ),
            Some(90) => {
                let why = crate::text::clean(stderr.trim());
                let why = why.strip_prefix("nerdfleet: ").unwrap_or(&why);
                format!("{why}\nWrite one on {what} first; `nerdfleet resident status {what}` shows an example.")
            }
            Some(89) => format!(
                "the test alert was not picked up on {what}: `systemctl --user status nerdfleet-notify.path` there says why"
            ),
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

/// Puts this viewer's agent on `host` as the resident one, in place of any
/// other, and returns the script's report. Prints nothing, for the viewer's
/// sake: `copying` is told when the agent has to be copied first.
pub fn install(host: &Host, control_dir: &Path, agents: &Agents, copying: impl FnOnce()) -> anyhow::Result<String> {
    let target = Target::of(host, control_dir)?;
    let agent = match &target {
        Target::Ssh(ssh) => agents.for_target(&ssh.probe()?)?,
        Target::Local => agents.for_target(&crate::agent::target())?,
    };
    let args = [agent.name.as_str(), agent.hash.as_str()];
    if target.run(VERIFY, &args, b"", &host.name).is_err() {
        copying();
        target.run(remote::UPLOAD_SCRIPT, &args, &agent.bytes, &host.name)?;
        target.run(VERIFY, &args, b"", &host.name)?;
    }
    target.run(WRITE_UNIT, &["nerdfleet-agent.service"], unit(agent).as_bytes(), &host.name)?;
    target.run(WRITE_UNIT, &["nerdfleet-notify.service"], notify_unit(agent, &host.name).as_bytes(), &host.name)?;
    target.run(ENABLE, &args, b"", &host.name)
}

/// An example hook, for `status` and `enable` to show.
pub const HOOK_EXAMPLE: &str = "#!/bin/sh
# ~/.config/nerdfleet/on-alert: run once per alert, with
# NERDFLEET_HOST (its name in your config), NERDFLEET_HOSTNAME,
# NERDFLEET_ALERT_KIND, NERDFLEET_ALERT_SUBJECT,
# NERDFLEET_ALERT_MESSAGE, NERDFLEET_ALERT_TIME and NERDFLEET_ALERT_ID set.
exec curl -fsS -m 15 -H \"Title: $NERDFLEET_HOST: $NERDFLEET_ALERT_KIND\" \\
    -d \"$NERDFLEET_ALERT_MESSAGE\" https://ntfy.sh/your-private-topic > /dev/null";

fn hook_hint(host: &str, report: &str) {
    if field(report, "hook") == "yes" {
        println!(
            "Its alerts go to ~/.config/nerdfleet/on-alert on {host}; `nerdfleet resident test-alert {host}` tries it."
        );
    } else {
        println!(
            "To get its alerts on your phone or in chat, put an executable ~/.config/nerdfleet/on-alert on {host}, \
             for example:\n\n{HOOK_EXAMPLE}\n\nthen `chmod 700` it and try it with `nerdfleet resident test-alert {host}`."
        );
    }
}

pub fn enable(host: &Host, control_dir: &Path) -> anyhow::Result<()> {
    let agents = Agents::load()?;
    let report = install(host, control_dir, &agents, || println!("Copying the agent to {}…", host.name))?;
    println!("The resident agent runs on {}: it keeps 24 hours of history and raises alerts on its own.", host.name);
    lingering_hint(&host.name, &report);
    hook_hint(&host.name, &report);
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
        _ => {
            println!("No resident agent on {}. `nerdfleet resident enable {}` starts one.", host.name, host.name);
            return Ok(());
        }
    }
    lingering_hint(&host.name, &report);
    if field(&report, "notify") != "active" {
        println!("Its alerts are not passed to a hook: `nerdfleet resident enable {}` again sets that up.", host.name);
    } else {
        hook_hint(&host.name, &report);
    }
    Ok(())
}

pub fn test_alert(host: &Host, control_dir: &Path) -> anyhow::Result<()> {
    let target = Target::of(host, control_dir)?;
    let report = target.run(TEST_ALERT, &[], b"", &host.name)?;
    match field(&report, "result") {
        "success" => println!("The hook on {} ran and succeeded: the test alert should have arrived.", host.name),
        r => anyhow::bail!(
            "the hook on {} ran and failed ({}): `journalctl --user -u nerdfleet-notify` there says why",
            host.name,
            if r.is_empty() { "no result" } else { r }
        ),
    }
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
    fn only_the_hook_service_reaches_the_network() {
        let n = notify_unit(&agent(), "gpu \"box\" 100%");
        assert!(n.contains("\nEnvironment=\"NERDFLEET_NAME=gpu \\\"box\\\" 100%%\"\n"), "{n}");
        let exec = n.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert!(!exec.replace("$$", "").contains('$'), "{exec}");
        assert!(exec.contains("agent --notify") && exec.ends_with(&format!(" {}", agent().hash)));
        assert!(n.contains("NoNewPrivileges=yes") && !n.contains("RestrictAddressFamilies"));
        assert!(unit(&agent()).contains("RestrictAddressFamilies=AF_UNIX\n"), "the agent keeps Unix sockets only");
        // The test alert finds the agent and hash in the hook's unit.
        assert!(exec.contains(&format!("' nerdfleet {} {}", agent().name, agent().hash)));
    }

    /// Runs a script as `Target::Local` would, against a scratch HOME, with
    /// fake `systemctl` and `loginctl` that log what they are asked.
    fn sh(home: &Path, script: &str, args: &[&str], input: &[u8]) -> std::process::Output {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in ["systemctl", "loginctl"] {
            let f = bin.join(tool);
            std::fs::write(&f, format!("#!/bin/sh\necho {tool} \"$@\" >> \"$HOME/calls\"\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("{PREPARE}{script}"))
            .arg("nerdfleet")
            .args(args)
            .env("HOME", home)
            .env("PATH", path)
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn enable_writes_the_units_and_disable_removes_them() {
        let home = std::env::temp_dir().join(format!("nerdfleet-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let units = home.join(".config/systemd/user");
        let a = agent();
        assert!(sh(&home, WRITE_UNIT, &["nerdfleet-agent.service"], unit(&a).as_bytes()).status.success());
        assert!(sh(&home, WRITE_UNIT, &["nerdfleet-notify.service"], notify_unit(&a, "gpu-box").as_bytes())
            .status
            .success());
        assert_eq!(sh(&home, WRITE_UNIT, &["other.service"], b"x").status.code(), Some(99), "only our units");
        assert!(!units.join("other.service").exists());

        let out = sh(&home, ENABLE, &[&a.name, &a.hash], b"");
        assert!(out.status.success(), "{out:?}");
        let report = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(field(&report, "hook"), "no");
        let path_unit = std::fs::read_to_string(units.join("nerdfleet-notify.path")).unwrap();
        let outbox = home.join(".local/state/nerdfleet/outbox");
        assert!(path_unit.contains(&format!("\nDirectoryNotEmpty={}\n", outbox.display())), "{path_unit}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&outbox).unwrap().permissions().mode() & 0o777, 0o700);
        let calls = std::fs::read_to_string(home.join("calls")).unwrap();
        assert!(calls.contains("enable nerdfleet-agent.service nerdfleet-notify.path"), "{calls}");

        // With a hook there, the report says so.
        let hook = home.join(".config/nerdfleet/on-alert");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let report = String::from_utf8_lossy(&sh(&home, STATUS, &[], b"").stdout).to_string();
        assert_eq!(field(&report, "hook"), "yes");

        assert!(sh(&home, DISABLE, &["purge"], b"").status.success());
        for u in ["nerdfleet-agent.service", "nerdfleet-notify.service", "nerdfleet-notify.path"] {
            assert!(!units.join(u).exists(), "{u}");
        }
        assert!(!outbox.exists());
        assert!(hook.exists(), "the hook is the user's");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn reports_are_read_by_key() {
        let r = "active=active enabled=enabled linger=no user=alice";
        assert_eq!(field(r, "linger"), "no");
        assert_eq!(field(r, "user"), "alice");
        assert_eq!(field(r, "missing"), "");
    }
}

//! Reaching hosts: SSH, and putting the agent on them.
//!
//! Everything about how to reach a host (address, user, key, jump hosts,
//! host keys) comes from the user's own `~/.ssh/config`; nerdfleet passes
//! only an alias and a few options that make unattended connections safe.
//!
//! The commands run on the host are fixed scripts in this file. The only
//! values that reach them are the agent's file name and hash, which
//! nerdfleet makes itself and checks again here, passed as positional
//! arguments rather than pasted into the script.

use crate::config::check_alias;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where agents live on a host, and the checks every script runs first:
/// the directory must be a real directory, ours, closed to everyone else.
/// Every check fails closed: a `stat` that cannot tell us the mode is a
/// refusal, not a pass.
/// One line, with no `!` and no backslashes: the constructs fish and
/// csh-family login shells treat differently inside single quotes.
pub(crate) const PREPARE: &str = "umask 077; \
    c=\"${XDG_CACHE_HOME:-}\"; case \"$c\" in /*) ;; *) c=\"$HOME/.cache\";; esac; d=\"$c/nerdfleet\"; \
    mkdir -p \"$d\" || exit 99; test -L \"$d\" && exit 99; test -O \"$d\" || exit 99; \
    p=\"$(stat -c %a \"$d\" 2>/dev/null || stat -f %Lp \"$d\" 2>/dev/null)\"; test \"$p\" = 700 || exit 99; ";

/// Prints the host's OS and architecture.
const PROBE: &str = "echo \"$(uname -s) $(uname -m)\"";

/// Runs agent `$1` if its SHA-256 is `$2`.
const RUN: &str = "f=\"$d/$1\"; test -f \"$f\" || exit 97; \
    h=\"$(sha256sum \"$f\" | cut -c1-64)\"; test \"$h\" = \"$2\" || exit 98; exec \"$f\" agent --stdio";

/// Stores stdin as agent `$1` if its SHA-256 is `$2`, atomically, and
/// removes older agents, except the one a resident agent's unit runs: it
/// would not start again after a restart or a reboot without it.
const UPLOAD: &str = "t=\"$(mktemp \"$d/.upload.XXXXXX\")\" || exit 99; cat > \"$t\" || { rm -f \"$t\"; exit 99; }; \
    h=\"$(sha256sum \"$t\" | cut -c1-64)\"; \
    if test \"$h\" = \"$2\"; then chmod 700 \"$t\" && mv -f \"$t\" \"$d/$1\"; else rm -f \"$t\"; exit 98; fi; \
    g=\"${XDG_CONFIG_HOME:-}\"; case \"$g\" in /*) ;; *) g=\"$HOME/.config\";; esac; \
    k=\"$(grep -o \"agent-[A-Za-z0-9._-]*\" \"$g/systemd/user/nerdfleet-agent.service\" 2>/dev/null | head -n 1)\"; \
    for o in \"$d\"/agent-*; do b=\"${o##*/}\"; test \"$b\" = \"$1\" || test \"$b\" = \"$k\" || rm -f \"$o\"; done";

/// The upload script, for callers outside this module.
pub(crate) const UPLOAD_SCRIPT: &str = UPLOAD;

pub const EXIT_MISSING: i32 = 97;
pub const EXIT_MISMATCH: i32 = 98;
pub const EXIT_BAD_DIR: i32 = 99;
pub const EXIT_SSH: i32 = 255;

/// An agent the viewer can put on a host.
pub struct AgentBuild {
    pub bytes: Vec<u8>,
    pub hash: String,
    /// `agent-<version>-<target>-<hash prefix>`.
    pub name: String,
    pub target: String,
}

impl AgentBuild {
    fn new(bytes: Vec<u8>, target: &str) -> AgentBuild {
        let hash = hex(&Sha256::digest(&bytes));
        let name = format!("agent-{}-{target}-{}", env!("CARGO_PKG_VERSION"), &hash[..12]);
        AgentBuild { bytes, hash, name, target: target.to_string() }
    }

    /// This program, which is an agent for hosts like this machine.
    pub fn current() -> anyhow::Result<AgentBuild> {
        Ok(AgentBuild::new(std::fs::read(self_exe())?, &crate::agent::target()))
    }
}

mod embedded {
    // Written by build.rs: the agents built into this viewer.
    include!(concat!(env!("OUT_DIR"), "/agents.rs"));
}

/// The agents this viewer deploys, one per target. A release build
/// carries one for every target it supports (see build.rs), and deploys
/// those even to hosts like this machine, so every viewer of a version
/// puts the same file on a host. A development build carries none and
/// deploys itself.
pub struct Agents {
    builds: Vec<AgentBuild>,
    embedded: bool,
}

impl Agents {
    pub fn load() -> anyhow::Result<Agents> {
        let builds: Vec<AgentBuild> = embedded::AGENTS.iter().map(|(t, b)| AgentBuild::new(b.to_vec(), t)).collect();
        if builds.is_empty() {
            return Ok(Agents { builds: vec![AgentBuild::current()?], embedded: false });
        }
        Ok(Agents { builds, embedded: true })
    }

    /// The agent for hosts of `target`, such as `aarch64-linux`.
    pub fn for_target(&self, target: &str) -> anyhow::Result<&AgentBuild> {
        if let Some(b) = self.builds.iter().find(|b| b.target == target) {
            return Ok(b);
        }
        let carried: Vec<&str> = self.builds.iter().map(|b| b.target.as_str()).collect();
        let more = if self.embedded { "" } else { "; a release build carries agents for more" };
        anyhow::bail!("the host is {target}, and this nerdfleet carries agents for {} only{more}", carried.join(", "))
    }
}

/// The running binary. On Linux `/proc/self/exe` still works when the file
/// was replaced or deleted after we started, as a rebuild does.
pub fn self_exe() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nerdfleet"))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `uname -s` and `uname -m` as an agent target.
pub fn target_from_uname(line: &str) -> Option<String> {
    let (os, arch) = line.trim().split_once(' ')?;
    let os = match os {
        "Linux" => "linux",
        "Darwin" => "macos",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return None,
    };
    Some(format!("{arch}-{os}"))
}

/// A word we made ourselves (an agent name or a hash), checked anyway
/// before it goes anywhere near a remote shell.
fn plain_word(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// `sh -c '<script>' nerdfleet <args>` as one string for the remote shell.
fn remote_command(script: &str, args: &[&str]) -> String {
    assert!(!script.contains('\'') && !script.contains('!') && !script.contains('\\'));
    assert!(args.iter().all(|a| plain_word(a)));
    let mut cmd = format!("sh -c '{PREPARE}{script}' nerdfleet");
    for a in args {
        cmd.push(' ');
        cmd.push_str(a);
    }
    cmd
}

pub struct Ssh {
    alias: String,
    control_dir: PathBuf,
}

impl Ssh {
    pub fn new(alias: &str, control_dir: &Path) -> anyhow::Result<Ssh> {
        check_alias(alias)?;
        Ok(Ssh { alias: alias.to_string(), control_dir: control_dir.to_path_buf() })
    }

    /// `ssh` with the options every nerdfleet connection uses. Arguments go
    /// to `ssh` directly, never through a local shell.
    fn base(&self) -> Command {
        let mut cmd = Command::new("ssh");
        // Fail instead of prompting where nobody can see the prompt.
        cmd.args(["-T", "-o", "BatchMode=yes"]);
        cmd.args(self.options());
        cmd
    }

    /// The options shared by every connection, as `-o` arguments.
    fn options(&self) -> Vec<String> {
        let mut out = Vec::new();
        for opt in [
            // One sign-in per host, reused by every connection after it.
            "ControlMaster=auto",
            &format!("ControlPath={}/%C", self.control_dir.display()),
            "ControlPersist=120",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            // Nothing of ours is handed to the host, whatever the user's
            // config says for interactive use.
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
        ] {
            out.push("-o".into());
            out.push(opt.to_string());
        }
        out
    }

    /// The command line for a terminal running `script` on the host: with
    /// a remote terminal, and free to ask for a password or a second
    /// factor, since a terminal pane is where the user can answer. The
    /// sign-in is shared with the agent's connection like any other.
    pub fn terminal(&self, script: &str) -> Vec<String> {
        let mut argv = vec!["ssh".to_string(), "-t".to_string()];
        argv.extend(self.options());
        // Without PREPARE: its umask would carry over into the user's shell.
        assert!(!script.contains('\'') && !script.contains('!') && !script.contains('\\'));
        argv.extend(["--".to_string(), self.alias.clone(), format!("sh -c '{script}'")]);
        argv
    }

    pub(crate) fn script(&self, script: &str, args: &[&str]) -> Command {
        let mut cmd = self.base();
        cmd.arg("--").arg(&self.alias).arg(remote_command(script, args));
        cmd
    }

    /// The host's agent target, such as `x86_64-linux`.
    pub fn probe(&self) -> anyhow::Result<String> {
        let out = self.script(PROBE, &[]).stdin(Stdio::null()).output()?;
        if !out.status.success() {
            anyhow::bail!(explain(out.status.code(), &String::from_utf8_lossy(&out.stderr), &self.alias));
        }
        let line = String::from_utf8_lossy(&out.stdout);
        target_from_uname(&line)
            .ok_or_else(|| anyhow::anyhow!("unsupported system: {}", crate::text::clean(line.trim())))
    }

    pub fn upload(&self, agent: &AgentBuild) -> anyhow::Result<()> {
        use std::io::Write;
        let mut child = self
            .script(UPLOAD, &[&agent.name, &agent.hash])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let written = child.stdin.take().expect("piped").write_all(&agent.bytes);
        let out = child.wait_with_output()?;
        if !out.status.success() {
            anyhow::bail!(explain(out.status.code(), &String::from_utf8_lossy(&out.stderr), &self.alias));
        }
        written?;
        Ok(())
    }

    /// Starts the agent. Its stdin and stdout are the protocol; stderr
    /// carries SSH's own complaints.
    pub fn run(&self, agent: &AgentBuild) -> Command {
        self.script(RUN, &[&agent.name, &agent.hash])
    }

    /// Closes this host's control master, if one is running.
    pub fn close(&self) {
        let _ = self
            .base()
            .args(["-O", "exit", "--", &self.alias])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A failed connection, in words that say what to do about it.
pub fn explain(code: Option<i32>, stderr: &str, alias: &str) -> String {
    let stderr = crate::text::clean(stderr.trim());
    let hint = if stderr.contains("Host key verification failed") {
        format!("the host key is not known yet: run `ssh {alias}` once to check and accept it")
    } else if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        "the host key CHANGED since last time: check before trusting this host again".to_string()
    } else if stderr.contains("Permission denied") {
        format!("SSH refused the key: check that `ssh {alias}` works without a password prompt")
    } else if stderr.contains("Could not resolve hostname") {
        format!("no such host: is `{alias}` in ~/.ssh/config, and is the network (Tailscale?) up?")
    } else {
        match code {
            Some(EXIT_BAD_DIR) => {
                "the agent directory on the host is not private (it must be yours, mode 700): refusing to use it"
                    .to_string()
            }
            Some(EXIT_MISMATCH) => "the agent on the host did not match this build: refusing to run it".to_string(),
            _ if stderr.is_empty() => format!("ssh failed (exit {})", code.map_or("?".into(), |c| c.to_string())),
            _ => stderr.lines().last().unwrap_or_default().to_string(),
        }
    };
    hint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_host_gets_the_agent_for_its_target() {
        let build = |t: &str| AgentBuild::new(t.as_bytes().to_vec(), t);
        let release = Agents { builds: vec![build("x86_64-linux"), build("aarch64-linux")], embedded: true };
        assert_eq!(release.for_target("aarch64-linux").unwrap().bytes, b"aarch64-linux");
        assert!(release.for_target("aarch64-linux").unwrap().name.starts_with("agent-"));
        let err = release.for_target("aarch64-macos").err().unwrap().to_string();
        assert_eq!(
            err,
            "the host is aarch64-macos, and this nerdfleet carries agents for x86_64-linux, aarch64-linux only"
        );

        let dev = Agents { builds: vec![build("x86_64-linux")], embedded: false };
        let err = dev.for_target("aarch64-linux").err().unwrap().to_string();
        assert!(err.ends_with("x86_64-linux only; a release build carries agents for more"), "{err}");
    }

    #[test]
    fn targets_from_uname() {
        assert_eq!(target_from_uname("Linux x86_64\n").as_deref(), Some("x86_64-linux"));
        assert_eq!(target_from_uname("Darwin arm64").as_deref(), Some("aarch64-macos"));
        assert_eq!(target_from_uname("Linux riscv64"), None);
        assert_eq!(target_from_uname("garbage"), None);
    }

    #[test]
    fn remote_commands_are_one_quoted_script_and_plain_words() {
        let cmd = remote_command(RUN, &["agent-0.1.0-x86_64-linux-abc", "deadbeef"]);
        assert!(cmd.starts_with("sh -c '"));
        assert!(cmd.ends_with("' nerdfleet agent-0.1.0-x86_64-linux-abc deadbeef"));
        assert_eq!(cmd.matches('\'').count(), 2);
    }

    #[test]
    #[should_panic]
    fn odd_words_never_reach_a_remote_shell() {
        remote_command(RUN, &["x; rm -rf ~"]);
    }

    #[test]
    fn scripts_survive_every_login_shell() {
        for s in [
            PREPARE,
            PROBE,
            RUN,
            UPLOAD,
            crate::resident_ctl::VERIFY,
            crate::resident_ctl::ENABLE,
            crate::resident_ctl::DISABLE,
            crate::resident_ctl::STATUS,
        ] {
            assert!(!s.contains('\'') && !s.contains('!') && !s.contains('\\') && !s.contains('\n'));
        }
    }

    #[test]
    fn scripts_work_in_sh() {
        // The real scripts, run by a local sh against a scratch HOME.
        let home = std::env::temp_dir().join(format!("nerdfleet-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sh = |script: &str, args: &[&str], input: &[u8]| {
            use std::io::Write;
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(format!("{PREPARE}{script}"))
                .arg("nerdfleet")
                .args(args)
                .env("HOME", &home)
                .env_remove("XDG_CACHE_HOME")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_STATE_HOME")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            child.wait_with_output().unwrap()
        };
        let body = b"#!/bin/sh\necho agent \"$@\"\n";
        let hash = hex(&Sha256::digest(body));
        assert_eq!(sh(RUN, &["agent-x", &hash], b"").status.code(), Some(EXIT_MISSING));
        assert_eq!(sh(UPLOAD, &["agent-x", "0000"], body).status.code(), Some(EXIT_MISMATCH));
        assert!(sh(UPLOAD, &["agent-x", &hash], body).status.success());
        let out = sh(RUN, &["agent-x", &hash], b"");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "agent agent --stdio\n");
        assert_eq!(sh(RUN, &["agent-x", "0000"], b"").status.code(), Some(EXIT_MISMATCH));
        // A new build replaces older ones, except the one a resident's unit
        // runs: it must still be there when the service next starts.
        let units = home.join(".config/systemd/user");
        std::fs::create_dir_all(&units).unwrap();
        std::fs::write(units.join("nerdfleet-agent.service"), "ExecStart=/bin/sh -c '...' nerdfleet agent-x 00\n")
            .unwrap();
        let newer = b"#!/bin/sh\necho newer\n";
        let newer_hash = hex(&Sha256::digest(newer));
        let dir = home.join(".cache/nerdfleet");
        std::fs::write(dir.join("agent-stale"), b"old").unwrap();
        assert!(sh(UPLOAD, &["agent-y", &newer_hash], newer).status.success());
        assert!(dir.join("agent-y").exists(), "the new build");
        assert!(dir.join("agent-x").exists(), "the resident's build is kept");
        assert!(!dir.join("agent-stale").exists(), "others go");

        // A directory others can enter is refused.
        use std::os::unix::fs::PermissionsExt;
        let dir = home.join(".cache/nerdfleet");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(sh(RUN, &["agent-x", &hash], b"").status.code(), Some(EXIT_BAD_DIR));
        let _ = std::fs::remove_dir_all(&home);
    }
}

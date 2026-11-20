//! Alerts pushed off the host: the resident agent's new alerts, handed to a
//! hook the user writes, `~/.config/nerdfleet/on-alert`, which can send
//! them anywhere (ntfy, a chat webhook, mail).
//!
//! The resident agent never runs the hook itself: its sandbox allows Unix
//! sockets only, and keeps doing so. It drops each new alert into an outbox
//! directory, and only when a hook exists. A systemd path unit watches the
//! outbox and starts `agent --notify`, a separate service allowed onto the
//! network, which runs the hook once per alert and empties the outbox.
//!
//! The hook gets the alert in environment variables, never in a command
//! line built from it: alert text comes from process and model names,
//! which other users of the host can choose.

use crate::model::Alert;
use crate::proto::Wire;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Alerts waiting in the outbox before new ones are dropped: a hook that
/// cannot keep up must not fill the disk.
const MAX_QUEUED: usize = 100;
/// How long one run of the hook may take before it is killed.
const HOOK_TIMEOUT: Duration = Duration::from_secs(20);
/// What an outbox file starts with, and the version of its layout.
const MAGIC: u32 = 0x4f42_464e; // "NFBO"
const VERSION: u32 = 1;

pub fn hook_path() -> PathBuf {
    crate::paths::config_dir().join("on-alert")
}

pub fn outbox_dir() -> PathBuf {
    crate::paths::state_dir().join("outbox")
}

/// One alert on its way to the hook.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub id: u64,
    pub time_ms: u64,
    /// The alert kind's label: "training stalled", "disk full", "test".
    pub kind: String,
    pub subject: String,
    pub message: String,
}

impl From<&Alert> for Notice {
    fn from(a: &Alert) -> Notice {
        Notice {
            id: a.id,
            time_ms: a.time_ms,
            kind: a.kind.label().to_string(),
            subject: a.subject.clone(),
            message: a.message.clone(),
        }
    }
}

fn encode(n: &Notice) -> Vec<u8> {
    let mut buf = Vec::new();
    MAGIC.put(&mut buf);
    VERSION.put(&mut buf);
    n.id.put(&mut buf);
    n.time_ms.put(&mut buf);
    n.kind.put(&mut buf);
    n.subject.put(&mut buf);
    n.message.put(&mut buf);
    buf
}

fn decode(mut input: &[u8]) -> std::io::Result<Notice> {
    let input = &mut input;
    if u32::get(input)? != MAGIC || u32::get(input)? != VERSION {
        return Err(crate::proto::bad("not an outbox file of this version"));
    }
    Ok(Notice {
        id: u64::get(input)?,
        time_ms: u64::get(input)?,
        kind: String::get(input)?,
        subject: String::get(input)?,
        message: String::get(input)?,
    })
}

/// The outbox's waiting alerts, oldest first. Names starting with a dot
/// are not finished yet.
fn queued(outbox: &Path) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(outbox) else { return Vec::new() };
    let mut files: Vec<PathBuf> =
        dir.flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).map(|e| e.path()).collect();
    files.sort();
    files
}

/// Puts `notices` in the outbox at `outbox`, each in a file of its own
/// that appears whole. Returns how many were dropped for a full outbox.
pub fn post_to(outbox: &Path, notices: &[Notice]) -> anyhow::Result<usize> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let outbox = crate::paths::private_dir(outbox)?;
    let mut room = MAX_QUEUED.saturating_sub(queued(&outbox).len());
    let mut dropped = 0;
    for n in notices {
        if room == 0 {
            dropped += 1;
            continue;
        }
        room -= 1;
        // Ids grow, so names in id order are names in time order.
        let name = format!("{:020}", n.id);
        let tmp = outbox.join(format!(".{name}.tmp"));
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&encode(n))?;
        f.sync_all()?;
        std::fs::rename(&tmp, outbox.join(name))?;
    }
    Ok(dropped)
}

/// Called by the resident agent with each batch of new alerts. Does
/// nothing unless a hook exists, so the outbox only fills for someone.
pub fn post(alerts: &[Alert]) {
    if alerts.is_empty() || std::fs::symlink_metadata(hook_path()).is_err() {
        return;
    }
    let notices: Vec<Notice> = alerts.iter().map(Notice::from).collect();
    match post_to(&outbox_dir(), &notices) {
        Ok(0) => {}
        Ok(n) => eprintln!("nerdfleet agent: the alert outbox is full; {n} alert(s) not passed to the hook"),
        Err(e) => eprintln!("nerdfleet agent: cannot write to the alert outbox: {e}"),
    }
}

/// Refuses a hook anyone but us could have written, like the config file,
/// and one that is not executable.
fn check_hook(hook: &Path) -> anyhow::Result<()> {
    if std::fs::metadata(hook).is_err() {
        anyhow::bail!("there is no alert hook at {}", hook.display());
    }
    crate::paths::check_owned_as(hook, "the alert hook")?;
    if std::fs::metadata(hook)?.mode() & 0o100 == 0 {
        anyhow::bail!("the alert hook {} is not executable (chmod u+x)", hook.display());
    }
    Ok(())
}

/// Which host an alert is from.
pub struct Origin {
    /// What the viewer's config calls the host, as `resident enable` wrote
    /// it into the hook's unit; the hostname if it did not.
    pub name: String,
    /// The machine's own hostname.
    pub hostname: String,
}

/// The variable the hook's unit sets to the host's name in the config.
pub const NAME_VAR: &str = super::NOTIFY_NAME_VAR;

impl Origin {
    fn here() -> Origin {
        let hostname = super::linux::hostname();
        let name = std::env::var(NAME_VAR).ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| hostname.clone());
        Origin { name, hostname }
    }
}

/// The environment the hook sees for `n`, every value cleaned of control
/// characters.
pub fn hook_env(n: &Notice, host: &Origin) -> Vec<(&'static str, String)> {
    let clean = crate::text::clean;
    vec![
        ("NERDFLEET_HOST", clean(&host.name)),
        ("NERDFLEET_HOSTNAME", clean(&host.hostname)),
        ("NERDFLEET_ALERT_ID", n.id.to_string()),
        ("NERDFLEET_ALERT_TIME", (n.time_ms / 1000).to_string()),
        ("NERDFLEET_ALERT_KIND", clean(&n.kind)),
        ("NERDFLEET_ALERT_SUBJECT", clean(&n.subject)),
        ("NERDFLEET_ALERT_MESSAGE", clean(&n.message)),
    ]
}

/// Runs `hook` for one notice, killing it after `timeout`. Its output goes
/// where ours does: the journal, under the notifier's unit.
fn run_hook(hook: &Path, n: &Notice, host: &Origin, timeout: Duration) -> anyhow::Result<()> {
    let mut child =
        Command::new(hook).envs(hook_env(n, host)).current_dir(crate::paths::home()).stdin(Stdio::null()).spawn()?;
    let until = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                anyhow::bail!("the alert hook failed ({status}) for alert {}", n.id);
            }
            return Ok(());
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the alert hook took over {}s for alert {} and was stopped", timeout.as_secs(), n.id);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Empties the outbox at `outbox` through `hook`, oldest first. Each file
/// is removed before its hook runs, so a hook that fails or hangs is not
/// retried for ever. Returns how many alerts the hook took, or an error
/// when it failed for any, so the service's result says so.
pub fn drain(outbox: &Path, hook: &Path, host: &Origin, timeout: Duration) -> anyhow::Result<usize> {
    let files = queued(outbox);
    if files.is_empty() {
        return Ok(0);
    }
    let hook_ok = check_hook(hook);
    let mut sent = 0;
    let mut failed = 0;
    for f in files {
        let bytes = std::fs::read(&f);
        std::fs::remove_file(&f)?;
        let Ok(bytes) = bytes else { continue };
        let Ok(n) = decode(&bytes) else {
            eprintln!("nerdfleet notify: skipping {}: not an outbox file", f.display());
            continue;
        };
        if hook_ok.is_err() {
            continue;
        }
        match run_hook(hook, &n, host, timeout) {
            Ok(()) => sent += 1,
            Err(e) => {
                failed += 1;
                eprintln!("nerdfleet notify: {e}");
            }
        }
    }
    // Said once, after the outbox is emptied, not once per alert.
    hook_ok?;
    if failed > 0 {
        anyhow::bail!("the alert hook failed for {failed} of {} alert(s)", failed + sent);
    }
    Ok(sent)
}

/// `agent --notify`: started by the path unit whenever the outbox has
/// something in it.
pub fn run() -> anyhow::Result<()> {
    drain(&outbox_dir(), &hook_path(), &Origin::here(), HOOK_TIMEOUT)?;
    Ok(())
}

/// `agent --test-alert`: puts a test alert in the outbox, so the path unit,
/// the notifier and the hook can be tried end to end.
pub fn test() -> anyhow::Result<()> {
    let hook = hook_path();
    check_hook(&hook).map_err(|e| anyhow::anyhow!("{e}"))?;
    let now = super::core::now_ms();
    let n = Notice {
        id: now * 1000,
        time_ms: now,
        kind: "test".into(),
        subject: "nerdfleet".into(),
        message: "A test alert from nerdfleet: the hook works.".into(),
    };
    post_to(&outbox_dir(), &[n])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nerdfleet-notify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn notice(id: u64, message: &str) -> Notice {
        Notice {
            id,
            time_ms: id / 1000,
            kind: "training stalled".into(),
            subject: "torchrun train.py".into(),
            message: message.into(),
        }
    }

    fn origin() -> Origin {
        Origin { name: "gpu box".into(), hostname: "host-a".into() }
    }

    /// A hook that appends what it was told to `log`.
    fn hook(dir: &Path, body: &str) -> PathBuf {
        let h = dir.join("on-alert");
        std::fs::write(&h, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o700)).unwrap();
        h
    }

    #[test]
    fn notices_survive_the_outbox() {
        let n = notice(1_700_000_000_000_001, "GPUs idle for 10m");
        assert_eq!(decode(&encode(&n)).unwrap(), n);
        assert!(decode(b"garbage").is_err());
    }

    #[test]
    fn the_hook_gets_each_alert_in_order_and_the_outbox_empties() {
        let dir = scratch("order");
        let outbox = dir.join("outbox");
        let log = dir.join("log");
        let h = hook(
            &dir,
            &format!(
                "echo \"$NERDFLEET_HOST ($NERDFLEET_HOSTNAME) $NERDFLEET_ALERT_ID $NERDFLEET_ALERT_KIND: $NERDFLEET_ALERT_MESSAGE\" >> '{}'",
                log.display()
            ),
        );
        // Posted out of order, run in id order; text that would mean
        // something to a shell reaches the hook as plain text.
        post_to(&outbox, &[notice(2000, "second"), notice(1000, "first $(touch pwned) `x`\x1b[2J")]).unwrap();
        assert_eq!(drain(&outbox, &h, &origin(), HOOK_TIMEOUT).unwrap(), 2);
        let got = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            got,
            "gpu box (host-a) 1000 training stalled: first $(touch pwned) `x`[2J\n\
             gpu box (host-a) 2000 training stalled: second\n"
        );
        assert!(!dir.join("pwned").exists() && !crate::paths::home().join("pwned").exists());
        assert!(queued(&outbox).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_outbox_drops_new_alerts() {
        let dir = scratch("full");
        let outbox = dir.join("outbox");
        let many: Vec<Notice> = (1..=MAX_QUEUED as u64 + 5).map(|i| notice(i, "x")).collect();
        assert_eq!(post_to(&outbox, &many).unwrap(), 5);
        assert_eq!(queued(&outbox).len(), MAX_QUEUED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hook_that_hangs_is_stopped_and_not_retried() {
        let dir = scratch("hang");
        let outbox = dir.join("outbox");
        let h = hook(&dir, "sleep 30");
        post_to(&outbox, &[notice(1, "x")]).unwrap();
        let t = Instant::now();
        let e = drain(&outbox, &h, &origin(), Duration::from_millis(300)).unwrap_err().to_string();
        assert!(e.contains("failed for 1 of 1"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(queued(&outbox).is_empty(), "removed, not retried");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hook_others_could_write_is_refused_and_the_outbox_still_empties() {
        let dir = scratch("unsafe");
        let outbox = dir.join("outbox");
        let log = dir.join("log");
        let h = hook(&dir, &format!("echo ran >> '{}'", log.display()));
        std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o777)).unwrap();
        post_to(&outbox, &[notice(1, "x")]).unwrap();
        let e = drain(&outbox, &h, &origin(), HOOK_TIMEOUT).unwrap_err().to_string();
        assert!(e.contains("not writable by group or others"), "{e}");
        assert!(!log.exists());
        assert!(queued(&outbox).is_empty());
        // Not executable is refused too.
        std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_hook(&h).unwrap_err().to_string().contains("not executable"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

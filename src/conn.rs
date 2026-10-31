//! One thread per host: connect, put the agent there if needed, pass its
//! messages to the viewer, and try again with growing pauses when the host
//! goes away: `TRIES` times, then not until the user asks.

use crate::config::{Access, Host};
use crate::model::{AgentMsg, Frame, ViewerMsg};
use crate::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use crate::remote::{self, Agents, NeedsUser, Ssh};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Connecting,
    Deploying,
    Live,
    /// Why, and what happens next.
    Down(String, Retry),
}

/// Connections tried in a row before giving up, the pauses between them
/// doubling from 2 seconds to a minute: about three minutes in all.
pub const TRIES: u32 = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum Retry {
    /// Try number `.1` (of `TRIES`) at `.0`.
    At(Instant, u32),
    /// `TRIES` tries failed; the next waits for the user.
    GaveUp,
    /// Trying again would fail the same way until the user fixes something.
    NeedsUser,
}

pub enum Event {
    State(usize, State),
    Agent(usize, AgentMsg),
    /// Updating the resident agent ended, with what went wrong if anything.
    Updated(usize, Option<String>),
}

/// What every host thread shares.
pub struct Shared {
    pub agents: Agents,
    pub control_dir: PathBuf,
    pub interval_ms: u32,
    pub stop: AtomicBool,
    /// The running `ssh` or local agent of each host, to end on quit.
    pub children: Mutex<Vec<Option<u32>>>,
    /// Per host: whether the user is looking at it closely, so its agent
    /// should send detail.
    pub detail: Vec<AtomicBool>,
    /// Per host: the live agent's stdin, for telling it so.
    pub writers: Vec<Mutex<Option<ChildStdin>>>,
    /// Per host: the user asked to try again now.
    retry: Vec<AtomicBool>,
}

impl Shared {
    pub fn new(agents: Agents, control_dir: PathBuf, interval_ms: u32, hosts: usize) -> Shared {
        Shared {
            agents,
            control_dir,
            interval_ms,
            stop: AtomicBool::new(false),
            children: Mutex::new(vec![None; hosts]),
            detail: (0..hosts).map(|_| AtomicBool::new(false)).collect(),
            writers: (0..hosts).map(|_| Mutex::new(None)).collect(),
            retry: (0..hosts).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    /// Ends host `index`'s wait: it tries again now, with a fresh count.
    pub fn retry_now(&self, index: usize) {
        self.retry[index].store(true, Ordering::SeqCst);
    }

    fn subscribe(&self, index: usize) -> Frame<ViewerMsg> {
        let detail = self.detail[index].load(Ordering::SeqCst);
        Frame { route: Vec::new(), msg: ViewerMsg::Subscribe { interval_ms: self.interval_ms, detail } }
    }

    /// Drops host `index`'s connection, which then starts over.
    fn reconnect(&self, index: usize) {
        if let Some(pid) = self.children.lock().unwrap()[index] {
            // SAFETY: signalling a child we started; at worst it is gone.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }

    /// Asks host `index` for detail, or to stop sending it.
    pub fn set_detail(&self, index: usize, on: bool) {
        if self.detail[index].swap(on, Ordering::SeqCst) == on {
            return;
        }
        if let Some(w) = self.writers[index].lock().unwrap().as_mut() {
            // A failed write means the connection is ending; the next one
            // subscribes with the new setting.
            let _ = write_frame(w, &self.subscribe(index));
        }
    }

    /// Ends every connection. Remote agents see their stdin close and exit.
    pub fn shutdown(&self, hosts: &[Host]) {
        self.stop.store(true, Ordering::SeqCst);
        for pid in self.children.lock().unwrap().iter().flatten() {
            // SAFETY: signalling a child we started; at worst it is gone.
            unsafe { libc::kill(*pid as i32, libc::SIGTERM) };
        }
        for h in hosts {
            if let Access::Ssh(alias) = &h.access {
                if let Ok(ssh) = Ssh::new(alias, &self.control_dir) {
                    ssh.close();
                }
            }
        }
    }
}

pub fn spawn(index: usize, host: Host, shared: Arc<Shared>, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let mut failed = 0u32;
        while !shared.stop.load(Ordering::SeqCst) {
            let _ = tx.send(Event::State(index, State::Connecting));
            let started = Instant::now();
            let (why, stuck) = match session(index, &host, &shared, &tx) {
                Ok(()) => ("the agent stopped".to_string(), false),
                Err(e) => (format!("{e:#}"), remote::needs_user(&e)),
            };
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            // A connection that held for a while was not a flapping one.
            if started.elapsed() > Duration::from_secs(60) {
                failed = 0;
            }
            failed += 1;
            let retry = if stuck {
                Retry::NeedsUser
            } else if failed >= TRIES {
                Retry::GaveUp
            } else {
                Retry::At(Instant::now() + pause(failed), failed + 1)
            };
            let until = match retry {
                Retry::At(at, _) => Some(at),
                _ => None,
            };
            shared.retry[index].store(false, Ordering::SeqCst);
            let _ = tx.send(Event::State(index, State::Down(why, retry)));
            while until.is_none_or(|u| Instant::now() < u)
                && !shared.stop.load(Ordering::SeqCst)
                && !shared.retry[index].load(Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_millis(200));
            }
            if shared.retry[index].swap(false, Ordering::SeqCst) {
                failed = 0;
            }
        }
    });
}

/// Replaces the resident agent on `host` with this viewer's, in the
/// background: it takes SSH and a service restart. Then the connection
/// starts over, to find the new agent; the old one may still be connected.
pub fn update_resident(index: usize, host: Host, shared: Arc<Shared>, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let result = crate::resident_ctl::install(&host, &shared.control_dir, &shared.agents, || {});
        let failed = result.err().map(|e| format!("{e:#}"));
        if failed.is_none() {
            shared.reconnect(index);
        }
        let _ = tx.send(Event::Updated(index, failed));
    });
}

/// The pause after `failed` failed tries in a row.
fn pause(failed: u32) -> Duration {
    (Duration::from_secs(2) * 2u32.pow(failed.saturating_sub(1).min(6))).min(Duration::from_secs(60))
}

/// Keeps the last few kilobytes a child writes to stderr, for error messages.
fn collect_stderr(child: &mut Child) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = child.stderr.take() {
        let buf = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n @ 1..) = err.read(&mut chunk) {
                let mut b = buf.lock().unwrap();
                b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if b.len() > 4096 {
                    let cut = b.len() - 4096;
                    let cut = (cut..b.len()).find(|&i| b.is_char_boundary(i)).unwrap_or(b.len());
                    b.drain(..cut);
                }
            }
        });
    }
    buf
}

enum Ended {
    /// The agent file is missing or not the one we expect: upload, retry.
    NeedsAgent,
    Done(anyhow::Result<()>),
}

fn session(index: usize, host: &Host, shared: &Shared, tx: &Sender<Event>) -> anyhow::Result<()> {
    match &host.access {
        Access::Local => {
            let mut cmd = Command::new(remote::self_exe());
            cmd.args(["agent", "--stdio"]);
            match talk(index, cmd, "local", &crate::agent::target(), shared, tx) {
                Ended::NeedsAgent => anyhow::bail!("the local agent did not start"),
                Ended::Done(r) => r,
            }
        }
        Access::Ssh(alias) => {
            let ssh = Ssh::new(alias, &shared.control_dir)?;
            let target = ssh.probe()?;
            let agent = shared.agents.for_target(&target)?;
            if let Ended::Done(r) = talk(index, ssh.run(agent), alias, &target, shared, tx) {
                return r;
            }
            let _ = tx.send(Event::State(index, State::Deploying));
            ssh.upload(agent)?;
            let _ = tx.send(Event::State(index, State::Connecting));
            match talk(index, ssh.run(agent), alias, &target, shared, tx) {
                Ended::NeedsAgent => Err(NeedsUser("the agent was copied but would not start".into()).into()),
                Ended::Done(r) => r,
            }
        }
    }
}

/// Runs one agent connection to its end. The agent must say it is built
/// for `target`, the host's.
fn talk(index: usize, mut cmd: Command, alias: &str, target: &str, shared: &Shared, tx: &Sender<Event>) -> Ended {
    let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ended::Done(Err(anyhow::anyhow!("cannot start {}: {e}", cmd.get_program().to_string_lossy())))
        }
    };
    shared.children.lock().unwrap()[index] = Some(child.id());
    let stderr = collect_stderr(&mut child);
    let mut stdin = Some(child.stdin.take().expect("piped"));
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("piped"));

    let mut greeted = false;
    let result = loop {
        match read_frame::<_, Frame<AgentMsg>>(&mut stdout) {
            Ok(Some(Frame { msg: AgentMsg::Hello(h), .. })) if !greeted => {
                if h.protocol != PROTOCOL_VERSION {
                    break Err(NeedsUser(format!(
                        "the agent speaks protocol {}, this viewer {PROTOCOL_VERSION}",
                        h.protocol
                    ))
                    .into());
                }
                if h.target != target {
                    break Err(
                        NeedsUser(format!("the agent is built for {}, but the host is {target}", h.target)).into()
                    );
                }
                greeted = true;
                let mut w = stdin.take().expect("taken once");
                if let Err(e) = write_frame(&mut w, &shared.subscribe(index)) {
                    break Err(e.into());
                }
                *shared.writers[index].lock().unwrap() = Some(w);
                let _ = tx.send(Event::State(index, State::Live));
                // The viewer wants to know, too: whether it is resident.
                let _ = tx.send(Event::Agent(index, AgentMsg::Hello(h)));
            }
            Ok(Some(_)) if !greeted => break Err(anyhow::anyhow!("the agent did not say hello")),
            // Relayed frames (clusters) are not used yet.
            Ok(Some(Frame { route, .. })) if !route.is_empty() => {}
            Ok(Some(Frame { msg, .. })) => {
                if tx.send(Event::Agent(index, msg)).is_err() {
                    break Ok(());
                }
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(anyhow::anyhow!("bad data from the agent: {e}")),
        }
    };
    drop(stdin);
    shared.writers[index].lock().unwrap().take();
    // After a clean end of stream, let the process exit on its own: its
    // exit code says whether the agent was missing. Kill only one that
    // lingers, or one that sent bad data.
    if result.is_ok() {
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let status = child.wait();
    shared.children.lock().unwrap()[index] = None;
    let code = status.ok().and_then(|s| s.code());
    if !greeted && matches!(code, Some(remote::EXIT_MISSING | remote::EXIT_MISMATCH)) {
        return Ended::NeedsAgent;
    }
    // Give the stderr thread a moment to catch the last words.
    std::thread::sleep(Duration::from_millis(50));
    let err = stderr.lock().unwrap().clone();
    match result {
        Ok(()) if !greeted => Ended::Done(Err(remote::failure(code, &err, alias))),
        r => Ended::Done(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_double_to_a_minute_and_stop_after_a_few_minutes() {
        let pauses: Vec<u64> = (1..TRIES).map(|n| pause(n).as_secs()).collect();
        assert_eq!(pauses, [2, 4, 8, 16, 32, 60, 60]);
        let total: u64 = pauses.iter().sum();
        assert!((120..=300).contains(&total), "{total}s of trying");
    }
}

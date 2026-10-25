//! Resident mode: an agent that keeps sampling while nobody watches, so
//! viewers that connect later get the history and the alerts they missed.
//!
//! It runs as a systemd user service and talks only through a Unix socket
//! in the user's private runtime directory (mode 0600, in a 0700
//! directory). Both ends check that the other runs as the same user. Every
//! viewer gets its own bounded queue: a slow one is dropped, never allowed
//! to hold up sampling for the rest. What it knows is saved every few
//! minutes and on shutdown, so a restart or a reboot keeps it.

use super::core::{without_detail, Core};
use crate::history::History;
use crate::model::{AgentMsg, Alert, Frame, Hello, ViewerMsg};
use crate::proto::{read_frame, write_frame, Wire, PROTOCOL_VERSION};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Frames queued for one viewer before it counts as stuck.
const QUEUE: usize = 64;
const SAVE_EVERY: Duration = Duration::from_secs(5 * 60);
const TICK: Duration = Duration::from_secs(1);
/// What the state file starts with, and the version of its layout.
const STATE_MAGIC: u32 = 0x5453_464e; // "NFST"
const STATE_VERSION: u32 = 1;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn socket_path() -> PathBuf {
    crate::paths::runtime_dir().join("agent.sock")
}

fn state_path() -> PathBuf {
    crate::paths::state_dir().join("agent-state.bin")
}

fn uid() -> u32 {
    // SAFETY: getuid has no failure modes.
    unsafe { libc::getuid() }
}

/// The user id of the process at the other end of a Unix socket.
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills a ucred of the size we pass.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn frame(msg: &AgentMsg) -> Arc<Vec<u8>> {
    let mut buf = Vec::new();
    let _ = write_frame(&mut buf, &Frame { route: Vec::new(), msg: msg.clone() });
    Arc::new(buf)
}

// ===== saved state ==========================================================

struct State {
    history: History,
    alerts: Vec<Alert>,
}

fn load_state(path: &Path) -> State {
    let fresh = State { history: History::default(), alerts: Vec::new() };
    let Ok(bytes) = std::fs::read(path) else { return fresh };
    let mut input = bytes.as_slice();
    let parsed = (|| -> std::io::Result<State> {
        if u32::get(&mut input)? != STATE_MAGIC || u32::get(&mut input)? != STATE_VERSION {
            return Err(crate::proto::bad("not a state file of this version"));
        }
        // Read back with the protocol's limits: the file is ours, but it is
        // on disk, and a damaged one must not take the agent down.
        Ok(State { history: History::get(&mut input)?, alerts: Vec::<Alert>::get(&mut input)? })
    })();
    parsed.unwrap_or(fresh)
}

fn save_state(path: &Path, history: &History, alerts: &[Alert]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut buf = Vec::new();
    STATE_MAGIC.put(&mut buf);
    STATE_VERSION.put(&mut buf);
    history.put(&mut buf);
    alerts.to_vec().put(&mut buf);
    let dir = crate::paths::private_dir(path.parent().unwrap_or(Path::new(".")))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let tmp = dir.join(".agent-state.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(&buf)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

// ===== the server ===========================================================

struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    detail: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
}

fn accept(stream: UnixStream) -> Option<Client> {
    if peer_uid(&stream) != Some(uid()) {
        return None;
    }
    let (tx, rx) = mpsc::sync_channel::<Arc<Vec<u8>>>(QUEUE);
    let detail = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));
    let mut writer = stream.try_clone().ok()?;
    let mut reader = stream;
    {
        let alive = alive.clone();
        std::thread::spawn(move || {
            for buf in rx {
                if writer.write_all(&buf).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::SeqCst);
            let _ = writer.shutdown(std::net::Shutdown::Both);
        });
    }
    {
        let (alive, detail) = (alive.clone(), detail.clone());
        std::thread::spawn(move || {
            while let Ok(Some(f)) = read_frame::<_, Frame<ViewerMsg>>(&mut reader) {
                match f.msg {
                    ViewerMsg::Subscribe { detail: d, .. } => detail.store(d, Ordering::SeqCst),
                    ViewerMsg::Bye => break,
                }
            }
            alive.store(false, Ordering::SeqCst);
            let _ = reader.shutdown(std::net::Shutdown::Both);
        });
    }
    Some(Client { tx, detail, alive })
}

/// Binds the socket, refusing to run twice and removing only a stale socket
/// of our own.
fn bind() -> anyhow::Result<(UnixListener, PathBuf)> {
    crate::paths::private_dir(&crate::paths::runtime_dir())?;
    let path = socket_path();
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if UnixStream::connect(&path).is_ok() {
            anyhow::bail!("a resident agent is already running ({})", path.display());
        }
        if !meta.file_type().is_socket() || meta.uid() != uid() {
            anyhow::bail!("{} is in the way and is not a socket of ours; refusing to touch it", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, path))
}

pub fn run() -> anyhow::Result<()> {
    let (listener, sock) = bind()?;
    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        // A viewer vanishing mid-write must not kill us.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let state_file = state_path();
    let state = load_state(&state_file);
    let mut core = Core::new(state.history, state.alerts);

    let (new_tx, new_rx) = mpsc::channel::<Client>();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Some(c) = accept(stream) {
                if new_tx.send(c).is_err() {
                    break;
                }
            }
        }
    });

    let hello = AgentMsg::Hello(Hello {
        protocol: PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: super::target(),
        resident: true,
    });
    let mut clients: Vec<Client> = Vec::new();
    let mut saved = Instant::now();
    let mut next = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        // Newcomers get the whole story first.
        while let Ok(c) = new_rx.try_recv() {
            let intro = [
                hello.clone(),
                AgentMsg::Inventory(core.inventory().clone()),
                AgentMsg::History(core.history.clone()),
                AgentMsg::Alerts(core.alerts.log.iter().cloned().collect()),
            ];
            if intro.iter().all(|m| c.tx.try_send(frame(m)).is_ok()) {
                clients.push(c);
            }
        }
        let detail = clients.iter().any(|c| c.detail.load(Ordering::SeqCst));
        if detail && !core.detailed() {
            core.work_now();
        }
        for msg in core.tick(detail) {
            let full = frame(&msg);
            let plain = without_detail(&msg).map(|m| frame(&m));
            for c in &clients {
                let bytes = match &plain {
                    Some(p) if !c.detail.load(Ordering::SeqCst) => p.clone(),
                    _ => full.clone(),
                };
                match c.tx.try_send(bytes) {
                    Ok(()) => {}
                    // Stuck or gone: let it go, and keep sampling for the rest.
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                        c.alive.store(false, Ordering::SeqCst)
                    }
                }
            }
        }
        clients.retain(|c| c.alive.load(Ordering::SeqCst));
        if saved.elapsed() >= SAVE_EVERY {
            saved = Instant::now();
            let log: Vec<Alert> = core.alerts.log.iter().cloned().collect();
            if let Err(e) = save_state(&state_file, &core.history, &log) {
                eprintln!("nerdfleet agent: cannot save state: {e}");
            }
        }
        next += TICK;
        let now = Instant::now();
        if next <= now {
            // Fell behind (the machine slept): start over from now.
            next = now + TICK;
        }
        while !STOP.load(Ordering::SeqCst) && Instant::now() < next {
            std::thread::sleep(Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())));
        }
    }
    let log: Vec<Alert> = core.alerts.log.iter().cloned().collect();
    let saved = save_state(&state_file, &core.history, &log);
    let _ = std::fs::remove_file(&sock);
    saved?;
    Ok(())
}

// ===== attaching ============================================================

/// What happened when a stdio agent looked for a resident one.
pub enum Attach {
    /// There is none to attach to.
    None,
    /// It served the viewer until one of them went away.
    Done,
    /// It speaks another protocol: the caller samples itself instead.
    Mismatch(Hello),
}

/// Connects this stdio session to the resident agent, if one runs and
/// speaks our protocol, and passes bytes both ways until either side ends.
pub fn attach() -> std::io::Result<Attach> {
    let path = socket_path();
    // The directory must be ours and closed, or the socket in it proves
    // nothing about who is listening.
    let Some(dir) = path.parent() else { return Ok(Attach::None) };
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && m.uid() == uid() && m.mode() & 0o077 == 0 => {}
        _ => return Ok(Attach::None),
    }
    let Ok(mut stream) = UnixStream::connect(&path) else { return Ok(Attach::None) };
    if peer_uid(&stream) != Some(uid()) {
        return Ok(Attach::None);
    }
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let hello = match read_frame::<_, Frame<AgentMsg>>(&mut stream)? {
        Some(Frame { msg: AgentMsg::Hello(h), .. }) => h,
        _ => return Ok(Attach::None),
    };
    if hello.protocol != PROTOCOL_VERSION {
        return Ok(Attach::Mismatch(hello));
    }
    stream.set_read_timeout(None)?;
    let mut stdout = std::io::stdout().lock();
    write_frame(&mut stdout, &Frame { route: Vec::new(), msg: AgentMsg::Hello(hello) })?;
    let mut to_resident = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_resident);
        let _ = to_resident.shutdown(std::net::Shutdown::Both);
    });
    let mut buf = [0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).and_then(|_| stdout.flush()).is_err() {
                    break;
                }
            }
        }
    }
    Ok(Attach::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_survives_a_save_and_bad_files_start_fresh() {
        let dir = std::env::temp_dir().join(format!("nerdfleet-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("agent-state.bin");
        let mut h = History::default();
        for t in 0..50u64 {
            h.push(t * 1000, &[1, 2, 3]);
        }
        let alerts = vec![Alert {
            id: 9,
            time_ms: 1,
            kind: crate::model::AlertKind::DiskFull,
            subject: "/".into(),
            message: "full".into(),
        }];
        save_state(&path, &h, &alerts).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let back = load_state(&path);
        assert_eq!(back.history, h);
        assert_eq!(back.alerts, alerts);
        std::fs::write(&path, b"garbage").unwrap();
        let fresh = load_state(&path);
        assert!(fresh.alerts.is_empty() && fresh.history == History::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

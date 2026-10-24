//! A resident agent and a viewer attaching to it through `agent --stdio`,
//! in scratch runtime and state directories.

use nerdfleet::model::{AgentMsg, Frame, ViewerMsg};
use nerdfleet::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nerdfleet-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn agent(args: &[&str], run: &std::path::Path, state: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nerdfleet"));
    c.args(args).env("XDG_RUNTIME_DIR", run).env("XDG_STATE_HOME", state);
    c
}

fn wait_for(path: &std::path::Path) {
    let until = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < until, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn term(mut c: Child) -> std::process::ExitStatus {
    // SAFETY: signalling a child we started.
    unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
    c.wait().unwrap()
}

#[test]
#[cfg(target_os = "linux")]
fn a_viewer_attaches_to_the_resident_agent_and_its_state_is_saved() {
    let run = scratch("run");
    let state = scratch("state");
    let resident = agent(&["agent", "--resident"], &run, &state).stderr(Stdio::inherit()).spawn().unwrap();
    let sock = run.join("nerdfleet/agent.sock");
    wait_for(&sock);
    assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);

    // A second resident refuses to start.
    let twice = agent(&["agent", "--resident"], &run, &state).stderr(Stdio::null()).status().unwrap();
    assert!(!twice.success());

    let mut viewer = agent(&["agent", "--stdio"], &run, &state).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = viewer.stdin.take().unwrap();
    write_frame(&mut stdin, &Frame { route: vec![], msg: ViewerMsg::Subscribe { interval_ms: 1000, detail: true } }).unwrap();
    let mut out = viewer.stdout.take().unwrap();
    let mut next = || read_frame::<_, Frame<AgentMsg>>(&mut out).unwrap().unwrap().msg;
    let AgentMsg::Hello(h) = next() else { panic!("no hello") };
    assert!(h.resident && h.protocol == PROTOCOL_VERSION);
    assert!(matches!(next(), AgentMsg::Inventory(_)));
    assert!(matches!(next(), AgentMsg::History(_)));
    assert!(matches!(next(), AgentMsg::Alerts(_)));
    let mut samples = 0;
    let mut detailed = false;
    while samples < 3 {
        match next() {
            AgentMsg::Sample(_) => samples += 1,
            AgentMsg::Workloads(w) => detailed |= !w.top.is_empty(),
            _ => {}
        }
    }
    assert!(detailed, "detail reaches the viewer that asked for it");
    drop(stdin);
    viewer.wait().unwrap();

    // Stopping saves what it knows, privately, and removes the socket.
    assert!(term(resident).success());
    let saved = state.join("nerdfleet/agent-state.bin");
    assert!(saved.exists());
    assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!sock.exists());

    // Without a resident, --stdio samples by itself.
    let mut alone = agent(&["agent", "--stdio"], &run, &state).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut out = alone.stdout.take().unwrap();
    let AgentMsg::Hello(h) = read_frame::<_, Frame<AgentMsg>>(&mut out).unwrap().unwrap().msg else { panic!() };
    assert!(!h.resident);
    drop(alone.stdin.take());
    alone.wait().unwrap();

    let _ = std::fs::remove_dir_all(&run);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
#[cfg(target_os = "linux")]
fn an_older_resident_is_left_alone_and_the_viewer_is_told() {
    use nerdfleet::model::Hello;
    use std::os::unix::net::UnixListener;
    let run = scratch("old-run");
    let state = scratch("old-state");
    let dir = run.join("nerdfleet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A resident from an older build: it says hello in protocol 2.
    let listener = UnixListener::bind(dir.join("agent.sock")).unwrap();
    let old = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let hello = Hello { protocol: 2, version: "0.0.9".into(), target: "x86_64-linux".into(), resident: true };
        write_frame(&mut s, &Frame { route: vec![], msg: AgentMsg::Hello(hello) }).unwrap();
    });
    let mut viewer = agent(&["agent", "--stdio"], &run, &state).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut out = viewer.stdout.take().unwrap();
    let mut next = || read_frame::<_, Frame<AgentMsg>>(&mut out).unwrap().unwrap().msg;
    let AgentMsg::Hello(h) = next() else { panic!("no hello") };
    assert!(!h.resident, "it samples by itself");
    let AgentMsg::Error(e) = next() else { panic!("not told") };
    assert!(e.contains("older build") && e.contains("resident enable"), "{e}");
    assert!(matches!(next(), AgentMsg::Inventory(_)));
    drop(viewer.stdin.take());
    viewer.wait().unwrap();
    old.join().unwrap();
    let _ = std::fs::remove_dir_all(&run);
    let _ = std::fs::remove_dir_all(&state);
}

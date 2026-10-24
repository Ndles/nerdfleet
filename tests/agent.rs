//! The agent as the viewer sees it: started as a child, speaking frames.

use nerdfleet::model::{AgentMsg, Frame, ViewerMsg};
use nerdfleet::proto::{read_frame, write_frame, PROTOCOL_VERSION};
use std::process::{Command, Stdio};

#[test]
fn agent_says_hello_then_describes_and_measures_the_host() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nerdfleet"))
        .args(["agent", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    write_frame(&mut stdin, &Frame { route: vec![], msg: ViewerMsg::Subscribe { interval_ms: 250, detail: true } }).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut next = || read_frame::<_, Frame<AgentMsg>>(&mut stdout).unwrap().unwrap().msg;

    let AgentMsg::Hello(hello) = next() else { panic!("no hello first") };
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    let mut inventory = None;
    let mut samples = Vec::new();
    while samples.len() < 2 {
        match next() {
            AgentMsg::Inventory(i) => inventory = Some(i),
            AgentMsg::Sample(s) => samples.push(s),
            AgentMsg::Error(e) => eprintln!("agent: {e}"),
            AgentMsg::Workloads(w) => {
                if cfg!(target_os = "linux") {
                    assert!(!w.top.is_empty(), "detail mode lists top processes");
                }
            }
            AgentMsg::Hello(_) => panic!("second hello"),
        }
    }
    let inventory = inventory.expect("no inventory before samples");
    if std::env::var_os("NERDFLEET_SHOW").is_some() {
        eprintln!("{inventory:#?}\n{:#?}", samples[1]);
    }
    if cfg!(target_os = "linux") {
        assert!(inventory.threads > 0 && inventory.mem_total > 0);
        assert_eq!(samples[1].cpu.cores.len(), inventory.threads as usize);
        assert_eq!(samples[1].gpus.len(), inventory.gpus.len());
        assert_eq!(samples[1].disks.len(), inventory.mounts.len());
    }

    // Closing stdin ends the agent.
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

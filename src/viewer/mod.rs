//! The viewer: the TUI you look at.

mod app;
mod host;
mod shell;
mod ui;

use crate::config::Config;
use crate::conn::{self, Event, Shared};
use crate::remote::AgentBuild;
use app::HostView;
use crossterm::event::{self as term, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use nerdmux::embed::TerminalEvent;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the screen shows.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Overview,
    /// One host up close, with a workload selected.
    Host { host: usize, workload: usize, window: usize },
    /// A terminal on a host, full screen.
    Shell { host: usize },
}

/// Puts the terminal back however the viewer ends, panics included.
struct Screen;

impl Screen {
    fn enter() -> anyhow::Result<Screen> {
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            crossterm::event::EnableBracketedPaste
        )?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            Screen::restore();
            hook(info);
        }));
        Ok(Screen)
    }

    fn restore() {
        let _ = execute!(
            std::io::stdout(),
            crossterm::event::DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        Screen::restore();
    }
}

/// What the viewer waits for besides agent news: terminal output, and
/// whatever the user does.
enum Wake {
    Pane(TerminalEvent),
    Input(TermEvent),
}

impl From<TerminalEvent> for Wake {
    fn from(e: TerminalEvent) -> Wake {
        Wake::Pane(e)
    }
}

/// Shortest time between two frames while a terminal is busy.
const FRAME: Duration = Duration::from_millis(12);

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    // The look every Nërd tool shares, from ~/.config/nerd-theme.toml.
    let theme = nerdmux::embed::load_theme();
    let control_dir = crate::paths::private_dir(&crate::paths::runtime_dir().join("ssh"))?;
    let shared = Arc::new(Shared::new(AgentBuild::current()?, control_dir.clone(), config.interval_ms, config.hosts.len()));

    let (tx, rx) = mpsc::channel();
    for (i, host) in config.hosts.iter().enumerate() {
        conn::spawn(i, host.clone(), shared.clone(), tx.clone());
    }
    let mut hosts: Vec<HostView> = config.hosts.iter().map(HostView::new).collect();
    for (name, id) in app::load_read() {
        if let Some(h) = hosts.iter_mut().find(|h| h.name == name) {
            h.read_up_to = id;
        }
    }

    let screen = Screen::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // Input is read on a thread of its own, so the loop wakes as soon as
    // either a key or terminal output arrives.
    let (wake_tx, wake_rx) = mpsc::sync_channel::<Wake>(256);
    {
        let tx = wake_tx.clone();
        std::thread::Builder::new().name("input".into()).spawn(move || {
            while let Ok(ev) = term::read() {
                if tx.send(Wake::Input(ev)).is_err() {
                    break;
                }
            }
        })?;
    }
    // One terminal per host at most, kept while the dashboard shows.
    let mut shells: Vec<Option<shell::Shell>> = config.hosts.iter().map(|_| None).collect();
    let mut next_shell = 0u32;
    let mut before_shell = Mode::Overview;
    let mut selected = 0usize;
    let mut scroll = 0u16;
    let mut dirty = true;
    let mut last_draw = Instant::now();
    let mut mode = Mode::Overview;
    let result = 'main: loop {
        // Terminal output and input first, then agent news, then at most
        // one redraw.
        let wait = if dirty { FRAME.saturating_sub(last_draw.elapsed()) } else { Duration::from_millis(100) };
        let mut woke = wake_rx.recv_timeout(wait).ok();
        while let Some(w) = woke.take().or_else(|| wake_rx.try_recv().ok()) {
            match w {
                Wake::Pane(TerminalEvent::Output(id, bytes)) => {
                    if let Some((i, s)) = find_shell(&mut shells, id) {
                        s.feed(&bytes);
                        dirty |= mode == Mode::Shell { host: i };
                    }
                }
                Wake::Pane(TerminalEvent::Eof(id) | TerminalEvent::Exit(id)) => {
                    if let Some((_, s)) = find_shell(&mut shells, id) {
                        s.ended = true;
                        dirty = true;
                    }
                }
                Wake::Input(TermEvent::Resize(cols, rows)) => {
                    for s in shells.iter_mut().flatten() {
                        s.pane.resize(cols, rows.saturating_sub(1));
                    }
                    dirty = true;
                }
                Wake::Input(TermEvent::Paste(text)) => {
                    if let Mode::Shell { host } = mode {
                        if let Some(s) = shells[host].as_mut().filter(|s| !s.ended) {
                            s.pane.paste(&text);
                        }
                    }
                }
                Wake::Input(TermEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                    dirty = true;
                    // In a terminal every key is the shell's, but one.
                    if let Mode::Shell { host } = mode {
                        let Some(s) = shells[host].as_mut() else { continue };
                        if shell::is_escape(&k) {
                            mode = before_shell;
                        } else if s.ended {
                            if k.code == KeyCode::Enter {
                                shells[host] = None;
                                match open_shell(&config.hosts[host], &control_dir, &mut next_shell, &term, &wake_tx) {
                                    Ok(s) => shells[host] = Some(s),
                                    Err(e) => hosts[host].apply(crate::model::AgentMsg::Error(format!("terminal: {e}"))),
                                }
                            }
                        } else {
                            s.pane.send_key(k);
                        }
                        continue;
                    }
                    if k.code == KeyCode::Char('q') && k.modifiers.is_empty()
                        || k.code == KeyCode::Char('c') && k.modifiers == KeyModifiers::CONTROL
                    {
                        break 'main Ok(());
                    }
                    // `s`: a shell on the host in view, or the selected one.
                    if k.code == KeyCode::Char('s') && k.modifiers.is_empty() && !hosts.is_empty() {
                        let host = match mode {
                            Mode::Host { host, .. } => host,
                            _ => selected,
                        };
                        if shells[host].as_ref().is_none_or(|s| s.ended) {
                            shells[host] = None;
                            match open_shell(&config.hosts[host], &control_dir, &mut next_shell, &term, &wake_tx) {
                                Ok(s) => shells[host] = Some(s),
                                Err(e) => {
                                    hosts[host].apply(crate::model::AgentMsg::Error(format!("terminal: {e}")));
                                    continue;
                                }
                            }
                        }
                        before_shell = mode;
                        mode = Mode::Shell { host };
                        continue;
                    }
                    if let Mode::Host { host, workload, window } = mode {
                        let n = hosts[host].workloads.list.len();
                        match k.code {
                            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                                shared.set_detail(host, false);
                                mode = Mode::Overview;
                            }
                            KeyCode::Up => mode = Mode::Host { host, workload: workload.saturating_sub(1), window },
                            KeyCode::Down => {
                                mode = Mode::Host { host, workload: (workload + 1).min(n.saturating_sub(1)), window }
                            }
                            KeyCode::Char('t') => {
                                mode = Mode::Host { host, workload, window: (window + 1) % host::WINDOWS.len() }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if k.code == KeyCode::Enter && !hosts.is_empty() {
                        // Looking closely: ask that host for command lines and
                        // its busiest processes.
                        shared.set_detail(selected, true);
                        mode = Mode::Host { host: selected, workload: 0, window: 0 };
                        continue;
                    }
                    // Arrows move, with or without Alt (nerdmux's pane keys).
                    if !(k.modifiers - KeyModifiers::ALT).is_empty() || hosts.is_empty() {
                        continue;
                    }
                    let cols = cols(term.size()?.width, hosts.len());
                    let n = hosts.len();
                    match k.code {
                        KeyCode::Left => selected = selected.saturating_sub(1),
                        KeyCode::Right => selected = (selected + 1).min(n - 1),
                        KeyCode::Up => selected = selected.saturating_sub(cols),
                        KeyCode::Down if selected + cols < n => selected += cols,
                        _ => {}
                    }
                }
                Wake::Input(_) => {}
            }
        }
        // A program may start a synchronized update and never end it.
        let now = Instant::now();
        for s in shells.iter_mut().flatten() {
            dirty |= s.pane.tick(now);
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::State(i, s) => hosts[i].set_state(s),
                Event::Agent(i, msg) => hosts[i].apply(msg),
            }
            dirty = true;
        }
        let mut read_changed = false;
        for (i, h) in hosts.iter_mut().enumerate() {
            h.check_reachable();
            // Alerts that just happened go to the desktop too, unless the
            // user is looking at that host already.
            let looking = matches!(mode, Mode::Host { host, .. } if host == i);
            for a in h.fresh.drain(..) {
                if !looking {
                    notify(&h.name, &a.message);
                }
                dirty = true;
            }
            read_changed |= looking && h.mark_read();
        }
        if read_changed {
            app::save_read(&hosts);
            dirty = true;
        }
        // Countdowns on down hosts tick even when nothing arrives.
        let due = dirty && last_draw.elapsed() >= FRAME || last_draw.elapsed() > Duration::from_secs(1);
        if !due {
            continue;
        }
        let drawn = term.draw(|f| {
            let area = f.area();
            match mode {
                Mode::Overview => ui::Ui { theme: &theme, hosts: &hosts, selected, scroll: &mut scroll, config_path }
                    .draw(area, f.buffer_mut()),
                Mode::Host { host, workload, window } => host::HostUi {
                    theme: &theme,
                    host: &hosts[host],
                    selected: workload,
                    own_bg: theme.own_palette,
                    window,
                }
                .draw(area, f.buffer_mut()),
                Mode::Shell { host } => {
                    let Some(s) = shells[host].as_mut() else { return };
                    let bar = Rect { height: 1.min(area.height), ..area };
                    let body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
                    let buf = f.buffer_mut();
                    shell::draw_bar(buf, bar, &theme, &hosts[host].name, s);
                    s.pane.draw(buf, body, &theme, !s.ended);
                    if let Some(c) = s.pane.cursor().filter(|_| !s.ended) {
                        if c.col < body.width && c.row < body.height {
                            f.set_cursor_position((body.x + c.col, body.y + c.row));
                        }
                    }
                }
            }
        });
        if let Err(e) = drawn {
            break Err(e.into());
        }
        dirty = false;
        last_draw = Instant::now();
    };
    drop(shells);
    drop(term);
    drop(screen);
    shared.shutdown(&config.hosts);
    result
}

fn find_shell(shells: &mut [Option<shell::Shell>], id: u32) -> Option<(usize, &mut shell::Shell)> {
    shells.iter_mut().enumerate().find_map(|(i, s)| s.as_mut().filter(|s| s.id == id).map(|s| (i, s)))
}

/// A terminal on `host` the size of the screen below the bar.
fn open_shell(
    host: &crate::config::Host,
    control_dir: &Path,
    next: &mut u32,
    term: &Terminal<CrosstermBackend<std::io::Stdout>>,
    tx: &mpsc::SyncSender<Wake>,
) -> anyhow::Result<shell::Shell> {
    let size = term.size()?;
    *next += 1;
    shell::open(host, control_dir, *next, size.width, size.height.saturating_sub(1), tx.clone())
}

/// A desktop notification, when the desktop has a way to show one. The
/// text came from a host, and was cleaned of control characters when it
/// was read; it goes to `notify-send` as an argument, never through a
/// shell, with markup characters escaped (notification daemons render
/// Pango markup, and a workload's label is whatever its command line said).
fn notify(host: &str, message: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("notify-send")
        .args(["--app-name=nerdfleet", "--", &format!("nerdfleet · {}", markup_escape(host)), &markup_escape(message)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        // Reaped on a thread of its own, so no zombie stays behind.
        .map(|mut child| std::thread::spawn(move || child.wait()));
}

fn markup_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// How many cards fit side by side; matches `ui::Ui::grid`.
fn cols(width: u16, hosts: usize) -> usize {
    ((width / 50) as usize).clamp(1, hosts.max(1))
}

/// Connects to one host the way the dashboard does and prints what comes
/// back: a way to see why a host will not show up.
pub fn probe(config_path: &Path, name: &str) -> anyhow::Result<()> {
    use crate::conn::State;
    use crate::model::AgentMsg;

    let config = Config::load(config_path)?;
    let Some(index) = config.hosts.iter().position(|h| h.name == name) else {
        anyhow::bail!("no host named {name:?} in {}", config_path.display());
    };
    let host = config.hosts[index].clone();
    let control_dir = crate::paths::private_dir(&crate::paths::runtime_dir().join("ssh"))?;
    let shared = Arc::new(Shared::new(AgentBuild::current()?, control_dir, 1000, config.hosts.len()));
    shared.set_detail(index, true);
    let (tx, rx) = mpsc::channel();
    conn::spawn(index, host.clone(), shared.clone(), tx);
    let mut view = HostView::new(&host);
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut samples = 0;
    let probe_samples = 36;
    let result = loop {
        let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
            break Err(anyhow::anyhow!("no answer within a minute"));
        };
        match ev {
            Event::State(_, State::Down(why, _)) => break Err(anyhow::anyhow!("{why}")),
            Event::State(_, s) => println!("{s:?}"),
            Event::Agent(_, msg) => {
                match &msg {
                    AgentMsg::Hello(h) => println!(
                        "agent {} ({}), protocol {}{}",
                        h.version,
                        h.target,
                        h.protocol,
                        if h.resident { ", resident" } else { "" }
                    ),
                    AgentMsg::Error(e) => println!("agent problem: {e}"),
                    AgentMsg::Inventory(_) | AgentMsg::Workloads(_) | AgentMsg::History(_) => {}
                    AgentMsg::Alerts(a) => {
                        for a in a {
                            println!("alert {}", a.message);
                        }
                    }
                    AgentMsg::Sample(_) => samples += 1,
                }
                view.apply(msg);
                // Long enough for per-process rates, and for a busy CPU job to
                // have lasted the 30 seconds that make it a workload.
                if samples == probe_samples {
                    break Ok(());
                }
            }
        }
    };
    shared.shutdown(std::slice::from_ref(&host));
    result?;
    let (Some(inv), Some(s)) = (&view.inventory, &view.sample) else { return Ok(()) };
    println!("{} · {} · kernel {}", inv.hostname, inv.os, inv.kernel);
    println!("cpu  {} · {} threads · {:.0}% busy · load {:.2} {:.2} {:.2}", inv.cpu_model, inv.threads, s.cpu.busy * 100.0, s.cpu.load[0], s.cpu.load[1], s.cpu.load[2]);
    println!("mem  {} of {} used · swap {} of {}", ui::gib(s.mem.total.saturating_sub(s.mem.available)), ui::gib(s.mem.total), ui::gib(s.mem.swap_used), ui::gib(s.mem.swap_total));
    for (m, d) in inv.mounts.iter().zip(&s.disks) {
        println!("disk {m}: {} of {}", ui::gib(d.used), ui::gib(d.total));
    }
    println!("io   net ↓{} ↑{} · disk r {} w {}", ui::rate(s.net.read), ui::rate(s.net.write), ui::rate(s.disk_io.read), ui::rate(s.disk_io.write));
    for (i, (g, x)) in inv.gpus.iter().zip(&s.gpus).enumerate() {
        println!("gpu{i} {} {} [{}] unified={} metrics={}", g.vendor.label(), g.name, g.bus, g.unified, g.metrics);
        println!("     total vram {:?} shared {:?} limit {:?}W", g.mem_total.map(ui::gib), g.shared_total.map(ui::gib), g.power_limit_w);
        println!("     {x:?}");
    }
    for w in &view.workloads.list {
        let gpus: Vec<String> = w
            .gpus
            .iter()
            .map(|g| format!("gpu{} {} {}", g.gpu, ui::gib(g.mem), g.busy.map_or("?".into(), |b| format!("{b}%"))))
            .collect();
        println!(
            "work {} · {}{} · {} procs · {:.1} cores · {} · ports {:?} · {}{}",
            w.kind.label(),
            w.label,
            if w.model.is_empty() { String::new() } else { format!(" · {}", w.model) },
            w.procs,
            w.cpu,
            ui::gib(w.rss),
            w.ports,
            gpus.join(", "),
            if w.gpu_idle_s > 0 { format!(" · GPUs idle {}s", w.gpu_idle_s) } else { String::new() },
        );
        println!("     $ {}", w.cmdline);
    }
    for p in &view.workloads.top {
        println!("top  {:>7} {:<20} {:>5.1} cores {:>7} gpu {}", p.pid, p.name, p.cpu, ui::gib(p.rss), ui::gib(p.gpu_mem));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::app::HostView;
    use super::ui::Ui;
    use crate::config::{Access, Host};
    use crate::conn::State;
    use crate::model::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn host(name: &str, gpus: Vec<(GpuInfo, Gpu)>, state: State) -> HostView {
        let mut v = HostView::new(&Host { name: name.into(), access: Access::Local });
        v.apply(AgentMsg::Inventory(Inventory {
            hostname: name.into(),
            os: "Ubuntu 26.04.1 LTS".into(),
            threads: 32,
            mem_total: 89 << 30,
            mounts: vec!["/".into(), "/mnt/data".into()],
            gpus: gpus.iter().map(|g| g.0.clone()).collect(),
            ..Default::default()
        }));
        for busy in [0.1, 0.4, 0.42] {
            v.apply(AgentMsg::Sample(Sample {
                cpu: Cpu { busy, cores: vec![0; 32], load: [2.0, 2.0, 2.0] },
                mem: Mem { total: 89 << 30, available: 78 << 30, ..Default::default() },
                disks: vec![Disk { total: 100, used: 56 }, Disk { total: 100, used: 93 }],
                net: Io { read: 1_200_000, write: 3000 },
                gpus: gpus.iter().map(|g| g.1.clone()).collect(),
                ..Default::default()
            }));
        }
        v.set_state(state);
        v
    }

    fn nvidia() -> (GpuInfo, Gpu) {
        (
            GpuInfo {
                vendor: Vendor::Nvidia,
                name: "NVIDIA RTX PRO 4500 Blackwell".into(),
                bus: "0000:01:00.0".into(),
                mem_total: Some(32 << 30),
                shared_total: None,
                unified: false,
                power_limit_w: Some(200.0),
                metrics: true,
            },
            Gpu { util: Some(97), mem_used: Some(27 << 30), temp_c: Some(76.0), power_w: Some(200.0), ..Default::default() },
        )
    }

    fn apu() -> (GpuInfo, Gpu) {
        (
            GpuInfo {
                vendor: Vendor::Amd,
                name: "Strix Halo [Radeon Graphics / Radeon 8060S Graphics]".into(),
                bus: "0000:66:00.0".into(),
                mem_total: Some(8 << 30),
                shared_total: Some(104 << 30),
                unified: true,
                power_limit_w: None,
                metrics: true,
            },
            Gpu { util: Some(93), mem_used: Some(8 << 30), shared_used: Some(22 << 30), ..Default::default() },
        )
    }

    fn sleeper() -> (GpuInfo, Gpu) {
        let mut info = nvidia().0;
        info.name = "AD107M [GeForce RTX 4060 Max-Q / Mobile]".into();
        info.metrics = false;
        (info, Gpu { power: Power::Asleep, ..Default::default() })
    }

    fn render(hosts: &[HostView], width: u16, height: u16) -> String {
        let theme = nerdmux::embed::Theme::default();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let mut scroll = 0;
        Ui { theme: &theme, hosts, selected: 0, scroll: &mut scroll, config_path: std::path::Path::new("/x") }
            .draw(area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_overview_shows_every_kind_of_gpu() {
        let hosts = vec![
            host("gpu-box", vec![nvidia(), nvidia()], State::Live),
            host("apu-box", vec![apu()], State::Live),
            host("laptop", vec![sleeper()], State::Live),
            host("gone", vec![], State::Down("no such host: is `gone` in ~/.ssh/config?".into(), std::time::Instant::now())),
        ];
        let screen = render(&hosts, 110, 40);
        if std::env::var_os("NERDFLEET_SHOW").is_some() {
            eprintln!("{screen}");
        }
        assert!(screen.contains("gpu-box") && screen.contains("RTX PRO 4500"));
        assert!(screen.contains("22.0G/104G sh"), "unified memory shows GTT");
        assert!(screen.contains("asleep"));
        assert!(screen.contains("down: no such host"));
        // Narrow terminals get one column and must not panic.
        render(&hosts, 30, 10);
        render(&hosts, 1, 1);
    }

    #[test]
    fn absurd_numbers_from_a_hostile_agent_do_not_panic() {
        let (mut info, _) = nvidia();
        info.mem_total = Some(u64::MAX);
        info.shared_total = Some(0);
        info.unified = true;
        let g = Gpu {
            util: Some(255),
            mem_used: Some(u64::MAX),
            shared_used: Some(u64::MAX),
            temp_c: Some(f32::MAX),
            power_w: Some(-f32::MAX),
            clock_mhz: Some(u32::MAX),
            ..Default::default()
        };
        let mut v = host("evil", vec![(info, g.clone())], State::Live);
        v.apply(AgentMsg::Sample(Sample {
            cpu: Cpu { busy: f32::MAX, cores: vec![255; 4], load: [f32::MAX, -1.0, 0.0] },
            mem: Mem { total: u64::MAX, available: 0, cached: u64::MAX, swap_total: 0, swap_used: u64::MAX },
            disks: vec![Disk { total: 0, used: u64::MAX }, Disk { total: u64::MAX, used: 0 }],
            disk_io: Io { read: u64::MAX, write: u64::MAX },
            net: Io { read: u64::MAX, write: 0 },
            gpus: vec![g],
            ..Default::default()
        }));
        render(&[v], 120, 30);
    }

    #[test]
    fn notifications_cannot_carry_markup() {
        assert_eq!(super::markup_escape("<b>x</b> & y"), "&lt;b&gt;x&lt;/b&gt; &amp; y");
    }

    #[test]
    fn an_empty_fleet_says_how_to_add_hosts() {
        assert!(render(&[], 80, 20).contains("No hosts yet."));
    }
}

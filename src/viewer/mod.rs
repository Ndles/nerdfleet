//! The viewer: the TUI you look at.

mod app;
mod host;
mod shell;
mod ui;

use crate::config::Config;
use crate::conn::{self, Event, Shared};
use crate::remote::Agents;
use app::HostView;
use crossterm::event::{self as term, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use nerdmux::embed::TerminalEvent;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use shell::Kind;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the dashboard shows.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Overview,
    /// One host up close, with a workload selected.
    Host {
        host: usize,
        workload: usize,
        window: usize,
    },
}

/// The terminal on screen, and how it shares the screen with the dashboard.
#[derive(Clone, Copy, Default)]
struct Panes {
    /// Whose terminal, and which kind.
    term: Option<(usize, Kind)>,
    /// The dashboard and the terminal side by side, or one above the other.
    split: bool,
    /// Keys go to the terminal. Without `split`, it has the whole screen.
    focus: bool,
}

/// From this width, a split puts the terminal beside the dashboard rather
/// than below it.
const SIDE_BY_SIDE: u16 = 160;

/// The parts of `area` for the dashboard and for a terminal with its bar;
/// either may be missing.
fn layout(area: Rect, panes: Panes) -> (Option<Rect>, Option<Rect>) {
    match (panes.term.is_some(), panes.split, panes.focus) {
        (false, ..) | (true, false, false) => (Some(area), None),
        (true, false, true) => (None, Some(area)),
        (true, true, _) if area.width >= SIDE_BY_SIDE => {
            let w = area.width / 2;
            (Some(Rect { width: area.width - w, ..area }), Some(Rect { x: area.x + area.width - w, width: w, ..area }))
        }
        (true, true, _) => {
            let h = area.height / 2;
            (
                Some(Rect { height: area.height - h, ..area }),
                Some(Rect { y: area.y + area.height - h, height: h, ..area }),
            )
        }
    }
}

/// A terminal's part without its one-line bar.
fn below_bar(r: Rect) -> Rect {
    Rect { y: r.y + 1.min(r.height), height: r.height.saturating_sub(1), ..r }
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
    // The look every Nërd tool shares, from ~/.config/nerd-theme.toml,
    // followed while we run: a change made in nerdmux's settings or by hand
    // shows here within a second.
    let mut theme = nerdmux::embed::load_theme();
    let theme_mtime = || std::fs::metadata(nerdmux::embed::theme_path()).and_then(|m| m.modified()).ok();
    let mut theme_changed = theme_mtime();
    let mut theme_checked = Instant::now();
    let control_dir = crate::paths::private_dir(&crate::paths::runtime_dir().join("ssh"))?;
    let shared = Arc::new(Shared::new(Agents::load()?, control_dir.clone(), config.interval_ms, config.hosts.len()));

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
    // One terminal of each kind per host at most, kept while others show.
    let mut shells: Vec<[Option<shell::Shell>; 2]> = config.hosts.iter().map(|_| [None, None]).collect();
    let mut next_shell = 0u32;
    let mut panes = Panes::default();
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
                    if let Some((slot, s)) = find_shell(&mut shells, id) {
                        s.feed(&bytes);
                        dirty |= panes.term == Some(slot) && (panes.split || panes.focus);
                    }
                }
                Wake::Pane(TerminalEvent::Eof(id) | TerminalEvent::Exit(id)) => {
                    if let Some((_, s)) = find_shell(&mut shells, id) {
                        s.ended = true;
                        dirty = true;
                    }
                }
                // The terminal on screen takes its new size as it is drawn.
                Wake::Input(TermEvent::Resize(..)) => dirty = true,
                Wake::Input(TermEvent::Paste(text)) => {
                    if let Some((host, kind)) = panes.term.filter(|_| panes.focus) {
                        if let Some(s) = shells[host][kind as usize].as_mut().filter(|s| !s.ended) {
                            s.pane.paste(&text);
                        }
                    }
                }
                Wake::Input(TermEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                    dirty = true;
                    // In a terminal every key is the program's, but one.
                    if let Some((host, kind)) = panes.term.filter(|_| panes.focus) {
                        // Ctrl+] leaves, and so does any key when the terminal
                        // failed to start again.
                        let Some(s) = shells[host][kind as usize].as_mut().filter(|_| !shell::is_escape(&k)) else {
                            panes.focus = false;
                            continue;
                        };
                        if s.ended {
                            if k.code == KeyCode::Enter {
                                let size = term_size(&term, panes)?;
                                if let Err(e) = open_shell(
                                    &mut shells,
                                    &config,
                                    host,
                                    kind,
                                    &control_dir,
                                    &mut next_shell,
                                    size,
                                    &wake_tx,
                                ) {
                                    hosts[host].apply(crate::model::AgentMsg::Error(format!("terminal: {e}")));
                                    panes.focus = false;
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
                    let in_view = match mode {
                        Mode::Host { host, .. } => host,
                        Mode::Overview => selected,
                    };
                    // `s`: a shell on the host in view, or the selected one;
                    // `S`: that host's own nerdmux.
                    if let KeyCode::Char(c @ ('s' | 'S')) = k.code {
                        if (k.modifiers - KeyModifiers::SHIFT).is_empty() && !hosts.is_empty() {
                            let kind = if c == 's' { Kind::Shell } else { Kind::Nerdmux };
                            let next = Panes { term: Some((in_view, kind)), focus: true, ..panes };
                            let size = term_size(&term, next)?;
                            let slot = &shells[in_view][kind as usize];
                            if slot.as_ref().is_none_or(|s| s.ended) {
                                if let Err(e) = open_shell(
                                    &mut shells,
                                    &config,
                                    in_view,
                                    kind,
                                    &control_dir,
                                    &mut next_shell,
                                    size,
                                    &wake_tx,
                                ) {
                                    hosts[in_view].apply(crate::model::AgentMsg::Error(format!("terminal: {e}")));
                                    continue;
                                }
                            }
                            panes = next;
                            continue;
                        }
                    }
                    // Ctrl+]: back to the terminal last shown.
                    if shell::is_escape(&k) {
                        panes.focus = panes.term.is_some_and(|(h, kind)| shells[h][kind as usize].is_some());
                        continue;
                    }
                    // `v`: the dashboard and a terminal together, or not. A
                    // split starts with the terminal last shown, or a shell
                    // on the host in view.
                    if k.code == KeyCode::Char('v') && k.modifiers.is_empty() && !hosts.is_empty() {
                        if panes.split {
                            panes.split = false;
                            continue;
                        }
                        let term_now = panes.term.filter(|&(h, kind)| shells[h][kind as usize].is_some());
                        let next =
                            Panes { term: Some(term_now.unwrap_or((in_view, Kind::Shell))), split: true, focus: true };
                        if term_now.is_none() {
                            let size = term_size(&term, next)?;
                            if let Err(e) = open_shell(
                                &mut shells,
                                &config,
                                in_view,
                                Kind::Shell,
                                &control_dir,
                                &mut next_shell,
                                size,
                                &wake_tx,
                            ) {
                                hosts[in_view].apply(crate::model::AgentMsg::Error(format!("terminal: {e}")));
                                continue;
                            }
                        }
                        panes = next;
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
                    let size = term.size()?;
                    let width =
                        layout(Rect::new(0, 0, size.width, size.height), panes).0.map_or(size.width, |d| d.width);
                    let cols = cols(width, hosts.len());
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
        if now.duration_since(theme_checked) >= Duration::from_secs(1) {
            theme_checked = now;
            let changed = theme_mtime();
            if changed != theme_changed {
                theme_changed = changed;
                theme = nerdmux::embed::load_theme();
                dirty = true;
            }
        }
        for s in shells.iter_mut().flatten().flatten() {
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
        let dashboard_shown = !(panes.term.is_some() && panes.focus && !panes.split);
        for (i, h) in hosts.iter_mut().enumerate() {
            h.check_reachable();
            // Alerts that just happened go to the desktop too, unless the
            // user is looking at that host already.
            let looking = dashboard_shown && matches!(mode, Mode::Host { host, .. } if host == i);
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
            let (dash, pane) = layout(f.area(), panes);
            if let Some(area) = dash {
                match mode {
                    Mode::Overview => {
                        ui::Ui { theme: &theme, hosts: &hosts, selected, scroll: &mut scroll, config_path }
                            .draw(area, f.buffer_mut())
                    }
                    Mode::Host { host, workload, window } => host::HostUi {
                        theme: &theme,
                        host: &hosts[host],
                        selected: workload,
                        own_bg: theme.own_palette,
                        window,
                    }
                    .draw(area, f.buffer_mut()),
                }
            }
            let (Some(area), Some((host, kind))) = (pane, panes.term) else { return };
            let Some(s) = shells[host][kind as usize].as_mut() else { return };
            let bar = Rect { height: 1.min(area.height), ..area };
            let body = below_bar(area);
            s.fit(body.width, body.height);
            let buf = f.buffer_mut();
            shell::draw_bar(buf, bar, &theme, &hosts[host].name, s, panes.focus);
            s.pane.draw(buf, body, &theme, panes.focus && !s.ended);
            if let Some(c) = s.pane.cursor().filter(|_| panes.focus && !s.ended) {
                if c.col < body.width && c.row < body.height {
                    f.set_cursor_position((body.x + c.col, body.y + c.row));
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

fn find_shell(shells: &mut [[Option<shell::Shell>; 2]], id: u32) -> Option<((usize, Kind), &mut shell::Shell)> {
    shells
        .iter_mut()
        .enumerate()
        .find_map(|(i, pair)| pair.iter_mut().flatten().find(|s| s.id == id).map(|s| ((i, s.kind), s)))
}

/// The size, columns by rows, of the terminal that `panes` puts on screen.
fn term_size(term: &Terminal<CrosstermBackend<std::io::Stdout>>, panes: Panes) -> anyhow::Result<(u16, u16)> {
    let size = term.size()?;
    let area = layout(Rect::new(0, 0, size.width, size.height), panes)
        .1
        .map_or(Rect::new(0, 0, size.width, size.height), below_bar);
    Ok((area.width, area.height))
}

/// Starts a terminal of `kind` on host `host`, in place of any it had.
#[allow(clippy::too_many_arguments)]
fn open_shell(
    shells: &mut [[Option<shell::Shell>; 2]],
    config: &Config,
    host: usize,
    kind: Kind,
    control_dir: &Path,
    next: &mut u32,
    size: (u16, u16),
    tx: &mpsc::SyncSender<Wake>,
) -> anyhow::Result<()> {
    let slot = &mut shells[host][kind as usize];
    *slot = None;
    *next += 1;
    *slot = Some(shell::open(&config.hosts[host], kind, control_dir, *next, size, tx.clone())?);
    Ok(())
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
    let shared = Arc::new(Shared::new(Agents::load()?, control_dir, 1000, config.hosts.len()));
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
    println!(
        "cpu  {} · {} threads · {:.0}% busy · load {:.2} {:.2} {:.2}",
        inv.cpu_model,
        inv.threads,
        s.cpu.busy * 100.0,
        s.cpu.load[0],
        s.cpu.load[1],
        s.cpu.load[2]
    );
    println!(
        "mem  {} of {} used · swap {} of {}",
        ui::gib(s.mem.total.saturating_sub(s.mem.available)),
        ui::gib(s.mem.total),
        ui::gib(s.mem.swap_used),
        ui::gib(s.mem.swap_total)
    );
    for (m, d) in inv.mounts.iter().zip(&s.disks) {
        println!("disk {m}: {} of {}", ui::gib(d.used), ui::gib(d.total));
    }
    println!(
        "io   net ↓{} ↑{} · disk r {} w {}",
        ui::rate(s.net.read),
        ui::rate(s.net.write),
        ui::rate(s.disk_io.read),
        ui::rate(s.disk_io.write)
    );
    for (i, (g, x)) in inv.gpus.iter().zip(&s.gpus).enumerate() {
        println!("gpu{i} {} {} [{}] unified={} metrics={}", g.vendor.label(), g.name, g.bus, g.unified, g.metrics);
        println!(
            "     total vram {:?} shared {:?} limit {:?}W",
            g.mem_total.map(ui::gib),
            g.shared_total.map(ui::gib),
            g.power_limit_w
        );
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
        let job = view.workloads.list.iter().find(|w| p.job != 0 && w.id == p.job).map_or("-", |w| w.label.as_str());
        println!(
            "top  {:>7} {:<20} {:>5.1} cores {:>7} gpu {}  in {job}",
            p.pid,
            p.name,
            p.cpu,
            ui::gib(p.rss),
            ui::gib(p.gpu_mem)
        );
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
            Gpu {
                util: Some(97),
                mem_used: Some(27 << 30),
                temp_c: Some(76.0),
                power_w: Some(200.0),
                ..Default::default()
            },
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
            host(
                "gone",
                vec![],
                State::Down("no such host: is `gone` in ~/.ssh/config?".into(), std::time::Instant::now()),
            ),
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
    fn a_split_shares_the_screen_and_a_terminal_alone_takes_it() {
        use super::{layout, Panes, SIDE_BY_SIDE};
        use crate::viewer::shell::Kind;
        let term = Some((0, Kind::Shell));
        let wide = Rect::new(0, 0, SIDE_BY_SIDE, 50);
        let (d, t) = layout(wide, Panes { term, split: true, focus: false });
        let (d, t) = (d.unwrap(), t.unwrap());
        assert_eq!((d.x, d.width + t.width, t.x), (0, SIDE_BY_SIDE, d.width), "side by side");
        assert_eq!((d.height, t.height), (50, 50));
        let narrow = Rect::new(0, 0, 100, 41);
        let (d, t) = layout(narrow, Panes { term, split: true, focus: true });
        let (d, t) = (d.unwrap(), t.unwrap());
        assert_eq!((d.height + t.height, t.y, t.width), (41, d.height, 100), "one above the other");
        assert_eq!(layout(narrow, Panes { term, split: false, focus: true }), (None, Some(narrow)));
        assert_eq!(layout(narrow, Panes { term, split: false, focus: false }), (Some(narrow), None));
        assert_eq!(layout(narrow, Panes { term: None, split: true, focus: true }), (Some(narrow), None));
        // The dashboard draws in either half without trouble.
        let hosts = vec![host("gpu-box", vec![nvidia()], State::Live), host("apu-box", vec![apu()], State::Live)];
        render(&hosts, SIDE_BY_SIDE / 2, 20);
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

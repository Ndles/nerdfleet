//! The viewer: the TUI you look at.

mod app;
mod host;
mod pick;
mod shell;
mod ui;

use crate::config::Config;
use crate::conn::{self, Event, Shared};
use crate::remote::Agents;
use app::HostView;
use crossterm::event::{self as term, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::style::Print;
use crossterm::{execute, terminal};
use nerdmux::embed::TerminalEvent;
use nerdmux::embed::Theme;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::Terminal;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use pick::pick;

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
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Panes {
    /// Whose terminal is shown: beside the dashboard, or zoomed.
    term: Option<usize>,
    /// The terminal has the whole screen, and the keys.
    zoom: bool,
    /// Keys go to the terminal.
    focus: bool,
}

/// From this width, the terminal goes beside the dashboard rather than
/// below it.
const SIDE_BY_SIDE: u16 = 160;

/// The parts of `area` for the dashboard and for a terminal with its bar;
/// either may be missing. Together, as in nerdmux, they sit inside a ring
/// one cell wide, with one column (side by side) or one row (one above the
/// other) between them: room for the separator and the focus outline.
fn layout(area: Rect, panes: Panes) -> (Option<Rect>, Option<Rect>) {
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    match (panes.term.is_some(), panes.zoom) {
        (false, _) => (Some(area), None),
        (true, true) => (None, Some(area)),
        _ if area.width >= SIDE_BY_SIDE => {
            let w = inner.width / 2;
            (
                Some(Rect { width: inner.width.saturating_sub(w + 1), ..inner }),
                Some(Rect { x: inner.x + inner.width - w, width: w, ..inner }),
            )
        }
        _ => {
            let h = inner.height / 2;
            (
                Some(Rect { height: inner.height.saturating_sub(h + 1), ..inner }),
                Some(Rect { y: inner.y + inner.height - h, height: h, ..inner }),
            )
        }
    }
}

/// The separator between the dashboard and the terminal, and the outline
/// of the one with the keys, drawn as nerdmux draws its panes: a plain
/// line in the border color between them, and thin lines hugging the
/// focused one's four edges in the theme's frame colors, fading from its
/// top left corner to its bottom right.
fn draw_frame(buf: &mut ratatui::buffer::Buffer, dash: Rect, term: Rect, term_focused: bool, theme: &Theme) {
    let normal = Style::default().fg(theme.border);
    if term.x > dash.x + dash.width {
        for y in dash.y..dash.y + dash.height {
            buf[(dash.x + dash.width, y)].set_symbol("│").set_style(normal);
        }
    } else {
        for x in dash.x..dash.x + dash.width {
            buf[(x, dash.y + dash.height)].set_symbol("─").set_style(normal);
        }
    }
    let act = if term_focused { term } else { dash };
    if act.width == 0 || act.height == 0 || act.x == 0 || act.y == 0 {
        return;
    }
    let area = buf.area;
    let (left, top) = (act.x - 1, act.y - 1);
    let (right, bottom) = (act.x + act.width, act.y + act.height);
    let (span_x, span_y) = ((right - left).max(1) as f32, (bottom - top).max(1) as f32);
    let mut mark = |x: u16, y: u16, glyph: &str| {
        if x < area.x + area.width && y < area.y + area.height {
            let t = ((x - left) as f32 / span_x + (y - top) as f32 / span_y) / 2.0;
            let color = nerdmux::embed::frame_color(theme, t);
            buf[(x, y)].set_symbol(glyph).set_style(Style::default().fg(color));
        }
    };
    for x in left + 1..right {
        mark(x, top, "▁");
        mark(x, bottom, "▔");
    }
    for y in top + 1..bottom {
        mark(left, y, "▕");
        mark(right, y, "▏");
    }
}

/// A terminal's part without its one-line bar.
fn below_bar(r: Rect) -> Rect {
    Rect { y: r.y + 1.min(r.height), height: r.height.saturating_sub(1), ..r }
}

/// What is under the mouse.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Part {
    Dashboard,
    /// The terminal's bar.
    Bar,
    /// The terminal, at this column and row of it.
    Terminal(u16, u16),
}

/// The part of the screen at `(x, y)`, by the rectangles it is drawn in. The
/// ring and the separator around them are no part.
fn part_at(area: Rect, panes: Panes, x: u16, y: u16) -> Option<Part> {
    let inside = |r: Rect| r.contains(ratatui::layout::Position { x, y });
    let (dash, term) = layout(area, panes);
    if let Some(term) = term.filter(|t| inside(*t)) {
        let body = below_bar(term);
        return Some(if inside(body) { Part::Terminal(x - body.x, y - body.y) } else { Part::Bar });
    }
    dash.filter(|d| inside(*d)).map(|_| Part::Dashboard)
}

/// Where in the terminal a drag or a release that began in it lands, however
/// far outside it the pointer has gone.
fn held_at(area: Rect, panes: Panes, x: u16, y: u16) -> Option<(u16, u16)> {
    let body = below_bar(layout(area, panes).1?);
    let last = |start: u16, len: u16| start + len.saturating_sub(1);
    Some((x.clamp(body.x, last(body.x, body.width)) - body.x, y.clamp(body.y, last(body.y, body.height)) - body.y))
}

/// Mouse reports for clicks, drags and the wheel, in the form that has no
/// limit on the screen's size. Not `EnableMouseCapture`: it also asks for
/// every pointer move, which nothing here wants.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1002l\x1b[?1000l\x1b[?1006l";

/// Puts the terminal back however the viewer ends, panics included.
struct Screen;

impl Screen {
    fn enter() -> anyhow::Result<Screen> {
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            crossterm::event::EnableBracketedPaste,
            Print(MOUSE_ON)
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
            Print(MOUSE_OFF),
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
    let mut shells: Vec<Option<shell::Shell>> = config.hosts.iter().map(|_| None).collect();
    let mut next_shell = 0u32;
    let mut panes = Panes::default();
    let mut selected = 0usize;
    let mut scroll = 0u16;
    // A mouse button went down in the terminal, which gets the rest of it.
    let mut held = false;
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
                    if let Some((host, s)) = find_shell(&mut shells, id) {
                        s.feed(&bytes);
                        dirty |= panes.term == Some(host);
                    }
                }
                Wake::Pane(TerminalEvent::Eof(id) | TerminalEvent::Exit(id)) => {
                    // A terminal whose program ended is closed, and the
                    // dashboard gets the screen and the keys back; `s`
                    // starts a new one (which finds the nerdmux session
                    // again, if the connection was what ended).
                    if let Some((host, _)) = find_shell(&mut shells, id) {
                        shells[host] = None;
                        if panes.term == Some(host) {
                            panes = Panes::default();
                        }
                        dirty = true;
                    }
                }
                // The terminal on screen takes its new size as it is drawn.
                Wake::Input(TermEvent::Resize(..)) => dirty = true,
                Wake::Input(TermEvent::Paste(text)) => {
                    if let Some(s) = panes.term.filter(|_| panes.focus).and_then(|h| shells[h].as_mut()) {
                        s.pane.paste(&text);
                    }
                }
                Wake::Input(TermEvent::Mouse(m)) => {
                    let size = term.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    let shown = panes.term.and_then(|h| shells[h].as_mut());
                    if matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
                        if let (true, Some(s), Some((col, row))) = (held, shown, held_at(area, panes, m.column, m.row))
                        {
                            s.pane.mouse(m, col, row);
                        }
                        held &= !matches!(m.kind, MouseEventKind::Up(_));
                        continue;
                    }
                    let part = part_at(area, panes, m.column, m.row);
                    match (m.kind, part, shown) {
                        // A click puts the keys where it landed, and goes to
                        // the program too, as a click on a pane of nerdmux does.
                        (MouseEventKind::Down(_), Some(Part::Dashboard), _) => panes.focus = false,
                        (MouseEventKind::Down(_), Some(Part::Bar), Some(_)) => panes.focus = true,
                        (MouseEventKind::Down(_), Some(Part::Terminal(col, row)), Some(s)) => {
                            panes.focus = true;
                            s.pane.mouse(m, col, row);
                            held = true;
                        }
                        // The wheel works wherever the pointer is, without
                        // moving the keys.
                        (
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown,
                            Some(Part::Terminal(col, row)),
                            Some(s),
                        ) => {
                            s.pane.mouse(m, col, row);
                        }
                        (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown, Some(Part::Dashboard), _)
                            if !hosts.is_empty() =>
                        {
                            let up = m.kind == MouseEventKind::ScrollUp;
                            let width = layout(area, panes).0.map_or(area.width, |d| d.width);
                            match &mut mode {
                                Mode::Host { host, workload, .. } => {
                                    let last = hosts[*host].workloads.list.len().saturating_sub(1);
                                    *workload = if up { workload.saturating_sub(1) } else { (*workload + 1).min(last) };
                                }
                                Mode::Overview => {
                                    let cols = cols(width, hosts.len());
                                    if up {
                                        selected = selected.saturating_sub(cols);
                                    } else if selected + cols < hosts.len() {
                                        selected += cols;
                                    }
                                }
                            }
                        }
                        _ => continue,
                    }
                    dirty = true;
                }
                Wake::Input(TermEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                    dirty = true;
                    // In a terminal every key is the program's, but one:
                    // Ctrl+] gives the keys to the dashboard, which comes
                    // back beside a zoomed terminal.
                    if let Some(host) = panes.term.filter(|_| panes.focus) {
                        match shells[host].as_mut() {
                            Some(s) if !shell::is_escape(&k) => {
                                s.pane.send_key(k);
                            }
                            Some(_) => panes = Panes { zoom: false, focus: false, ..panes },
                            None => panes = Panes::default(),
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
                    // `s`: the terminal of the host in view (the selected
                    // one, from the overview) beside its stats, or, when it
                    // is there already, away; it keeps running meanwhile.
                    // `z`: that terminal zoomed to the whole screen.
                    if let KeyCode::Char(c @ ('s' | 'z')) = k.code {
                        if !k.modifiers.is_empty() || hosts.is_empty() {
                            continue;
                        }
                        if c == 's' && panes.term == Some(in_view) {
                            panes = Panes::default();
                            continue;
                        }
                        let next = Panes { term: Some(in_view), zoom: c == 'z', focus: true };
                        if shells[in_view].is_none() {
                            let size = term_size(&term, next)?;
                            if let Err(e) =
                                open_shell(&mut shells, &config, in_view, &control_dir, &mut next_shell, size, &wake_tx)
                            {
                                hosts[in_view].apply(crate::model::AgentMsg::Error(format!("terminal: {e}")));
                                continue;
                            }
                        }
                        panes = next;
                        show_host(&mut mode, &shared, in_view);
                        continue;
                    }
                    // Ctrl+]: the keys back to the terminal beside.
                    if shell::is_escape(&k) {
                        panes.focus = panes.term.is_some();
                        continue;
                    }
                    // `r`: a down host tries again now.
                    if k.code == KeyCode::Char('r') && k.modifiers.is_empty() && !hosts.is_empty() {
                        if matches!(hosts[in_view].state, crate::conn::State::Down(..)) {
                            shared.retry_now(in_view);
                        }
                        continue;
                    }
                    // `u`: a resident agent older than this viewer is updated.
                    if k.code == KeyCode::Char('u') && k.modifiers.is_empty() && !hosts.is_empty() {
                        let h = &mut hosts[in_view];
                        if h.live() && h.outdated.is_some() && !h.updating {
                            h.updating = true;
                            conn::update_resident(in_view, config.hosts[in_view].clone(), shared.clone(), tx.clone());
                        }
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
        for s in shells.iter_mut().flatten() {
            dirty |= s.pane.tick(now);
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::State(i, s) => hosts[i].set_state(s),
                Event::Agent(i, msg) => hosts[i].apply(msg),
                Event::Updated(i, failed) => {
                    hosts[i].updating = false;
                    if let Some(why) = failed {
                        hosts[i].apply(crate::model::AgentMsg::Error(format!("update: {why}")));
                    }
                }
            }
            dirty = true;
        }
        let mut read_changed = false;
        let dashboard_shown = !(panes.term.is_some() && panes.zoom);
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
            let (Some(area), Some(host)) = (pane, panes.term) else { return };
            let Some(s) = shells[host].as_mut() else { return };
            if let Some(d) = dash {
                draw_frame(f.buffer_mut(), d, area, panes.focus, &theme);
            }
            let bar = Rect { height: 1.min(area.height), ..area };
            let body = below_bar(area);
            s.fit(body.width, body.height);
            let buf = f.buffer_mut();
            shell::draw_bar(buf, bar, &theme, &hosts[host].name, s, panes.focus, dash.is_none());
            s.pane.draw(buf, body, &theme, panes.focus);
            if let Some(c) = s.pane.cursor().filter(|_| panes.focus) {
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

/// The dashboard shows `host` up close, asking it for detail.
fn show_host(mode: &mut Mode, shared: &Shared, host: usize) {
    if !matches!(*mode, Mode::Host { host: h, .. } if h == host) {
        shared.set_detail(host, true);
        *mode = Mode::Host { host, workload: 0, window: 0 };
    }
}

fn find_shell(shells: &mut [Option<shell::Shell>], id: u32) -> Option<(usize, &mut shell::Shell)> {
    shells.iter_mut().enumerate().find_map(|(i, s)| s.as_mut().filter(|s| s.id == id).map(|s| (i, s)))
}

/// The size, columns by rows, of the terminal that `panes` puts on screen.
fn term_size(term: &Terminal<CrosstermBackend<std::io::Stdout>>, panes: Panes) -> anyhow::Result<(u16, u16)> {
    let size = term.size()?;
    let area = Rect::new(0, 0, size.width, size.height);
    let r = layout(area, panes).1.map_or(area, below_bar);
    Ok((r.width, r.height))
}

/// Starts the terminal on host `host`, in place of any it had.
fn open_shell(
    shells: &mut [Option<shell::Shell>],
    config: &Config,
    host: usize,
    control_dir: &Path,
    next: &mut u32,
    size: (u16, u16),
    tx: &mpsc::SyncSender<Wake>,
) -> anyhow::Result<()> {
    shells[host] = None;
    *next += 1;
    shells[host] = Some(shell::open(&config.hosts[host], control_dir, *next, size, tx.clone())?);
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
            Event::Updated(..) => {}
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
                    AgentMsg::OldResident(v) => println!(
                        "the resident agent here is version {v}, too old for this viewer: \
                         `nerdfleet resident enable` updates it"
                    ),
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
                State::Down("no such host: is `gone` in ~/.ssh/config?".into(), crate::conn::Retry::GaveUp),
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
    fn a_terminal_goes_beside_the_dashboard_or_takes_the_screen_zoomed() {
        use super::{draw_frame, layout, Panes, SIDE_BY_SIDE};
        let term = Some(0);
        let wide = Rect::new(0, 0, SIDE_BY_SIDE, 50);
        let (d, t) = layout(wide, Panes { term, zoom: false, focus: false });
        let (d, t) = (d.unwrap(), t.unwrap());
        assert_eq!((d.x, d.y, t.y), (1, 1, 1), "a ring around both");
        assert_eq!((d.width + 1 + t.width, t.x), (SIDE_BY_SIDE - 2, d.x + d.width + 1), "a separator between");
        assert_eq!((d.height, t.height), (48, 48));
        // The focused side is outlined on all four edges; the separator
        // between them belongs to whichever has the keys.
        let theme = nerdmux::embed::Theme::default();
        let glyphs = |focus: bool| {
            let mut buf = Buffer::empty(wide);
            draw_frame(&mut buf, d, t, focus, &theme);
            let at = |x: u16, y: u16| buf[(x, y)].symbol().to_string();
            [at(d.x + 2, 0), at(d.x + 2, d.y + d.height), at(0, 10), at(d.x + d.width, 10), at(t.x + 2, 0)]
        };
        assert_eq!(glyphs(false), ["▁", "▔", "▕", "▏", " "], "the dashboard outlined");
        assert_eq!(glyphs(true), [" ", " ", " ", "▕", "▁"], "the terminal outlined");

        let narrow = Rect::new(0, 0, 100, 41);
        let (d, t) = layout(narrow, Panes { term, zoom: false, focus: true });
        let (d, t) = (d.unwrap(), t.unwrap());
        assert_eq!((d.height + 1 + t.height, t.y, t.width), (39, d.y + d.height + 1, 98), "one above the other");
        let mut buf = Buffer::empty(narrow);
        draw_frame(&mut buf, d, t, true, &theme);
        assert_eq!(buf[(5, d.y + d.height)].symbol(), "▁", "the terminal's top edge, on the separator");
        assert_eq!(buf[(5, t.y + t.height)].symbol(), "▔");
        assert_eq!(layout(narrow, Panes { term, zoom: true, focus: true }), (None, Some(narrow)));
        assert_eq!(layout(narrow, Panes { term: None, zoom: false, focus: false }), (Some(narrow), None));
        // The dashboard draws in either half without trouble.
        let hosts = vec![host("gpu-box", vec![nvidia()], State::Live), host("apu-box", vec![apu()], State::Live)];
        render(&hosts, SIDE_BY_SIDE / 2, 20);
    }

    #[test]
    fn the_mouse_finds_the_part_the_screen_draws_there() {
        use super::{held_at, layout, part_at, Panes, Part, SIDE_BY_SIDE};
        let area = Rect::new(0, 0, SIDE_BY_SIDE, 50);
        assert_eq!(part_at(area, Panes::default(), 0, 0), Some(Part::Dashboard));
        let panes = Panes { term: Some(0), zoom: false, focus: false };
        let (d, t) = layout(area, panes);
        let (d, t) = (d.unwrap(), t.unwrap());
        assert_eq!(part_at(area, panes, d.x, d.y), Some(Part::Dashboard));
        assert_eq!(part_at(area, panes, 0, 0), None, "the ring");
        assert_eq!(part_at(area, panes, d.x + d.width, 10), None, "the separator");
        assert_eq!(part_at(area, panes, t.x + 3, t.y), Some(Part::Bar));
        assert_eq!(part_at(area, panes, t.x + 3, t.y + 1), Some(Part::Terminal(3, 0)));
        assert_eq!(part_at(area, panes, t.x + 3, t.y + 5), Some(Part::Terminal(3, 4)));
        // A drag stays in the terminal, however far it wanders.
        assert_eq!(held_at(area, panes, 0, 0), Some((0, 0)));
        assert_eq!(held_at(area, panes, 500, 500), Some((t.width - 1, t.height - 2)));
        assert_eq!(held_at(area, Panes::default(), 5, 5), None);
        // Zoomed, there is only the terminal.
        let zoom = Panes { term: Some(0), zoom: true, focus: true };
        assert_eq!(part_at(area, zoom, 0, 0), Some(Part::Bar));
        assert_eq!(part_at(area, zoom, 7, 1), Some(Part::Terminal(7, 0)));
    }

    #[test]
    fn an_older_resident_agent_is_offered_an_update() {
        let hello = |version: &str, resident| {
            AgentMsg::Hello(Hello { protocol: 5, version: version.into(), target: "t".into(), resident })
        };
        let mut v = host("gpu-box", vec![], State::Live);
        v.apply(hello("0.0.1", true));
        assert!(v.update_note().unwrap().contains("0.0.1"));
        assert!(render(std::slice::from_ref(&v), 110, 20).contains("u updates it"));
        // Whole in the host view's bar, beside a terminal too.
        let theme = nerdmux::embed::Theme::default();
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        super::host::HostUi { theme: &theme, host: &v, selected: 0, own_bg: false, window: 0 }.draw(area, &mut buf);
        let bar: String = (0..80).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(bar.contains("u updates it"), "{bar}");
        // Not while it is away, and not for a new one or one of the viewer's own age.
        v.set_state(State::Connecting);
        assert_eq!(v.update_note(), None);
        v.set_state(State::Live);
        v.updating = true;
        assert_eq!(v.update_note().unwrap(), "updating the resident agent…");
        v.updating = false;
        v.apply(hello(env!("CARGO_PKG_VERSION"), true));
        assert_eq!(v.update_note(), None);
        v.apply(hello("999.0.0", true));
        assert_eq!(v.update_note(), None, "a newer one is never downgraded");
        v.apply(hello("0.0.1", false));
        assert_eq!(v.update_note(), None, "a viewer's own agent goes with it");
        // One too old to speak the protocol is reported by the viewer's own agent.
        v.apply(AgentMsg::OldResident("0.0.1".into()));
        assert!(v.update_note().is_some());
        v.apply(hello(env!("CARGO_PKG_VERSION"), true));
        assert_eq!(v.update_note(), None, "the next connection clears it");
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

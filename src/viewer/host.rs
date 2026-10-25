//! One host up close: graphs over the history the viewer keeps, every GPU,
//! every workload with its processes, and the busiest processes.

use super::app::HostView;
use super::ui::{fit, gib, kind_color, level, level_char, rate, short_name, span, STALL_S};
use crate::history::NONE;
use crate::conn::State;
use crate::model::{Kind, Power};
use nerdmux::embed::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use std::collections::VecDeque;

pub struct HostUi<'a> {
    pub theme: &'a Theme,
    pub host: &'a HostView,
    /// The selected workload, by index.
    pub selected: usize,
    pub own_bg: bool,
    /// Which history ring the graphs show: an index into `WINDOWS`.
    pub window: usize,
}

/// `series` (0 to 100, or `NONE`) as `width` cells on a fixed time scale:
/// the whole ring (`cap` points) fits the width, each cell is the highest
/// point of its stretch (so short spikes still show), and the newest cell
/// is at the right edge.
///
/// `newest` is the absolute number of the last point. Cells are cut at
/// fixed absolute positions, not counted back from the newest point, so a
/// finished cell never changes: only the rightmost one fills in, and the
/// line moves left one cell at a time. (Counting back made every cell's
/// stretch shift with each new point, and a scale that depended on how
/// much history there was made the line jump whenever it grew.)
fn spark(series: &VecDeque<u8>, newest: u64, cap: usize, width: usize) -> String {
    if width == 0 || series.is_empty() {
        return " ".repeat(width);
    }
    let per = cap.div_ceil(width).max(1) as u64;
    let first = (newest + 1).saturating_sub(series.len() as u64);
    let last_cell = newest / per;
    // As many cells as the ring fills completely, plus the newest, which is
    // filling. Showing more would put a part-empty cell at the left edge,
    // changing as its oldest points age out.
    let cells_shown = (width as u64).min(cap as u64 / per);
    let first_cell = (first / per).max((last_cell + 1).saturating_sub(cells_shown));
    let mut cells = vec![NONE; (last_cell - first_cell + 1) as usize];
    for (i, &v) in series.iter().enumerate() {
        let cell = (first + i as u64) / per;
        if cell >= first_cell && v != NONE {
            let c = &mut cells[(cell - first_cell) as usize];
            *c = if *c == NONE { v } else { (*c).max(v) };
        }
    }
    let mut s = " ".repeat(width.saturating_sub(cells.len()));
    s.extend(cells.iter().map(|&v| level_char(v)));
    s
}

/// The graph windows `t` steps through: one per history ring.
pub const WINDOWS: [&str; 3] = ["15m", "6h", "24h"];

impl HostUi<'_> {
    pub fn draw(&self, area: Rect, buf: &mut Buffer) {
        let t = self.theme;
        let bg = if self.own_bg { t.bg } else { Color::Reset };
        buf.set_style(area, Style::default().bg(bg).fg(t.fg));
        let h = self.host;
        let w = area.width.saturating_sub(2) as usize;
        let dim = Style::default().fg(t.dim);
        let head = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)));

        // The status bar.
        let bar_area = Rect { height: 1, ..area };
        buf.set_style(bar_area, Style::default().bg(t.status_bg).fg(t.status_fg));
        let mut left = vec![
            Span::styled(" nërdfleet ", Style::default().bg(t.accent).fg(t.accent_fg).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" › {} ", h.name), Style::default().fg(t.status_fg).add_modifier(Modifier::BOLD)),
        ];
        if h.state != State::Live {
            left.push(Span::styled(" not live ", Style::default().fg(t.red)));
        }
        Line::from(left).render(bar_area, buf);
        let keys = "↑↓ workload  t window  s shell  Esc back  q quit ";
        let kw = keys.chars().count() as u16;
        if area.width > kw + 30 {
            Span::styled(keys, dim).render(Rect { x: area.x + area.width - kw, width: kw, ..bar_area }, buf);
        }

        let mut lines: Vec<Line> = Vec::new();
        let (Some(inv), Some(s)) = (&h.inventory, &h.sample) else {
            lines.push(Line::from(Span::styled("waiting for the host's first report…", dim)));
            Paragraph::new(lines).render(Rect { x: area.x + 1, y: area.y + 2, width: area.width.saturating_sub(2), height: area.height.saturating_sub(2) }, buf);
            return;
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{} · {} · kernel {} · {} · {} threads · {} RAM",
                inv.hostname, inv.os, inv.kernel, inv.cpu_model, inv.threads, gib(inv.mem_total)
            ),
            dim,
        )));
        lines.push(Line::from(""));

        // Graphs: a label line with the numbers, then the history.
        let ring = &h.history.rings[self.window.min(h.history.rings.len() - 1)];
        let empty = VecDeque::new();
        let graph = |i: usize| spark(ring.series.get(i).unwrap_or(&empty), ring.end, ring.cap, w);
        let kept = if h.resident { "" } else { ", since this viewer connected" };
        let window = format!("last {}{kept}", WINDOWS[self.window.min(WINDOWS.len() - 1)]);
        lines.push(Line::from(vec![
            Span::styled("CPU  ", dim),
            Span::styled(format!("{:>3.0}%", s.cpu.busy * 100.0), Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  load {:.2} {:.2} {:.2}", s.cpu.load[0], s.cpu.load[1], s.cpu.load[2]), dim),
            Span::styled(format!("   {window}"), dim),
        ]));
        lines.push(Line::from(Span::styled(graph(0), Style::default().fg(t.accent))));
        let used = s.mem.total.saturating_sub(s.mem.available);
        let mem_pct = used as f32 * 100.0 / s.mem.total.max(1) as f32;
        lines.push(Line::from(vec![
            Span::styled("MEM  ", dim),
            Span::styled(format!("{mem_pct:>3.0}%"), Style::default().fg(level(t, mem_pct)).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    "  {} of {} · cache {} · swap {} of {}",
                    gib(used),
                    gib(s.mem.total),
                    gib(s.mem.cached),
                    gib(s.mem.swap_used),
                    gib(s.mem.swap_total)
                ),
                dim,
            ),
        ]));
        lines.push(Line::from(Span::styled(graph(1), Style::default().fg(level(t, mem_pct)))));

        for (i, (info, g)) in inv.gpus.iter().zip(&s.gpus).enumerate() {
            let mut spans = vec![
                Span::styled(format!("GPU{i} "), dim),
                Span::styled(info.vendor.label(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!(" {}", short_name(&info.name))),
            ];
            match g.power {
                Power::Asleep => spans.push(Span::styled("  asleep, not woken to ask", dim)),
                Power::Idle => spans.push(Span::styled("  idle, left alone so it can sleep", dim)),
                Power::Awake if !info.metrics => spans.push(Span::styled("  no numbers for this GPU yet", dim)),
                Power::Awake => {
                    let mut facts = Vec::new();
                    if let Some(u) = g.util {
                        facts.push(format!("{u}%"));
                    }
                    let (mem_used, mem_total, what) = if info.unified {
                        (g.shared_used, info.shared_total, " shared")
                    } else {
                        (g.mem_used, info.mem_total, "")
                    };
                    if let (Some(u), Some(tot)) = (mem_used, mem_total) {
                        facts.push(format!("{}/{}{what}", gib(u), gib(tot)));
                    }
                    if info.unified {
                        if let (Some(u), Some(tot)) = (g.mem_used, info.mem_total) {
                            facts.push(format!("VRAM {}/{}", gib(u), gib(tot)));
                        }
                    }
                    if let Some(c) = g.temp_c {
                        facts.push(format!("{c:.0}°C"));
                    }
                    match (g.power_w, info.power_limit_w) {
                        (Some(p), Some(l)) => facts.push(format!("{p:.0}/{l:.0}W")),
                        (Some(p), None) => facts.push(format!("{p:.0}W")),
                        _ => {}
                    }
                    if let Some(c) = g.clock_mhz {
                        facts.push(format!("{c} MHz"));
                    }
                    spans.push(Span::styled(format!("  {}", facts.join(" · ")), dim));
                }
            }
            lines.push(Line::from(spans));
            lines.push(Line::from(Span::styled(graph(2 + i), Style::default().fg(t.accent))));
        }

        let mut io = vec![Span::styled("DISK ", dim)];
        for (m, d) in inv.mounts.iter().zip(&s.disks) {
            let pct = d.used as f32 * 100.0 / d.total.max(1) as f32;
            io.push(Span::raw(format!("{m} ")));
            io.push(Span::styled(format!("{pct:.0}% of {}  ", gib(d.total)), Style::default().fg(level(t, pct))));
        }
        lines.push(Line::from(io));
        lines.push(Line::from(vec![
            Span::styled("I/O  ", dim),
            Span::raw(format!(
                "net ↓{} ↑{}  disk r {} w {}",
                rate(s.net.read),
                rate(s.net.write),
                rate(s.disk_io.read),
                rate(s.disk_io.write)
            )),
        ]));
        lines.push(Line::from(""));

        // Workloads.
        let now_s = s.time_ms / 1000;
        let work = &h.workloads.list;
        lines.push(head(&format!("WORKLOADS  {}", work.len())));
        if work.is_empty() {
            lines.push(Line::from(Span::styled(
                "nothing training, serving or keeping the CPUs busy right now",
                dim,
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!("{:<11}{:<28}{:<30}{:>6}{:>7}{:>8}{:>9}{:>6}{:>6}", "KIND", "NAME", "MODEL", "PROCS", "CPU", "RSS", "GPU MEM", "GPU", "AGE"),
                dim,
            )));
            for (i, wl) in work.iter().enumerate() {
                let gpu_mem: u64 = wl.gpus.iter().map(|g| g.mem).sum();
                let busy = wl.gpus.iter().filter_map(|g| g.busy).max();
                let model = if wl.model.is_empty() && wl.kind == Kind::Inference { "no model loaded" } else { &wl.model };
                let row = format!(
                    "{:<28}{:<30}{:>6}{:>7.1}{:>8}{:>9}{:>6}{:>6}",
                    fit(&wl.label, 27),
                    fit(model, 29),
                    wl.procs,
                    wl.cpu,
                    gib(wl.rss),
                    if gpu_mem > 0 { gib(gpu_mem) } else { "-".into() },
                    busy.map_or("-".into(), |b| format!("{b}%")),
                    span(now_s.saturating_sub(wl.started_s)),
                );
                let kind = format!("{:<11}", wl.kind.label());
                let selected = i == self.selected;
                let style = if selected { Style::default().bg(t.border).add_modifier(Modifier::BOLD) } else { Style::default() };
                lines.push(Line::from(vec![
                    Span::styled(kind, style.fg(kind_color(t, wl.kind))),
                    Span::styled(row, style.fg(t.fg)),
                ]));
            }
            // The selected workload in full.
            if let Some(wl) = work.get(self.selected) {
                lines.push(Line::from(""));
                let mut facts = Vec::new();
                if !wl.ports.is_empty() {
                    let ports: Vec<String> = wl.ports.iter().map(|p| format!(":{p}")).collect();
                    facts.push(format!("listens on {}", ports.join(" ")));
                }
                for g in &wl.gpus {
                    facts.push(format!(
                        "gpu{} {}{}",
                        g.gpu,
                        gib(g.mem),
                        g.busy.map_or(String::new(), |b| format!(" {b}%"))
                    ));
                }
                if wl.kind == Kind::Training && wl.gpu_idle_s > 0 {
                    facts.push(format!("GPUs idle {}", span(wl.gpu_idle_s as u64)));
                }
                facts.push(format!("pid {}", wl.id));
                let stalled = wl.kind == Kind::Training && wl.gpu_idle_s >= STALL_S;
                lines.push(Line::from(vec![
                    Span::styled(format!("{}  ", wl.label), Style::default().fg(kind_color(t, wl.kind)).add_modifier(Modifier::BOLD)),
                    Span::styled(facts.join(" · "), Style::default().fg(if stalled { t.red } else { t.dim })),
                ]));
                if stalled {
                    lines.push(Line::from(Span::styled(
                        "possibly stalled: its processes are alive but its GPUs have done almost nothing",
                        Style::default().fg(t.red),
                    )));
                }
                if !wl.cmdline.is_empty() {
                    // Masked on the host: values of --api-key and the like
                    // never left it.
                    for chunk in wrap_chars(&format!("$ {}", wl.cmdline), w).into_iter().take(3) {
                        lines.push(Line::from(Span::styled(chunk, dim)));
                    }
                }
            }
        }
        lines.push(Line::from(""));

        // Alerts, newest first.
        if !h.alerts.is_empty() {
            let now = s.time_ms;
            lines.push(head(&format!("ALERTS  {}", h.alerts.len())));
            for a in h.alerts.iter().rev().take(8) {
                let new = a.id > h.read_up_to;
                let ago = format!("{:>4} ago  ", span(now.saturating_sub(a.time_ms) / 1000));
                let style = if new { Style::default().fg(t.red).add_modifier(Modifier::BOLD) } else { Style::default().fg(t.fg) };
                lines.push(Line::from(vec![Span::styled(ago, dim), Span::styled(fit(&a.message, w.saturating_sub(11)), style)]));
            }
            lines.push(Line::from(""));
        } else if !h.resident {
            lines.push(Line::from(Span::styled(
                "alerts and history from before this viewer connected need the resident agent: nerdfleet resident enable",
                dim,
            )));
            lines.push(Line::from(""));
        }

        // The busiest processes.
        let top = &h.workloads.top;
        if !top.is_empty() {
            lines.push(head("TOP PROCESSES"));
            lines.push(Line::from(Span::styled(format!("{:>8}  {:<24}{:>7}{:>9}{:>9}", "PID", "NAME", "CPU", "RSS", "GPU MEM"), dim)));
            for p in top {
                lines.push(Line::from(Span::raw(format!(
                    "{:>8}  {:<24}{:>7.1}{:>9}{:>9}",
                    p.pid,
                    fit(&p.name, 23),
                    p.cpu,
                    gib(p.rss),
                    if p.gpu_mem > 0 { gib(p.gpu_mem) } else { "-".into() }
                ))));
            }
        }

        let body = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(1),
        };
        Paragraph::new(lines).render(body, buf);
    }
}

/// `s` broken into lines of `width` characters, anywhere.
fn wrap_chars(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::spark;
    use std::collections::VecDeque;

    use crate::history::{NONE, RINGS};
    const HISTORY: usize = RINGS[0].1;

    /// Feeds points one at a time, as samples arrive, and renders after each.
    fn frames(width: usize, n: u64, value: impl Fn(u64) -> u8) -> Vec<String> {
        let mut s = VecDeque::new();
        (0..n)
            .map(|i| {
                if s.len() == HISTORY {
                    s.pop_front();
                }
                s.push_back(value(i));
                spark(&s, i, HISTORY, width)
            })
            .collect()
    }

    #[test]
    fn graphs_grow_and_scroll_without_jumping() {
        let width = 120;
        let frames = frames(width, 3 * HISTORY as u64, |i| ((i * 37) % 101) as u8);
        let mut drawn_before = 0;
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.chars().count(), width);
            // The drawn part never shrinks: no resets to the middle.
            let drawn = f.trim_start().chars().count();
            assert!(drawn >= drawn_before, "frame {i}: {drawn} cells after {drawn_before}");
            drawn_before = drawn;
        }
        let per = HISTORY.div_ceil(width);
        assert_eq!(drawn_before, HISTORY / per, "a full history fills the graph");
    }

    #[test]
    fn finished_cells_never_change() {
        let width = 120;
        let frames = frames(width, 3 * HISTORY as u64, |i| ((i * 37) % 101) as u8);
        let mut shifts = 0;
        for (i, pair) in frames.windows(2).enumerate() {
            let a: Vec<char> = pair[0].trim_start().chars().collect();
            let b: Vec<char> = pair[1].trim_start().chars().collect();
            let (n, m) = (a.len(), b.len());
            if n < 2 {
                continue;
            }
            // Only the newest cell changed...
            let same = n == m && a[..n - 1] == b[..n - 1];
            // ...or a new cell started: a growing graph keeps every cell,
            // a full one drops its oldest.
            let grew = m == n + 1 && a[..n - 1] == b[..n - 1];
            let scrolled = m == n && a[1..n - 1] == b[..n - 2];
            assert!(same || grew || scrolled, "frame {}: cells moved in place", i + 1);
            shifts += scrolled as u32;
        }
        assert!(shifts > 100, "a full graph scrolls");
    }

    #[test]
    fn short_spikes_still_show() {
        let s: VecDeque<u8> = (0..100).map(|i| if i == 37 { 100 } else { 0 }).collect();
        assert!(spark(&s, 99, HISTORY, 10).contains('█'));
        assert_eq!(spark(&VecDeque::new(), 0, HISTORY, 3), "   ");
        let gaps: VecDeque<u8> = [50, NONE, NONE, 50].into_iter().collect();
        assert_eq!(spark(&gaps, 3, 4, 4), "▅  ▅", "a gap is blank, not bridged");
    }
}

//! One host up close: graphs over the history the viewer keeps, every GPU,
//! every workload with its processes, and the busiest processes.

use super::app::HostView;
use super::ui::{bar, fit, gib, kind_color, level, rate, short_name, span, wrap, STALL_S};
use crate::conn::State;
use crate::history::NONE;
use crate::model::{AlertKind, Gpu, GpuInfo, Kind, Power, Sample, Vendor};
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
fn cells(series: &VecDeque<u8>, newest: u64, cap: usize, width: usize) -> Vec<u8> {
    if width == 0 || series.is_empty() {
        return vec![NONE; width];
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
    let mut out = vec![NONE; width.saturating_sub(cells.len())];
    out.extend(cells);
    out
}

/// `cells` as one row of blocks.
#[cfg(test)]
fn spark(series: &VecDeque<u8>, newest: u64, cap: usize, width: usize) -> String {
    cells(series, newest, cap, width).into_iter().map(super::ui::level_char).collect()
}

/// The graph windows `t` steps through: one per history ring.
pub const WINDOWS: [&str; 3] = ["15m", "6h", "24h"];

/// Width of the column of names and numbers left of each graph.
const GUTTER: usize = 38;
/// From this width, the lists below the graphs sit in two columns.
const TWO_COLUMNS: u16 = 140;

impl HostUi<'_> {
    pub fn draw(&self, area: Rect, buf: &mut Buffer) {
        let t = self.theme;
        let bg = if self.own_bg { t.bg } else { Color::Reset };
        buf.set_style(area, Style::default().bg(bg).fg(t.fg));
        let h = self.host;
        let dim = Style::default().fg(t.dim);

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

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(1),
        };
        let (Some(inv), Some(s)) = (&h.inventory, &h.sample) else {
            let wait = Line::from(Span::styled("waiting for the host's first report…", dim));
            Paragraph::new(vec![Line::from(""), wait]).render(inner, buf);
            return;
        };
        let w = inner.width as usize;

        // The lists below the graphs, laid out first: the graphs get the
        // height that is left.
        let two = area.width >= TWO_COLUMNS;
        let (left_w, right_w) = if two {
            let l = (w * 11 / 20).max(60);
            (l, w.saturating_sub(l + 3))
        } else {
            (w, w)
        };
        let mut left = self.workloads(left_w, s.time_ms / 1000);
        let mut right = self.disks(right_w);
        right.push(Line::from(""));
        right.extend(self.alerts(right_w));
        right.extend(self.top(right_w));
        let lower_h = if two {
            left.len().max(right.len())
        } else {
            left.push(Line::from(""));
            left.append(&mut right);
            left.len()
        };

        // The header: what the machine is, and which stretch the graphs show.
        let mut y = inner.y;
        let mut put = |line: Line, y: &mut u16| {
            if *y < inner.y + inner.height {
                line.render(Rect { y: *y, height: 1, ..inner }, buf);
            }
            *y += 1;
        };
        put(
            Line::from(Span::styled(
                format!(
                    "{} · {} · kernel {} · {} · {} threads · {} RAM",
                    inv.hostname,
                    inv.os,
                    inv.kernel,
                    inv.cpu_model,
                    inv.threads,
                    gib(inv.mem_total)
                ),
                dim,
            )),
            &mut y,
        );
        y += 1;
        let current = self.window.min(WINDOWS.len() - 1);
        let mut title = format!("HISTORY  last {}", WINDOWS[current]);
        if !h.resident {
            title.push_str(", since this viewer connected");
        }
        let mut choices = vec![Span::styled("t ", dim)];
        for (i, name) in WINDOWS.iter().enumerate() {
            let style = if i == current { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) } else { dim };
            choices.push(Span::styled(format!(" {name}"), style));
        }
        put(self.rule(&title, choices, w), &mut y);
        y += 1;

        // The graphs: names and numbers on the left, history on the right,
        // as tall as the leftover height allows.
        let mut rows = vec![self.cpu_rows(s), self.mem_rows(s)];
        for (i, (info, g)) in inv.gpus.iter().zip(&s.gpus).enumerate() {
            rows.push(self.gpu_rows(i, info, g));
        }
        let free = (inner.y + inner.height).saturating_sub(y) as usize;
        let free = free.saturating_sub(lower_h + 1);
        // Two rows at least, so each graph has room for its numbers; on a
        // short terminal the lists give way instead.
        let least = if inner.height >= 30 { 2 } else { 1 };
        let graph_h = (free / rows.len()).saturating_sub(1).clamp(least, 4);
        let ring = &h.history.rings[self.window.min(h.history.rings.len() - 1)];
        let empty = VecDeque::new();
        let graph_w = w.saturating_sub(GUTTER);
        for (i, (text, color)) in rows.into_iter().enumerate() {
            let cells = cells(ring.series.get(i).unwrap_or(&empty), ring.end, ring.cap, graph_w);
            let graph = graph_rows(&cells, graph_h, color, t.border);
            for (r, row) in graph.into_iter().enumerate() {
                let mut spans = match text.get(r) {
                    Some(line) => line.spans.clone(),
                    None => Vec::new(),
                };
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                spans.push(Span::raw(" ".repeat(GUTTER.saturating_sub(used))));
                spans.extend(row);
                put(Line::from(spans), &mut y);
            }
            y += 1;
        }

        // The lists.
        let lower = Rect { y, height: (inner.y + inner.height).saturating_sub(y), ..inner };
        if two {
            Paragraph::new(left).render(Rect { width: left_w as u16, ..lower }, buf);
            let x = lower.x + left_w as u16 + 3;
            Paragraph::new(right).render(Rect { x, width: right_w as u16, ..lower }, buf);
        } else {
            Paragraph::new(left).render(lower, buf);
        }
    }

    /// A section title followed by a rule to `width`, with `extra` at the
    /// right end.
    fn rule<'b>(&self, title: &str, extra: Vec<Span<'b>>, width: usize) -> Line<'b> {
        let t = self.theme;
        let extra_w: usize = extra.iter().map(|s| s.content.chars().count()).sum();
        let used = title.chars().count() + 2 + if extra_w > 0 { extra_w + 1 } else { 0 };
        let mut spans = vec![
            Span::styled(title.to_string(), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled("─".repeat(width.saturating_sub(used) + 1), Style::default().fg(t.border)),
        ];
        if extra_w > 0 {
            spans.push(Span::raw(" "));
            spans.extend(extra);
        }
        Line::from(spans)
    }

    /// The first line beside a graph: a name, and its number large.
    fn headline(&self, name: &str, value: String, color: Color) -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("{name:<6}"), Style::default().fg(self.theme.dim)),
            Span::styled(value, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        ]
    }

    fn cpu_rows(&self, s: &Sample) -> (Vec<Line<'static>>, Color) {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let pct = s.cpu.busy * 100.0;
        let text = vec![
            Line::from(self.headline("CPU", format!("{pct:.0}%"), t.fg)),
            Line::from(Span::styled(
                format!("      load {:.2} {:.2} {:.2}", s.cpu.load[0], s.cpu.load[1], s.cpu.load[2]),
                dim,
            )),
        ];
        (text, t.accent)
    }

    fn mem_rows(&self, s: &Sample) -> (Vec<Line<'static>>, Color) {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let used = s.mem.total.saturating_sub(s.mem.available);
        let pct = used as f32 * 100.0 / s.mem.total.max(1) as f32;
        let mut text = vec![
            Line::from(self.headline("MEM", format!("{pct:.0}%"), level(t, pct))),
            Line::from(Span::styled(format!("      {} of {}", gib(used), gib(s.mem.total)), dim)),
            Line::from(Span::styled(format!("      cache {}", gib(s.mem.cached)), dim)),
        ];
        if s.mem.swap_total > 0 {
            text.push(Line::from(Span::styled(
                format!("      swap {} of {}", gib(s.mem.swap_used), gib(s.mem.swap_total)),
                dim,
            )));
        }
        (text, level(t, pct))
    }

    fn gpu_rows(&self, i: usize, info: &GpuInfo, g: &Gpu) -> (Vec<Line<'static>>, Color) {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let value = match (g.power, g.util) {
            (Power::Awake, Some(u)) if info.metrics => format!("{u}%"),
            _ => "–".into(),
        };
        let mut text = vec![Line::from(self.headline(&format!("GPU{i}"), value, t.fg))];
        let name = gpu_name(info);
        text.push(Line::from(Span::styled(format!("      {}", fit(&name, GUTTER - 8)), Style::default().fg(t.fg))));
        let mut facts = Vec::new();
        match g.power {
            Power::Asleep => facts.push("asleep, not woken to ask".to_string()),
            Power::Idle => facts.push("idle, left to sleep".to_string()),
            Power::Awake if !info.metrics => facts.push("no numbers for this GPU yet".to_string()),
            Power::Awake => {
                let (mem_used, mem_total, what) = if info.unified {
                    (g.shared_used, info.shared_total, " shared")
                } else {
                    (g.mem_used, info.mem_total, "")
                };
                if let (Some(u), Some(tot)) = (mem_used, mem_total) {
                    facts.push(format!("{} of {}{what}", gib(u), gib(tot)));
                }
                let mut more = Vec::new();
                if info.unified {
                    if let (Some(u), Some(tot)) = (g.mem_used, info.mem_total) {
                        more.push(format!("VRAM {}/{}", gib(u), gib(tot)));
                    }
                }
                if let Some(c) = g.temp_c {
                    more.push(format!("{c:.0}°C"));
                }
                match (g.power_w, info.power_limit_w) {
                    (Some(p), Some(l)) => more.push(format!("{p:.0}/{l:.0}W")),
                    (Some(p), None) => more.push(format!("{p:.0}W")),
                    _ => {}
                }
                if !more.is_empty() {
                    facts.push(more.join(" · "));
                }
            }
        }
        for f in facts {
            text.push(Line::from(Span::styled(format!("      {}", fit(&f, GUTTER - 8)), dim)));
        }
        (text, t.accent)
    }

    /// Every workload, then the selected one in full.
    fn workloads(&self, width: usize, now_s: u64) -> Vec<Line<'static>> {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let work = &self.host.workloads.list;
        let mut lines = vec![self.rule(&format!("WORKLOADS  {}", work.len()), Vec::new(), width)];
        if work.is_empty() {
            lines.push(Line::from(Span::styled("nothing training, serving or keeping the CPUs busy right now", dim)));
            return lines;
        }
        // Fixed columns, then the name and (when any workload has one) the
        // model share what is left.
        const FIXED: usize = 11 + 6 + 7 + 8 + 9 + 6 + 6;
        let models = work.iter().any(|wl| !wl.model.is_empty() || wl.kind == Kind::Inference);
        let flex = width.saturating_sub(FIXED).max(16);
        let (name_w, model_w) =
            if models { ((flex / 2).min(40), (flex - flex / 2).min(40)) } else { (flex.min(48), 0) };
        lines.push(Line::from(Span::styled(
            format!(
                "{:<11}{:<name_w$}{:<model_w$}{:>6}{:>7}{:>8}{:>9}{:>6}{:>6}",
                "KIND",
                "NAME",
                if models { "MODEL" } else { "" },
                "PROCS",
                "CPU",
                "RSS",
                "GPU MEM",
                "GPU",
                "AGE"
            ),
            dim,
        )));
        for (i, wl) in work.iter().enumerate() {
            let gpu_mem: u64 = wl.gpus.iter().map(|g| g.mem).sum();
            let busy = wl.gpus.iter().filter_map(|g| g.busy).max();
            let model = if wl.model.is_empty() && wl.kind == Kind::Inference { "no model loaded" } else { &wl.model };
            let model = if models { fit(model, model_w.saturating_sub(1)) } else { String::new() };
            let row = format!(
                "{:<name_w$}{:<model_w$}{:>6}{:>7.1}{:>8}{:>9}{:>6}{:>6}",
                fit(&wl.label, name_w.saturating_sub(1)),
                model,
                wl.procs,
                wl.cpu,
                gib(wl.rss),
                if gpu_mem > 0 { gib(gpu_mem) } else { "-".into() },
                busy.map_or("-".into(), |b| format!("{b}%")),
                span(now_s.saturating_sub(wl.started_s)),
            );
            let kind = format!("{:<11}", wl.kind.label());
            let selected = i == self.selected;
            let style =
                if selected { Style::default().bg(t.border).add_modifier(Modifier::BOLD) } else { Style::default() };
            let pad = width.saturating_sub(11 + row.chars().count());
            lines.push(Line::from(vec![
                Span::styled(kind, style.fg(kind_color(t, wl.kind))),
                Span::styled(row, style.fg(t.fg)),
                Span::styled(" ".repeat(pad), style),
            ]));
        }

        // The selected workload in full.
        let Some(wl) = work.get(self.selected) else { return lines };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            wl.label.clone(),
            Style::default().fg(kind_color(t, wl.kind)).add_modifier(Modifier::BOLD),
        )));
        let mut facts = vec![format!("pid {}", wl.id)];
        if !wl.ports.is_empty() {
            let ports: Vec<String> = wl.ports.iter().map(|p| format!(":{p}")).collect();
            facts.push(format!("listens on {}", ports.join(" ")));
        }
        for g in &wl.gpus {
            facts.push(format!("gpu{} {}{}", g.gpu, gib(g.mem), g.busy.map_or(String::new(), |b| format!(" {b}%"))));
        }
        let stalled = wl.kind == Kind::Training && wl.gpu_idle_s >= STALL_S;
        if wl.kind == Kind::Training && wl.gpu_idle_s > 0 {
            facts.push(format!("GPUs idle {}", span(wl.gpu_idle_s as u64)));
        }
        lines.push(Line::from(Span::styled(facts.join(" · "), dim)));
        if stalled {
            lines.push(Line::from(Span::styled(
                "possibly stalled: its processes are alive but its GPUs have done almost nothing",
                Style::default().fg(t.red),
            )));
        }
        if !wl.cmdline.is_empty() {
            // Masked on the host: values of --api-key and the like never
            // left it. Broken between words, and indented after the first
            // line so the command stands apart from what follows.
            let mut cmd = wrap(&wl.cmdline, width.saturating_sub(4));
            if cmd.len() > 5 {
                cmd.truncate(5);
                let last = cmd.last_mut().expect("five lines");
                *last = fit(&format!("{last} …"), width.saturating_sub(4));
            }
            for (i, chunk) in cmd.into_iter().enumerate() {
                let lead = if i == 0 { "$ " } else { "    " };
                lines.push(Line::from(vec![Span::styled(lead, dim), Span::styled(chunk, Style::default().fg(t.fg))]));
            }
        }
        lines
    }

    /// Each mount as a bar, then the I/O rates.
    fn disks(&self, width: usize) -> Vec<Line<'static>> {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let (Some(inv), Some(s)) = (&self.host.inventory, &self.host.sample) else { return Vec::new() };
        let mut lines = vec![self.rule("DISKS", Vec::new(), width)];
        let mount_w = inv.mounts.iter().map(|m| m.chars().count()).max().unwrap_or(1).clamp(1, 24) + 2;
        let bar_w = width.saturating_sub(mount_w + 22).clamp(4, 20);
        for (m, d) in inv.mounts.iter().zip(&s.disks) {
            let pct = d.used as f32 * 100.0 / d.total.max(1) as f32;
            let mut spans = vec![Span::raw(format!("{:<mount_w$}", fit(m, mount_w - 2)))];
            spans.extend(bar(t, bar_w, pct, level(t, pct)));
            spans.push(Span::styled(format!(" {pct:>3.0}%",), Style::default().fg(level(t, pct))));
            spans.push(Span::styled(format!("  {} of {}", size(d.used), size(d.total)), dim));
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(vec![
            Span::styled("net  ", dim),
            Span::raw(format!("↓{} ↑{}", rate(s.net.read), rate(s.net.write))),
            Span::styled("   disk  ", dim),
            Span::raw(format!("read {} write {}", rate(s.disk_io.read), rate(s.disk_io.write))),
        ]));
        lines
    }

    /// Alerts, newest first.
    fn alerts(&self, width: usize) -> Vec<Line<'static>> {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let h = self.host;
        let Some(s) = &h.sample else { return Vec::new() };
        let mut lines = Vec::new();
        if h.alerts.is_empty() {
            if !h.resident {
                lines.push(self.rule("ALERTS", Vec::new(), width));
                for l in wrap("alerts and history from before this viewer connected need the resident agent: nerdfleet resident enable", width) {
                    lines.push(Line::from(Span::styled(l, dim)));
                }
                lines.push(Line::from(""));
            }
            return lines;
        }
        let new = h.alerts.iter().filter(|a| a.id > h.read_up_to).count();
        let extra =
            if new > 0 { vec![Span::styled(format!("{new} new"), Style::default().fg(t.red))] } else { Vec::new() };
        lines.push(self.rule(&format!("ALERTS  {}", h.alerts.len()), extra, width));
        for a in h.alerts.iter().rev().take(8) {
            let fresh = a.id > h.read_up_to;
            let color = match a.kind {
                AlertKind::WorkloadEnded => t.fg,
                AlertKind::DiskFull | AlertKind::HostUnreachable | AlertKind::TrainingStalled => t.red,
                _ => t.yellow,
            };
            let mut style = Style::default().fg(color);
            if fresh {
                style = style.add_modifier(Modifier::BOLD);
            }
            let ago = span(s.time_ms.saturating_sub(a.time_ms) / 1000);
            lines.push(Line::from(vec![
                Span::styled(if fresh { "● " } else { "  " }, Style::default().fg(t.red)),
                Span::styled(format!("{ago:>4}  "), dim),
                Span::styled(fit(&a.message, width.saturating_sub(8)), style),
            ]));
        }
        lines.push(Line::from(""));
        lines
    }

    /// The busiest processes, each with the workload it is part of; those
    /// of the selected workload stand out in its color.
    fn top(&self, width: usize) -> Vec<Line<'static>> {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let top = &self.host.workloads.top;
        if top.is_empty() {
            return Vec::new();
        }
        let work = &self.host.workloads.list;
        let selected = work.get(self.selected).map(|w| w.id);
        let flex = width.saturating_sub(8 + 2 + 7 + 8 + 9);
        let name_w = (flex * 2 / 5).clamp(10, 24);
        let job_w = flex.saturating_sub(name_w).clamp(8, 32);
        let mut lines = vec![self.rule("TOP PROCESSES", Vec::new(), width)];
        lines.push(Line::from(Span::styled(
            format!("{:>8}  {:<name_w$}{:<job_w$}{:>7}{:>8}{:>9}", "PID", "NAME", "WORKLOAD", "CPU", "RSS", "GPU MEM"),
            dim,
        )));
        for p in top {
            let job = work.iter().find(|w| p.job != 0 && w.id == p.job);
            let mine = job.is_some_and(|w| Some(w.id) == selected);
            let (label, job_style) = match job {
                Some(w) if mine => {
                    (w.label.as_str(), Style::default().fg(kind_color(t, w.kind)).add_modifier(Modifier::BOLD))
                }
                Some(w) => (w.label.as_str(), Style::default().fg(kind_color(t, w.kind))),
                None => ("-", dim),
            };
            let text =
                if mine { Style::default().fg(t.fg).add_modifier(Modifier::BOLD) } else { Style::default().fg(t.fg) };
            lines.push(Line::from(vec![
                Span::styled(format!("{:>8}  ", p.pid), dim),
                Span::styled(format!("{:<name_w$}", fit(&p.name, name_w.saturating_sub(1))), text),
                Span::styled(format!("{:<job_w$}", fit(label, job_w.saturating_sub(1))), job_style),
                Span::styled(
                    format!(
                        "{:>7.1}{:>8}{:>9}",
                        p.cpu,
                        gib(p.rss),
                        if p.gpu_mem > 0 { gib(p.gpu_mem) } else { "-".into() }
                    ),
                    text,
                ),
            ]));
        }
        lines
    }
}

/// A GPU's vendor and short name, without saying the vendor twice
/// ("NVIDIA NVIDIA RTX ...").
fn gpu_name(info: &GpuInfo) -> String {
    let name = short_name(&info.name);
    let vendor = info.vendor.label();
    if name.to_lowercase().starts_with(&vendor.to_lowercase()) || info.vendor == Vendor::Other {
        name
    } else {
        format!("{vendor} {name}")
    }
}

/// Bytes, in terabytes from a thousand gigabytes up.
fn size(bytes: u64) -> String {
    let t = bytes as f64 / (1u64 << 40) as f64;
    if t >= 1.0 {
        format!("{t:.1}T")
    } else {
        gib(bytes)
    }
}

/// `cells` (0 to 100, or `NONE`) as `height` rows of blocks with eighth-row
/// steps, top row first. Every measured cell shows at least a sliver along
/// the bottom, in the track color while it is near zero, so an idle
/// stretch reads as a baseline and a gap stays blank.
fn graph_rows(cells: &[u8], height: usize, color: Color, track: Color) -> Vec<Vec<Span<'static>>> {
    const EIGHTHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let height = height.max(1);
    (0..height)
        .map(|r| {
            let from_bottom = height - 1 - r;
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut run_color = None;
            for &v in cells {
                let (ch, c) = if v == NONE {
                    (' ', None)
                } else {
                    let filled = (v.min(100) as usize * height * 8 + 50) / 100;
                    let here = filled.saturating_sub(from_bottom * 8).min(8);
                    match here {
                        0 if from_bottom == 0 => ('▁', Some(track)),
                        0 => (' ', None),
                        n => (EIGHTHS[n - 1], Some(color)),
                    }
                };
                if c != run_color && !run.is_empty() {
                    let style = run_color.map_or(Style::default(), |c| Style::default().fg(c));
                    spans.push(Span::styled(std::mem::take(&mut run), style));
                }
                run_color = c;
                run.push(ch);
            }
            if !run.is_empty() {
                let style = run_color.map_or(Style::default(), |c| Style::default().fg(c));
                spans.push(Span::styled(run, style));
            }
            spans
        })
        .collect()
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

    /// Renders the host view of a busy machine at a wide and a narrow size;
    /// `cargo test host_view -- --nocapture` prints them.
    #[test]
    fn host_view_lays_out_at_any_width() {
        use super::HostUi;
        use crate::config::{Access, Host};
        use crate::conn::State;
        use crate::model::*;
        use crate::viewer::app::HostView;
        use nerdmux::embed::Theme;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut v = HostView::new(&Host { name: "host-a".into(), access: Access::Local });
        let nvidia = GpuInfo {
            vendor: Vendor::Nvidia,
            name: "NVIDIA RTX PRO 4500 Blackwell".into(),
            bus: "0000:01:00.0".into(),
            mem_total: Some(32 << 30),
            shared_total: None,
            unified: false,
            power_limit_w: Some(200.0),
            metrics: true,
        };
        let apu = GpuInfo {
            vendor: Vendor::Amd,
            name: "Granite Ridge [Radeon Graphics]".into(),
            mem_total: Some(2 << 30),
            shared_total: Some(45 << 30),
            unified: true,
            power_limit_w: None,
            ..nvidia.clone()
        };
        v.apply(AgentMsg::Inventory(Inventory {
            hostname: "host-a".into(),
            os: "Ubuntu 26.04.1 LTS".into(),
            kernel: "7.0.0-38-generic".into(),
            cpu_model: "AMD Ryzen 9 9950X 16-Core Processor".into(),
            threads: 32,
            mem_total: 90 << 30,
            mounts: vec!["/".into(), "/archive".into(), "/data".into()],
            gpus: vec![nvidia.clone(), nvidia, apu],
        }));
        let gpu = |util| Gpu {
            util: Some(util),
            mem_used: Some(512 << 20),
            temp_c: Some(28.0),
            power_w: Some(11.0),
            ..Default::default()
        };
        for i in 0..900u64 {
            v.apply(AgentMsg::Sample(Sample {
                time_ms: 1_000_000_000 + i * 1000,
                cpu: Cpu { busy: ((i % 300) as f32 / 300.0).min(0.9), cores: vec![0; 32], load: [11.91, 8.37, 5.43] },
                mem: Mem {
                    total: 90 << 30,
                    available: 68 << 30,
                    cached: 47 << 30,
                    swap_total: 8 << 30,
                    swap_used: 3 << 30,
                },
                disks: vec![
                    Disk { total: 1832 << 30, used: 1026 << 30 },
                    Disk { total: 3726 << 30, used: 520 << 30 },
                    Disk { total: 1833 << 30, used: 605 << 30 },
                ],
                net: Io { read: 5000, write: 3000 },
                disk_io: Io { read: 29_700_000, write: 344_000 },
                gpus: vec![gpu(if i > 600 { 95 } else { 0 }), gpu(0), gpu((i % 7) as u8)],
            }));
        }
        v.apply(AgentMsg::Workloads(Workloads {
            list: vec![Workload {
                id: 947791,
                kind: Kind::Cpu,
                label: "uv export.py".into(),
                model: String::new(),
                ports: vec![],
                procs: 17,
                started_s: 1_000_000 - 26 * 60,
                cpu: 12.0,
                rss: 19 << 30,
                gpus: vec![],
                gpu_idle_s: 0,
                cmdline: "/usr/bin/time -v uv run --no-sync python scripts/export.py --store /data/run/store --holdout v1 --eval-dir /data/run/eval --decontam-rule coverage:0.5 --unit-limit 100000 --near-dedup nl,math --layout repo --jobs 12 --solo-share 0.25 --out /data/run/shards".into(),
            }],
            top: (0..8)
                .map(|i| Proc {
                    pid: 956324 + i,
                    name: if i < 7 { "python3" } else { "rg" }.into(),
                    cpu: 1.0,
                    rss: 1 << 30,
                    gpu_mem: 0,
                    job: if i < 7 { 947791 } else { 0 },
                })
                .collect(),
        }));
        v.apply(AgentMsg::Alerts(
            (0..6)
                .map(|i| Alert {
                    id: i + 1,
                    time_ms: 1_000_000_000 - (6 - i) * 3_600_000,
                    kind: AlertKind::WorkloadEnded,
                    subject: "uv experiment.py".into(),
                    message: format!("training uv experiment.py ended after {}m", 10 * i + 4),
                })
                .collect(),
        ));
        v.set_state(State::Live);
        let theme = Theme::default();
        for (w, h) in [(210, 55), (100, 40)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            HostUi { theme: &theme, host: &v, selected: 0, own_bg: false, window: 0 }.draw(area, &mut buf);
            let text: Vec<String> = (0..h)
                .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
                .collect();
            println!("{w}x{h}\n{}\n", text.join("\n"));
            let all = text.join("\n");
            assert!(!all.contains("NVIDIA NVIDIA"), "the vendor is said once");
            for want in ["CPU", "MEM", "GPU2", "WORKLOADS", "DISKS", "ALERTS"] {
                assert!(all.contains(want), "{w}x{h} shows {want}");
            }
            if w >= super::TWO_COLUMNS {
                // The right column only: the row also holds the left one.
                let row = text.iter().find_map(|l| l.split_once("956324  python3").map(|(_, rest)| rest));
                assert!(
                    row.is_some_and(|r| r.trim_start().starts_with("uv export.py")),
                    "top processes name their workload"
                );
            }
        }
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

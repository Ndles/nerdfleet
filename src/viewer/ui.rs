//! Drawing the overview: a status bar and a grid of host cards.
//!
//! Utilization (CPU, GPU) is drawn in the accent color, because a busy GPU
//! is a good thing; capacity (memory, disk) goes from green to yellow to
//! red as it fills, because a full one is not.

use super::app::HostView;
use crate::conn::State;
use crate::model::{Gpu, GpuInfo, Kind, Power, Vendor, Workload};
use nerdmux::embed::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};
use std::path::Path;
use std::time::Instant;

/// Narrowest card worth drawing; wider screens get more columns.
const CARD_MIN_WIDTH: u16 = 50;
const LABEL: usize = 5;
const TAIL: usize = 20;
/// Width of a workload's kind on a card.
const KIND: usize = LABEL + 1;
/// Workloads listed on a card; the rest are counted.
const CARD_WORKLOADS: usize = 3;
/// A training run's GPUs idle this long is called out as possibly stalled.
pub const STALL_S: u32 = 10 * 60;

pub struct Ui<'a> {
    pub theme: &'a Theme,
    pub hosts: &'a [HostView],
    pub selected: usize,
    pub scroll: &'a mut u16,
    pub config_path: &'a Path,
}

impl Ui<'_> {
    fn body_bg(&self) -> Color {
        if self.theme.own_palette {
            self.theme.bg
        } else {
            Color::Reset
        }
    }

    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        let t = self.theme;
        buf.set_style(area, Style::default().bg(self.body_bg()).fg(t.fg));
        let bar = Rect { height: 1, ..area };
        self.status_bar(bar, buf);
        let body = Rect { y: area.y + 1, height: area.height.saturating_sub(1), ..area };
        if self.hosts.is_empty() {
            let msg = vec![
                Line::from(Span::styled("No hosts yet.", Style::default().fg(t.fg).add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Add them to ", Style::default().fg(t.dim)),
                    Span::styled(self.config_path.display().to_string(), Style::default().fg(t.cyan)),
                ]),
                Line::from(vec![
                    Span::styled("`nerdfleet config`", Style::default().fg(t.cyan)),
                    Span::styled(" prints an example.", Style::default().fg(t.dim)),
                ]),
            ];
            let h = msg.len() as u16;
            let r = Rect { y: body.y + body.height.saturating_sub(h) / 2, height: h.min(body.height), ..body };
            Paragraph::new(msg).alignment(ratatui::layout::Alignment::Center).render(r, buf);
            return;
        }
        self.grid(body, buf);
    }

    fn status_bar(&self, area: Rect, buf: &mut Buffer) {
        let t = self.theme;
        buf.set_style(area, Style::default().bg(t.status_bg).fg(t.status_fg));
        let live = self.hosts.iter().filter(|h| h.live()).count();
        let left = Line::from(vec![
            Span::styled(" nërdfleet ", Style::default().bg(t.accent).fg(t.accent_fg).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {live}/{} live", self.hosts.len()), Style::default().fg(t.status_fg)),
        ]);
        left.render(area, buf);
        let keys = Line::from(Span::styled("←↑↓→ select  Enter open  s shell  q quit ", Style::default().fg(t.dim)));
        let w = keys.width() as u16;
        if area.width > w + 20 {
            let r = Rect { x: area.x + area.width - w, width: w, ..area };
            keys.render(r, buf);
        }
    }

    fn grid(&mut self, area: Rect, buf: &mut Buffer) {
        let cols = (area.width / CARD_MIN_WIDTH).clamp(1, self.hosts.len() as u16);
        let width = area.width / cols;
        // Each row is as tall as its tallest card.
        let rows: Vec<(usize, u16)> = self
            .hosts
            .chunks(cols as usize)
            .enumerate()
            .map(|(i, row)| (i, row.iter().map(|h| card_lines(self.theme, h, width).len() as u16 + 2).max().unwrap_or(3)))
            .collect();
        // Scroll so the selected card's row is on screen.
        let sel_row = self.selected / cols as usize;
        let top: u16 = rows[..sel_row].iter().map(|r| r.1).sum();
        let bottom = top + rows[sel_row].1;
        if top < *self.scroll {
            *self.scroll = top;
        } else if bottom > *self.scroll + area.height {
            *self.scroll = bottom.saturating_sub(area.height);
        }
        let mut y = 0u16;
        for (row, height) in rows {
            for col in 0..cols as usize {
                let i = row * cols as usize + col;
                let Some(host) = self.hosts.get(i) else { break };
                let x = area.x + col as u16 * width;
                // The last column takes the remainder, so cards fill the line.
                let w = if col as u16 == cols - 1 { area.width - col as u16 * width } else { width };
                self.card(host, i == self.selected, Rect { x, y: 0, width: w, height }, y, area, buf);
            }
            y += height;
        }
    }

    /// Draws `host` at virtual row `vy` of the scrolled grid, clipped to
    /// `area`.
    fn card(&self, host: &HostView, selected: bool, size: Rect, vy: u16, area: Rect, buf: &mut Buffer) {
        let t = self.theme;
        let top = vy as i32 - *self.scroll as i32;
        if top + size.height as i32 <= 0 || top >= area.height as i32 {
            return;
        }
        // Draw into a scratch buffer, then copy the visible part.
        let full = Rect { x: 0, y: 0, width: size.width, height: size.height };
        let mut scratch = Buffer::empty(full);
        scratch.set_style(full, Style::default().bg(self.body_bg()).fg(t.fg));
        let border = if selected { t.accent } else { t.border };
        let dim = !host.live();
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .border_style(Style::default().fg(border))
            .title(Line::from(Span::styled(
                format!(" {} ", host.name),
                Style::default().fg(if selected { t.accent } else { t.fg }).add_modifier(Modifier::BOLD),
            )));
        if let Some(inv) = &host.inventory {
            block = block.title(
                Line::from(Span::styled(
                    format!(" {} · {} threads{} ", inv.os, inv.threads, if host.resident { " · resident" } else { "" }),
                    Style::default().fg(t.dim),
                ))
                    .right_aligned(),
            );
        }
        let inner = block.inner(full);
        block.render(full, &mut scratch);
        let mut lines = card_lines(t, host, size.width);
        if dim {
            for l in &mut lines {
                for s in &mut l.spans {
                    if s.style.fg != Some(t.red) && s.style.fg != Some(t.yellow) {
                        s.style = s.style.add_modifier(Modifier::DIM);
                    }
                }
            }
        }
        Paragraph::new(lines).render(inner, &mut scratch);
        for sy in 0..size.height {
            let y = top + sy as i32;
            if y < 0 || y >= area.height as i32 {
                continue;
            }
            for sx in 0..size.width {
                let (dx, dy) = (size.x + sx, area.y + y as u16);
                if dx < area.x + area.width {
                    buf[(dx, dy)] = scratch[(sx, sy)].clone();
                }
            }
        }
    }
}

// ===== card contents ========================================================

fn card_lines<'a>(t: &Theme, host: &'a HostView, width: u16) -> Vec<Line<'a>> {
    // Borders and a column of padding on each side.
    let iw = width.saturating_sub(4) as usize;
    let bar_w = iw.saturating_sub(LABEL + TAIL + 1).max(4);
    let mut lines = Vec::new();
    let label = |s: &str| Span::styled(format!("{s:<LABEL$}"), Style::default().fg(t.dim));

    if let (Some(inv), Some(s)) = (&host.inventory, &host.sample) {
        // CPU, with a sparkline of the last minutes in the tail.
        let cpu = s.cpu.busy * 100.0;
        let mut l = vec![label("CPU")];
        l.extend(bar(t, bar_w, cpu, t.accent));
        let pct = format!(" {:>3.0}% ", cpu);
        let spark_w = TAIL.saturating_sub(pct.len());
        l.push(Span::styled(pct, Style::default().fg(t.fg)));
        let empty = std::collections::VecDeque::new();
        let cpu_history = host.history.recent(0).unwrap_or(&empty);
        l.push(Span::styled(sparkline(cpu_history, spark_w), Style::default().fg(t.accent)));
        lines.push(Line::from(l));

        // Memory.
        let used = s.mem.total.saturating_sub(s.mem.available);
        let frac = used as f32 * 100.0 / s.mem.total.max(1) as f32;
        let mut l = vec![label("MEM")];
        l.extend(bar(t, bar_w, frac, level(t, frac)));
        l.push(Span::styled(
            fit(&format!(" {:>3.0}% {}/{}", frac, gib(used), gib(s.mem.total)), TAIL),
            Style::default().fg(t.fg),
        ));
        lines.push(Line::from(l));

        for (i, (info, g)) in inv.gpus.iter().zip(&s.gpus).enumerate() {
            lines.push(gpu_name_line(t, i, info, g, iw));
            lines.push(gpu_bar_line(t, info, g, bar_w));
        }

        let work = &host.workloads.list;
        for w in work.iter().take(CARD_WORKLOADS) {
            lines.push(workload_line(t, w, iw));
        }
        if work.len() > CARD_WORKLOADS {
            let more = format!("     +{} more · Enter for all", work.len() - CARD_WORKLOADS);
            lines.push(Line::from(Span::styled(more, Style::default().fg(t.dim))));
        }

        // Disks: each shown mount's fullness, as many as fit.
        let mut l = vec![label("DISK")];
        let mut used_w = LABEL;
        for (m, d) in inv.mounts.iter().zip(&s.disks) {
            let frac = d.used as f32 * 100.0 / d.total.max(1) as f32;
            let item = format!("{m} {frac:.0}%  ");
            if used_w + item.chars().count() > iw {
                break;
            }
            used_w += item.chars().count();
            l.push(Span::styled(format!("{m} "), Style::default().fg(t.fg)));
            l.push(Span::styled(format!("{frac:.0}%  "), Style::default().fg(level(t, frac))));
        }
        lines.push(Line::from(l));

        lines.push(Line::from(vec![
            label("I/O"),
            Span::styled("net ", Style::default().fg(t.dim)),
            Span::styled(format!("↓{} ↑{}", rate(s.net.read), rate(s.net.write)), Style::default().fg(t.fg)),
            Span::styled("  disk ", Style::default().fg(t.dim)),
            Span::styled(format!("r {} w {}", rate(s.disk_io.read), rate(s.disk_io.write)), Style::default().fg(t.fg)),
        ]));
    }

    match &host.state {
        State::Live => {}
        State::Connecting => lines.push(Line::from(Span::styled("connecting…", Style::default().fg(t.dim)))),
        State::Deploying => lines.push(Line::from(Span::styled("copying the agent…", Style::default().fg(t.dim)))),
        State::Down(why, retry) => {
            let secs = retry.saturating_duration_since(Instant::now()).as_secs();
            let when = if secs > 0 { format!(" · retry in {secs}s") } else { String::new() };
            for chunk in wrap(&format!("down: {why}{when}"), iw).into_iter().take(3) {
                lines.push(Line::from(Span::styled(chunk, Style::default().fg(t.red))));
            }
        }
    }
    // Unread alerts, newest first: what happened while nobody looked.
    let unread: Vec<&crate::model::Alert> = host.unread().collect();
    if let Some(newest) = unread.last() {
        let text = if unread.len() > 1 {
            format!("⚠ {} new · {}", unread.len(), newest.message)
        } else {
            format!("⚠ {}", newest.message)
        };
        lines.push(Line::from(Span::styled(fit(&text, iw), Style::default().fg(t.red).add_modifier(Modifier::BOLD))));
    }
    if let Some(p) = host.problems.last() {
        lines.push(Line::from(Span::styled(fit(&format!("! {p}"), iw), Style::default().fg(t.yellow))));
    }
    lines
}

pub fn kind_color(t: &Theme, k: Kind) -> Color {
    match k {
        Kind::Inference => t.cyan,
        Kind::Training => t.magenta,
        Kind::Cpu => t.yellow,
    }
}

/// `12s`, `4m`, `3h`, `2d`: the largest unit that fits.
pub fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// What a workload uses, short: `30.1G gpu0` or `6.2 cores`.
pub fn workload_use(w: &Workload) -> String {
    let gpu_mem: u64 = w.gpus.iter().map(|g| g.mem).sum();
    match w.gpus.len() {
        0 => format!("{:.1} cores", w.cpu),
        1 => format!("{} gpu{}", gib(gpu_mem), w.gpus[0].gpu),
        n => format!("{} {n} GPUs", gib(gpu_mem)),
    }
}

/// One line on a card: kind, name, what it serves or runs, and its use.
fn workload_line<'a>(t: &Theme, w: &Workload, iw: usize) -> Line<'a> {
    // One column wider than the other labels: "TRAIN" fills five.
    let kind = format!("{:<KIND$}", match w.kind {
        Kind::Inference => "INFR",
        Kind::Training => "TRAIN",
        Kind::Cpu => "CPU",
    });
    let stalled = w.kind == Kind::Training && w.gpu_idle_s >= STALL_S;
    let right = if stalled {
        format!("GPUs idle {}", span(w.gpu_idle_s as u64))
    } else {
        workload_use(w)
    };
    let mut detail = w.label.clone();
    if !w.model.is_empty() {
        detail.push_str(" · ");
        detail.push_str(&w.model);
    } else if w.kind == Kind::Inference {
        detail.push_str(" · no model loaded");
    }
    if let Some(p) = w.ports.first() {
        detail.push_str(&format!(" :{p}"));
    }
    let room = iw.saturating_sub(KIND + right.chars().count() + 1);
    let detail = fit(&detail, room);
    let pad = iw.saturating_sub(KIND + detail.chars().count() + right.chars().count());
    Line::from(vec![
        Span::styled(kind, Style::default().fg(kind_color(t, w.kind)).add_modifier(Modifier::BOLD)),
        Span::styled(detail, Style::default().fg(t.fg)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(if stalled { t.red } else { t.dim })),
    ])
}

fn vendor_color(t: &Theme, v: Vendor) -> Color {
    match v {
        Vendor::Nvidia => t.green,
        Vendor::Amd => t.red,
        Vendor::Intel => t.cyan,
        Vendor::Apple | Vendor::Other => t.fg,
    }
}

/// `Codename [Marketing name / Other name]` from the PCI database becomes
/// `Codename · Marketing name`.
pub fn short_name(name: &str) -> String {
    match name.split_once('[') {
        Some((code, rest)) => {
            let first = rest.trim_end_matches(']').split('/').next().unwrap_or("").trim();
            let code = code.trim();
            if first.is_empty() {
                code.to_string()
            } else if code.is_empty() {
                first.to_string()
            } else {
                format!("{code} · {first}")
            }
        }
        None => name.trim().to_string(),
    }
}

fn gpu_name_line<'a>(t: &Theme, i: usize, info: &GpuInfo, g: &Gpu, iw: usize) -> Line<'a> {
    let mut right = String::new();
    if let Some(c) = g.temp_c {
        right.push_str(&format!("{c:.0}°C"));
    }
    if let Some(w) = g.power_w {
        if !right.is_empty() {
            right.push(' ');
        }
        right.push_str(&format!("{w:.0}W"));
    }
    let tag = info.vendor.label();
    let name = short_name(&info.name);
    // Drop a vendor prefix the name repeats ("NVIDIA RTX ..." after "NVIDIA").
    let name = name.strip_prefix(tag).map(str::trim_start).unwrap_or(&name).to_string();
    let label = format!("GPU{i:<2}");
    let room = iw.saturating_sub(label.len() + tag.len() + 1 + right.chars().count() + 1);
    let name = fit(&name, room);
    let pad = iw.saturating_sub(label.len() + tag.len() + 1 + name.chars().count() + right.chars().count());
    Line::from(vec![
        Span::styled(label, Style::default().fg(t.dim)),
        Span::styled(tag.to_string(), Style::default().fg(vendor_color(t, info.vendor)).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {name}"), Style::default().fg(t.fg)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::default().fg(t.dim)),
    ])
}

fn gpu_bar_line<'a>(t: &Theme, info: &GpuInfo, g: &Gpu, bar_w: usize) -> Line<'a> {
    let indent = Span::raw(" ".repeat(LABEL));
    match g.power {
        Power::Awake => {}
        Power::Asleep => {
            return Line::from(vec![indent, Span::styled("asleep, not woken to ask", Style::default().fg(t.dim))])
        }
        Power::Idle => {
            return Line::from(vec![indent, Span::styled("idle, left alone so it can sleep", Style::default().fg(t.dim))])
        }
    }
    if !info.metrics {
        return Line::from(vec![indent, Span::styled("no numbers for this GPU yet", Style::default().fg(t.dim))]);
    }
    let util = g.util.unwrap_or(0) as f32;
    let mut l = vec![indent];
    l.extend(bar(t, bar_w, util, t.accent));
    // On unified memory the working set lives in shared memory (GTT), not
    // in the small VRAM carve-out.
    let (used, total, suffix) = if info.unified {
        (g.shared_used, info.shared_total, " sh")
    } else {
        (g.mem_used, info.mem_total, "")
    };
    let mem = match (used, total) {
        (Some(u), Some(tot)) => format!("{}/{}{suffix}", gib(u), gib(tot)),
        (Some(u), None) => format!("{}{suffix}", gib(u)),
        _ => String::new(),
    };
    let util_s = g.util.map_or(" n/a ".to_string(), |u| format!(" {u:>3}% "));
    l.push(Span::styled(util_s, Style::default().fg(t.fg)));
    let mem_frac = match (used, total) {
        (Some(u), Some(tot)) if tot > 0 => u as f32 * 100.0 / tot as f32,
        _ => 0.0,
    };
    l.push(Span::styled(fit(&mem, TAIL.saturating_sub(6)), Style::default().fg(level(t, mem_frac))));
    Line::from(l)
}

// ===== drawing helpers ======================================================

/// Green, yellow or red for how full something is.
pub fn level(t: &Theme, pct: f32) -> Color {
    if pct >= 90.0 {
        t.red
    } else if pct >= 75.0 {
        t.yellow
    } else {
        t.green
    }
}

/// A bar `width` cells wide filled to `pct`, with eighth-cell precision,
/// on a track in the border color.
pub fn bar<'a>(t: &Theme, width: usize, pct: f32, color: Color) -> Vec<Span<'a>> {
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = ((pct.clamp(0.0, 100.0) / 100.0) * (width * 8) as f32).round() as usize;
    let full = eighths / 8;
    let part = eighths % 8;
    let mut spans = vec![Span::styled("█".repeat(full), Style::default().fg(color))];
    let mut rest = width - full;
    if part > 0 && rest > 0 {
        spans.push(Span::styled(EIGHTHS[part].to_string(), Style::default().fg(color).bg(t.border)));
        rest -= 1;
    }
    spans.push(Span::styled("█".repeat(rest), Style::default().fg(t.border)));
    spans
}

/// The last `width` points of `series` (0 to 100) as block characters.
pub fn sparkline(series: &std::collections::VecDeque<u8>, width: usize) -> String {
    let skip = series.len().saturating_sub(width);
    let mut s: String = series.iter().skip(skip).map(|&v| level_char(v)).collect();
    // Right-align, so the newest point is always at the same place.
    let pad = width.saturating_sub(s.chars().count());
    s.insert_str(0, &" ".repeat(pad));
    s
}

/// A block for a value from 0 to 100; a gap (`history::NONE`) is blank.
pub fn level_char(v: u8) -> char {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if v == crate::history::NONE {
        return ' ';
    }
    LEVELS[(v.min(100) as usize * 7 + 50) / 100]
}

/// `s` cut to `width` characters, with an ellipsis when cut.
pub fn fit(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = vec![String::new()];
    for word in s.split(' ') {
        let cur = lines.last_mut().expect("never empty");
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(String::new());
        }
        let cur = lines.last_mut().expect("never empty");
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&fit(word, width));
    }
    lines
}

/// Bytes as GiB with one decimal, or MiB below one GiB.
pub fn gib(bytes: u64) -> String {
    let g = bytes as f64 / (1u64 << 30) as f64;
    if g >= 100.0 {
        format!("{g:.0}G")
    } else if g >= 1.0 {
        format!("{g:.1}G")
    } else {
        format!("{}M", bytes >> 20)
    }
}

/// Bytes per second, short.
pub fn rate(bps: u64) -> String {
    let b = bps as f64;
    if b >= 1e9 {
        format!("{:.1}G/s", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1}M/s", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0}K/s", b / 1e3)
    } else {
        format!("{bps}B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(
            short_name("Strix Halo [Radeon Graphics / Radeon 8050S Graphics / Radeon 8060S Graphics]"),
            "Strix Halo · Radeon Graphics"
        );
        assert_eq!(short_name("NVIDIA RTX PRO 4500 Blackwell"), "NVIDIA RTX PRO 4500 Blackwell");
        assert_eq!(short_name("AD107M [GeForce RTX 4060 Max-Q / Mobile]"), "AD107M · GeForce RTX 4060 Max-Q");
    }

    #[test]
    fn sizes_and_rates() {
        assert_eq!(gib(89 << 30), "89.0G");
        assert_eq!(gib(104 << 30), "104G");
        assert_eq!(gib(512 << 20), "512M");
        assert_eq!(rate(1_500_000), "1.5M/s");
        assert_eq!(rate(12), "12B/s");
    }

    #[test]
    fn bars_are_exactly_their_width() {
        let t = Theme::default();
        for pct in [0.0, 12.5, 50.0, 99.9, 100.0, 140.0] {
            let w: usize = bar(&t, 20, pct, t.accent).iter().map(|s| s.content.chars().count()).sum();
            assert_eq!(w, 20, "at {pct}%");
        }
    }

    #[test]
    fn sparklines_are_right_aligned() {
        let s: std::collections::VecDeque<u8> = [0, 100].into_iter().collect();
        assert_eq!(sparkline(&s, 4), "  ▁█");
        let gap: std::collections::VecDeque<u8> = [50, crate::history::NONE, 50].into_iter().collect();
        assert_eq!(sparkline(&gap, 3), "▅ ▅");
    }
}

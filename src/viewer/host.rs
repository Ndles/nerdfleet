//! One host up close: graphs over the history the viewer keeps, every GPU,
//! every workload with its processes, and the busiest processes.

use super::app::{HostView, Item};
use super::ui::{
    bar, clock, fit, gib, kind_color, left as duration, level, loss, rate, run_color, short_name, si, span, wrap,
    STALL_S,
};
use crate::conn::State;
use crate::history::NONE;
use crate::model::{Alert, AlertKind, Gpu, GpuInfo, Kind, Power, QueueInfo, RunInfo, RunStatus, Sample, Vendor};
use nerdmux::embed::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Widget};
use std::collections::VecDeque;

/// The lists of the host view that take the arrow keys, in `Tab` order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Section {
    Runs,
    #[default]
    Workloads,
    Alerts,
}

/// What is selected in the host view: a section, a line in each list (by
/// position in the list as shown), and whether that line is open in full.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sel {
    pub section: Section,
    pub workload: usize,
    pub run: usize,
    /// Newest first.
    pub alert: usize,
    /// The first alert in view: the list scrolls only when the selection
    /// would leave it, and stays put while other sections have the keys.
    pub alert_top: usize,
    pub detail: bool,
}

impl Sel {
    /// Runs first when the host has any, the workloads otherwise.
    pub fn new(h: &HostView) -> Sel {
        let section = if Sel::sections(h).contains(&Section::Runs) { Section::Runs } else { Section::Workloads };
        Sel { section, ..Sel::default() }
    }

    fn len(h: &HostView, section: Section) -> usize {
        match section {
            Section::Runs => h.items().len(),
            Section::Workloads => h.workloads.list.len(),
            Section::Alerts => h.alerts.len(),
        }
    }

    /// The sections there is something in; the workloads always, and the
    /// runs while any are hidden, so `H` can bring them back.
    fn sections(h: &HostView) -> Vec<Section> {
        [Section::Runs, Section::Workloads, Section::Alerts]
            .into_iter()
            .filter(|&s| match s {
                Section::Workloads => true,
                Section::Runs => Sel::len(h, s) > 0 || h.hidden_count() > 0,
                Section::Alerts => Sel::len(h, s) > 0,
            })
            .collect()
    }

    fn index(&mut self) -> &mut usize {
        match self.section {
            Section::Runs => &mut self.run,
            Section::Workloads => &mut self.workload,
            Section::Alerts => &mut self.alert,
        }
    }

    /// The next section (`back`: the one before), round.
    pub fn cycle(&mut self, h: &HostView, back: bool) {
        let all = Sel::sections(h);
        let at = all.iter().position(|&s| s == self.section).unwrap_or(0);
        let n = all.len();
        self.section = all[if back { (at + n - 1) % n } else { (at + 1) % n }];
        self.detail = false;
        self.clamp(h);
    }

    /// Moves within the section.
    pub fn step(&mut self, h: &HostView, down: bool) {
        let last = Sel::len(h, self.section).saturating_sub(1);
        let i = self.index();
        *i = if down { (*i + 1).min(last) } else { i.saturating_sub(1) };
        self.scroll_alerts();
    }

    fn scroll_alerts(&mut self) {
        if self.alert < self.alert_top {
            self.alert_top = self.alert;
        } else if self.alert >= self.alert_top + ALERT_ROWS {
            self.alert_top = self.alert + 1 - ALERT_ROWS;
        }
    }

    /// Keeps every line in its list as the lists change, and leaves a
    /// section that emptied.
    pub fn clamp(&mut self, h: &HostView) {
        if !Sel::sections(h).contains(&self.section) {
            self.section = Section::Workloads;
            self.detail = false;
        }
        self.run = self.run.min(Sel::len(h, Section::Runs).saturating_sub(1));
        self.workload = self.workload.min(Sel::len(h, Section::Workloads).saturating_sub(1));
        self.alert = self.alert.min(Sel::len(h, Section::Alerts).saturating_sub(1));
        self.alert_top = self.alert_top.min(Sel::len(h, Section::Alerts).saturating_sub(ALERT_ROWS));
        self.scroll_alerts();
        if self.detail && (self.section == Section::Workloads || Sel::len(h, self.section) == 0) {
            self.detail = false;
        }
    }

    /// The selected line of the runs list.
    pub fn item<'a>(&self, h: &'a HostView) -> Option<Item<'a>> {
        h.items().get(self.run).copied()
    }

    /// The selected alert.
    pub fn alert<'a>(&self, h: &'a HostView) -> Option<&'a Alert> {
        h.alerts.iter().rev().nth(self.alert)
    }

    /// The run `n` opens: the selected one, while the runs have the keys.
    pub fn run_to_open<'a>(&self, h: &'a HostView) -> Option<&'a RunInfo> {
        match (self.section, self.item(h)) {
            (Section::Runs, Some(Item::Run(r))) => Some(r),
            _ => h.focus_run(Some(self.workload)),
        }
    }
}

/// Alerts shown at once; the list scrolls.
const ALERT_ROWS: usize = 8;

pub struct HostUi<'a> {
    pub theme: &'a Theme,
    pub host: &'a HostView,
    pub sel: Sel,
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

/// `series` (any numbers) as `width` cells of blocks, scaled between its own
/// lowest and highest values: a loss curve's shape, not its level.
fn line_spark(series: &[f32], width: usize) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if series.is_empty() || width == 0 {
        return String::new();
    }
    let n = series.len().min(width);
    let cells: Vec<f32> = (0..n)
        .map(|i| {
            let (a, b) = (i * series.len() / n, ((i + 1) * series.len() / n).max(i * series.len() / n + 1));
            series[a..b].iter().sum::<f32>() / (b - a) as f32
        })
        .collect();
    let (lo, hi) = cells.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let span = (hi - lo).max(f32::EPSILON);
    cells.iter().map(|v| BLOCKS[(((v - lo) / span) * 7.0).round().clamp(0.0, 7.0) as usize]).collect()
}

/// The graph windows `t` steps through: one per history ring.
pub const WINDOWS: [&str; 3] = ["15m", "6h", "24h"];

/// Width of the column of names and numbers left of each graph.
const GUTTER: usize = 40;
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
        match &h.state {
            State::Live => {
                if let Some(note) = h.update_note() {
                    left.push(Span::styled(format!(" {note} "), Style::default().fg(t.yellow)));
                }
            }
            State::Down(_, retry) => {
                left.push(Span::styled(" down ", Style::default().fg(t.red)));
                left.push(Span::raw(" "));
                left.extend(super::ui::retry_line(t, retry));
            }
            _ => left.push(Span::styled(" not live ", Style::default().fg(t.red))),
        }
        let left = Line::from(left);
        let left_w = left.width() as u16;
        left.render(bar_area, buf);
        // As many keys as leave the left side whole, the least needed going first.
        let esc = if self.sel.detail { "close" } else { "back" };
        let all = [
            ("Tab", "section"),
            ("↑↓", "select"),
            ("⏎", "details"),
            ("t", "window"),
            ("s", "terminal"),
            ("n", "nerdtui"),
            ("z", "zoom"),
            ("Esc", esc),
            ("q", "quit"),
        ];
        for drop in [&[][..], &["q"], &["q", "z"], &["q", "z", "t"], &["q", "z", "t", "s"]] {
            let keys: Vec<_> = all.iter().copied().filter(|(k, _)| !drop.contains(k)).collect();
            let keys = super::ui::keys_line(t, &keys);
            let kw = keys.width() as u16;
            if area.width > left_w.max(30) + kw {
                keys.render(Rect { x: area.x + area.width - kw, width: kw, ..bar_area }, buf);
                break;
            }
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
        let mut left = self.runs(left_w);
        left.extend(self.workloads(left_w, s.time_ms / 1000));
        let mut right = self.alerts(right_w);
        // Disks and the busiest processes, to look at, not to select: last,
        // side by side across the whole width.
        let mut strip = self.machine_strip(w);
        let lower_h = if two {
            left.len().max(right.len()) + 1 + strip.len()
        } else {
            left.push(Line::from(""));
            left.append(&mut right);
            left.append(&mut strip);
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
                {
                    // The kernel goes first when the line is too long.
                    let tail = format!("{} · {} threads · {} RAM", inv.cpu_model, inv.threads, gib(inv.mem_total));
                    let full = format!("{} · {} · kernel {} · {tail}", inv.hostname, inv.os, inv.kernel);
                    let short = format!("{} · {} · {tail}", inv.hostname, inv.os);
                    fit(if full.chars().count() <= w { &full } else { &short }, w)
                },
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
        // One row for the lists' gap, one for the time axis.
        let free = free.saturating_sub(lower_h + 2);
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
        // The time axis under the last graph: how far back the left edge is.
        if graph_w > 12 {
            let ago = format!("{} ago", WINDOWS[current]);
            let gap = graph_w.saturating_sub(ago.chars().count() + 3);
            put(Line::from(Span::styled(format!("{}{ago}{}now", " ".repeat(GUTTER), " ".repeat(gap)), dim)), &mut y);
            y += 1;
        }

        // The lists.
        let lower = Rect { y, height: (inner.y + inner.height).saturating_sub(y), ..inner };
        if two {
            let lists_h = left.len().max(right.len()) as u16 + 1;
            Paragraph::new(left).render(Rect { width: left_w as u16, ..lower }, buf);
            let x = lower.x + left_w as u16 + 3;
            Paragraph::new(right).render(Rect { x, width: right_w as u16, ..lower }, buf);
            let y = lower.y + lists_h.min(lower.height);
            let below = Rect { y, height: (lower.y + lower.height).saturating_sub(y), ..lower };
            Paragraph::new(strip).render(below, buf);
        } else {
            Paragraph::new(left).render(lower, buf);
        }

        if self.sel.detail {
            self.popup(area, buf, s.time_ms);
        }
    }

    /// The selected run, queue or alert in full, over the middle of the view.
    fn popup(&self, area: Rect, buf: &mut Buffer, now_ms: u64) {
        let t = self.theme;
        let h = self.host;
        let w = area.width.saturating_sub(4).min(100);
        let inner_w = w.saturating_sub(4) as usize;
        let (title, color, mut lines) = match self.sel.section {
            Section::Runs => match self.sel.item(h) {
                Some(Item::Run(r)) => (r.name.clone(), run_color(t, r), self.run_detail(r, inner_w, now_ms)),
                Some(Item::Queue(q)) => (q.name.clone(), t.cyan, self.queue_detail(q, inner_w, now_ms)),
                None => return,
            },
            Section::Alerts => match self.sel.alert(h) {
                Some(a) => (a.kind.label().to_string(), alert_color(t, a.kind), self.alert_detail(a, inner_w, now_ms)),
                None => return,
            },
            Section::Workloads => return,
        };
        let mut hints = vec![("Esc", "close"), ("↑↓", "next")];
        match self.sel.section {
            Section::Runs if self.sel.item(h).is_some_and(|i| i.status() != RunStatus::Running) => {
                hints.push(("x", "hide"))
            }
            Section::Alerts => hints.push(("x", "dismiss")),
            _ => {}
        }
        if matches!(self.sel.item(h), Some(Item::Run(_))) && self.sel.section == Section::Runs {
            hints.push(("n", "nerdtui"));
        }
        lines.push(Line::from(""));
        lines.push(super::ui::keys_line(t, &hints));
        let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
        if w < 20 || height < 4 {
            return;
        }
        let rect = Rect {
            x: area.x + (area.width - w) / 2,
            y: area.y + 1 + (area.height.saturating_sub(1 + height)) / 2,
            width: w,
            height,
        };
        Clear.render(rect, buf);
        let bg = if self.own_bg { t.bg } else { Color::Reset };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color))
            .title(Span::styled(
                format!(" {} ", fit(&title, w.saturating_sub(6) as usize)),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(bg).fg(t.fg));
        Paragraph::new(lines).block(block).render(rect, buf);
    }

    /// A label and its value, the value wrapped under itself.
    fn field(&self, label: &str, value: &str, style: Style, width: usize) -> Vec<Line<'static>> {
        const LABEL: usize = 10;
        let dim = Style::default().fg(self.theme.dim);
        wrap(value, width.saturating_sub(LABEL).max(8))
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let lead = if i == 0 { format!("{label:<LABEL$}") } else { " ".repeat(LABEL) };
                Line::from(vec![Span::styled(lead, dim), Span::styled(chunk, style)])
            })
            .collect()
    }

    fn run_detail(&self, r: &RunInfo, width: usize, now_ms: u64) -> Vec<Line<'static>> {
        let t = self.theme;
        let fg = Style::default().fg(t.fg);
        let mut out = Vec::new();
        let mut status = r.status.label().to_string();
        if r.status == RunStatus::Running && r.quiet_s >= super::ui::QUIET_S {
            status.push_str(&format!(", no step for {}", duration(r.quiet_s as u64)));
        }
        out.extend(self.field("status", &status, Style::default().fg(run_color(t, r)), width));
        let mut steps = if r.max_steps > 0 {
            let done = r.step.saturating_sub(r.start_step) as f32;
            let all = r.max_steps.saturating_sub(r.start_step).max(1) as f32;
            format!("{} of {} ({:.0}%)", r.step, r.max_steps, (done / all * 100.0).min(100.0))
        } else {
            format!("{}", r.step)
        };
        if r.start_step > 0 {
            steps.push_str(&format!(", from {}", r.start_step));
        }
        out.extend(self.field("steps", &steps, fg, width));
        if r.started_ms > 0 {
            let ago = span(now_ms.saturating_sub(r.started_ms) / 1000);
            out.extend(self.field("started", &format!("{} ({ago} ago)", clock(r.started_ms)), fg, width));
        }
        match r.status {
            RunStatus::Running => {
                if r.started_ms > 0 {
                    out.extend(self.field("running", &duration(now_ms.saturating_sub(r.started_ms) / 1000), fg, width));
                }
                if let Some(eta) = r.eta_s {
                    let at = clock(now_ms + eta as u64 * 1000);
                    out.extend(self.field("ETA", &format!("{} left, about {at}", duration(eta as u64)), fg, width));
                }
            }
            _ if r.ended_ms > 0 => {
                let mut end = format!("{} ({} ago)", clock(r.ended_ms), span(now_ms.saturating_sub(r.ended_ms) / 1000));
                if r.started_ms > 0 && r.ended_ms > r.started_ms {
                    end.push_str(&format!(", after {}", duration((r.ended_ms - r.started_ms) / 1000)));
                }
                out.extend(self.field("ended", &end, fg, width));
            }
            _ => {}
        }
        if r.max_steps > 0 && r.step < r.max_steps && r.status != RunStatus::Running && r.status != RunStatus::Finished
        {
            out.extend(self.field("short", &format!("{} steps", r.max_steps - r.step), fg, width));
        }
        let mut pace = Vec::new();
        if let Some(st) = r.step_s {
            pace.push(format!("{st:.2} s/step"));
        }
        if let Some(tps) = r.tokens_per_s {
            pace.push(format!("{} tok/s", si(tps as f64)));
        }
        if !pace.is_empty() {
            out.extend(self.field("pace", &pace.join(" · "), fg, width));
        }
        let lo = |v: &[f32]| v.iter().copied().fold(None, |m: Option<f32>, x| Some(m.map_or(x, |m| m.min(x))));
        let mut l = loss(r.loss);
        if let (Some(first), Some(min)) = (r.series.first(), lo(&r.series)) {
            l.push_str(&format!("  (first {}, lowest {})", loss(Some(*first)), loss(Some(min))));
        }
        out.extend(self.field("loss", &l, fg, width));
        if r.series.len() > 1 {
            let spark = line_spark(&r.series, width.saturating_sub(10).min(80));
            out.push(Line::from(vec![
                Span::raw(" ".repeat(10)),
                Span::styled(spark, Style::default().fg(run_color(t, r))),
            ]));
        }
        if r.eval.is_some() {
            let mut e = format!("{} at step {}", loss(r.eval), r.eval_step);
            if let Some(best) = lo(&r.evals) {
                e.push_str(&format!("  (lowest {}, {} evals)", loss(Some(best)), r.evals.len()));
            }
            out.extend(self.field("eval", &e, fg, width));
            if r.evals.len() > 1 {
                let spark = line_spark(&r.evals, width.saturating_sub(10).min(80));
                out.push(Line::from(vec![Span::raw(" ".repeat(10)), Span::styled(spark, Style::default().fg(t.cyan))]));
            }
        }
        if r.nonfinite > 0 || r.bad_lines > 0 {
            let style = Style::default().fg(t.yellow);
            out.extend(self.field(
                "problems",
                &format!("{} non-finite losses · {} unreadable lines", r.nonfinite, r.bad_lines),
                style,
                width,
            ));
        }
        if r.workload != 0 {
            let label = self.host.workloads.list.iter().find(|w| w.id == r.workload).map(|w| w.label.clone());
            let wl = label.map_or(format!("pid {}", r.workload), |l| format!("{l}, pid {}", r.workload));
            out.extend(self.field("workload", &wl, fg, width));
        }
        out.extend(self.field("run dir", &r.run_dir, fg, width));
        out.extend(self.field("card", &format!("{}.json", r.key), Style::default().fg(t.dim), width));
        out
    }

    fn queue_detail(&self, q: &QueueInfo, width: usize, now_ms: u64) -> Vec<Line<'static>> {
        let fg = Style::default().fg(self.theme.fg);
        let mut out = self.field("status", q.status.label(), fg, width);
        let mut progress = format!("{} of {} done", q.done, q.total);
        if q.failed > 0 {
            progress.push_str(&format!(", {} failed", q.failed));
        }
        out.extend(self.field("progress", &progress, fg, width));
        if !q.current.is_empty() {
            out.extend(self.field("now", &q.current, fg, width));
        }
        for (label, ms) in [("started", q.started_ms), ("updated", q.updated_ms)] {
            if ms > 0 {
                let ago = span(now_ms.saturating_sub(ms) / 1000);
                out.extend(self.field(label, &format!("{} ({ago} ago)", clock(ms)), fg, width));
            }
        }
        out.extend(self.field("card", &format!("{}.json", q.key), Style::default().fg(self.theme.dim), width));
        out
    }

    fn alert_detail(&self, a: &Alert, width: usize, now_ms: u64) -> Vec<Line<'static>> {
        let t = self.theme;
        let fg = Style::default().fg(t.fg);
        let ago = span(now_ms.saturating_sub(a.time_ms) / 1000);
        let mut out = self.field("when", &format!("{} ({ago} ago)", clock(a.time_ms)), fg, width);
        out.extend(self.field("about", &a.subject, fg, width));
        out.extend(self.field("what", &a.message, Style::default().fg(alert_color(t, a.kind)), width));
        // The run it is about, while the host still has it.
        let run = self.host.runs.runs.iter().find(|r| r.name == a.subject);
        if let Some(r) = run {
            out.extend(self.field("run now", &super::ui::run_text(r), Style::default().fg(t.dim), width));
        }
        out
    }

    /// The title of a section that takes the keys, marked while it has
    /// them, with what its keys do at the right end then.
    fn section_rule<'b>(
        &self,
        section: Section,
        title: &str,
        mut extra: Vec<Span<'b>>,
        keys: &[(&str, &str)],
        width: usize,
    ) -> Line<'b> {
        if self.sel.section != section {
            return self.rule(title, extra, width);
        }
        if !keys.is_empty() {
            if !extra.is_empty() {
                extra.push(Span::raw("  "));
            }
            extra.extend(super::ui::keys_line(self.theme, keys).spans);
        }
        // The keys go when they would crowd out the title.
        let extra_w: usize = extra.iter().map(|s| s.content.chars().count()).sum();
        if title.chars().count() + 6 + extra_w > width {
            extra.clear();
        }
        // Marked by color alone, so nothing moves when the keys move on.
        let t = self.theme;
        let title_style = Style::default().bg(t.accent).fg(t.accent_fg).add_modifier(Modifier::BOLD);
        self.rule_styled(title, title_style, t.accent, extra, width)
    }

    /// A section title followed by a rule to `width`, with `extra` at the
    /// right end.
    fn rule<'b>(&self, title: &str, extra: Vec<Span<'b>>, width: usize) -> Line<'b> {
        let t = self.theme;
        self.rule_styled(title, Style::default().fg(t.accent).add_modifier(Modifier::BOLD), t.border, extra, width)
    }

    fn rule_styled<'b>(
        &self,
        title: &str,
        title_style: Style,
        rule_color: Color,
        extra: Vec<Span<'b>>,
        width: usize,
    ) -> Line<'b> {
        let extra_w: usize = extra.iter().map(|s| s.content.chars().count()).sum();
        let used = title.chars().count() + 2 + if extra_w > 0 { extra_w + 1 } else { 0 };
        let mut spans = vec![
            Span::styled(title.to_string(), title_style),
            Span::raw(" "),
            Span::styled("─".repeat(width.saturating_sub(used) + 1), Style::default().fg(rule_color)),
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

    /// Training runs and queues from the host's run cards, without those
    /// put away; the selected one, or the one `n` opens, marked. Nothing when
    /// the host has none.
    fn runs(&self, width: usize) -> Vec<Line<'static>> {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let h = self.host;
        let items = h.items();
        let hidden = h.hidden_count();
        if items.is_empty() && hidden == 0 {
            return Vec::new();
        }
        let focused = self.sel.section == Section::Runs;
        let runs = items.iter().filter(|i| matches!(i, Item::Run(_))).count();
        let mut extra = Vec::new();
        if hidden > 0 {
            extra.push(Span::styled(format!("{hidden} hidden"), dim));
        }
        let mut keys = vec![("⏎", "details")];
        if self.sel.item(h).is_some_and(|i| i.status() != RunStatus::Running) {
            keys.push(("x", "hide"));
        }
        if items.iter().any(|i| i.status() != RunStatus::Running) {
            keys.push(("X", "hide ended"));
        }
        if hidden > 0 {
            keys.push(("H", "show hidden"));
        }
        let mut lines = vec![self.section_rule(Section::Runs, &format!("RUNS  {runs}"), extra, &keys, width)];
        if items.is_empty() {
            lines.push(Line::from(Span::styled("every run is hidden: H shows them again", dim)));
            lines.push(Line::from(""));
            return lines;
        }
        let selected = if focused { Some(self.sel.run) } else { None };
        let opens = self.sel.run_to_open(h).map(|r| r.key.clone());
        let sel_bg = Style::default().bg(t.border);
        for (i, item) in items.iter().enumerate() {
            let chosen = selected == Some(i);
            let row = if chosen { sel_bg } else { Style::default() };
            match item {
                Item::Queue(q) => {
                    let color = if q.status == RunStatus::Running { t.cyan } else { t.dim };
                    let mark = if chosen { "▶ " } else { "≡ " };
                    let text = fit(&super::ui::queue_text(q), width.saturating_sub(2));
                    let pad = width.saturating_sub(2 + text.chars().count());
                    lines.push(Line::from(vec![
                        Span::styled(mark, row.fg(if chosen { t.accent } else { color })),
                        Span::styled(text, row.fg(if chosen { t.fg } else { t.dim })),
                        Span::styled(" ".repeat(pad), row),
                    ]));
                }
                Item::Run(run) => lines.extend(self.run_lines(run, chosen, opens.as_deref(), width)),
            }
        }
        lines.push(Line::from(""));
        lines
    }

    /// A run's two lines: name and numbers with its loss so far, then what
    /// it is doing and where.
    fn run_lines(&self, run: &RunInfo, chosen: bool, opens: Option<&str>, width: usize) -> Vec<Line<'static>> {
        let t = self.theme;
        const SPARK: usize = 24;
        let row = if chosen { Style::default().bg(t.border) } else { Style::default() };
        let marked = chosen || opens == Some(run.key.as_str()) && self.sel.section != Section::Runs;
        let color = run_color(t, run);
        let steps = if run.max_steps > 0 { format!("{}/{}", run.step, run.max_steps) } else { format!("{}", run.step) };
        let mut nums = format!("{steps:>11}  loss {:<7}", loss(run.loss));
        if run.eval.is_some() {
            nums.push_str(&format!(" val {:<7}", loss(run.eval)));
        }
        let spark = if run.series.len() > 1 { line_spark(&run.series, SPARK) } else { String::new() };
        let name_w = width.saturating_sub(2 + nums.chars().count() + SPARK + 2).max(12);
        let mark = if marked { "▶ " } else { "  " };
        let name_style = if marked { row.add_modifier(Modifier::BOLD) } else { row };
        let used = 2 + name_w + nums.chars().count() + 2 + spark.chars().count();
        let first = Line::from(vec![
            Span::styled(mark, row.fg(t.accent)),
            Span::styled(format!("{:<name_w$}", fit(&run.name, name_w.saturating_sub(1))), name_style.fg(t.fg)),
            Span::styled(nums, row.fg(t.fg)),
            Span::styled("  ", row),
            Span::styled(spark, row.fg(color)),
            Span::styled(" ".repeat(width.saturating_sub(used)), row),
        ]);
        let mut info = vec![run.status.label().to_string()];
        match run.status {
            RunStatus::Running => {
                if let Some(eta) = run.eta_s {
                    info.push(format!("{} left", duration(eta as u64)));
                }
                if run.quiet_s >= super::ui::QUIET_S {
                    info.push(format!("no step for {}", duration(run.quiet_s as u64)));
                }
            }
            _ => {
                if run.max_steps > 0 && run.step < run.max_steps && run.status != RunStatus::Finished {
                    info.push(format!("stopped {} short", run.max_steps - run.step));
                }
                if run.ended_ms > 0 {
                    let now = self.host.sample.as_ref().map_or_else(super::app::now_ms, |s| s.time_ms);
                    info.push(format!("{} ago", span(now.saturating_sub(run.ended_ms) / 1000)));
                }
            }
        }
        if let Some(tps) = run.tokens_per_s {
            info.push(format!("{} tok/s", si(tps as f64)));
        }
        if let Some(st) = run.step_s {
            info.push(format!("{st:.2} s/step"));
        }
        if run.eval.is_some() {
            info.push(format!("eval at {}", run.eval_step));
        }
        if run.nonfinite > 0 {
            info.push(format!("{} non-finite", run.nonfinite));
        }
        if run.bad_lines > 0 {
            info.push(format!("{} unreadable lines", run.bad_lines));
        }
        info.push(run.run_dir.clone());
        let text = fit(&info.join(" · "), width.saturating_sub(2));
        let pad = width.saturating_sub(2 + text.chars().count());
        let second = Line::from(vec![
            Span::styled("  ", row),
            Span::styled(text, row.fg(if color == t.magenta { t.dim } else { color })),
            Span::styled(" ".repeat(pad), row),
        ]);
        vec![first, second]
    }

    fn workloads(&self, width: usize, now_s: u64) -> Vec<Line<'static>> {
        let t = self.theme;
        let dim = Style::default().fg(t.dim);
        let work = &self.host.workloads.list;
        let mut lines =
            vec![self.section_rule(Section::Workloads, &format!("WORKLOADS  {}", work.len()), Vec::new(), &[], width)];
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
            // The selected one is the one shown in full below; its row is
            // lit only while the workloads have the keys.
            let style = match (i == self.sel.workload, self.sel.section == Section::Workloads) {
                (true, true) => Style::default().bg(t.border).add_modifier(Modifier::BOLD),
                (true, false) => Style::default().add_modifier(Modifier::BOLD),
                _ => Style::default(),
            };
            let pad = width.saturating_sub(11 + row.chars().count());
            lines.push(Line::from(vec![
                Span::styled(kind, style.fg(kind_color(t, wl.kind))),
                Span::styled(row, style.fg(t.fg)),
                Span::styled(" ".repeat(pad), style),
            ]));
        }

        // The selected workload in full.
        let Some(wl) = work.get(self.sel.workload) else { return lines };
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
        if !wl.model.is_empty() {
            for (i, chunk) in wrap(&wl.model, width.saturating_sub(8)).into_iter().take(3).enumerate() {
                let lead = if i == 0 { "models  " } else { "        " };
                lines.push(Line::from(vec![Span::styled(lead, dim), Span::styled(chunk, Style::default().fg(t.fg))]));
            }
        }
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

    /// The disks and the busiest processes beside each other, a line
    /// between them; one above the other where they do not both fit.
    fn machine_strip(&self, width: usize) -> Vec<Line<'static>> {
        const SIDE_BY_SIDE: usize = 100;
        if width < SIDE_BY_SIDE {
            let mut lines = self.disks(width);
            lines.push(Line::from(""));
            lines.extend(self.top(width));
            return lines;
        }
        let disks_w = (width * 2 / 5).clamp(40, 60);
        let top_w = width - disks_w - 3;
        let disks = self.disks(disks_w);
        let top = self.top(top_w);
        let sep = Span::styled(" │ ", Style::default().fg(self.theme.border));
        (0..disks.len().max(top.len()))
            .map(|i| {
                let mut spans = disks.get(i).map_or_else(Vec::new, |l| clip(l, disks_w));
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                spans.push(Span::raw(" ".repeat(disks_w - used)));
                spans.push(sep.clone());
                if let Some(l) = top.get(i) {
                    spans.extend(clip(l, top_w));
                }
                Line::from(spans)
            })
            .collect()
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
        let net = vec![Span::styled("net   ", dim), Span::raw(format!("↓{} ↑{}", rate(s.net.read), rate(s.net.write)))];
        let io = vec![
            Span::styled("disk  ", dim),
            Span::raw(format!("read {} write {}", rate(s.disk_io.read), rate(s.disk_io.write))),
        ];
        // On one line where it fits, else two.
        let one: usize = net.iter().chain(&io).map(|s| s.content.chars().count()).sum::<usize>() + 3;
        if one <= width {
            let mut spans = net;
            spans.push(Span::raw("   "));
            spans.extend(io);
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(net));
            lines.push(Line::from(io));
        }
        lines
    }

    /// Alerts, newest first, as many as fit in `ALERT_ROWS`, scrolled to
    /// keep the selected one in view.
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
            } else if h.cleared_up_to > 0 {
                lines.push(self.rule("ALERTS", Vec::new(), width));
                lines.push(Line::from(Span::styled("none since they were cleared", dim)));
                lines.push(Line::from(""));
            }
            return lines;
        }
        let focused = self.sel.section == Section::Alerts;
        let n = h.alerts.len();
        let first = self.sel.alert_top.min(n.saturating_sub(1));
        let last = (first + ALERT_ROWS).min(n);
        let mut extra = Vec::new();
        let new = h.unread().count();
        if new > 0 {
            extra.push(Span::styled(format!("{new} new"), Style::default().fg(t.red)));
        }
        if n > ALERT_ROWS {
            if !extra.is_empty() {
                extra.push(Span::raw("  "));
            }
            extra.push(Span::styled(format!("{}–{last} of {n}", first + 1), dim));
        }
        let keys = [("⏎", "details"), ("x", "dismiss"), ("c", "clear all")];
        lines.push(self.section_rule(Section::Alerts, &format!("ALERTS  {n}"), extra, &keys, width));
        for (i, a) in h.alerts.iter().rev().enumerate().skip(first).take(ALERT_ROWS) {
            let chosen = focused && i == self.sel.alert;
            let row = if chosen { Style::default().bg(t.border) } else { Style::default() };
            let fresh = a.id > h.read_up_to;
            let mut style = row.fg(alert_color(t, a.kind));
            if fresh || chosen {
                style = style.add_modifier(Modifier::BOLD);
            }
            let ago = span(s.time_ms.saturating_sub(a.time_ms) / 1000);
            let mark = if chosen {
                "▶ "
            } else if fresh {
                "● "
            } else {
                "  "
            };
            let text = fit(&a.message, width.saturating_sub(8));
            let pad = width.saturating_sub(8 + text.chars().count());
            lines.push(Line::from(vec![
                Span::styled(mark, row.fg(if chosen { t.accent } else { t.red })),
                Span::styled(format!("{ago:>4}  "), row.fg(t.dim)),
                Span::styled(text, style),
                Span::styled(" ".repeat(pad), row),
            ]));
        }
        if n > last {
            lines.push(Line::from(Span::styled(format!("  … {} older", n - last), dim)));
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
        let selected = work.get(self.sel.workload).map(|w| w.id);
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
            // A process doing nothing right now is there for its memory; it
            // steps back.
            let idle = p.cpu < 0.05 && p.gpu_mem == 0;
            let text = match (mine, idle) {
                (true, _) => Style::default().fg(t.fg).add_modifier(Modifier::BOLD),
                (false, true) => dim,
                (false, false) => Style::default().fg(t.fg),
            };
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

/// The spans of `line`, cut to `width` characters.
fn clip(line: &Line<'static>, width: usize) -> Vec<Span<'static>> {
    let mut left = width;
    let mut out = Vec::new();
    for span in &line.spans {
        if left == 0 {
            break;
        }
        let n = span.content.chars().count();
        if n <= left {
            out.push(span.clone());
            left -= n;
        } else {
            out.push(Span::styled(span.content.chars().take(left).collect::<String>(), span.style));
            left = 0;
        }
    }
    out
}

/// The color an alert is told in: red for what needs a look, yellow for
/// what may, plain for news.
fn alert_color(t: &Theme, kind: AlertKind) -> Color {
    match kind {
        AlertKind::WorkloadEnded | AlertKind::RunEnded => t.fg,
        AlertKind::DiskFull
        | AlertKind::HostUnreachable
        | AlertKind::TrainingStalled
        | AlertKind::RunDied
        | AlertKind::RunNonfinite => t.red,
        _ => t.yellow,
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
                    // Anything above zero shows, so a quiet line stays a line.
                    let filled = ((v.min(100) as usize * height * 8 + 50) / 100).max(usize::from(v > 0));
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
        for (w, h) in [(210, 55), (117, 60), (100, 40)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            HostUi { theme: &theme, host: &v, sel: super::Sel::default(), own_bg: false, window: 0 }
                .draw(area, &mut buf);
            let text: Vec<String> = (0..h)
                .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
                .collect();
            println!("{w}x{h}\n{}\n", text.join("\n"));
            let all = text.join("\n");
            assert!(!all.contains("NVIDIA NVIDIA"), "the vendor is said once");
            for want in ["CPU", "MEM", "GPU2", "WORKLOADS", "ALERTS"] {
                assert!(all.contains(want), "{w}x{h} shows {want}");
            }
            // Disks and processes come last: a short screen cuts them first.
            if h >= 55 {
                assert!(all.contains("DISKS") && all.contains("TOP PROCESSES"), "{w}x{h} shows the machine");
            }
            if w >= 100 && h >= 55 {
                assert!(all.lines().any(|l| l.contains("DISKS") && l.contains("│ TOP PROCESSES")), "side by side");
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

#[cfg(test)]
mod bar_tests {
    use super::HostUi;
    use crate::config::{Access, Host};
    use crate::conn::{Retry, State};
    use crate::viewer::app::HostView;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn the_keys_never_cover_what_a_down_host_does_next() {
        let theme = nerdmux::embed::Theme::default();
        let mut host = HostView::new(&Host { name: "host-a".into(), access: Access::Local });
        host.set_state(State::Down("connection timed out".into(), Retry::GaveUp));
        for width in [80, 99, 115, 133, 160, 200] {
            let area = Rect::new(0, 0, width, 20);
            let mut buf = Buffer::empty(area);
            HostUi { theme: &theme, host: &host, sel: super::Sel::default(), own_bg: false, window: 0 }
                .draw(area, &mut buf);
            let bar: String = (0..width).map(|x| buf[(x, 0)].symbol()).collect();
            assert!(bar.contains("r tries again"), "{width}: {bar}");
        }
    }
}

#[cfg(test)]
mod select_tests {
    use super::{HostUi, Section, Sel};
    use crate::config::{Access, Host};
    use crate::conn::State;
    use crate::model::*;
    use crate::viewer::app::{now_ms, HostView};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn busy_host() -> HostView {
        let mut v = HostView::new(&Host { name: "host-a".into(), access: Access::Local });
        v.apply(AgentMsg::Inventory(Inventory {
            hostname: "host-a".into(),
            threads: 32,
            mem_total: 90 << 30,
            mounts: vec!["/".into()],
            ..Default::default()
        }));
        let now = now_ms();
        v.apply(AgentMsg::Sample(Sample {
            time_ms: now,
            cpu: Cpu { busy: 0.1, cores: vec![0; 32], load: [1.0; 3] },
            mem: Mem { total: 90 << 30, available: 80 << 30, ..Default::default() },
            disks: vec![Disk { total: 100, used: 50 }],
            ..Default::default()
        }));
        let run = |name: &str, status, step, started: u64| RunInfo {
            key: format!("{name}-key"),
            name: name.into(),
            run_dir: format!("/data/exp/runs/{name}"),
            status,
            workload: 0,
            start_step: 0,
            max_steps: 2400,
            step,
            loss: Some(3.271),
            eval: Some(2.21),
            eval_step: 2200,
            tokens_per_s: Some(34_900.0),
            step_s: Some(15.0),
            eta_s: (status == RunStatus::Running).then_some(840),
            quiet_s: 0,
            nonfinite: 0,
            bad_lines: 0,
            series: (0..60).map(|i| 11.0 - i as f32 * 0.1).collect(),
            evals: vec![3.0, 2.5, 2.21],
            started_ms: now - started,
            ended_ms: if status == RunStatus::Running { 0 } else { now - 60_000 },
        };
        v.apply(AgentMsg::Runs(Runs {
            runs: vec![
                run("exp/run_a", RunStatus::Running, 2340, 3_600_000),
                run("run_b", RunStatus::Finished, 2400, 7_200_000),
                run("run_c", RunStatus::Died, 10, 7_200_000),
            ],
            queues: vec![QueueInfo {
                key: "q".into(),
                name: "exp".into(),
                status: RunStatus::Running,
                done: 4,
                total: 8,
                current: "train run_a".into(),
                failed: 0,
                started_ms: now - 3_600_000,
                updated_ms: now,
            }],
        }));
        v.apply(AgentMsg::Alerts(
            (0..20)
                .map(|i| Alert {
                    id: (now - 1000) * 1000 + i,
                    time_ms: now - (20 - i) * 60_000,
                    kind: AlertKind::RunEnded,
                    subject: "run_b".into(),
                    message: format!("alert number {i}: run_b finished at step 2400 of 2400: loss 3.271, eval 2.210"),
                })
                .collect(),
        ));
        v.set_state(State::Live);
        v
    }

    fn draw(v: &HostView, sel: Sel, w: u16, h: u16) -> String {
        let theme = nerdmux::embed::Theme::default();
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        HostUi { theme: &theme, host: v, sel, own_bg: false, window: 0 }.draw(area, &mut buf);
        let text: Vec<String> = (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect();
        text.join("\n")
    }

    #[test]
    fn nothing_moves_when_another_section_takes_the_keys() {
        let v = busy_host();
        let mut sel = Sel::new(&v);
        sel.alert = 12;
        sel.scroll_alerts();
        let left = |sel: Sel| -> Vec<String> {
            draw(&v, sel, 117, 60)
                .lines()
                .map(|l| l.chars().take(60).collect::<String>().replace(['▶', '≡', '●'], " "))
                .collect()
        };
        let first = left(sel);
        for _ in 0..3 {
            sel.cycle(&v, false);
            assert_eq!(left(sel), first, "{:?}", sel.section);
        }
    }

    /// `cargo test select_tests -- --nocapture` prints them.
    #[test]
    fn runs_and_alerts_take_the_keys_and_open_in_full() {
        let v = busy_host();
        let mut sel = Sel::new(&v);
        assert_eq!(sel.section, Section::Runs);
        sel.step(&v, true);
        sel.step(&v, true);
        let list = draw(&v, sel, 117, 60);
        println!("{list}\n");
        assert!(list.contains("\n RUNS  3 "), "the focused section's title stays where it was");
        assert!(list.contains("▶ run_b"), "the selected run is marked");
        sel.detail = true;
        let popup = draw(&v, sel, 117, 60);
        println!("{popup}\n");
        for want in ["/data/exp/runs/run_b", "run_b-key.json", "2400 of 2400", "lowest 2.210"] {
            assert!(popup.contains(want), "the run in full shows {want}");
        }
        sel.cycle(&v, false);
        sel.cycle(&v, false);
        assert_eq!(sel.section, Section::Alerts);
        for _ in 0..12 {
            sel.step(&v, true);
        }
        let alerts = draw(&v, sel, 117, 60);
        println!("{alerts}\n");
        assert!(alerts.contains("6–13 of 20"), "the alerts scroll to the selection");
        assert!(alerts.contains("▶") && alerts.contains("alert number 7"));
        sel.detail = true;
        let popup = draw(&v, sel, 117, 60);
        println!("{popup}");
        assert!(popup.contains("run ended") && popup.contains("eval 2.210"));
    }
}

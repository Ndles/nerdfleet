//! A checklist on the whole screen, for `nerdfleet add`.

use super::ui::keys_line;
use super::Screen;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Terminal;

/// The list and what is happening to it, apart from the screen.
struct Picker<'a> {
    /// Each alias, and whether the fleet already has it.
    items: &'a [(String, bool)],
    checked: Vec<bool>,
    cursor: usize,
}

enum Outcome {
    Going,
    Done,
    Cancelled,
}

impl Picker<'_> {
    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Outcome {
        match code {
            KeyCode::Char('c') if modifiers == KeyModifiers::CONTROL => return Outcome::Cancelled,
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancelled,
            KeyCode::Enter => return Outcome::Done,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.items.len().saturating_sub(1))
            }
            KeyCode::Char(' ') if !self.items[self.cursor].1 => self.checked[self.cursor] ^= true,
            _ => {}
        }
        Outcome::Going
    }

    fn chosen(&self) -> Vec<String> {
        self.items.iter().zip(&self.checked).filter(|(_, c)| **c).map(|(i, _)| i.0.clone()).collect()
    }

    fn draw(&self, area: Rect, buf: &mut ratatui::buffer::Buffer, theme: &nerdmux::embed::Theme) {
        let t = theme;
        let bar = Rect { height: 1.min(area.height), ..area };
        buf.set_style(bar, Style::default().bg(t.status_bg).fg(t.status_fg));
        Line::from(vec![
            Span::styled(" nërdfleet ", Style::default().bg(t.accent).fg(t.accent_fg).add_modifier(Modifier::BOLD)),
            Span::raw("  add hosts from your SSH config"),
        ])
        .render(bar, buf);
        let keys = keys_line(t, &[("Space", "choose"), ("Enter", "add"), ("Esc", "cancel")]);
        let w = keys.width() as u16;
        if area.width > w + 36 {
            keys.render(Rect { x: area.x + area.width - w, width: w, ..bar }, buf);
        }
        let rows = area.height.saturating_sub(2) as usize;
        let first = (self.cursor + 1).saturating_sub(rows);
        let lines: Vec<Line> = (first..self.items.len())
            .take(rows)
            .map(|i| {
                let (name, listed) = &self.items[i];
                let (mark, note) = match (*listed, self.checked[i]) {
                    (true, _) => ("[·] ", "  in the fleet"),
                    (false, true) => ("[x] ", ""),
                    (false, false) => ("[ ] ", ""),
                };
                let mut style = Style::default().fg(if *listed { t.dim } else { t.fg });
                if i == self.cursor {
                    style = style.fg(t.accent).add_modifier(Modifier::BOLD);
                }
                Line::from(vec![
                    Span::styled(format!(" {mark}{name}"), style),
                    Span::styled(note, Style::default().fg(t.dim)),
                ])
            })
            .collect();
        let body = Rect { y: area.y + 2.min(area.height), height: area.height.saturating_sub(2), ..area };
        Paragraph::new(lines).render(body, buf);
    }
}

/// Lets the user tick some of `items`. `None` if they cancelled.
pub fn pick(items: &[(String, bool)]) -> anyhow::Result<Option<Vec<String>>> {
    let mut picker = Picker { items, checked: vec![false; items.len()], cursor: 0 };
    let theme = nerdmux::embed::load_theme();
    let _screen = Screen::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    loop {
        term.draw(|f| {
            let area = f.area();
            picker.draw(area, f.buffer_mut(), &theme);
        })?;
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Release {
                continue;
            }
            match picker.key(k.code, k.modifiers) {
                Outcome::Going => {}
                Outcome::Done => return Ok(Some(picker.chosen())),
                Outcome::Cancelled => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_ticked_and_those_in_the_fleet_cannot_be() {
        let items = vec![("host-a".to_string(), false), ("host-b".to_string(), true), ("host-c".to_string(), false)];
        let mut p = Picker { items: &items, checked: vec![false; 3], cursor: 0 };
        let none = KeyModifiers::NONE;
        p.key(KeyCode::Char(' '), none);
        p.key(KeyCode::Down, none);
        p.key(KeyCode::Char(' '), none);
        p.key(KeyCode::Down, none);
        p.key(KeyCode::Down, none);
        assert_eq!(p.cursor, 2, "stops at the end");
        p.key(KeyCode::Char(' '), none);
        p.key(KeyCode::Char(' '), none);
        assert_eq!(p.chosen(), ["host-a"]);
        assert!(matches!(p.key(KeyCode::Enter, none), Outcome::Done));
        assert!(matches!(p.key(KeyCode::Esc, none), Outcome::Cancelled));
    }

    #[test]
    fn the_cursor_stays_on_screen_at_any_size() {
        let items: Vec<(String, bool)> = (0..50).map(|i| (format!("host-{i}"), false)).collect();
        let p = Picker { items: &items, checked: vec![false; 50], cursor: 40 };
        let theme = nerdmux::embed::Theme::default();
        for (w, h) in [(80, 10), (10, 3), (1, 1)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            p.draw(area, &mut buf, &theme);
            if h == 10 {
                let screen: String = buf.content().iter().map(|c| c.symbol()).collect();
                assert!(screen.contains("host-40"), "{screen}");
            }
        }
    }
}

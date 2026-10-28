//! Terminals: a host's own nerdmux, in a pane of nerdmux's terminal engine.
//!
//! The terminal runs the host's nerdmux as the user knows it, status bar,
//! windows and keys included, attached to their last session there: the
//! same interface on every machine, and sessions that outlive the
//! connection and the viewer. A host without nerdmux gets a login shell,
//! which ends with the connection.

use crate::config::{Access, Host};
use crate::remote::Ssh;
use nerdmux::embed::{Notice, Options, Terminal, TerminalEvent, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::path::Path;
use std::sync::mpsc::SyncSender;

/// What the terminal runs on a host: the first nerdmux on PATH, in
/// `~/.local/bin` or in `~/.cargo/bin` (non-interactive SSH commands rarely
/// have the last two on PATH), through a login shell so its shells get the
/// PATH a login would; or else a login shell. The path ends up in a command
/// line, so one with anything but plain characters is skipped.
const SCRIPT: &str = "for n in nerdmux \"$HOME/.local/bin/nerdmux\" \"$HOME/.cargo/bin/nerdmux\"; do \
    m=\"$(command -v \"$n\" 2>/dev/null)\" || continue; \
    printf %s \"$m\" | grep -q \"[^A-Za-z0-9/._-]\" && continue; \
    exec \"${SHELL:-/bin/sh}\" -l -c \"exec $m\"; done; \
    echo \"nerdfleet: no nerdmux on this host, so this shell ends with the connection.\"; \
    exec \"${SHELL:-/bin/sh}\" -l";

pub struct Shell {
    /// Tells this pane's events from those of the one it replaced.
    pub id: u32,
    pub pane: Terminal,
    /// The pane's size, columns by rows, as last given to the program.
    pub size: (u16, u16),
}

/// Starts the terminal on `host`, `cols` by `rows`.
pub fn open<E: From<TerminalEvent> + Send + 'static>(
    host: &Host,
    control_dir: &Path,
    id: u32,
    (cols, rows): (u16, u16),
    tx: SyncSender<E>,
) -> anyhow::Result<Shell> {
    let command = match &host.access {
        Access::Ssh(alias) => Ssh::new(alias, control_dir)?.terminal(SCRIPT),
        Access::Local => local_command(),
    };
    let env = environment();
    let cwd = std::env::var_os("HOME").map(Into::into);
    let pane = Terminal::spawn(id, Options { cols, rows, command, cwd, env: &env }, tx)?;
    Ok(Shell { id, pane, size: (cols, rows) })
}

impl Shell {
    /// Program output. Bells are dropped, and so are clipboard writes: a
    /// remote host does not get to set this machine's clipboard.
    pub fn feed(&mut self, bytes: &[u8]) {
        for notice in self.pane.feed(bytes) {
            match notice {
                Notice::Bell | Notice::Clipboard(_) => {}
            }
        }
    }

    /// Gives the program a new size, if it is one.
    pub fn fit(&mut self, cols: u16, rows: u16) {
        if self.size != (cols, rows) {
            self.size = (cols, rows);
            self.pane.resize(cols, rows);
        }
    }

    /// The title the program set, safe to draw.
    pub fn title(&self) -> String {
        crate::text::clean(self.pane.title()).chars().take(80).collect()
    }
}

/// The bar above a terminal: whose it is, what it runs, and the keys that
/// move away from it. `alone`, with the whole screen, it carries the
/// nërdfleet badge; beside the dashboard, that badge is the dashboard's.
pub fn draw_bar(buf: &mut Buffer, area: Rect, theme: &Theme, host: &str, shell: &Shell, focused: bool, alone: bool) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Widget;
    let t = theme;
    buf.set_style(area, Style::default().bg(t.status_bg).fg(t.status_fg));
    let mut left = Vec::new();
    if alone {
        left.push(Span::styled(
            " nërdfleet ",
            Style::default().bg(t.accent).fg(t.accent_fg).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(" › ", Style::default().add_modifier(Modifier::BOLD)));
    } else {
        left.push(Span::raw(" "));
    }
    let name = Style::default().fg(if focused { t.fg } else { t.dim }).add_modifier(Modifier::BOLD);
    left.push(Span::styled(format!("{} ", crate::text::clean(host)), name));
    let title = shell.title();
    if !title.is_empty() {
        left.push(Span::styled(format!(" {title} "), Style::default().fg(t.dim)));
    }
    let left = Line::from(left);
    let left_w = left.width() as u16;
    left.render(area, buf);
    let keys: &[(&str, &str)] =
        if focused { &[("Ctrl+]", "dashboard")] } else { &[("Ctrl+]", "terminal"), ("z", "zoom"), ("s", "close")] };
    let keys = super::ui::keys_line(t, keys);
    let kw = keys.width() as u16;
    if area.width > left_w + kw {
        keys.render(Rect { x: area.x + area.width - kw, width: kw, ..area }, buf);
    }
}

/// The key that leaves a terminal for the dashboard: Ctrl+]. Terminals
/// send it as byte 0x1d, which crossterm reads as Ctrl+5; terminals that
/// report keys in full send Ctrl+].
pub fn is_escape(k: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    k.modifiers == KeyModifiers::CONTROL && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5'))
}

/// The viewer's environment, which carries what SSH needs (the agent socket
/// above all), without nerdmux's own variables: a shell started here is
/// not inside the nerdmux the viewer may be running in, and the terminal
/// never goes to a server named by `NERDMUX_SOCKET`.
fn environment() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !matches!(k.as_str(), "NERDMUX" | "NERDMUX_PANE" | "NERDMUX_SOCKET"))
        .collect()
}

/// On this machine: its nerdmux, or the user's shell. Inside nerdmux the
/// shell: attaching to the session that shows the viewer would draw the
/// viewer inside itself, and that nerdmux is right there anyway.
fn local_command() -> Vec<String> {
    let inside = std::env::var_os("NERDMUX").is_some_and(|v| !v.is_empty());
    match local_nerdmux().filter(|_| !inside) {
        Some(m) => vec![m],
        None => vec![std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into())],
    }
}

/// The first nerdmux on PATH, in `~/.local/bin` or in `~/.cargo/bin`.
fn local_nerdmux() -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut candidates: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("nerdmux")).collect())
        .unwrap_or_default();
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/nerdmux"));
        candidates.push(home.join(".cargo/bin/nerdmux"));
    }
    candidates.into_iter().find(|c| c.is_file()).map(|c| c.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_script_is_one_fixed_command_for_every_login_shell() {
        // It goes inside single quotes, through the remote login shell.
        assert!(!SCRIPT.contains('\'') && !SCRIPT.contains('!') && !SCRIPT.contains('\\'));
        let ssh = Ssh::new("gpu-box", Path::new("/run/user/1000/nerdfleet/ssh")).unwrap();
        let argv = ssh.terminal(SCRIPT);
        assert_eq!(&argv[..2], ["ssh", "-t"]);
        let dashdash = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[dashdash + 1], "gpu-box");
        assert_eq!(argv.len(), dashdash + 3, "one remote command after the alias");
        assert!(!argv[dashdash + 2].contains("umask"), "the user's shell keeps its own umask");
        assert!(argv.contains(&"ForwardAgent=no".to_string()) && argv.contains(&"ClearAllForwardings=yes".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("BatchMode")), "a terminal may ask for a password");
    }

    #[test]
    fn ctrl_bracket_leaves_in_both_spellings() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(is_escape(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(is_escape(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(!is_escape(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert!(!is_escape(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
    }

    /// Runs the script under `shell` with `HOME` set to `home`, `SHELL` set
    /// to `login` and a PATH without nerdmux, and returns what it printed.
    fn run_script(shell: &str, home: &Path, login: &str) -> String {
        let out = std::process::Command::new(shell)
            .arg("-c")
            .arg(SCRIPT)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", login)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    }

    #[test]
    fn the_script_finds_nerdmux_off_path_under_every_sh() {
        use std::os::unix::fs::PermissionsExt;
        // Ubuntu's sh is dash, which reads `[^...]` in a case pattern
        // literally; the script must not depend on it.
        let home = std::env::temp_dir().join(format!("nerdfleet-found-{}", std::process::id()));
        let bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("nerdmux");
        std::fs::write(&fake, "#!/bin/sh\necho \"ran nerdmux with [$*]\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        for shell in ["sh", "dash", "bash"] {
            if std::process::Command::new(shell).arg("-c").arg("true").output().is_err() {
                continue;
            }
            let text = run_script(shell, &home, "/bin/sh");
            assert!(text.contains("ran nerdmux with []"), "{shell}: {text}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_scripts_fallback_runs_under_sh() {
        // With no nerdmux anywhere, the script says so and starts a shell.
        let dir = std::env::temp_dir().join(format!("nerdfleet-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = run_script("sh", &dir, "/bin/echo");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(text.contains("no nerdmux on this host"), "{text}");
        assert!(text.trim_end().ends_with("-l"), "then a login shell: {text}");
    }
}

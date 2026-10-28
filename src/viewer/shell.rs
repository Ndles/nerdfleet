//! Terminals: a shell on a host, in a pane of nerdmux's terminal engine.
//!
//! A shell runs inside `nerdmux attach --plain`, so it outlives the
//! connection and the viewer: the next terminal on that host attaches to the
//! same session. A host without a nerdmux that knows `--plain` gets a plain
//! login shell instead, and the pane says it will not survive. The other
//! kind of terminal is the host's own nerdmux, status bar, keys and all.

use crate::config::{Access, Host};
use crate::remote::Ssh;
use nerdmux::embed::{Notice, Options, Terminal, TerminalEvent, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::path::Path;
use std::sync::mpsc::SyncSender;

/// The nerdmux session every terminal on a host attaches to.
pub const SESSION: &str = "fleet";

/// What the terminal runs on a host: the first nerdmux that knows
/// `--plain`, attached to the fleet session through a login shell (so the
/// session's shells get the same PATH a login would), or else a login
/// shell. Non-interactive SSH commands rarely have `~/.local/bin` or
/// `~/.cargo/bin` on PATH, so those are tried by name. The path ends up in
/// a command line, so one with anything but plain characters is skipped.
const SHELL_SCRIPT: &str = "for n in nerdmux \"$HOME/.local/bin/nerdmux\" \"$HOME/.cargo/bin/nerdmux\"; do \
    m=\"$(command -v \"$n\" 2>/dev/null)\" || continue; \
    printf %s \"$m\" | grep -q \"[^A-Za-z0-9/._-]\" && continue; \
    \"$m\" --help 2>/dev/null | grep -q -- --plain || continue; \
    exec \"${SHELL:-/bin/sh}\" -l -c \"exec $m attach --plain -t fleet\"; done; \
    echo \"nerdfleet: no nerdmux with --plain on this host, so this shell ends with the connection.\"; \
    exec \"${SHELL:-/bin/sh}\" -l";

/// Which program a terminal runs on the host.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// A shell in the fleet session, kept alive by `nerdmux attach --plain`.
    Shell = 0,
    /// The host's nerdmux as the user knows it: their last session, or a new one.
    Nerdmux = 1,
}

/// What `Kind::Nerdmux` runs on a host: the first nerdmux found, through a
/// login shell, as `SHELL_SCRIPT` finds it; or else a login shell.
const NERDMUX_SCRIPT: &str = "for n in nerdmux \"$HOME/.local/bin/nerdmux\" \"$HOME/.cargo/bin/nerdmux\"; do \
    m=\"$(command -v \"$n\" 2>/dev/null)\" || continue; \
    printf %s \"$m\" | grep -q \"[^A-Za-z0-9/._-]\" && continue; \
    exec \"${SHELL:-/bin/sh}\" -l -c \"exec $m\"; done; \
    echo \"nerdfleet: no nerdmux on this host, so this is a plain login shell.\"; \
    exec \"${SHELL:-/bin/sh}\" -l";

pub struct Shell {
    /// Tells this pane's events from those of the one it replaced.
    pub id: u32,
    pub kind: Kind,
    pub pane: Terminal,
    /// The pane's size, columns by rows, as last given to the program.
    pub size: (u16, u16),
    /// The program ended, or the connection dropped.
    pub ended: bool,
}

/// Starts a terminal of `kind` on `host`, `cols` by `rows`.
pub fn open<E: From<TerminalEvent> + Send + 'static>(
    host: &Host,
    kind: Kind,
    control_dir: &Path,
    id: u32,
    (cols, rows): (u16, u16),
    tx: SyncSender<E>,
) -> anyhow::Result<Shell> {
    let command = match (&host.access, kind) {
        (Access::Ssh(alias), Kind::Shell) => Ssh::new(alias, control_dir)?.terminal(SHELL_SCRIPT),
        (Access::Ssh(alias), Kind::Nerdmux) => Ssh::new(alias, control_dir)?.terminal(NERDMUX_SCRIPT),
        (Access::Local, Kind::Shell) => local_command(),
        (Access::Local, Kind::Nerdmux) => local_nerdmux_command()?,
    };
    let env = environment();
    let cwd = std::env::var_os("HOME").map(Into::into);
    let pane = Terminal::spawn(id, Options { cols, rows, command, cwd, env: &env }, tx)?;
    Ok(Shell { id, kind, pane, size: (cols, rows), ended: false })
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

/// The bar above a terminal: whose shell it is, and the way back. A
/// terminal beside the dashboard without the focus says how to get it.
pub fn draw_bar(buf: &mut Buffer, area: Rect, theme: &Theme, host: &str, shell: &Shell, focused: bool) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Widget;
    let t = theme;
    buf.set_style(area, Style::default().bg(t.status_bg).fg(t.status_fg));
    let mut left = vec![
        Span::styled(" nërdfleet ", Style::default().bg(t.accent).fg(t.accent_fg).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" › {} ", crate::text::clean(host)), Style::default().add_modifier(Modifier::BOLD)),
    ];
    if shell.kind == Kind::Nerdmux {
        left.push(Span::styled(" nerdmux ", Style::default().fg(t.dim)));
    }
    let title = shell.title();
    if !title.is_empty() {
        left.push(Span::styled(format!(" {title} "), Style::default().fg(t.dim)));
    }
    if shell.ended {
        left.push(Span::styled(" ended ", Style::default().fg(t.red).add_modifier(Modifier::BOLD)));
    }
    Line::from(left).render(area, buf);
    let keys = match (focused, shell.ended) {
        (false, _) => "Ctrl+] here ",
        (true, true) => "Enter reconnect  Ctrl+] dashboard ",
        (true, false) => "Ctrl+] dashboard ",
    };
    let kw = keys.chars().count() as u16;
    if area.width > kw + 20 {
        Span::styled(keys, Style::default().fg(t.dim))
            .render(Rect { x: area.x + area.width - kw, width: kw, ..area }, buf);
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
/// not inside the nerdmux the viewer may be running in, and the fleet
/// session never goes to a server named by `NERDMUX_SOCKET`.
fn environment() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !matches!(k.as_str(), "NERDMUX" | "NERDMUX_PANE" | "NERDMUX_SOCKET"))
        .collect()
}

/// On this machine: nerdmux if it knows `--plain`, or the user's shell.
fn local_command() -> Vec<String> {
    match local_nerdmux(true) {
        Some(m) => vec![m, "attach".into(), "--plain".into(), "-t".into(), SESSION.into()],
        None => vec![std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into())],
    }
}

/// This machine's nerdmux, as the user knows it. Not from inside a
/// nerdmux: attaching to the session that shows the viewer would draw the
/// viewer inside itself.
fn local_nerdmux_command() -> anyhow::Result<Vec<String>> {
    if std::env::var_os("NERDMUX").is_some_and(|v| !v.is_empty()) {
        anyhow::bail!("nerdfleet already runs inside nerdmux on this machine");
    }
    Ok(vec![local_nerdmux(false).ok_or_else(|| anyhow::anyhow!("no nerdmux on this machine"))?])
}

/// The first nerdmux on PATH, in `~/.local/bin` or in `~/.cargo/bin`;
/// with `plain`, the first that knows `--plain`.
fn local_nerdmux(plain: bool) -> Option<String> {
    use std::process::{Command, Stdio};
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut candidates: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("nerdmux")).collect())
        .unwrap_or_default();
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/nerdmux"));
        candidates.push(home.join(".cargo/bin/nerdmux"));
    }
    candidates.into_iter().filter(|c| c.is_file()).find_map(|c| {
        if !plain {
            return Some(c.to_string_lossy().into_owned());
        }
        let out = Command::new(&c).arg("--help").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        String::from_utf8_lossy(&out.stdout).contains("--plain").then(|| c.to_string_lossy().into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_script_is_one_fixed_command_for_every_login_shell() {
        // It goes inside single quotes, through the remote login shell.
        assert!(!SHELL_SCRIPT.contains('\'') && !SHELL_SCRIPT.contains('!') && !SHELL_SCRIPT.contains('\\'));
        assert!(SHELL_SCRIPT.contains(&format!("-t {SESSION}")));
        let ssh = Ssh::new("gpu-box", Path::new("/run/user/1000/nerdfleet/ssh")).unwrap();
        let argv = ssh.terminal(SHELL_SCRIPT);
        assert_eq!(&argv[..2], ["ssh", "-t"]);
        let dashdash = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(argv[dashdash + 1], "gpu-box");
        assert_eq!(argv.len(), dashdash + 3, "one remote command after the alias");
        assert!(!argv[dashdash + 2].contains("umask"), "the user's shell keeps its own umask");
        assert!(argv.contains(&"ForwardAgent=no".to_string()) && argv.contains(&"ClearAllForwardings=yes".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("BatchMode")), "a terminal may ask for a password");
    }

    #[test]
    fn the_nerdmux_script_is_fixed_too_and_wants_no_plain() {
        assert!(!NERDMUX_SCRIPT.contains('\'') && !NERDMUX_SCRIPT.contains('!') && !NERDMUX_SCRIPT.contains('\\'));
        assert!(!NERDMUX_SCRIPT.contains("--plain") && !NERDMUX_SCRIPT.contains("attach"));
    }

    #[test]
    fn ctrl_bracket_leaves_in_both_spellings() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(is_escape(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)));
        assert!(is_escape(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(!is_escape(&KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert!(!is_escape(&KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL | KeyModifiers::ALT)));
    }

    /// Runs `script` under `shell` with `HOME` set to `home` and a PATH
    /// without nerdmux, and returns what it printed.
    fn run_script(shell: &str, script: &str, home: &Path) -> String {
        let out = std::process::Command::new(shell)
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
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
        std::fs::write(
            &fake,
            "#!/bin/sh\nif [ \"$1\" = --help ]; then echo 'attach --plain'; else echo \"ran $*\"; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        for shell in ["sh", "dash", "bash"] {
            if std::process::Command::new(shell).arg("-c").arg("true").output().is_err() {
                continue;
            }
            let text = run_script(shell, SHELL_SCRIPT, &home);
            assert!(text.contains("ran attach --plain -t fleet"), "{shell}: {text}");
            let text = run_script(shell, NERDMUX_SCRIPT, &home);
            assert!(text.trim_end().ends_with("ran"), "{shell}: nerdmux with no arguments: {text}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_scripts_fallback_runs_under_sh() {
        // With no nerdmux anywhere, the script says so and starts a shell.
        let dir = std::env::temp_dir().join(format!("nerdfleet-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(SHELL_SCRIPT)
            .env_clear()
            .env("HOME", &dir)
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/echo")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("no nerdmux with --plain"), "{text}");
        assert!(text.trim_end().ends_with("-l"), "then a login shell: {text}");
    }
}

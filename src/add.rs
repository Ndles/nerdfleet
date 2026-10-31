//! `nerdfleet add`: picking hosts from `~/.ssh/config` for the fleet.
//!
//! The SSH config is only read for its `Host` names, which go through the
//! same check as any alias in the fleet's config. Hosts are added to the end
//! of the fleet's config as text, so the user's comments and layout stay.

use crate::config::{check_alias, Access, Config, TEMPLATE};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// As deep as OpenSSH follows `Include`.
const MAX_INCLUDES: u32 = 16;

/// The aliases `path` defines, in order: the names on its `Host` lines and
/// those of the files it includes, without patterns and without names that
/// could not be used as an alias.
pub fn aliases(path: &Path, home: &Path) -> Vec<String> {
    let base = path.parent().unwrap_or(Path::new("."));
    let mut found = Vec::new();
    read_config(path, base, home, 0, &mut found);
    found
}

fn read_config(path: &Path, base: &Path, home: &Path, depth: u32, found: &mut Vec<String>) {
    if depth > MAX_INCLUDES {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
        let (key, rest) = line.split_at(end);
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest);
        if key.eq_ignore_ascii_case("host") {
            for name in words(rest) {
                let pattern = name.contains(['*', '?', '!']);
                if !pattern && check_alias(&name).is_ok() && !found.contains(&name) {
                    found.push(name);
                }
            }
        } else if key.eq_ignore_ascii_case("include") {
            for word in words(rest) {
                let pattern = match word.strip_prefix("~/") {
                    Some(tail) => home.join(tail),
                    None => base.join(word),
                };
                for file in expand(&pattern) {
                    read_config(&file, base, home, depth + 1, found);
                }
            }
        }
    }
}

/// A config line's arguments: split at whitespace, with double quotes
/// holding a word together, up to a comment.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() && !rest.starts_with('#') {
        let (word, tail) = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len())),
        };
        out.push(word.to_string());
        rest = tail.trim_start();
    }
    out
}

/// The files a path with `*` and `?` in it matches, sorted within each
/// directory, as OpenSSH reads them.
fn expand(pattern: &Path) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::new()];
    for part in pattern.components() {
        let part = part.as_os_str().to_string_lossy();
        if !part.contains(['*', '?']) {
            paths.iter_mut().for_each(|p| p.push(part.as_ref()));
            continue;
        }
        let mut next = Vec::new();
        for dir in &paths {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut names: Vec<String> =
                entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.retain(|n| !n.starts_with('.') && wild(part.as_bytes(), n.as_bytes()));
            names.sort();
            next.extend(names.iter().map(|n| dir.join(n)));
        }
        paths = next;
    }
    paths
}

fn wild(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some(b'*'), _) => wild(&pattern[1..], name) || (!name.is_empty() && wild(pattern, &name[1..])),
        (Some(b'?'), Some(_)) => wild(&pattern[1..], &name[1..]),
        (Some(a), Some(b)) if a == b => wild(&pattern[1..], &name[1..]),
        _ => false,
    }
}

/// The config `text` with `aliases` added as hosts of the same names. An
/// alias holds only plain characters (`check_alias`), so it is safe to quote.
fn with_hosts(text: &str, aliases: &[String]) -> String {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for a in aliases {
        out.push_str(&format!("\n[[host]]\nname = \"{a}\"\nssh = \"{a}\"\n"));
    }
    out
}

/// Replaces the config at once. A config that is a symlink, as dotfile
/// managers make it, stays one: the file it points to is what changes.
fn write_config(path: &Path, text: &str) -> anyhow::Result<()> {
    let (target, mode) = if path.exists() {
        let real = std::fs::canonicalize(path)?;
        let mode = std::fs::metadata(&real)?.permissions().mode() & 0o777;
        (real, mode)
    } else {
        // A directory already there is checked when the config is next read.
        if let Some(dir) = path.parent().filter(|d| !d.exists()) {
            crate::paths::private_dir(dir)?;
        }
        (path.to_path_buf(), 0o600)
    };
    let tmp = target.with_file_name(format!(".config.toml.{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)))
        .and_then(|()| std::fs::rename(&tmp, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// Whether the fleet already has `alias`, by its name or by its address: a
/// second host of that name would make the config invalid.
fn listed(config: &Config, alias: &str) -> bool {
    config.hosts.iter().any(|h| h.name == alias || matches!(&h.access, Access::Ssh(a) if a == alias))
}

/// `nerdfleet add [ALIAS...]`: the named hosts, or those picked from a list.
pub fn run(args: &[&str], config_path: &Path, ssh_config: &Path, home: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    let found = aliases(ssh_config, home);
    let chosen: Vec<String> = if args.is_empty() {
        if found.is_empty() {
            anyhow::bail!("no Host aliases in {}: add your machines there first", ssh_config.display());
        }
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            anyhow::bail!("picking from a list needs a terminal; name the hosts instead: nerdfleet add ALIAS...");
        }
        let items: Vec<(String, bool)> = found.iter().map(|a| (a.clone(), listed(&config, a))).collect();
        match crate::viewer::pick(&items)? {
            Some(chosen) => chosen,
            None => return Ok(()),
        }
    } else {
        for a in args {
            if !found.iter().any(|f| f == a) {
                anyhow::bail!("{a:?} is not a Host alias in {}", ssh_config.display());
            }
        }
        args.iter().map(|a| a.to_string()).collect()
    };
    let mut new: Vec<String> = Vec::new();
    for a in chosen {
        if listed(&config, &a) {
            println!("{a} is in the fleet already.");
        } else if !new.contains(&a) {
            new.push(a);
        }
    }
    if new.is_empty() {
        println!("Nothing added.");
        return Ok(());
    }
    let text = if config_path.exists() { std::fs::read_to_string(config_path)? } else { TEMPLATE.to_string() };
    let text = with_hosts(&text, &new);
    Config::parse(&text).map_err(|e| anyhow::anyhow!("adding these would break {}: {e}", config_path.display()))?;
    write_config(config_path, &text)?;
    println!("Added {} to {}.", new.join(", "), config_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nerdfleet-add-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    #[test]
    fn aliases_are_read_the_way_ssh_writes_them() {
        let d = scratch("read");
        let path = d.join("config");
        std::fs::write(
            &path,
            "# Host commented-out\n\
             Host gpu-box login-node # the lab\n\
             \tHostName 10.0.0.1\n\
             host=lower.lan\n\
             HOST \"quoted\" *.wild !negated o?e bad;name\n\
             Match host gpu-box\n\
             Host gpu-box\n",
        )
        .unwrap();
        assert_eq!(aliases(&path, &d), ["gpu-box", "login-node", "lower.lan", "quoted"]);
        assert!(aliases(&d.join("missing"), &d).is_empty());
    }

    #[test]
    fn includes_are_followed_with_globs_home_and_a_limit() {
        let d = scratch("include");
        std::fs::create_dir_all(d.join("config.d")).unwrap();
        std::fs::write(d.join("config.d/b.conf"), "Host from-b\n").unwrap();
        std::fs::write(d.join("config.d/a.conf"), "Host from-a\n").unwrap();
        std::fs::write(d.join("config.d/other"), "Host skipped\n").unwrap();
        std::fs::write(d.join("homed"), "Host from-home\n").unwrap();
        let path = d.join("config");
        std::fs::write(&path, "Include config.d/*.conf\nInclude ~/homed\nHost own\n").unwrap();
        assert_eq!(aliases(&path, &d), ["from-a", "from-b", "from-home", "own"]);
        // A file including itself ends.
        std::fs::write(d.join("loop"), "Host looped\nInclude loop\n").unwrap();
        assert_eq!(aliases(&d.join("loop"), &d), ["looped"]);
    }

    #[test]
    fn adding_keeps_what_the_user_wrote() {
        let text = "# my fleet\n[sampling]\ninterval_ms = 500\n\n[[host]]\nname = \"a\"\nlocal = true";
        let out = with_hosts(text, &["gpu-box".to_string()]);
        assert!(out.starts_with(text));
        let c = Config::parse(&out).unwrap();
        assert_eq!(c.interval_ms, 500);
        assert_eq!(c.hosts[1].access, Access::Ssh("gpu-box".into()));
        assert!(Config::parse(&with_hosts(TEMPLATE, &["gpu-box".to_string()])).is_ok());
    }

    #[test]
    fn a_symlinked_config_stays_a_symlink() {
        let d = scratch("link");
        let real = d.join("real.toml");
        std::fs::write(&real, "# mine\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = d.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_config(&link, "# new\n").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "# new\n");
        assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2, "no temporary file left");
    }

    #[test]
    fn a_config_directory_made_by_hand_will_do() {
        let d = scratch("dir");
        let dir = d.join("nerdfleet");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_config(&dir.join("config.toml"), "# new\n").unwrap();
        assert!(Config::load(&dir.join("config.toml")).is_ok());
    }

    #[test]
    fn named_hosts_are_added_once_and_only_from_the_ssh_config() {
        let d = scratch("run");
        let ssh = d.join("ssh_config");
        std::fs::write(&ssh, "Host gpu-box login-node\n").unwrap();
        let conf = d.join("conf/config.toml");
        run(&["gpu-box"], &conf, &ssh, &d).unwrap();
        run(&["gpu-box", "login-node", "login-node"], &conf, &ssh, &d).unwrap();
        let c = Config::load(&conf).unwrap();
        let names: Vec<&str> = c.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["gpu-box", "login-node"]);
        assert!(std::fs::read_to_string(&conf).unwrap().starts_with("# nerdfleet:"), "from the template");
        assert!(run(&["typo"], &conf, &ssh, &d).is_err());
    }
}

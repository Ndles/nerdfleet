//! The fleet: `~/.config/nerdfleet/config.toml`.
//!
//! The config names hosts and nothing about how to reach them: each host is
//! an alias from the user's `~/.ssh/config`, which already holds the
//! address, user, key and jump hosts. There is no place here for a
//! password, token or key, and nothing ships with a default host.

use serde::Deserialize;
use std::path::Path;

pub const TEMPLATE: &str = r#"# nerdfleet: the machines to watch.
#
# Each host is an alias from ~/.ssh/config. Put the address, user, key and
# any jump host there, not here: this file holds no secrets and has no
# place for one.

# [[host]]
# name = "gpu-box"      # the label shown in nerdfleet
# ssh = "gpu-box"       # a Host alias from ~/.ssh/config, or user@host

# The machine nerdfleet runs on, without SSH.
# [[host]]
# name = "laptop"
# local = true

# [sampling]
# interval_ms = 1000    # how often agents measure, 250 to 60000
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub hosts: Vec<Host>,
    pub interval_ms: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Host {
    pub name: String,
    pub access: Access,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    /// Over SSH, by an alias the user's SSH config knows.
    Ssh(String),
    /// This machine: the agent runs as a child process.
    Local,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    host: Vec<RawHost>,
    #[serde(default)]
    sampling: RawSampling,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHost {
    name: String,
    ssh: Option<String>,
    #[serde(default)]
    local: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSampling {
    interval_ms: Option<u32>,
}

impl Config {
    /// Reads the config at `path`, after checking that nobody else could
    /// have written it. A missing file is an empty fleet.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        if !path.exists() {
            return Ok(Config { hosts: Vec::new(), interval_ms: 1000 });
        }
        crate::paths::check_owned(path)?;
        let text = std::fs::read_to_string(path)?;
        Config::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Config> {
        let raw: Raw = toml_edit::de::from_str(text)?;
        let mut hosts: Vec<Host> = Vec::new();
        for h in raw.host {
            check_name(&h.name)?;
            if hosts.iter().any(|o| o.name == h.name) {
                anyhow::bail!("host {:?} is listed twice", h.name);
            }
            let access = match (h.ssh, h.local) {
                (Some(_), true) => anyhow::bail!("host {:?}: use either ssh or local, not both", h.name),
                (None, false) => anyhow::bail!("host {:?}: needs ssh = \"<alias>\" or local = true", h.name),
                (None, true) => Access::Local,
                (Some(alias), false) => {
                    check_alias(&alias).map_err(|e| anyhow::anyhow!("host {:?}: {e}", h.name))?;
                    Access::Ssh(alias)
                }
            };
            hosts.push(Host { name: h.name, access });
        }
        let interval_ms = raw.sampling.interval_ms.unwrap_or(1000);
        if !(crate::model::MIN_INTERVAL_MS..=crate::model::MAX_INTERVAL_MS).contains(&interval_ms) {
            anyhow::bail!("sampling.interval_ms must be between 250 and 60000");
        }
        Ok(Config { hosts, interval_ms })
    }
}

fn check_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        anyhow::bail!("host name {name:?} must be 1 to 64 printable characters");
    }
    Ok(())
}

/// An SSH destination as a plain alias or `user@alias`. Anything else is
/// refused, so a config value can never become an option to `ssh` (a
/// leading `-`) or reach a shell.
pub fn check_alias(alias: &str) -> anyhow::Result<()> {
    let ok_part = |p: &str| {
        !p.is_empty() && !p.starts_with('-') && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let valid = match alias.split_once('@') {
        Some((user, host)) => ok_part(user) && ok_part(host),
        None => ok_part(alias),
    };
    if !valid || alias.len() > 255 {
        anyhow::bail!(
            "ssh = {alias:?} must be an alias from ~/.ssh/config or user@host, using letters, digits, '.', '_' and '-'"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_a_valid_empty_fleet() {
        let c = Config::parse(TEMPLATE).unwrap();
        assert!(c.hosts.is_empty());
        assert_eq!(c.interval_ms, 1000);
    }

    #[test]
    fn hosts_are_read() {
        let c = Config::parse(
            "[[host]]\nname = \"a\"\nssh = \"a\"\n[[host]]\nname = \"me\"\nlocal = true\n[sampling]\ninterval_ms = 500\n",
        )
        .unwrap();
        assert_eq!(c.hosts[0].access, Access::Ssh("a".into()));
        assert_eq!(c.hosts[1].access, Access::Local);
        assert_eq!(c.interval_ms, 500);
    }

    #[test]
    fn aliases_that_could_become_options_or_commands_are_refused() {
        for bad in ["-oProxyCommand=sh", "a b", "a;b", "host`id`", "$(id)", "a@-b", "@a", "a@", "", "a/b", "a\nb"] {
            assert!(check_alias(bad).is_err(), "{bad:?} was accepted");
        }
        for good in ["gpu-box", "gpu-box.lan", "user@login-node", "node_01"] {
            assert!(check_alias(good).is_ok(), "{good:?} was refused");
        }
    }

    #[test]
    fn mistakes_are_reported() {
        assert!(Config::parse("[[host]]\nname = \"a\"\n").is_err());
        assert!(Config::parse("[[host]]\nname = \"a\"\nssh = \"a\"\nlocal = true\n").is_err());
        assert!(Config::parse("[[host]]\nname = \"a\"\nssh = \"a\"\n[[host]]\nname = \"a\"\nssh = \"b\"\n").is_err());
        assert!(Config::parse("[[host]]\nname = \"a\"\nssh = \"a\"\npassword = \"x\"\n").is_err());
        assert!(Config::parse("[sampling]\ninterval_ms = 10\n").is_err());
    }
}

//! Where nerdfleet keeps things, and the checks that keep them private.

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

fn uid() -> u32 {
    // SAFETY: getuid has no failure modes.
    unsafe { libc::getuid() }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var).filter(|d| !d.is_empty()).map(PathBuf::from) {
        // The spec says relative paths are invalid and must be ignored.
        Some(dir) if dir.is_absolute() => dir,
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("nerdfleet")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// What a resident agent remembers between runs.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("nerdfleet")
}

/// Per-session sockets: SSH control masters now, the resident agent's
/// socket later.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from) {
        if dir.is_absolute() {
            return dir.join("nerdfleet");
        }
    }
    let tmp = std::env::var_os("TMPDIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    tmp.unwrap_or_else(|| PathBuf::from("/tmp")).join(format!("nerdfleet-{}", uid()))
}

/// Creates `dir` (mode 0700) if it is missing, and refuses to go on unless
/// it is a real directory, owned by us, that nobody else can enter.
pub fn private_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
        anyhow::bail!(
            "{} must be a directory owned by you and closed to others (chmod 700); refusing to use it",
            dir.display()
        );
    }
    Ok(dir.to_path_buf())
}

/// Refuses a file anyone but us could have written: owned by someone else,
/// or writable by group or others, or in a directory that is. Symlinks are
/// followed, so dotfiles managed with stow or similar still work, and the
/// file they point to is what is checked.
pub fn check_owned(path: &Path) -> anyhow::Result<()> {
    let check = |p: &Path, what: &str| -> anyhow::Result<()> {
        let meta = std::fs::metadata(p)?;
        if meta.uid() != uid() || meta.mode() & 0o022 != 0 {
            anyhow::bail!(
                "{what} {} must be owned by you and not writable by group or others (chmod go-w); refusing to use it",
                p.display()
            );
        }
        Ok(())
    };
    let real = std::fs::canonicalize(path)?;
    check(&real, "the config file")?;
    if let Some(dir) = real.parent() {
        check(dir, "the directory of")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nerdfleet-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn private_dir_refuses_an_open_directory() {
        let base = scratch("private");
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&open).is_err());
        assert!(private_dir(&base.join("fresh")).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn check_owned_refuses_a_world_writable_file() {
        let base = scratch("owned");
        let file = base.join("config.toml");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_owned(&file).is_ok());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(check_owned(&file).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}

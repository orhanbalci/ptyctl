use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Directory holding sockets and logs: `$PTYCTL_DIR`, else `~/.ptyctl`.
pub fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PTYCTL_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".ptyctl"))
}

pub fn ensure_state_dir() -> Result<PathBuf> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    if !ok {
        bail!("invalid session name {name:?}: use letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

pub struct SessionPaths {
    pub socket: PathBuf,
    pub log: PathBuf,
    pub daemon_log: PathBuf,
}

impl SessionPaths {
    pub fn new(dir: &std::path::Path, name: &str) -> Self {
        SessionPaths {
            socket: dir.join(format!("{name}.sock")),
            log: dir.join(format!("{name}.log")),
            daemon_log: dir.join(format!("{name}.daemon.log")),
        }
    }
}

/// Unix socket paths are limited to ~104 bytes (macOS) / 108 (Linux). When the
/// full path is too long, move into its directory and use the bare file name.
/// Only safe in processes that don't depend on their cwd afterwards.
pub fn short_socket_path(socket: &std::path::Path) -> Result<PathBuf> {
    if socket.as_os_str().len() < 100 {
        return Ok(socket.to_path_buf());
    }
    let dir = socket.parent().context("socket path has no parent")?;
    std::env::set_current_dir(dir).with_context(|| format!("entering {}", dir.display()))?;
    Ok(PathBuf::from(
        socket.file_name().context("socket path has no file name")?,
    ))
}

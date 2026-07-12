use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_file: PathBuf,
    pub database: PathBuf,
    pub socket: PathBuf,
    pub lock_file: PathBuf,
    pub sessions_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let project = ProjectDirs::from("dev", "loomterm", "loomterm")
            .ok_or_else(|| Error::Config("could not determine platform directories".into()))?;

        let state_dir = std::env::var_os("LOOMTERM_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| project.data_local_dir().to_path_buf());
        let runtime_dir = std::env::var_os("LOOMTERM_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("XDG_RUNTIME_DIR").map(|p| PathBuf::from(p).join("loomterm"))
            })
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!(
                    "loomterm-{}",
                    nix::unistd::Uid::effective().as_raw()
                ))
            });
        let config_file = std::env::var_os("LOOMTERM_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| project.config_dir().join("config.toml"));

        Ok(Self {
            database: state_dir.join("loom.db"),
            socket: runtime_dir.join("loomd.sock"),
            lock_file: runtime_dir.join("loomd.lock"),
            sessions_dir: state_dir.join("sessions"),
            state_dir,
            runtime_dir,
            config_file,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        create_private_dir(&self.state_dir)?;
        create_private_dir(&self.sessions_dir)?;
        create_private_dir(&self.runtime_dir)?;
        if let Some(parent) = self.config_file.parent() {
            create_private_dir(parent)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub max_concurrent_executions: usize,
    pub capture_limit_bytes: u64,
    pub retention_days: u64,
    pub retention_bytes: u64,
    pub cancel_grace_ms: u64,
    pub shell: String,
    pub supervisor_path: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent_executions: 8,
            capture_limit_bytes: 256 * 1024 * 1024,
            retention_days: 7,
            retention_bytes: 1024 * 1024 * 1024,
            cancel_grace_ms: 2_000,
            shell: "/bin/sh".into(),
            supervisor_path: None,
        }
    }
}

impl Settings {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        let source = fs::read_to_string(&paths.config_file)?;
        let settings: Self = toml::from_str(&source)
            .map_err(|e| Error::Config(format!("{}: {e}", paths.config_file.display())))?;
        if settings.max_concurrent_executions == 0 {
            return Err(Error::Config(
                "max_concurrent_executions must be greater than zero".into(),
            ));
        }
        Ok(settings)
    }
}

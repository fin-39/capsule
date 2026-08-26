//! Installation of Capsule's per-user playback-only PipeWire policy.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const PIPEWIRE_PULSE_POLICY: &str = include_str!("../../assets/pipewire/60-capsule-playback.conf");
const PIPEWIRE_SINK_POLICY: &str =
    include_str!("../../assets/pipewire/60-capsule-playback-sink.conf");
const WIREPLUMBER_POLICY: &str = include_str!("../../assets/wireplumber/60-capsule-playback.conf");

const POLICY_FILES: [(&str, &str); 3] = [
    (
        "pipewire/pipewire-pulse.conf.d/60-capsule-playback.conf",
        PIPEWIRE_PULSE_POLICY,
    ),
    (
        "pipewire/pipewire.conf.d/60-capsule-playback-sink.conf",
        PIPEWIRE_SINK_POLICY,
    ),
    (
        "wireplumber/wireplumber.conf.d/60-capsule-playback.conf",
        WIREPLUMBER_POLICY,
    ),
];

pub fn install_user_policy() -> Result<Vec<PathBuf>, AudioIntegrationError> {
    let config_root = user_config_root()?;
    let installed = install_policy_at(&config_root)?;
    restart_audio_services()?;
    Ok(installed)
}

fn user_config_root() -> Result<PathBuf, AudioIntegrationError> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path);
        }
        return Err(AudioIntegrationError::UnsafeConfigRoot(path));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(AudioIntegrationError::MissingConfigRoot)?;
    if !home.is_absolute() {
        return Err(AudioIntegrationError::UnsafeConfigRoot(home));
    }
    Ok(home.join(".config"))
}

fn install_policy_at(config_root: &Path) -> Result<Vec<PathBuf>, AudioIntegrationError> {
    if !config_root.is_absolute() {
        return Err(AudioIntegrationError::UnsafeConfigRoot(
            config_root.to_path_buf(),
        ));
    }
    // Validate all existing destinations before changing any of them. A
    // customized fragment must never leave the other two files half-updated.
    for (relative, contents) in POLICY_FILES {
        let destination = config_root.join(relative);
        if let Ok(metadata) = fs::symlink_metadata(&destination) {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(AudioIntegrationError::UnsafePolicyPath(destination));
            }
            let existing = fs::read_to_string(&destination)
                .map_err(|source| io_error(&destination, source))?;
            if existing != contents {
                return Err(AudioIntegrationError::PolicyConflict(destination));
            }
        }
    }

    let mut installed = Vec::with_capacity(POLICY_FILES.len());
    for (relative, contents) in POLICY_FILES {
        let destination = config_root.join(relative);
        let parent = destination
            .parent()
            .ok_or_else(|| AudioIntegrationError::UnsafePolicyPath(destination.clone()))?;
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        if destination.is_file() {
            installed.push(destination);
            continue;
        }

        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))
            .and_then(|()| temporary.write_all(contents.as_bytes()))
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| io_error(&destination, source))?;
        temporary
            .persist(&destination)
            .map_err(|error| io_error(&destination, error.error))?;
        installed.push(destination);
    }
    Ok(installed)
}

fn restart_audio_services() -> Result<(), AudioIntegrationError> {
    let systemctl = std::env::var_os("CAPSULE_SYSTEMCTL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/systemctl"));
    if !systemctl.is_absolute() {
        return Err(AudioIntegrationError::UnsafeSystemctl(systemctl));
    }
    let status = Command::new(&systemctl)
        .args([
            "--user",
            "restart",
            "pipewire.service",
            "pipewire-pulse.service",
            "wireplumber.service",
        ])
        .status()
        .map_err(|source| AudioIntegrationError::StartSystemctl {
            path: systemctl.clone(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioIntegrationError::RestartFailed(status))
    }
}

fn io_error(path: &Path, source: io::Error) -> AudioIntegrationError {
    AudioIntegrationError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AudioIntegrationError {
    #[error("XDG_CONFIG_HOME or HOME is required to install audio integration")]
    MissingConfigRoot,
    #[error("audio configuration root must be absolute: {0:?}")]
    UnsafeConfigRoot(PathBuf),
    #[error("refusing to replace a non-regular audio policy path: {0:?}")]
    UnsafePolicyPath(PathBuf),
    #[error("an existing Capsule audio policy was modified; review it manually: {0:?}")]
    PolicyConflict(PathBuf),
    #[error("audio integration I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("systemctl path must be absolute: {0:?}")]
    UnsafeSystemctl(PathBuf),
    #[error("could not start {path:?}: {source}")]
    StartSystemctl {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("audio services did not restart successfully: {0}")]
    RestartFailed(std::process::ExitStatus),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_only_the_three_owned_policy_files() {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("config");
        let installed = install_policy_at(&config).unwrap();
        assert_eq!(installed.len(), 3);
        for (relative, contents) in POLICY_FILES {
            assert_eq!(fs::read_to_string(config.join(relative)).unwrap(), contents);
        }
    }

    #[test]
    fn refuses_to_overwrite_a_modified_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("config");
        let path = config.join(POLICY_FILES[0].0);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "user customization\n").unwrap();
        assert!(matches!(
            install_policy_at(&config),
            Err(AudioIntegrationError::PolicyConflict(conflict)) if conflict == path
        ));
        assert!(!config.join(POLICY_FILES[1].0).exists());
        assert!(!config.join(POLICY_FILES[2].0).exists());
    }
}

use std::env;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub library_file: PathBuf,
    pub capsules_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub icons_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self, PathError> {
        let data_root = match env::var_os("XDG_DATA_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => {
                let home = env::var_os("HOME").filter(|path| !path.is_empty()).ok_or(
                    PathError::MissingEnvironment {
                        variables: "XDG_DATA_HOME or HOME",
                    },
                )?;
                PathBuf::from(home).join(".local/share")
            }
        };
        let cache_root = match env::var_os("XDG_CACHE_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => {
                let home = env::var_os("HOME").filter(|path| !path.is_empty()).ok_or(
                    PathError::MissingEnvironment {
                        variables: "XDG_CACHE_HOME or HOME",
                    },
                )?;
                PathBuf::from(home).join(".cache")
            }
        };
        Ok(Self::with_roots(
            data_root.join("capsule"),
            cache_root.join("capsule"),
        ))
    }

    pub fn under(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let cache_dir = data_dir.join("cache");
        Self::with_roots(data_dir, cache_dir)
    }

    fn with_roots(data_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            library_file: data_dir.join("library.json"),
            capsules_dir: data_dir.join("capsules"),
            icons_dir: cache_dir.join("icons"),
            logs_dir: cache_dir.join("logs"),
            cache_dir,
            data_dir,
        }
    }

    pub fn ensure(&self) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.capsules_dir)?;
        std::fs::create_dir_all(&self.icons_dir)?;
        std::fs::create_dir_all(&self.logs_dir)
    }

    pub fn capsule_path(&self, name: &str) -> PathBuf {
        self.capsules_dir
            .join(format!("{}.capsule", safe_file_stem(name)))
    }

    /// Disposable artwork derived from the executable inside a capsule.
    pub fn icon_path(&self, id: uuid::Uuid) -> PathBuf {
        self.icons_dir.join(format!("{}.png", id.simple()))
    }

    pub fn legacy_icon_path(&self, id: uuid::Uuid) -> PathBuf {
        self.icons_dir.join(format!("{}.ico", id.simple()))
    }

    pub fn launch_log_path(&self, id: uuid::Uuid) -> PathBuf {
        self.logs_dir.join(format!("{}.log", id.simple()))
    }
}

pub fn safe_file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => character,
            _ => '-',
        })
        .collect();
    let stem = stem.trim_matches(['-', '.']);
    if stem.is_empty() {
        "untitled".into()
    } else {
        stem.to_owned()
    }
}

pub fn runtime_root() -> Result<PathBuf, PathError> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .ok_or(PathError::MissingEnvironment {
            variables: "XDG_RUNTIME_DIR",
        })?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(PathError::NotAbsolute(runtime));
    }
    Ok(runtime.join("capsule"))
}

pub fn is_safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

#[derive(Debug, Error)]
pub enum PathError {
    #[error("missing required environment: {variables}")]
    MissingEnvironment { variables: &'static str },
    #[error("runtime path is not absolute: {0}")]
    NotAbsolute(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stems_are_portable_and_never_empty() {
        assert_eq!(safe_file_stem("Legacy Game"), "Legacy-Game");
        assert_eq!(safe_file_stem("strange app.exe"), "strange-app.exe");
        assert_eq!(safe_file_stem("../../../"), "untitled");
    }

    #[test]
    fn relative_path_validation_rejects_escape() {
        assert!(is_safe_relative(Path::new("drive_c/game.exe")));
        assert!(!is_safe_relative(Path::new("../game.exe")));
        assert!(!is_safe_relative(Path::new("/game.exe")));
    }

    #[test]
    fn launch_logs_use_stable_uuid_file_names() {
        let paths = AppPaths::under("/tmp/capsule-path-test");
        let id = uuid::Uuid::parse_str("03541d61-eebe-4325-b121-5b0b8aa9338a").unwrap();

        assert_eq!(
            paths.launch_log_path(id),
            paths.logs_dir.join("03541d61eebe4325b1215b0b8aa9338a.log")
        );
    }
}
